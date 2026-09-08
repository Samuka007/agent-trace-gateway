//! Protocol descriptors: compile-time data tables describing each wire
//! protocol, plus the single interpretation engine. Adding a provider means
//! adding a descriptor file and one DESCRIPTORS entry — unpack/session/
//! adaptor/lib never grow protocol branches again.
//!
//! Structure mirrors the design doc (`.tmp-atg-protocol-descriptor-design.md`);
//! every table field is asserted against official docs in `mod tests`.
use serde_json::Value;

/// Where a session/identity fact lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Loc {
    Body,
    Header,
}

/// Semantic class of a session/identity source (spec session layering).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sem {
    /// Strongest explicit root: client-declared session id.
    SessionRoot,
    /// Client-chosen stable affinity key, borrowed as session (namespaced).
    StableAffinity,
    /// Response-chain link; needs resp_id->root state (v1: skipped).
    ResponseChain,
    /// End-user identity → langfuse.user.id, never a session.
    EndUser,
}

/// Where the user-turn text is found for this protocol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputShape {
    /// `messages` replay array (chat, anthropic).
    Messages,
    /// `input`: string | items (bare or typed) — no fingerprint replay.
    Responses,
}

/// How streaming tool calls arrive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolCallStrategy {
    /// `response.output_item.done` with a complete item (responses).
    DoneItems,
    /// content_block_start opens + input_json_delta fragments (anthropic).
    DeltaAssembly,
    /// chat: delta.tool_calls[] incremental assembly by index.
    ChunkedToolCalls,
}

/// One streaming rule: an SSE event drives one extraction target.
#[derive(Clone, Copy)]
pub enum SseAction {
    /// Append text from a Value path to the output.
    Text(&'static [&'static str]),
    /// Harvest usage from a Value path (partial; merged last-wins).
    Usage(&'static [&'static str]),
    /// Open/extend a tool_use block (name from block path, args appended).
    ToolOpen,
    /// Append argument bytes for the pending tool block.
    ToolArg,
    /// Close the pending tool block.
    ToolClose,
    /// responses: complete tool item on output_item.done.
    ToolDone,
    /// chat: delta.tool_calls incremental fragment.
    ToolChunk,
}

pub struct SseRule {
    /// Exact event name; "*" matches chunks without an event name (chat).
    pub on: &'static str,
    /// Outer event-type gate: only run the action when the data's `type`
    /// equals this (None = always). chat uses None (frames carry no type).
    pub data_type: Option<&'static str>,
    /// Inner delta-type gate for anthropic content_block_delta.
    pub delta_type: Option<&'static str>,
    pub action: SseAction,
}

/// One usage frame location: event gate + object path within the frame.
pub struct UsageFrame {
    pub on_event: Option<&'static str>,
    pub obj_path: &'static [&'static str],
}

/// Whether the protocol's input token count already includes cache reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenInclusion {
    /// input_tokens includes cached_tokens (OpenAI) — must be derived.
    Inclusive,
    /// Buckets are mutually exclusive (Anthropic) — map directly.
    Exclusive,
}

pub struct ProtocolDescriptor {
    pub name: &'static str,
    pub path_prefixes: &'static [&'static str],
    /// Key holding the replayable messages array; None = fingerprint skipped.
    pub messages_path: Option<&'static str>,
    pub input_shape: InputShape,
    /// Multi-protocol-specific user-input extraction (responses only).
    pub user_input: Option<fn(&Value) -> Option<String>>,
    /// Ordered body sources (first hit wins).
    pub body_sources: &'static [BodySource],
    /// Ordered header sources (checked after body sources).
    pub header_sources: &'static [&'static str],
    /// Response-chain sources (v1: recorded, skipped).
    pub chain_sources: &'static [&'static str],
    /// End-user identity sources → langfuse.user.id.
    pub user_sources: &'static [&'static str],
    pub sse_rules: &'static [SseRule],
    pub tool_calls: ToolCallStrategy,
    pub usage_frames: &'static [UsageFrame],
    pub usage_inclusion: TokenInclusion,
    /// Non-streaming final-output extraction path.
    pub final_output_path: &'static [&'static str],
}

/// A body session source: optional nested path (e.g. metadata.session_id) or
/// a two-form value (conversation: string | {id}).
#[derive(Clone, Copy)]
pub struct BodySource {
    /// Value path; None = the two-form conversation reader.
    pub path: Option<&'static [&'static str]>,
    /// conversation-style: accept string or {id}.
    pub two_form: bool,
}

pub const GEN_SPAN_ID_SEED: &str = "\u{0}gen";

impl ProtocolDescriptor {
    /// The descriptor whose path prefix matches, longest first.
    pub fn detect(path: &str) -> Option<&'static ProtocolDescriptor> {
        DESCRIPTORS
            .iter()
            .filter(|d| d.path_prefixes.iter().any(|p| path.starts_with(p)))
            .max_by_key(|d| d.path_prefixes.iter().map(|p| p.len()).max().unwrap_or(0))
            .copied()
    }

    /// Session id from body sources in priority order (body before header).
    pub fn session_from_body(&self, body: &Value) -> Option<String> {
        for src in self.body_sources {
            if let Some(sid) = read_source(body, src) {
                return Some(sid);
            }
        }
        None
    }

    /// Session id from headers (lowercased names, in order).
    pub fn session_from_headers(&self, get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        for name in self.header_sources {
            if let Some(v) = get(name).filter(|s| !s.trim().is_empty()) {
                return Some(v.trim().to_string());
            }
        }
        None
    }

    /// End-user identity (first EndUser source hit) — never a session.
    pub fn end_user(&self, body: &Value) -> Option<String> {
        for name in self.user_sources {
            if let Some(v) = read_path(body, &[name]).filter(|s| !s.trim().is_empty()) {
                return Some(v.trim().to_string());
            }
        }
        None
    }

    /// User-input text from the request body.
    pub fn user_input(&self, req: &Value) -> Option<String> {
        match self.input_shape {
            InputShape::Messages => {
                let item = req["messages"].as_array()?.iter().rev().find(|m| m["role"] == "user")?;
                content_text(&item["content"])
            }
            InputShape::Responses => (self.user_input?)(&req["input"]),
        }
    }

    /// Final output text from a non-streaming response body.
    pub fn final_output(&self, resp: &Value) -> Option<String> {
        content_text(resolve_path(resp, self.final_output_path()))
    }

    fn final_output_path(&self) -> &'static [&'static str] {
        match self.name {
            "openai.chat_completions" => &["choices", "0", "message", "content"],
            "anthropic.messages" => &["content"],
            _ => &["output"],
        }
    }
}

/// Two-form reader: conversation may be a plain id string or {id}.
fn read_source(body: &Value, src: &BodySource) -> Option<String> {
    let path = src.path?;
    let v = resolve_path(body, path);
    if src.two_form {
        if let Some(s) = v.as_str() {
            return Some(s.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(s) = v["id"].as_str() {
            return Some(s.trim().to_string()).filter(|s| !s.is_empty());
        }
        return None;
    }
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn read_path(body: &Value, path: &[&str]) -> Option<String> {
    let mut cur = body;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::to_string)
}

pub fn resolve_path<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    let mut cur = v;
    for key in path {
        // "0"/"1"... index into arrays.
        cur = match cur {
            Value::Array(arr) => key.parse::<usize>().ok().and_then(|i| arr.get(i)).unwrap_or(&Value::Null),
            _ => cur.get(key).unwrap_or(&Value::Null),
        };
    }
    cur
}

/// Flatten OpenAI/Anthropic content fields: plain string or block arrays.
pub fn content_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string()).filter(|s| !s.is_empty());
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

pub static DESCRIPTORS: &[&ProtocolDescriptor] = &[
    &anthropic::DESCRIPTOR,
    &responses::DESCRIPTOR,
    &chat::DESCRIPTOR,
];

pub mod anthropic {
    use super::*;

    pub static DESCRIPTOR: ProtocolDescriptor = ProtocolDescriptor {
        name: "anthropic.messages",
        path_prefixes: &["/v1/messages"],
        messages_path: Some("messages"),
        input_shape: InputShape::Messages,
        user_input: None,
        // Body: metadata.session_id (client convention) then metadata.user_id
        // legacy composite (Claude Code convention, transform extracts uuid).
        body_sources: &[
            BodySource { path: Some(&["metadata", "session_id"]), two_form: false },
            BodySource { path: Some(&["metadata", "user_id"]), two_form: false },
        ],
        header_sources: &["x-claude-code-session-id", "session-id", "session_id"],
        chain_sources: &[],
        user_sources: &["metadata", "user_id"],
        sse_rules: &[
            SseRule {
                on: "content_block_delta",
                data_type: None,
                delta_type: Some("text_delta"),
                action: SseAction::Text(&["delta", "text"]),
            },
            SseRule {
                on: "content_block_start",
                data_type: None,
                delta_type: None,
                action: SseAction::ToolOpen,
            },
            SseRule {
                on: "content_block_delta",
                data_type: None,
                delta_type: Some("input_json_delta"),
                action: SseAction::ToolArg,
            },
            SseRule {
                on: "content_block_stop",
                data_type: None,
                delta_type: None,
                action: SseAction::ToolClose,
            },
        ],
        tool_calls: ToolCallStrategy::DeltaAssembly,
        usage_frames: &[
            UsageFrame { on_event: Some("message_start"), obj_path: &["message", "usage"] },
            UsageFrame { on_event: Some("message_delta"), obj_path: &["usage"] },
        ],
        usage_inclusion: TokenInclusion::Exclusive,
        final_output_path: &["content"],
    };
}

pub mod responses {
    use super::*;

    pub static DESCRIPTOR: ProtocolDescriptor = ProtocolDescriptor {
        name: "openai.responses",
        path_prefixes: &["/v1/responses", "/compatible-mode/v1/responses"],
        messages_path: None,
        input_shape: InputShape::Responses,
        user_input: Some(responses_user_input),
        body_sources: &[
            // Strongest explicit root: conversation (string or {id}).
            BodySource { path: Some(&["conversation"]), two_form: true },
            BodySource { path: Some(&["metadata", "session_id"]), two_form: false },
            BodySource { path: Some(&["client_metadata", "session_id"]), two_form: false },
            // prompt_cache_key: official cache-routing key, borrowed as a
            // stable affinity (namespaced `pck:`) when no stronger source.
            BodySource { path: Some(&["prompt_cache_key"]), two_form: false },
        ],
        header_sources: &["session-id", "session_id", "x-claude-code-session-id", "x-grok-conv-id"],
        chain_sources: &["previous_response_id"],
        user_sources: &["safety_identifier", "user"],
        sse_rules: &[
            SseRule {
                on: "response.output_text.delta",
                data_type: None,
                delta_type: None,
                action: SseAction::Text(&["delta"]),
            },
            SseRule {
                on: "response.output_item.done",
                data_type: None,
                delta_type: None,
                action: SseAction::ToolDone,
            },
            SseRule {
                on: "response.completed",
                data_type: None,
                delta_type: None,
                action: SseAction::Usage(&["response", "usage"]),
            },
        ],
        tool_calls: ToolCallStrategy::DoneItems,
        usage_frames: &[UsageFrame { on_event: Some("response.completed"), obj_path: &["response", "usage"] }],
        usage_inclusion: TokenInclusion::Inclusive,
        final_output_path: &["output"],
    };
}

pub mod chat {
    use super::*;

    pub static DESCRIPTOR: ProtocolDescriptor = ProtocolDescriptor {
        name: "openai.chat_completions",
        path_prefixes: &["/v1/chat", "/compatible-mode/v1/chat"],
        messages_path: Some("messages"),
        input_shape: InputShape::Messages,
        user_input: None,
        body_sources: &[
            BodySource { path: Some(&["metadata", "session_id"]), two_form: false },
            BodySource { path: Some(&["prompt_cache_key"]), two_form: false },
        ],
        header_sources: &["session-id", "session_id"],
        chain_sources: &[],
        user_sources: &["safety_identifier", "user"],
        sse_rules: &[
            SseRule {
                on: "*",
                data_type: None,
                delta_type: None,
                action: SseAction::Text(&["choices", "0", "delta", "content"]),
            },
            SseRule {
                on: "*",
                data_type: None,
                delta_type: None,
                action: SseAction::ToolChunk,
            },
            SseRule {
                on: "*",
                data_type: None,
                delta_type: None,
                action: SseAction::Usage(&["usage"]),
            },
        ],
        tool_calls: ToolCallStrategy::ChunkedToolCalls,
        usage_frames: &[UsageFrame { on_event: None, obj_path: &["usage"] }],
        usage_inclusion: TokenInclusion::Inclusive,
        final_output_path: &["choices", "0", "message", "content"],
    };
}

/// responses input reader (named fn referenced by the descriptor): bare items
/// (no `type`) count as message items; content is string or input_text blocks.
fn responses_user_input(input: &Value) -> Option<String> {
    if let Some(s) = input.as_str() {
        return Some(s.to_string()).filter(|s| !s.is_empty());
    }
    let items = input.as_array()?;
    let user_item = items.iter().rev().find(|i| {
        i["type"].as_str().is_none_or(|t| t == "message") && i["role"] == "user"
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn value(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    /// Path detection for every real-world path variant, data-driven.
    #[test]
    fn path_prefixes_match_official_routes() {
        assert_eq!(ProtocolDescriptor::detect("/v1/messages").unwrap().name, "anthropic.messages");
        assert_eq!(ProtocolDescriptor::detect("/v1/responses").unwrap().name, "openai.responses");
        assert_eq!(
            ProtocolDescriptor::detect("/compatible-mode/v1/responses").unwrap().name,
            "openai.responses"
        );
        assert_eq!(ProtocolDescriptor::detect("/v1/chat/completions").unwrap().name, "openai.chat_completions");
        assert_eq!(
            ProtocolDescriptor::detect("/compatible-mode/v1/chat/completions")
                .unwrap()
                .name,
            "openai.chat_completions"
        );
        assert!(ProtocolDescriptor::detect("/v1/models").is_none());
        assert!(ProtocolDescriptor::detect("/__atg/records").is_none());
    }

    /// anthropic usage locations: message_start.message.usage +
    /// message_delta.usage (official streaming docs).
    #[test]
    fn anthropic_usage_frames_match_official_docs() {
        let d = &anthropic::DESCRIPTOR;
        assert_eq!(d.usage_frames.len(), 2);
        assert_eq!(d.usage_frames[0].on_event, Some("message_start"));
        assert_eq!(d.usage_frames[0].obj_path, &["message", "usage"]);
        assert_eq!(d.usage_frames[1].on_event, Some("message_delta"));
        assert_eq!(d.usage_frames[1].obj_path, &["usage"]);
        assert_eq!(d.usage_inclusion, TokenInclusion::Exclusive);
    }

    /// responses usage: only response.completed.response.usage; inclusive
    /// input (cached_tokens included) per official reference.
    #[test]
    fn responses_usage_frame_matches_official_docs() {
        let d = &responses::DESCRIPTOR;
        assert_eq!(d.usage_frames.len(), 1);
        assert_eq!(d.usage_frames[0].on_event, Some("response.completed"));
        assert_eq!(d.usage_frames[0].obj_path, &["response", "usage"]);
        assert_eq!(d.usage_inclusion, TokenInclusion::Inclusive);
        assert_eq!(d.tool_calls, ToolCallStrategy::DoneItems);
    }

    /// chat: no event names (frames are bare chunks); usage object on any
    /// frame; tool calls assembled from delta.tool_calls.
    #[test]
    fn chat_descriptor_matches_official_docs() {
        let d = &chat::DESCRIPTOR;
        assert!(d.sse_rules.iter().all(|r| r.on == "*"));
        assert_eq!(d.tool_calls, ToolCallStrategy::ChunkedToolCalls);
        assert_eq!(d.usage_inclusion, TokenInclusion::Inclusive);
        assert!(d.chain_sources.is_empty(), "chat has no chain primitive");
    }

    /// anthropic session sources: body metadata.session_id, then
    /// metadata.user_id (legacy transform); headers CC-first.
    #[test]
    fn anthropic_session_layering() {
        let d = &anthropic::DESCRIPTOR;
        assert_eq!(d.body_sources.len(), 2);
        assert_eq!(d.body_sources[0].path, Some(&["metadata", "session_id"] as &[&str]));
        assert_eq!(d.body_sources[1].path, Some(&["metadata", "user_id"] as &[&str]));
        assert_eq!(d.header_sources[0], "x-claude-code-session-id");
        assert!(d.chain_sources.is_empty(), "anthropic has no chain primitive");
    }

    /// responses session layering: conversation > metadata.session_id >
    /// client_metadata.session_id > prompt_cache_key; chain = previous_response_id.
    #[test]
    fn responses_session_layering() {
        let d = &responses::DESCRIPTOR;
        assert_eq!(d.body_sources[0].path, Some(&["conversation"] as &[&str]));
        assert!(d.body_sources[0].two_form, "conversation accepts object-id form");
        assert_eq!(
            d.body_sources.iter().filter_map(|s| s.path).next_back(),
            Some(&["prompt_cache_key"] as &[&str])
        );
        assert_eq!(d.chain_sources, &["previous_response_id"]);
        assert_eq!(d.user_sources, &["safety_identifier", "user"]);
    }

    #[test]
    fn conversation_two_form_reading() {
        let d = &responses::DESCRIPTOR;
        assert_eq!(
            d.session_from_body(&value(r#"{"conversation":"conv_123"}"#)),
            Some("conv_123".to_string())
        );
        assert_eq!(
            d.session_from_body(&value(r#"{"conversation":{"id":"conv_9"}}"#)),
            Some("conv_9".to_string())
        );
        assert!(d.session_from_body(&value(r#"{"conversation":null}"#)).is_none());
    }

    #[test]
    fn bare_items_count_as_messages() {
        let body = value(r#"{"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#);
        let d = &responses::DESCRIPTOR;
        assert_eq!(d.user_input.unwrap()(&body["input"]), Some("hi".to_string()));
    }
}
