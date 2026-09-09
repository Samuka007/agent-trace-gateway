//! openai.live — Realtime API (WebSocket) descriptor.
//! https://developers.openai.com/api/reference/resources/realtime
//! Turn markers and frame geometry are consumed by the ws turn assembler.
pub static DESCRIPTOR: crate::ProtocolDescriptor = crate::ProtocolDescriptor {
    name: "openai.live",
    path_prefixes: &[],
    messages_path: None,
    input_shape: crate::InputShape::Responses,
    user_input: None,
    final_output: None,
    // WS session: client_metadata.session_id is a sub2api injection
    // convention (not OpenAI Realtime spec); kept lowest priority.
    body_sources: &[
        crate::BodySource {
            path: &["metadata", "session_id"],
            two_form: false,
            transform: None,
        },
        crate::BodySource {
            path: &["client_metadata", "session_id"],
            two_form: false,
            transform: None,
        },
    ],
    header_sources: &[
        crate::mounts::HDR_SESSION_ID,
        crate::mounts::HDR_SESSION_ID_ALT,
        crate::mounts::HDR_GROK_CONV,
    ],
    chain_sources: &[],
    user_sources: &["safety_identifier", "user"],
    sse_rules: &[
        crate::SseRule {
            on: "response.audio_transcript.delta",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::Text(&["delta"]),
        },
        crate::SseRule {
            on: "response.output_item.done",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::ToolDone,
        },
        crate::SseRule {
            on: "response.done",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::Usage(&["response", "usage"]),
        },
    ],
    tool_calls: crate::ToolCallStrategy::DoneItems,
    usage_frames: &[crate::UsageFrame {
        on_event: Some("response.done"),
        obj_path: &["response", "usage"],
    }],
    usage_shape: crate::UsageShape {
        input: &[&["input_tokens"]],
        output: &[&["output_tokens"]],
        cache_read: &[&["input_token_details", "cached_tokens"]],
        cache_write: &[&["input_token_details", "cache_write_tokens"]],
    },
    usage_inclusion: crate::TokenInclusion::Inclusive,
    final_output_path: &["output"],
};
