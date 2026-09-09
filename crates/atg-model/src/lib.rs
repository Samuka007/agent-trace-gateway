//! Langfuse semantic vocabulary + the turn data model. This is the single
//! home of attribute keys, tag constants and usage-bucket semantics — zero
//! protocol knowledge, zero harness knowledge; upper layers translate INTO
//! this vocabulary (trace/export is the only OTLP wire translation layer).
use serde::Serialize;

/// Langfuse vocabulary constants, aligned with sub2api modeltrace
/// (backend/internal/modeltrace/middleware.go): same attribute keys and the
/// line tag let Langfuse-side queries aggregate both producers identically.
pub const LANGFUSE_TRACE_NAME: &str = "agent.turn";
pub const LANGFUSE_TRACE_TAG: &str = "line:atg";

/// agent is a first-class observation type (2025-08): the agent.turn root
/// span carries it. usage/cost/completionStartTime are generation-exclusive —
/// they live on the generation child span, never on the agent span (values
/// set on an agent span are silently ignored by ingestion).
pub const ATTR_OBSERVATION_TYPE: &str = "langfuse.observation.type";
pub const OBSERVATION_TYPE_AGENT: &str = "agent";
pub const OBSERVATION_TYPE_GENERATION: &str = "generation";
pub const GENERATION_SPAN_NAME: &str = "agent.turn.generation";

/// Official observation content keys (UI panel reads input/output).
pub const ATTR_OBSERVATION_INPUT: &str = "langfuse.observation.input";
pub const ATTR_OBSERVATION_OUTPUT: &str = "langfuse.observation.output";
/// Trace-level end-user identity (copied to every span like session).
pub const ATTR_USER_ID: &str = "langfuse.user.id";
/// Generation-exclusive model name.
pub const ATTR_MODEL_NAME: &str = "langfuse.observation.model.name";
/// Ingestion version header (v4 = current Langfuse OTLP protocol).
pub const INGESTION_VERSION_HEADER: &str = "x-langfuse-ingestion-version";
pub const INGESTION_VERSION: &str = "4";

/// gen_ai.* attribute keys (values written only when Some).
pub const ATTR_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const ATTR_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub const ATTR_USAGE_TOTAL_TOKENS: &str = "gen_ai.usage.total_tokens";
pub const ATTR_USAGE_DETAILS: &str = "langfuse.observation.usage_details";

/// Per-turn token usage in Langfuse's mutually-exclusive bucket form.
/// `input_tokens` is a DERIVED exclusive bucket: for Inclusive protocols
/// (OpenAI responses/chat/live), `usage_from_obj` has already subtracted
/// cache_read/cache_creation from the protocol-reported input count — the
/// protocol-original value survives only in `raw_response`. Consumers of
/// this struct (records endpoint, exporters) must NOT subtract the cache
/// buckets again — that would double-discount. Exclusive protocols
/// (Anthropic) report natively exclusive buckets and pass through
/// unchanged. Missing fields are `None` (unreported) — never defaulted
/// to 0, so Langfuse-side fill-rate metrics stay truthful.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct TurnUsage {
    /// Derived exclusive bucket: for Inclusive protocols the cache counts
    /// are already subtracted here (see atg_protocol::usage::usage_from_obj;
    /// the original count lives in raw_response).
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl TurnUsage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_creation_tokens.is_none()
            && self.total_tokens.is_none()
    }
}

/// Merge a partial frame usage into an accumulator: Some fields win, None
/// fields keep the earlier value (anthropic splits input/output across
/// message_start and message_delta).
pub fn merge_usage(acc: &mut Option<TurnUsage>, next: TurnUsage) {
    match acc {
        Some(cur) => {
            if next.input_tokens.is_some() {
                cur.input_tokens = next.input_tokens;
            }
            if next.output_tokens.is_some() {
                cur.output_tokens = next.output_tokens;
            }
            if next.cache_read_tokens.is_some() {
                cur.cache_read_tokens = next.cache_read_tokens;
            }
            if next.cache_creation_tokens.is_some() {
                cur.cache_creation_tokens = next.cache_creation_tokens;
            }
            if next.total_tokens.is_some() {
                cur.total_tokens = next.total_tokens;
            }
        }
        None => *acc = Some(next),
    }
}

/// Build the `langfuse.observation.usage_details` attribute value in the OTLP
/// KV form modeltrace writes (a JSON object keyed by usage dimension).
/// Entries appear only for reported, NON-ZERO dimensions: Langfuse treats
/// usage_details keys as optional per-bucket counters and derives `total`
/// as the sum of the buckets present ("total is the sum of the buckets",
/// token-and-cost-tracking), so a 0-valued bucket is indistinguishable
/// from an unreported one — omitting zeros is loss-free and kills the
/// canary's `{"input":0,"output":0}` idle-turn noise. (Anthropic never
/// reports `total`; Langfuse derives it — likewise omitted.)
pub fn usage_details_json(usage: &TurnUsage) -> String {
    let mut detail = serde_json::Map::new();
    let mut put = |key: &str, val: Option<u64>| {
        if val.is_some_and(|v| v > 0) {
            detail.insert(key.to_string(), serde_json::Value::from(val.unwrap()));
        }
    };
    put("input", usage.input_tokens);
    put("output", usage.output_tokens);
    put("cache_read_input_tokens", usage.cache_read_tokens);
    put("cache_creation_input_tokens", usage.cache_creation_tokens);
    put("total", usage.total_tokens);
    serde_json::Value::Object(detail).to_string()
}

/// One completed agent/model turn — the unit the trace layer stores,
/// exports and the `/__atg/records` endpoint serves.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TurnRecord {
    pub protocol: String,
    pub session_id: String,
    pub user_input: String,
    pub final_output: String,
    /// Verbatim business content (D4 content fidelity): original request and
    /// response bodies. Transport-layer noise (per-hop headers, TCP metadata)
    /// is never captured here because only body bytes are accumulated.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub raw_request: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub raw_response: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub breakpoint: bool,
    /// Turn timing in unix nanoseconds (0 = unknown).
    #[serde(default)]
    pub start_ns: u64,
    #[serde(default)]
    pub end_ns: u64,
    /// Token usage reported by the model protocol (None = not reported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    /// Model name from the request body (generation-exclusive attribute).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_name: String,
    /// End-user identity (langfuse.user.id; OpenAI user/safety_identifier).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_id: String,
    /// Protocol error marker (response.failed / event:error) when the turn
    /// terminated abnormally; serde-skipped when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Harness attribution (claude-code / codex / grok / opencode); empty
    /// when unidentified (never blocks session extraction).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub harness: String,
    /// Same-strength identification conflicts (CC header + codex body etc.)
    /// — recorded as metadata, never force-disambiguated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness_candidates: Vec<String>,
    /// Harness evidence hit outside its declared protocols (mimicry noise).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub harness_anomaly: bool,
    /// Harness identity enrichments (cc_account, codex_installation) —
    /// emitted as langfuse.trace.metadata.<key>.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness_enrich: Vec<(String, String)>,
    /// The session id was synthesized by the prefix stitcher (not observed
    /// on the wire) — exported as langfuse.trace.metadata.session_synthetic
    /// and excluded from session hit-rate numerators (§E ruling).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_synthetic: bool,
}

/// One tool invocation observed in a turn.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_usage_is_not_exported() {
        let u = TurnUsage::default();
        assert!(u.is_empty());
    }

    /// AMB-2: zero-valued buckets are omitted — 0 ≙ unreported. Langfuse
    /// derives `total` from the buckets present, so this is loss-free and
    /// kills the canary's `{"input":0,"output":0}` idle-turn noise.
    #[test]
    fn usage_details_skips_zero_entries() {
        let u = TurnUsage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_creation_tokens: Some(0),
            total_tokens: Some(0),
        };
        assert_eq!(usage_details_json(&u), "{}");
        let u = TurnUsage {
            input_tokens: Some(5),
            output_tokens: Some(0),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            total_tokens: Some(5),
        };
        assert_eq!(usage_details_json(&u), r#"{"input":5,"total":5}"#);
    }
}
