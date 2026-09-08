//! Langfuse adaptor: the single translation layer from sub2api modeltrace
//! semantics to OTLP/Langfuse vocabulary. Attribute keys, tag constants and
//! usage_details assembly live here so vocabulary changes never scatter
//! across export/unpack code.
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

/// gen_ai.* attribute keys (values written only when Some).
pub const ATTR_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const ATTR_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub const ATTR_USAGE_TOTAL_TOKENS: &str = "gen_ai.usage.total_tokens";
pub const ATTR_USAGE_DETAILS: &str = "langfuse.observation.usage_details";

/// Per-turn token usage as reported by the model protocol. Missing fields are
/// `None` (the protocol simply did not report them) — never defaulted to 0,
/// so Langfuse-side fill-rate metrics stay truthful.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct TurnUsage {
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

/// Extract usage facts from one SSE frame already parsed as JSON, inside the
/// reassembly loop (zero extra passes over the body). Returns Some(usage)
/// only for frames that carry usage data.
pub fn usage_from_sse_frame(protocol: &str, v: &Value) -> Option<TurnUsage> {
    match protocol {
        // message_start: input side; message_delta: output side. Neither
        // carries both, so partial updates are merged by the caller.
        "anthropic.messages" => {
            let usage = match v["type"].as_str()? {
                "message_start" => &v["message"]["usage"],
                "message_delta" => &v["usage"],
                _ => return None,
            };
            if usage.is_null() {
                return None;
            }
            Some(TurnUsage {
                input_tokens: opt_u64(&usage["input_tokens"]),
                output_tokens: opt_u64(&usage["output_tokens"]),
                cache_read_tokens: opt_u64(&usage["cache_read_input_tokens"]),
                cache_creation_tokens: opt_u64(&usage["cache_creation_input_tokens"]),
                total_tokens: None,
            })
        }
        // response.completed carries the full usage object once.
        "openai.responses" => {
            if v["type"].as_str()? != "response.completed" {
                return None;
            }
            let usage = &v["response"]["usage"];
            if usage.is_null() {
                return None;
            }
            Some(TurnUsage {
                input_tokens: opt_u64(&usage["input_tokens"]),
                output_tokens: opt_u64(&usage["output_tokens"]),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                total_tokens: opt_u64(&usage["total_tokens"]),
            })
        }
        // Chat streams a usage-only final chunk (choices empty).
        "openai.chat_completions" => {
            let usage = &v["usage"];
            if usage.is_null() {
                return None;
            }
            Some(TurnUsage {
                input_tokens: opt_u64(&usage["prompt_tokens"]),
                output_tokens: opt_u64(&usage["completion_tokens"]),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                total_tokens: opt_u64(&usage["total_tokens"]),
            })
        }
        _ => None,
    }
}

/// Extract usage facts from one non-streaming response body (single parse;
/// caller already has the parsed body).
pub fn usage_from_nonstreaming(protocol: &str, resp: &Value) -> Option<TurnUsage> {
    let usage = match protocol {
        "anthropic.messages" => &resp["usage"],
        "openai.responses" => &resp["usage"],
        "openai.chat_completions" => &resp["usage"],
        _ => return None,
    };
    if usage.is_null() {
        return None;
    }
    Some(match protocol {
        "anthropic.messages" => TurnUsage {
            input_tokens: opt_u64(&usage["input_tokens"]),
            output_tokens: opt_u64(&usage["output_tokens"]),
            cache_read_tokens: opt_u64(&usage["cache_read_input_tokens"]),
            cache_creation_tokens: opt_u64(&usage["cache_creation_input_tokens"]),
            total_tokens: None,
        },
        _ => TurnUsage {
            input_tokens: opt_u64(&usage["input_tokens"])
                .or_else(|| opt_u64(&usage["prompt_tokens"])),
            output_tokens: opt_u64(&usage["output_tokens"])
                .or_else(|| opt_u64(&usage["completion_tokens"])),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            total_tokens: opt_u64(&usage["total_tokens"]),
        },
    })
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

fn opt_u64(v: &Value) -> Option<u64> {
    v.as_u64()
}

/// Build the `langfuse.observation.usage_details` attribute value in the OTLP
/// KV form modeltrace writes (a JSON object keyed by usage dimension; entries
/// appear only for reported dimensions).
pub fn usage_details_json(usage: &TurnUsage) -> String {
    let mut detail = serde_json::Map::new();
    if let Some(v) = usage.input_tokens {
        detail.insert("input".to_string(), Value::from(v));
    }
    if let Some(v) = usage.output_tokens {
        detail.insert("output".to_string(), Value::from(v));
    }
    if let Some(v) = usage.cache_read_tokens {
        detail.insert("cache_read_input_tokens".to_string(), Value::from(v));
    }
    if let Some(v) = usage.cache_creation_tokens {
        detail.insert("cache_creation_input_tokens".to_string(), Value::from(v));
    }
    if let Some(v) = usage.total_tokens {
        detail.insert("total".to_string(), Value::from(v));
    }
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
        if let Some(u) = usage_from_sse_frame("anthropic.messages", &start) {
            merge_usage(&mut acc, u);
        }
        if let Some(u) = usage_from_sse_frame("anthropic.messages", &delta) {
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
        let u = usage_from_sse_frame("openai.responses", &done).expect("usage");
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
        let u = usage_from_sse_frame("openai.chat_completions", &chunk).expect("usage");
        assert_eq!(u.input_tokens, Some(8));
        assert_eq!(u.output_tokens, Some(2));
        assert_eq!(u.total_tokens, Some(10));
        // Frames without usage must not disturb the accumulator.
        assert!(usage_from_sse_frame(
            "openai.chat_completions",
            &frame(r#"{"choices":[{"delta":{"content":"x"}}]}"#)
        )
        .is_none());
    }

    #[test]
    fn nonstreaming_bodies_yield_usage() {
        let anth = frame(
            r#"{"id":"m","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}"#,
        );
        let u = usage_from_nonstreaming("anthropic.messages", &anth).unwrap();
        assert_eq!(u.input_tokens, Some(5));
        assert_eq!(u.cache_read_tokens, Some(3));
        let chat = frame(
            r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
        );
        let u = usage_from_nonstreaming("openai.chat_completions", &chat).unwrap();
        assert_eq!(u.input_tokens, Some(8));
        assert_eq!(u.total_tokens, Some(10));
        // Missing usage object => None, never zeros.
        let bare = frame(r#"{"id":"m"}"#);
        assert!(usage_from_nonstreaming("openai.responses", &bare).is_none());
    }

    #[test]
    fn empty_usage_is_not_exported() {
        let u = TurnUsage::default();
        assert!(u.is_empty());
    }
}
