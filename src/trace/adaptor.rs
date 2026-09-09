//! Langfuse adaptor: the single translation layer from sub2api modeltrace
//! semantics to OTLP/Langfuse vocabulary. Attribute keys, tag constants and
//! usage_details assembly live here so vocabulary changes never scatter
//! across export/unpack code.
use crate::trace::descriptor::resolve_path;
use serde_json::Value;

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
    /// are already subtracted here (see usage_from_obj; the original count
    /// lives in raw_response).
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

/// Extract usage facts from one SSE frame (single traversal — called inside
/// the reassembly loop). Delegates to the descriptor table's usage frames.
pub fn usage_from_sse_frame(
    d: &crate::trace::descriptor::ProtocolDescriptor,
    v: &Value,
) -> Option<TurnUsage> {
    let event = v["type"].as_str();
    for uf in d.usage_frames {
        let matches = match (uf.on_event, event) {
            (Some(ev), Some(e)) => ev == e,
            (None, _) => true,
            (Some(_), None) => false,
        };
        if matches {
            let obj = crate::trace::descriptor::resolve_path(v, uf.obj_path);
            if !obj.is_null() {
                return Some(usage_from_obj(d, obj));
            }
        }
    }
    None
}

/// Extract usage facts from one non-streaming response body (single parse;
/// caller already has the parsed body).
pub fn usage_from_nonstreaming(
    d: &crate::trace::descriptor::ProtocolDescriptor,
    resp: &Value,
) -> Option<TurnUsage> {
    // Non-streaming response bodies always carry usage at the top level,
    // regardless of the streaming frame geometry (message.usage is an SSE
    // concern only).
    let obj = crate::trace::descriptor::resolve_path(resp, &["usage"]);
    if obj.is_null() {
        return None;
    }
    Some(usage_from_obj(d, obj))
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

/// Build TurnUsage from an already-located usage object per protocol.
/// UsageShape fields are full nested paths (e.g. responses'
/// input_tokens_details.cached_tokens), each alternative tried via
/// resolve_path — never flattened to top-level key probes.
pub fn usage_from_obj(
    d: &crate::trace::descriptor::ProtocolDescriptor,
    usage: &Value,
) -> TurnUsage {
    let shape = &d.usage_shape;
    let read = |alts: &[&[&str]]| -> Option<u64> {
        alts.iter().find_map(|p| opt_u64(resolve_path(usage, p)))
    };
    let mut u = TurnUsage {
        input_tokens: read(shape.input),
        output_tokens: read(shape.output),
        cache_read_tokens: read(shape.cache_read),
        cache_creation_tokens: read(shape.cache_write),
        total_tokens: opt_u64(&usage["total_tokens"]),
    };
    // Exclusive-bucket derivation, official basis:
    // - OpenAI: the `input_tokens_details` (responses/live) /
    //   `prompt_tokens_details` (chat) sub-counts — cached_tokens AND
    //   cache_write_tokens — are INCLUSIVE, i.e. contained in the parent
    //   input/prompt count. OpenAI's per-run spending controller cookbook
    //   (developers.openai.com/cookbook, actual_cost) prices "ordinary"
    //   input as input_tokens - cached - written, proving both details
    //   sub-fields sit inside input_tokens (cache-write is billed
    //   separately but counted inside).
    // - Langfuse: token-and-cost-tracking, "Usage types are mutually
    //   exclusive buckets" — every usage_details key is a non-overlapping
    //   bucket, and the normalization table says flat
    //   `langfuse.observation.usage_details` is "stored unchanged; values
    //   must already be exclusive" — so the subtraction MUST happen here,
    //   producer-side (Langfuse does not normalize flat keys).
    // (Anthropic's input is already exclusive — unchanged.)
    if d.usage_inclusion == crate::trace::descriptor::TokenInclusion::Inclusive {
        if let (Some(input), Some(cached)) = (u.input_tokens, u.cache_read_tokens) {
            u.input_tokens = Some(input.saturating_sub(cached));
        }
        if let (Some(input), Some(creation)) = (u.input_tokens, u.cache_creation_tokens) {
            u.input_tokens = Some(input.saturating_sub(creation));
        }
    }
    u
}

fn opt_u64(v: &Value) -> Option<u64> {
    v.as_u64()
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
            detail.insert(key.to_string(), Value::from(val.unwrap()));
        }
    };
    put("input", usage.input_tokens);
    put("output", usage.output_tokens);
    put("cache_read_input_tokens", usage.cache_read_tokens);
    put("cache_creation_input_tokens", usage.cache_creation_tokens);
    put("total", usage.total_tokens);
    Value::Object(detail).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn anthropic_usage_splits_across_frames() {
        let start = frame(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}}"#,
        );
        let delta = frame(r#"{"type":"message_delta","usage":{"output_tokens":7}}"#);
        let mut acc: Option<TurnUsage> = None;
        let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name("anthropic.messages")
            .unwrap();
        if let Some(u) = usage_from_sse_frame(d, &start) {
            merge_usage(&mut acc, u);
        }
        if let Some(u) = usage_from_sse_frame(d, &delta) {
            merge_usage(&mut acc, u);
        }
        let u = acc.expect("usage accumulated");
        assert_eq!(u.input_tokens, Some(12));
        assert_eq!(u.output_tokens, Some(7));
        assert_eq!(u.cache_read_tokens, Some(3));
        assert_eq!(u.cache_creation_tokens, Some(4));
        assert_eq!(u.total_tokens, None);
        assert_eq!(
            usage_details_json(&u),
            // Official snake_case cache bucket keys.
            r#"{"cache_creation_input_tokens":4,"cache_read_input_tokens":3,"input":12,"output":7}"#
        );
    }

    #[test]
    fn responses_completed_carries_full_usage() {
        let done = frame(
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":11,"output_tokens":22,"total_tokens":33}}}"#,
        );
        let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.responses")
            .unwrap();
        let u = usage_from_sse_frame(d, &done).expect("usage");
        assert_eq!(u.input_tokens, Some(11));
        assert_eq!(u.output_tokens, Some(22));
        assert_eq!(u.total_tokens, Some(33));
        assert_eq!(u.cache_read_tokens, None);
    }

    #[test]
    fn chat_final_chunk_reports_usage() {
        let chunk = frame(
            r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
        );
        let d =
            crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.chat_completions")
                .unwrap();
        let u = usage_from_sse_frame(d, &chunk).expect("usage");
        assert_eq!(u.input_tokens, Some(8));
        assert_eq!(u.output_tokens, Some(2));
        assert_eq!(u.total_tokens, Some(10));
        // Frames without usage must not disturb the accumulator.
        assert!(
            usage_from_sse_frame(d, &frame(r#"{"choices":[{"delta":{"content":"x"}}]}"#)).is_none()
        );
    }

    #[test]
    fn nonstreaming_bodies_yield_usage() {
        let anth = frame(
            r#"{"id":"m","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}"#,
        );
        let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name("anthropic.messages")
            .unwrap();
        let u = usage_from_nonstreaming(d, &anth).unwrap();
        assert_eq!(u.input_tokens, Some(5));
        assert_eq!(u.cache_read_tokens, Some(3));
        let chat = frame(
            r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
        );
        let d =
            crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.chat_completions")
                .unwrap();
        let u = usage_from_nonstreaming(d, &chat)
            .expect("chat nonstreaming usage should resolve from top-level usage object");
        assert_eq!(u.input_tokens, Some(8));
        assert_eq!(u.total_tokens, Some(10));
        // Missing usage object => None, never zeros.
        let bare = frame(r#"{"id":"m"}"#);
        let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.responses")
            .unwrap();
        assert!(usage_from_nonstreaming(d, &bare).is_none());
    }

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

    /// R1 nails: openai cache tokens arrive on NESTED paths — each
    /// protocol's details bucket must resolve via resolve_path, never as a
    /// flattened top-level key probe (slop#3 regression pin).
    #[test]
    fn responses_cached_tokens_resolves_nested_path() {
        let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.responses")
            .unwrap();
        let v = frame(
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":100,"output_tokens":22,"total_tokens":122,"input_tokens_details":{"cached_tokens":64,"cache_write_tokens":2}}}}"#,
        );
        let u = usage_from_sse_frame(d, &v).expect("usage");
        // P0-2: inclusive input is derived exclusive (100 - 64 - 2 = 34).
        assert_eq!(u.input_tokens, Some(34));
        assert_eq!(u.output_tokens, Some(22));
        assert_eq!(u.total_tokens, Some(122));
        assert_eq!(u.cache_read_tokens, Some(64), "nested details path");
        assert_eq!(u.cache_creation_tokens, Some(2));
    }

    #[test]
    fn chat_cached_tokens_resolves_nested_path() {
        let d =
            crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.chat_completions")
                .unwrap();
        let v = frame(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":40}}}"#,
        );
        let u = usage_from_sse_frame(d, &v).expect("usage");
        // P0-2: inclusive prompt_tokens (100) derives exclusive input (60).
        assert_eq!(u.input_tokens, Some(60));
        assert_eq!(u.output_tokens, Some(5));
        assert_eq!(u.total_tokens, Some(105));
        assert_eq!(u.cache_read_tokens, Some(40), "nested details path");
    }

    /// P0-2: inclusive protocols derive the exclusive input bucket —
    /// input(100 with cached 64 + creation 2 inside) exports as 34.
    #[test]
    fn inclusive_input_derives_exclusive_bucket() {
        let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.responses")
            .unwrap();
        let v = frame(
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":100,"output_tokens":22,"total_tokens":122,"input_tokens_details":{"cached_tokens":64,"cache_write_tokens":2}}}}"#,
        );
        let u = usage_from_sse_frame(d, &v).expect("usage");
        assert_eq!(
            u.input_tokens,
            Some(34),
            "inclusive input must be derived exclusive: input - cached - write"
        );
        assert_eq!(u.cache_read_tokens, Some(64));
        assert_eq!(u.cache_creation_tokens, Some(2));
        // usage_details reflects the derived buckets.
        assert_eq!(
            usage_details_json(&u),
            r#"{"cache_creation_input_tokens":2,"cache_read_input_tokens":64,"input":34,"output":22,"total":122}"#
        );
    }

    #[test]
    fn live_cached_tokens_resolves_nested_path() {
        let d =
            crate::trace::descriptor::ProtocolDescriptor::detect_by_name("openai.live").unwrap();
        let v = frame(
            r#"{"type":"response.done","response":{"status":"completed","usage":{"input_tokens":100,"output_tokens":8,"total_tokens":108,"input_token_details":{"cached_tokens":30}}}}"#,
        );
        let u = usage_from_sse_frame(d, &v).expect("usage");
        // P0-2: inclusive live input derives exclusive input (70).
        assert_eq!(u.input_tokens, Some(70));
        assert_eq!(u.output_tokens, Some(8));
        assert_eq!(u.total_tokens, Some(108));
        assert_eq!(u.cache_read_tokens, Some(30), "nested live details path");
    }
}
