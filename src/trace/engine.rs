//! The single protocol interpretation engine: consumes a `ProtocolDescriptor`
//! and produces turn facts. This is the only place allowed to branch on
//! wire-format knowledge; protocol details themselves live in the const
//! descriptor tables.
use crate::trace::descriptor::{
    resolve_path, ProtocolDescriptor, SseAction, SseRule, ToolCallStrategy,
};
use crate::trace::store::ToolCall;

/// Split an SSE body into `data:` payload strings per the SSE spec: frames
/// separated by blank lines (tolerating CRLF line endings), multiple data
/// lines within a frame joined with LF.
pub fn sse_data_frames(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body).replace("\r\n", "\n");
    let mut out = Vec::new();
    for frame in text.split("\n\n") {
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start_matches(' '));
            }
        }
        if !data.is_empty() {
            out.push(data);
        }
    }
    out
}

/// Streaming accumulator state driven by the descriptor's sse_rules.
#[derive(Default)]
pub struct SseAccum {
    pub text: String,
    pub usage: Option<crate::trace::adaptor::TurnUsage>,
    tools: Vec<ToolCall>,
    pending: Option<(u64, ToolCall)>,
    chat_tools: Vec<(u64, ToolCall)>,
}

impl SseAccum {
    pub fn finish(mut self, strategy: ToolCallStrategy) -> Vec<ToolCall> {
        if strategy == ToolCallStrategy::DeltaAssembly {
            if let Some((_, call)) = self.pending.take() {
                self.tools.push(call);
            }
        } else if strategy == ToolCallStrategy::ChunkedToolCalls {
            self.chat_tools.sort_by_key(|(i, _)| *i);
            self.tools.extend(self.chat_tools.into_iter().map(|(_, c)| c));
        }
        self.tools
    }
}

/// Rule matching: `on` gates on the frame's data `type` ("*" = any); the
/// inner delta-type gate distinguishes anthropic's content_block_delta kinds.
fn matches_rule(rule: &SseRule, event: Option<&str>, v: &serde_json::Value) -> bool {
    match (rule.on, event) {
        ("*", _) => {}
        (name, Some(ev)) if ev == name => {}
        _ => return false,
    }
    if let Some(dt) = rule.data_type {
        if v["type"].as_str() != Some(dt) {
            return false;
        }
    }
    if let Some(dt) = rule.delta_type {
        if v["delta"]["type"].as_str() != Some(dt) {
            return false;
        }
    }
    true
}

/// Run the descriptor's sse_rules over one parsed frame.
pub fn apply_sse_rule(
    acc: &mut SseAccum,
    d: &ProtocolDescriptor,
    event: Option<&str>,
    v: &serde_json::Value,
) {
    for rule in d.sse_rules {
        if !matches_rule(rule, event, v) {
            continue;
        }
        match rule.action {
            SseAction::Text(path) => {
                if let Some(t) = resolve_path(v, path).as_str() {
                    acc.text.push_str(t);
                }
            }
            SseAction::Usage(_) => {
                // Harvest through the descriptor's usage_frames (last-wins
                // merge keeps anthropic's split frames correct).
                for uf in d.usage_frames {
                    let event_matches = match (uf.on_event, event) {
                        (Some(ev), Some(e)) => ev == e,
                        (None, _) => true,
                        _ => false,
                    };
                    if event_matches {
                        let usage = resolve_path(v, uf.obj_path);
                        if !usage.is_null() {
                            crate::trace::adaptor::merge_usage(
                                &mut acc.usage,
                                crate::trace::adaptor::usage_from_obj(d, usage),
                            );
                        }
                    }
                }
            }
            SseAction::ToolOpen => {
                if v["content_block"]["type"] == "tool_use" {
                    if let Some((_, call)) = acc.pending.take() {
                        acc.tools.push(call);
                    }
                    acc.pending = Some((
                        v["index"].as_u64().unwrap_or(0),
                        ToolCall {
                            name: v["content_block"]["name"]
                                .as_str()
                                .unwrap_or("")
                                .to_string(),
                            arguments: String::new(),
                        },
                    ));
                }
            }
            SseAction::ToolArg => {
                let index = v["index"].as_u64().unwrap_or(0);
                if let Some((pi, call)) = acc.pending.as_mut() {
                    if *pi == index {
                        if let Some(d) = v["delta"]["partial_json"].as_str() {
                            call.arguments.push_str(d);
                        }
                    }
                }
            }
            SseAction::ToolClose => {
                let index = v["index"].as_u64().unwrap_or(0);
                let matches = acc
                    .pending
                    .as_ref()
                    .map(|(pi, _)| *pi == index)
                    .unwrap_or(false);
                if matches {
                    if let Some((_, call)) = acc.pending.take() {
                        acc.tools.push(call);
                    }
                }
            }
            SseAction::ToolDone => {
                let item = &v["item"];
                let item_type = item["type"].as_str().unwrap_or("");
                if item_type == "function_call" || item_type == "custom_tool_call" {
                    let arguments = item["arguments"]
                        .as_str()
                        .or_else(|| item["input"].as_str())
                        .unwrap_or("")
                        .to_string();
                    acc.tools.push(ToolCall {
                        name: item["name"].as_str().unwrap_or("").to_string(),
                        arguments,
                    });
                }
            }
            SseAction::ToolChunk => {
                if let Some(calls) = v["choices"][0]["delta"]["tool_calls"].as_array() {
                    for tc in calls {
                        let idx = tc["index"].as_u64().unwrap_or(0);
                        let entry = match acc.chat_tools.iter_mut().find(|(i, _)| *i == idx) {
                            Some(e) => e,
                            None => {
                                acc.chat_tools.push((idx, ToolCall {
                                    name: String::new(),
                                    arguments: String::new(),
                                }));
                                acc.chat_tools.last_mut().unwrap()
                            }
                        };
                        if let Some(n) = tc["function"]["name"].as_str() {
                            entry.1.name.push_str(n);
                        }
                        if let Some(a) = tc["function"]["arguments"].as_str() {
                            entry.1.arguments.push_str(a);
                        }
                    }
                }
            }
        }
        // One rule per event fires; rules with the same trigger are all
        // evaluated, so chat's three "*" rules each get a chance.
    }
}

/// Stream a captured SSE response through the descriptor: text output, token
/// usage and tool calls in a single per-frame pass.
pub fn stream_response(
    d: &ProtocolDescriptor,
    body: &[u8],
) -> (
    String,
    Option<crate::trace::adaptor::TurnUsage>,
    Vec<ToolCall>,
) {
    let mut acc = SseAccum::default();
    for data in sse_data_frames(body) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let event = v["type"].as_str();
        apply_sse_rule(&mut acc, d, event, &v);
    }
    let text = std::mem::take(&mut acc.text);
    let usage = acc.usage.take();
    let tools = acc.finish(d.tool_calls);
    (text, usage, tools)
}

/// Non-streaming: user_input/final_output/usage in one pass over the
/// already-parsed bodies.
pub fn nonstreaming(
    d: &ProtocolDescriptor,
    req: &serde_json::Value,
    resp: &serde_json::Value,
) -> Option<crate::trace::store::TurnRecord> {
    let user_input = d.user_input(req)?;
    let final_output = d.final_output(resp).unwrap_or_default();
    let usage = crate::trace::adaptor::usage_from_nonstreaming(d, resp);
    Some(crate::trace::store::TurnRecord {
        protocol: d.name.to_string(),
        user_input,
        final_output,
        usage,
        ..Default::default()
    })
}
