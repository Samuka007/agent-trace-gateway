//! anthropic.messages — Messages API descriptor.
//! https://platform.claude.com/docs/en/api/messages

pub static DESCRIPTOR: crate::ProtocolDescriptor = crate::ProtocolDescriptor {
    name: "anthropic.messages",
    path_prefixes: &["/v1/messages"],
    loose_endpoints: &["messages"],
    messages_path: Some("messages"),
    input_shape: crate::InputShape::Messages,
    user_input: None,
    final_output: None,
    // Generic body mounts only. The Claude Code user_id shapes (envelope /
    // legacy composite) are HARNESS session rules (atg-harness
    // claude_code.rs), evaluated between these body sources and the header
    // sources — same effective priority as the pre-v0.3.0 table rows.
    body_sources: &[
        // Shared v0.2.0 top-level sources (modeltrace nine-source port).
        crate::BodySource {
            path: &["session_id"],
            two_form: false,
            transform: None,
        },
        // anthropic-specific: metadata.session_id (client convention).
        crate::BodySource {
            path: &["metadata", "session_id"],
            two_form: false,
            transform: None,
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
    stitch_eligible: true,
    turn_markers: None,
    nonstreaming_tools: Some(nonstreaming_tools),
};

/// Non-streaming tool_use blocks: content[] items of type tool_use carry
/// name + input (a JSON value — serialized to the arguments string).
fn nonstreaming_tools(resp: &serde_json::Value) -> Vec<atg_model::ToolCall> {
    let Some(blocks) = resp["content"].as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| atg_model::ToolCall {
            name: b["name"].as_str().unwrap_or_default().to_string(),
            arguments: match &b["input"] {
                serde_json::Value::String(s) => s.clone(),
                v => v.to_string(),
            },
        })
        .collect()
}
