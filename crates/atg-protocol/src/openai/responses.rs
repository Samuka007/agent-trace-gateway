//! openai.responses — Responses API descriptor.
//! https://developers.openai.com/api/reference/resources/responses/methods/create.md
pub static DESCRIPTOR: crate::ProtocolDescriptor = crate::ProtocolDescriptor {
    name: "openai.responses",
    path_prefixes: &["/v1/responses", "/compatible-mode/v1/responses"],
    messages_path: None,
    input_shape: crate::InputShape::Responses,
    user_input: Some(responses_user_input),
    final_output: Some(responses_final_output),
    body_sources: &[
        crate::BodySource {
            path: &["session_id"],
            two_form: false,
            transform: None,
        },
        // Strongest explicit root: conversation (string or {id}).
        crate::BodySource {
            path: &["conversation"],
            two_form: true,
            transform: None,
        },
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
        // prompt_cache_key: official cache-routing key, borrowed as a
        // stable affinity (namespaced `pck:`) when no stronger source.
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
        crate::mounts::HDR_GROK_CONV,
    ],
    chain_sources: &["previous_response_id"],
    user_sources: &["safety_identifier", "user"],
    sse_rules: &[
        crate::SseRule {
            on: "response.output_text.delta",
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
    ],
    tool_calls: crate::ToolCallStrategy::DoneItems,
    usage_frames: &[crate::UsageFrame {
        on_event: Some("response.completed"),
        obj_path: &["response", "usage"],
    }],
    usage_shape: crate::UsageShape {
        input: &[&["input_tokens"]],
        output: &[&["output_tokens"]],
        cache_read: &[&["input_tokens_details", "cached_tokens"]],
        cache_write: &[&["input_tokens_details", "cache_write_tokens"]],
    },
    usage_inclusion: crate::TokenInclusion::Inclusive,
    final_output_path: &["output"],
    stitch_eligible: true,
    turn_markers: None,
};

/// responses input reader (named fn referenced by the descriptor): bare items
/// (no `type`) count as message items; content is string or input_text blocks.
pub fn responses_user_input(input: &serde_json::Value) -> Option<String> {
    if let Some(s) = input.as_str() {
        return Some(s.to_string()).filter(|s| !s.is_empty());
    }
    let items = input.as_array()?;
    let user_item = items
        .iter()
        .rev()
        .find(|i| i["type"].as_str().is_none_or(|t| t == "message") && i["role"] == "user")?;
    if let Some(s) = user_item["content"].as_str() {
        return Some(s.to_string()).filter(|s| !s.is_empty());
    }
    let blocks = user_item["content"].as_array()?;
    let mut out = Vec::new();
    for b in blocks {
        if b["type"] == "input_text" {
            if let Some(t) = b["text"].as_str() {
                out.push(t.to_string());
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join("\n"))
}

/// responses final output: output[] items each with content[] of
/// output_text blocks (join all text across all items).
pub fn responses_final_output(resp: &serde_json::Value) -> Option<String> {
    let items = resp["output"].as_array()?;
    let mut out = Vec::new();
    for item in items {
        if let Some(blocks) = item["content"].as_array() {
            for b in blocks {
                if b["type"] == "output_text" {
                    if let Some(text) = b["text"].as_str() {
                        out.push(text.to_string());
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join("\n"))
}
