//! Usage extraction: descriptor-table-driven readers producing the model
//! layer's TurnUsage buckets. Protocol spellings live in the descriptor
//! UsageShape tables — never in code or_else chains.
use crate::{ProtocolDescriptor, TokenInclusion};
use atg_model::TurnUsage;
use serde_json::Value;

/// Extract usage facts from one SSE frame (single traversal — called inside
/// the reassembly loop). Delegates to the descriptor table's usage frames.
pub fn usage_from_sse_frame(d: &ProtocolDescriptor, v: &Value) -> Option<TurnUsage> {
    let event = v["type"].as_str();
    for uf in d.usage_frames {
        let matches = match (uf.on_event, event) {
            (Some(ev), Some(e)) => ev == e,
            (None, _) => true,
            (Some(_), None) => false,
        };
        if matches {
            let obj = crate::resolve_path(v, uf.obj_path);
            if !obj.is_null() {
                return Some(usage_from_obj(d, obj));
            }
        }
    }
    None
}

/// Extract usage facts from one non-streaming response body (single parse;
/// caller already has the parsed body).
pub fn usage_from_nonstreaming(d: &ProtocolDescriptor, resp: &Value) -> Option<TurnUsage> {
    // Non-streaming response bodies always carry usage at the top level,
    // regardless of the streaming frame geometry (message.usage is an SSE
    // concern only).
    let obj = crate::resolve_path(resp, &["usage"]);
    if obj.is_null() {
        return None;
    }
    Some(usage_from_obj(d, obj))
}

/// Build TurnUsage from an already-located usage object per protocol.
/// UsageShape fields are full nested paths (e.g. responses'
/// input_tokens_details.cached_tokens), each alternative tried via
/// resolve_path — never flattened to top-level key probes.
pub fn usage_from_obj(d: &ProtocolDescriptor, usage: &Value) -> TurnUsage {
    let shape = &d.usage_shape;
    let read = |alts: &[&[&str]]| -> Option<u64> {
        alts.iter()
            .find_map(|p| opt_u64(crate::resolve_path(usage, p)))
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
    if d.usage_inclusion == TokenInclusion::Inclusive {
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

#[cfg(test)]
mod tests {
    use super::*;
    use atg_model::{merge_usage, usage_details_json};

    fn frame(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn anthropic_usage_splits_across_frames() {
        let start = frame(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}}"#,
        );
        let delta = frame(r#"{"type":"message_delta","usage":{"output_tokens":7}}"#);
        let mut acc: Option<TurnUsage> = None;
        let d = ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
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
        let d = ProtocolDescriptor::detect_by_name("openai.responses").unwrap();
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
        let d = ProtocolDescriptor::detect_by_name("openai.chat_completions").unwrap();
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
        let d = ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
        let u = usage_from_nonstreaming(d, &anth).unwrap();
        assert_eq!(u.input_tokens, Some(5));
        assert_eq!(u.cache_read_tokens, Some(3));
        let chat = frame(
            r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
        );
        let d = ProtocolDescriptor::detect_by_name("openai.chat_completions").unwrap();
        let u = usage_from_nonstreaming(d, &chat)
            .expect("chat nonstreaming usage should resolve from top-level usage object");
        assert_eq!(u.input_tokens, Some(8));
        assert_eq!(u.total_tokens, Some(10));
        // Missing usage object => None, never zeros.
        let bare = frame(r#"{"id":"m"}"#);
        let d = ProtocolDescriptor::detect_by_name("openai.responses").unwrap();
        assert!(usage_from_nonstreaming(d, &bare).is_none());
    }

    /// R1 nails: openai cache tokens arrive on NESTED paths — each
    /// protocol's details bucket must resolve via resolve_path, never as a
    /// flattened top-level key probe (slop#3 regression pin).
    #[test]
    fn responses_cached_tokens_resolves_nested_path() {
        let d = ProtocolDescriptor::detect_by_name("openai.responses").unwrap();
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
        let d = ProtocolDescriptor::detect_by_name("openai.chat_completions").unwrap();
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
        let d = ProtocolDescriptor::detect_by_name("openai.responses").unwrap();
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
        let d = ProtocolDescriptor::detect_by_name("openai.live").unwrap();
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
