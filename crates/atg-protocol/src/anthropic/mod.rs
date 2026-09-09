//! anthropic.messages — Messages API descriptor.
//! https://platform.claude.com/docs/en/api/messages
use crate::session::metadata_envelope_transform;

/// user_id transform for the anthropic table row: runs the three-form reader
/// (JSON envelope / legacy composite / object) on the raw string.
fn user_id_transform(v: &str) -> Option<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(v).unwrap_or(serde_json::Value::String(v.to_string()));
    crate::session::metadata_user_id_session(&parsed)
}

pub static DESCRIPTOR: crate::ProtocolDescriptor = crate::ProtocolDescriptor {
    name: "anthropic.messages",
    path_prefixes: &["/v1/messages"],
    messages_path: Some("messages"),
    input_shape: crate::InputShape::Messages,
    user_input: None,
    final_output: None,
    // Body: metadata.session_id (client convention) then metadata.user_id
    // legacy composite (Claude Code convention, transform extracts uuid).
    body_sources: &[
        // Shared v0.2.0 top-level sources (modeltrace nine-source port).
        crate::BodySource {
            path: &["session_id"],
            two_form: false,
            transform: None,
        },
        // anthropic-specific: metadata.session_id (client convention),
        // then metadata.user_id — plain JSON envelope {session_id}, or
        // the Claude Code legacy composite (transform extracts uuid).
        crate::BodySource {
            path: &["metadata", "session_id"],
            two_form: false,
            transform: None,
        },
        crate::BodySource {
            path: &["metadata", "user_id"],
            two_form: false,
            transform: Some(user_id_transform),
        },
        // metadata itself as a JSON envelope string {"user_id": ...} —
        // a real Claude Code traffic form (v0.2.0 covered it).
        crate::BodySource {
            path: &["metadata"],
            two_form: false,
            transform: Some(metadata_envelope_transform),
        },
    ],
    header_sources: &[
        crate::mounts::HDR_CC_SESSION,
        crate::mounts::HDR_SESSION_ID,
        crate::mounts::HDR_SESSION_ID_ALT,
    ],
    chain_sources: &[],
    user_sources: &["metadata", "user_id"],
    sse_rules: &[
        crate::SseRule {
            on: "content_block_delta",
            data_type: None,
            delta_type: Some("text_delta"),
            action: crate::SseAction::Text(&["delta", "text"]),
        },
        crate::SseRule {
            on: "content_block_start",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::ToolOpen,
        },
        crate::SseRule {
            on: "content_block_delta",
            data_type: None,
            delta_type: Some("input_json_delta"),
            action: crate::SseAction::ToolArg,
        },
        crate::SseRule {
            on: "content_block_stop",
            data_type: None,
            delta_type: None,
            action: crate::SseAction::ToolClose,
        },
    ],
    tool_calls: crate::ToolCallStrategy::DeltaAssembly,
    usage_frames: &[
        crate::UsageFrame {
            on_event: Some("message_start"),
            obj_path: &["message", "usage"],
        },
        crate::UsageFrame {
            on_event: Some("message_delta"),
            obj_path: &["usage"],
        },
    ],
    usage_shape: crate::UsageShape {
        input: &[&["input_tokens"]],
        output: &[&["output_tokens"]],
        cache_read: &[&["cache_read_input_tokens"]],
        cache_write: &[&["cache_creation_input_tokens"]],
    },
    usage_inclusion: crate::TokenInclusion::Exclusive,
    final_output_path: &["content"],
};
