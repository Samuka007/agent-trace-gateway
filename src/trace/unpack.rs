//! Protocol unpacking: request/response bytes -> turn facts.
//! Slice 2.1 scope: non-streaming user_input + final_output for the three
//! model protocols. SSE/WS reassembly lands in later slices.
use crate::trace::store::{ToolCall, TurnRecord};

pub fn detect_protocol(path: &str) -> Option<&'static str> {
    if path.starts_with("/v1/chat") || path.starts_with("/compatible-mode/v1/chat") {
        Some("openai.chat_completions")
    } else if path.starts_with("/v1/messages") {
        Some("anthropic.messages")
    } else if path.starts_with("/v1/responses") || path.starts_with("/compatible-mode/v1/responses")
    {
        Some("openai.responses")
    } else {
        None
    }
}

/// Detect whether a captured response is an SSE stream (by content type).
pub fn looks_like_sse(content_type: &str) -> bool {
    content_type.contains("text/event-stream")
}

/// Reassemble the final output text of a streaming response from captured SSE
/// frames. Concatenates output_text deltas in arrival order.
/// Supports OpenAI Responses deltas, OpenAI chat deltas and Anthropic
/// content_block_delta.
/// Returns (reassembled text, token usage). Usage is harvested inside the
/// same per-frame loop — no second pass over the body.
pub fn reassemble_sse_output(
    protocol: &str,
    response_body: &[u8],
) -> (String, Option<crate::trace::adaptor::TurnUsage>) {
    let text = String::from_utf8_lossy(response_body);
    let mut out = String::new();
    let mut usage: Option<crate::trace::adaptor::TurnUsage> = None;
    for frame in text.split("\n\n") {
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if let Some(u) = crate::trace::adaptor::usage_from_sse_frame(protocol, &v) {
            crate::trace::adaptor::merge_usage(&mut usage, u);
        }
        match protocol {
            "openai.responses" => {
                if v["type"] == "response.output_text.delta" {
                    if let Some(d) = v["delta"].as_str() {
                        out.push_str(d);
                    }
                }
            }
            "anthropic.messages" => {
                if v["type"] == "content_block_delta" {
                    if let Some(d) = v["delta"]["text"].as_str() {
                        out.push_str(d);
                    }
                }
            }
            "openai.chat_completions" => {
                if let Some(choices) = v["choices"].as_array() {
                    if let Some(d) = choices.first().and_then(|c| c["delta"]["content"].as_str()) {
                        out.push_str(d);
                    }
                }
            }
            _ => {}
        }
    }
    (out, usage)
}

/// Extract complete tool calls from a streaming response. Tool calls are read
/// from `response.output_item.done` events, which carry the fully assembled
/// function_call item (name + complete arguments), avoiding the need to
/// reassemble argument deltas. The anthropic.messages protocol emits
/// `content_block_start` (type=tool_use, carries the name) followed by
/// `input_json_delta` partial-argument events; those are reassembled into the
/// same ToolCall shape (canonical JSON: type/call_id/name/arguments, aligned
/// with modeltrace conversation_delta.go's chat.tool_call item).
pub fn extract_sse_tool_calls(
    protocol: &str,
    response_body: &[u8],
) -> Vec<crate::trace::store::ToolCall> {
    let mut out = Vec::new();
    if protocol != "openai.responses" && protocol != "anthropic.messages" {
        return out;
    }
    let mut pending: Option<(u64, crate::trace::store::ToolCall)> = None;
    let text = String::from_utf8_lossy(response_body);
    for frame in text.split("\n\n") {
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if protocol == "openai.responses" {
            if v["type"] == "response.output_item.done" && v["item"]["type"] == "function_call" {
                out.push(crate::trace::store::ToolCall {
                    name: v["item"]["name"].as_str().unwrap_or("").to_string(),
                    arguments: v["item"]["arguments"].as_str().unwrap_or("").to_string(),
                });
            }
            continue;
        }
        // anthropic.messages: content_block_start opens a tool_use block
        // (carries the tool name); argument fragments arrive on the OUTER
        // content_block_delta event as delta.type=input_json_delta with the
        // bytes in delta.partial_json (Anthropic wire format — the inner
        // delta type is never the event type); a new content_block_start or
        // the end of the stream closes the pending call.
        match v["type"].as_str() {
            Some("content_block_start") if v["content_block"]["type"] == "tool_use" => {
                if let Some((_, call)) = pending.take() {
                    out.push(call);
                }
                pending = Some((
                    v["index"].as_u64().unwrap_or(0),
                    crate::trace::store::ToolCall {
                        name: v["content_block"]["name"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                        arguments: String::new(),
                    },
                ));
            }
            Some("content_block_delta") if v["delta"]["type"] == "input_json_delta" => {
                let index = v["index"].as_u64().unwrap_or(0);
                if let Some((pending_index, call)) = pending.as_mut() {
                    if *pending_index == index {
                        if let Some(d) = v["delta"]["partial_json"].as_str() {
                            call.arguments.push_str(d);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some((_, call)) = pending.take() {
        out.push(call);
    }
    out
}

/// Extract user input + final output from one non-streaming request/response
/// pair. Returns None when the protocol is unknown or bodies are not JSON.
pub fn unpack_nonstreaming(
    protocol: &str,
    request_body: &[u8],
    response_body: &[u8],
) -> Option<TurnRecord> {
    let req: serde_json::Value = serde_json::from_slice(request_body).ok()?;
    let resp: serde_json::Value = serde_json::from_slice(response_body).ok()?;
    match protocol {
        "openai.chat_completions" => {
            let user_input = req["messages"]
                .as_array()?
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| content_text(&m["content"]))?;
            let final_output = resp["choices"]
                .as_array()
                .and_then(|c| c.first())
                .and_then(|c| content_text(&c["message"]["content"]))
                .unwrap_or_default();
            Some(TurnRecord {
                protocol: protocol.to_string(),
                user_input,
                final_output,
                usage: crate::trace::adaptor::usage_from_nonstreaming(protocol, &resp),
                ..Default::default()
            })
        }
        "anthropic.messages" => {
            let user_input = req["messages"]
                .as_array()?
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| content_text(&m["content"]))?;
            let final_output = resp["content"]
                .as_array()
                .and_then(|blocks| {
                    blocks.iter().find_map(|b| {
                        if b["type"] == "text" {
                            b["text"].as_str().map(str::to_string)
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or_default();
            Some(TurnRecord {
                protocol: protocol.to_string(),
                user_input,
                final_output,
                usage: crate::trace::adaptor::usage_from_nonstreaming(protocol, &resp),
                ..Default::default()
            })
        }
        "openai.responses" => {
            let user_input = responses_user_input(&req)?;
            let final_output = resp["output"]
                .as_array()
                .and_then(|items| {
                    items.iter().find_map(|item| {
                        item["content"].as_array().and_then(|blocks| {
                            blocks.iter().find_map(|b| {
                                if b["type"] == "output_text" {
                                    b["text"].as_str().map(str::to_string)
                                } else {
                                    None
                                }
                            })
                        })
                    })
                })
                .unwrap_or_default();
            Some(TurnRecord {
                protocol: protocol.to_string(),
                user_input,
                final_output,
                usage: crate::trace::adaptor::usage_from_nonstreaming(protocol, &resp),
                ..Default::default()
            })
        }
        _ => None,
    }
}

/// Extract only the user input from a request body (streaming path; response
/// reassembly is handled separately).
pub fn extract_user_input(protocol: &str, request_body: &[u8]) -> Option<String> {
    let req: serde_json::Value = serde_json::from_slice(request_body).ok()?;
    match protocol {
        "openai.chat_completions" | "anthropic.messages" => req["messages"]
            .as_array()?
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .and_then(|m| content_text(&m["content"])),
        "openai.responses" => responses_user_input(&req),
        _ => None,
    }
}

/// OpenAI Responses `input` is either a plain string or an array of items
/// (messages with role + content blocks). For the array form, take the last
/// user message's input_text blocks. Real clients (codex and probes) also
/// send bare items without the `type` field — a missing type counts as a
/// message item.
fn responses_user_input(req: &serde_json::Value) -> Option<String> {
    if let Some(s) = req["input"].as_str() {
        return Some(s.to_string());
    }
    let items = req["input"].as_array()?;
    let user_item = items
        .iter()
        .rev()
        .find(|i| {
            i["type"]
                .as_str()
                .is_none_or(|t| t == "message")
                && i["role"] == "user"
        })?;
    // Content is a string or a block array (input_text blocks).
    if let Some(s) = user_item["content"].as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
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

/// Extract the full messages array from a chat/anthropic request body for
/// prefix stitching. Returns None for non-message protocols.
pub fn extract_messages(request_body: &[u8]) -> Option<Vec<serde_json::Value>> {
    let req: serde_json::Value = serde_json::from_slice(request_body).ok()?;
    req["messages"]
        .as_array()
        .map(|arr| arr.to_vec())
        .filter(|v| !v.is_empty())
}

/// Flatten OpenAI/Anthropic content fields: plain string or block arrays.
fn content_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(blocks) = content.as_array() {
        let mut out = Vec::new();
        for b in blocks {
            if let Some(t) = b["text"].as_str() {
                out.push(t.to_string());
            }
        }
        if !out.is_empty() {
            return Some(out.join("\n"));
        }
    }
    None
}

#[allow(dead_code)]
fn _keep_toolcall_import() -> Option<ToolCall> {
    None
}
