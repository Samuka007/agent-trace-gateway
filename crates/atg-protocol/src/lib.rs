//! Protocol descriptors: compile-time data tables describing each wire
//! protocol, plus the generic table evaluators. Adding a provider means
//! adding a descriptor file under `anthropic/` or `openai/` and one
//! DESCRIPTORS entry — engine/session/usage evaluators never grow protocol
//! branches again.
//!
//! Structure mirrors the design doc (`.tmp-atg-protocol-descriptor-design.md`);
//! every table field is asserted against official docs in `mod tests`.
//! Dependency direction (workspace red line): atg-protocol → atg-model only.
use serde_json::Value;

pub mod anthropic;
pub mod mounts;
pub mod openai;
pub mod session;
pub mod usage;

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

/// WebSocket turn boundary events (openai.live): the client frame that
/// opens a turn and the server frame that closes it (carrying usage).
pub struct TurnMarkers {
    pub start: &'static str,
    pub end: &'static str,
}

/// Field spellings for one protocol's usage object (protocol knowledge lives
/// here, never in code or_else chains).
/// Field spellings as ALTERNATIVE full paths (segments for resolve_path);
/// e.g. openai cache_read is the nested ["input_tokens_details",
/// "cached_tokens"].
#[derive(Clone, Copy)]
pub struct UsageShape {
    pub input: &'static [&'static [&'static str]],
    pub output: &'static [&'static [&'static str]],
    pub cache_read: &'static [&'static [&'static str]],
    pub cache_write: &'static [&'static [&'static str]],
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
    /// Multi-protocol-specific final-output extraction (responses only).
    pub final_output: Option<fn(&Value) -> Option<String>>,
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
    pub usage_shape: UsageShape,
    pub usage_inclusion: TokenInclusion,
    /// Non-streaming final-output extraction path.
    pub final_output_path: &'static [&'static str],
    /// Whether the prefix stitcher may mint synthetic sessions for this
    /// protocol. USER RULING (§E, Langfuse best-practices: "If your
    /// application is single-request/single-response with no continuity
    /// between calls, you probably don't need sessions"): chat-completions
    /// SDK traffic is stateless — force-stitching sessions from accidental
    /// shared prefixes is noise. anthropic/responses carry session
    /// semantics (multi-turn replay); live has no replayable messages.
    pub stitch_eligible: bool,
    /// WS turn boundaries (live only; None for SSE protocols).
    pub turn_markers: Option<TurnMarkers>,
    /// Non-streaming tool-call extraction from the response body (G2: the
    /// streaming strategies had no non-streaming counterpart — complete
    /// tool items sit in the response body at rest).
    pub nonstreaming_tools: Option<fn(&Value) -> Vec<atg_model::ToolCall>>,
}

/// A body session source: a value path plus an optional transform. The
/// transform converts a composite value into a session id (e.g. anthropic's
/// legacy user_id envelope); when the transform fails the source falls
/// through to the next table row — never blocks.
#[derive(Clone, Copy)]
pub struct BodySource {
    pub path: &'static [&'static str],
    /// conversation-style: accept a plain string or a {id} object.
    pub two_form: bool,
    pub transform: Option<fn(&str) -> Option<String>>,
}

/// The registered protocol tables. A new provider appends one line here.
pub static DESCRIPTORS: &[&ProtocolDescriptor] = &[
    &anthropic::DESCRIPTOR,
    &openai::responses::DESCRIPTOR,
    &openai::chat::DESCRIPTOR,
    &openai::live::DESCRIPTOR,
];

impl ProtocolDescriptor {
    /// The descriptor whose path prefix matches, longest first.
    pub fn detect(path: &str) -> Option<&'static ProtocolDescriptor> {
        DESCRIPTORS
            .iter()
            .filter(|d| d.path_prefixes.iter().any(|p| path.starts_with(p)))
            .max_by_key(|d| d.path_prefixes.iter().map(|p| p.len()).max().unwrap_or(0))
            .copied()
    }

    /// The descriptor by protocol name (e.g. "openai.responses").
    pub fn detect_by_name(name: &str) -> Option<&'static ProtocolDescriptor> {
        DESCRIPTORS.iter().copied().find(|d| d.name == name)
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
                let item = req["messages"]
                    .as_array()?
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")?;
                content_text(&item["content"])
            }
            InputShape::Responses => (self.user_input?)(&req["input"]),
        }
    }

    /// Final output text from a non-streaming response body.
    pub fn final_output(&self, resp: &Value) -> Option<String> {
        match self.final_output {
            Some(f) => f(resp),
            None => content_text(resolve_path(resp, self.final_output_path)),
        }
    }
}

/// Two-form reader: conversation may be a plain id string or {id}.
fn read_source(body: &Value, src: &BodySource) -> Option<String> {
    let v = resolve_path(body, src.path);
    let raw = if src.two_form {
        // conversation: string or {id}.
        v.as_str()
            .or_else(|| v["id"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string()
    } else {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string()
    };
    match src.transform {
        Some(f) => f(&raw),
        None => Some(raw),
    }
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
            Value::Array(arr) => key
                .parse::<usize>()
                .ok()
                .and_then(|i| arr.get(i))
                .unwrap_or(&Value::Null),
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

/// Namespaces the borrowed prompt_cache_key affinity so it can never collide
/// with an explicit session id (`pck:` prefix per the design doc).
pub(crate) fn pck_namespace(v: &str) -> Option<String> {
    format!("pck:{v}").into()
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
        assert_eq!(
            ProtocolDescriptor::detect("/v1/messages").unwrap().name,
            "anthropic.messages"
        );
        assert_eq!(
            ProtocolDescriptor::detect("/v1/responses").unwrap().name,
            "openai.responses"
        );
        assert_eq!(
            ProtocolDescriptor::detect("/compatible-mode/v1/responses")
                .unwrap()
                .name,
            "openai.responses"
        );
        assert_eq!(
            ProtocolDescriptor::detect("/v1/chat/completions")
                .unwrap()
                .name,
            "openai.chat_completions"
        );
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
        let d = &openai::responses::DESCRIPTOR;
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
        let d = &openai::chat::DESCRIPTOR;
        assert!(d.sse_rules.iter().all(|r| r.on == "*"));
        assert_eq!(d.tool_calls, ToolCallStrategy::ChunkedToolCalls);
        assert_eq!(d.usage_inclusion, TokenInclusion::Inclusive);
        assert!(d.chain_sources.is_empty(), "chat has no chain primitive");
    }

    /// anthropic session sources: body metadata.session_id, then
    /// harness rules (CC user_id shapes — atg-harness); headers CC-first.
    #[test]
    fn anthropic_session_layering() {
        let d = &anthropic::DESCRIPTOR;
        assert_eq!(d.body_sources.len(), 2);
        assert_eq!(d.body_sources[0].path, &["session_id"]);
        assert_eq!(d.body_sources[1].path, &["metadata", "session_id"]);
        assert!(
            d.body_sources.iter().all(|s| s.transform.is_none()),
            "generic mounts carry no harness transforms (F2)"
        );
        assert_eq!(d.header_sources[0], mounts::HDR_CC_SESSION);
        assert!(
            d.chain_sources.is_empty(),
            "anthropic has no chain primitive"
        );
    }

    /// responses session layering: conversation > metadata.session_id >
    /// client_metadata.session_id > prompt_cache_key; chain = previous_response_id.
    #[test]
    fn responses_session_layering() {
        let d = &openai::responses::DESCRIPTOR;
        assert_eq!(d.body_sources[1].path, &["conversation"]);
        assert_eq!(d.body_sources[0].path, &["session_id"]);
        assert!(
            d.body_sources[1].two_form,
            "conversation accepts object-id form"
        );
        assert_eq!(d.body_sources.last().unwrap().path, &["prompt_cache_key"]);
        assert_eq!(d.chain_sources, &["previous_response_id"]);
        assert_eq!(d.user_sources, &["safety_identifier", "user"]);
    }

    #[test]
    fn conversation_two_form_reading() {
        let d = &openai::responses::DESCRIPTOR;
        assert_eq!(
            d.session_from_body(&value(r#"{"conversation":"conv_123"}"#)),
            Some("conv_123".to_string())
        );
        assert_eq!(
            d.session_from_body(&value(r#"{"conversation":{"id":"conv_9"}}"#)),
            Some("conv_9".to_string())
        );
        assert!(d
            .session_from_body(&value(r#"{"conversation":null}"#))
            .is_none());
    }

    #[test]
    fn bare_items_count_as_messages() {
        let body =
            value(r#"{"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#);
        let d = &openai::responses::DESCRIPTOR;
        assert_eq!(
            d.user_input.unwrap()(&body["input"]),
            Some("hi".to_string())
        );
    }

    /// G2: non-streaming tool extraction mirrors the streaming strategies —
    /// anthropic content[].tool_use, responses output[].function_call and
    /// chat choices[0].message.tool_calls[] all yield complete items.
    #[test]
    fn nonstreaming_tools_extract_from_all_three_shapes() {
        let anth = value(
            r#"{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"get_weather","input":{"city":"Paris"}}]}"#,
        );
        let d = &anthropic::DESCRIPTOR;
        let tools = d.nonstreaming_tools.unwrap()(&anth);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_weather");
        assert_eq!(tools[0].arguments, r#"{"city":"Paris"}"#);

        let resp = value(
            r#"{"output":[{"type":"function_call","name":"read_file","arguments":"{\"path\":\"/tmp/x\"}"}]}"#,
        );
        let d = &openai::responses::DESCRIPTOR;
        let tools = d.nonstreaming_tools.unwrap()(&resp);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].arguments, r#"{"path":"/tmp/x"}"#);

        let chat = value(
            r#"{"choices":[{"message":{"tool_calls":[{"function":{"name":"f","arguments":"{}"}}]}}]}"#,
        );
        let d = &openai::chat::DESCRIPTOR;
        let tools = d.nonstreaming_tools.unwrap()(&chat);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "f");
        assert_eq!(tools[0].arguments, "{}");
    }
}
