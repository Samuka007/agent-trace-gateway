//! openai.chat_completions — Chat Completions descriptor (stateless: no
//! session/replay primitives; the prefix stitcher does not run for it).
pub static DESCRIPTOR: crate::ProtocolDescriptor = crate::ProtocolDescriptor {
    name: "openai.chat_completions",
    path_prefixes: &["/v1/chat", "/compatible-mode/v1/chat"],
    messages_path: Some("messages"),
    input_shape: crate::InputShape::Messages,
    user_input: None,
    final_output: None,
    body_sources: &[
        crate::BodySource {
            path: &["session_id"],
            two_form: false,
            transform: None,
        },
        crate::BodySource {
            path: &["metadata", "session_id"],
            two_form: false,
            transform: None,
        },
        crate::BodySource {
            path: &["prompt_cache_key"],
            two_form: false,
            transform: Some(crate::pck_namespace),
        },
    ],
    header_sources: &[
        crate::mounts::HDR_SESSION_ID,
        crate::mounts::HDR_SESSION_ID_ALT,
        crate::mounts::HDR_CC_SESSION,
    ],
    chain_sources: &[],
    user_sources: &["safety_identifier", "user"],
    sse_rules: &[
        crate::SseRule {
            on: "*",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::Text(&["choices", "0", "delta", "content"]),
        },
        crate::SseRule {
            on: "*",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::ToolChunk,
        },
        crate::SseRule {
            on: "*",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::Usage(&["usage"]),
        },
    ],
    tool_calls: crate::ToolCallStrategy::ChunkedToolCalls,
    usage_frames: &[crate::UsageFrame {
        on_event: None,
        obj_path: &["usage"],
    }],
    usage_shape: crate::UsageShape {
        input: &[&["prompt_tokens"]],
        output: &[&["completion_tokens"]],
        cache_read: &[&["prompt_tokens_details", "cached_tokens"]],
        cache_write: &[&["prompt_tokens_details", "cache_write_tokens"]],
    },
    usage_inclusion: crate::TokenInclusion::Inclusive,
    final_output_path: &["choices", "0", "message", "content"],
    // USER RULING: stateless SDK traffic — the stitcher must not mint sessions.
    stitch_eligible: false,
};
