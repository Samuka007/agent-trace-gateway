//! openai.live — Realtime API (WebSocket) descriptor plus the WS wire
//! assembly. https://developers.openai.com/api/reference/resources/realtime
//! Turn boundaries come from protocol frames (turn_markers), never from
//! connection lifetime; the text/tool event names are sourced from this
//! descriptor's sse_rules (the same knowledge the SSE engine would use).
use crate::{ProtocolDescriptor, SseAction};
use atg_model::{ToolCall, TurnRecord};

pub static DESCRIPTOR: ProtocolDescriptor = ProtocolDescriptor {
    name: "openai.live",
    path_prefixes: &[],
    // WS upgrade connections: no HTTP path to match.
    loose_endpoints: &[],
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
            action: SseAction::Text(&["delta"]),
        },
        crate::SseRule {
            on: "response.output_item.done",
            data_type: None,
            delta_type: None,
            action: SseAction::ToolDone,
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
    stitch_eligible: false,
    turn_markers: Some(crate::TurnMarkers {
        start: "response.create",
        end: "response.done",
    }),
    nonstreaming_tools: None, // WS turn assembly owns tool extraction.
};

/// Event names sourced from the descriptor table (no literals here).
fn text_event() -> &'static str {
    DESCRIPTOR
        .sse_rules
        .iter()
        .find_map(|r| match r.action {
            SseAction::Text(_) => Some(r.on),
            _ => None,
        })
        .expect("live descriptor carries a Text rule")
}

fn tool_event() -> &'static str {
    DESCRIPTOR
        .sse_rules
        .iter()
        .find_map(|r| matches!(r.action, SseAction::ToolDone).then_some(r.on))
        .expect("live descriptor carries a ToolDone rule")
}

/// WebSocket frame parsing for upgraded connections: client->server frames
/// are masked per RFC 6455, server frames are not. Split frames are
/// buffered until whole; binary/control frames are ignored for trace
/// purposes.
pub struct WsFrameParser {
    buf: Vec<u8>,
    #[allow(dead_code)]
    expect_masked: bool,
}

impl WsFrameParser {
    pub fn new(expect_masked: bool) -> Self {
        Self {
            buf: Vec::new(),
            expect_masked,
        }
    }

    /// Feed raw bytes; returns complete text-frame payloads.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 2 {
                break;
            }
            let opcode = self.buf[0] & 0x0f;
            let masked = self.buf[1] & 0x80 != 0;
            let mut len = (self.buf[1] & 0x7f) as usize;
            let mut hdr = 2usize;
            if len == 126 {
                if self.buf.len() < 4 {
                    break;
                }
                len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                hdr = 4;
            } else if len == 127 {
                if self.buf.len() < 10 {
                    break;
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&self.buf[2..10]);
                len = u64::from_be_bytes(arr) as usize;
                hdr = 10;
            }
            let mask_len = if masked { 4 } else { 0 };
            let total = hdr + mask_len + len;
            if self.buf.len() < total {
                break;
            }
            let mut payload = self.buf[hdr + mask_len..total].to_vec();
            if masked {
                let mask = &self.buf[hdr..hdr + 4];
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            self.buf.drain(..total);
            if opcode == 0x1 {
                out.push(payload);
            }
        }
        out
    }
}

/// One in-progress WS turn.
#[derive(Default)]
pub struct WsTurnState {
    pub input: Option<String>,
    pub session_id: String,
    pub usage: Option<atg_model::TurnUsage>,
    pub output: String,
    pub tool_calls: Vec<ToolCall>,
    /// Verbatim client frame payload of the turn (response.create).
    pub raw_request: String,
    /// Verbatim server frame payloads accumulated for the turn.
    pub raw_response: String,
    model_name: String,
    user_id: String,
}

impl WsTurnState {
    pub fn active(&self) -> bool {
        self.input.is_some()
    }

    /// Client frames: the turn-start marker frame opens a new turn; the
    /// session id comes from the frame's client_metadata (sub2api injection
    /// convention), and the model/user identity ride the same frame body
    /// (§E ledger: ws take_record model/user).
    pub fn apply_client_frame(&mut self, payload: &[u8]) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return;
        };
        let start = DESCRIPTOR
            .turn_markers
            .as_ref()
            .map(|m| m.start)
            .unwrap_or_default();
        if v["type"] == start {
            let text = String::from_utf8_lossy(payload).to_string();
            self.input = Some(text.clone());
            self.raw_request = text;
            self.raw_response.clear();
            self.session_id = v["client_metadata"]["session_id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            self.model_name = v["model"].as_str().unwrap_or("").to_string();
            self.user_id = DESCRIPTOR.end_user(&v).unwrap_or_default();
            self.output.clear();
            self.tool_calls.clear();
        }
    }

    /// Server frames: accumulate output/tool calls; the turn-end marker
    /// closes the turn and returns its record (usage at the descriptor's
    /// usage path).
    pub fn apply_server_frame(&mut self, payload: &[u8]) -> Option<TurnRecord> {
        self.raw_response
            .push_str(&String::from_utf8_lossy(payload));
        self.raw_response.push('\n');
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return None;
        };
        let event = v["type"].as_str();
        if event == Some(tool_event()) && v["item"]["type"] == "function_call" {
            self.tool_calls.push(ToolCall {
                name: v["item"]["name"].as_str().unwrap_or("").to_string(),
                arguments: v["item"]["arguments"].as_str().unwrap_or("").to_string(),
            });
        } else if event == Some(text_event()) {
            if let Some(d) = v["delta"].as_str() {
                self.output.push_str(d);
            }
        } else if event == DESCRIPTOR.turn_markers.as_ref().map(|m| m.end) {
            let usage_path = DESCRIPTOR.usage_frames[0].obj_path;
            let usage = crate::resolve_path(&v, usage_path);
            if !usage.is_null() {
                self.usage = Some(crate::usage::usage_from_obj(&DESCRIPTOR, usage));
            }
            return Some(self.take_record());
        }
        None
    }

    pub fn take_record(&mut self) -> TurnRecord {
        TurnRecord {
            protocol: DESCRIPTOR.name.to_string(),
            session_id: std::mem::take(&mut self.session_id),
            user_input: self.input.take().unwrap_or_default(),
            final_output: std::mem::take(&mut self.output),
            raw_request: std::mem::take(&mut self.raw_request),
            raw_response: std::mem::take(&mut self.raw_response),
            tool_calls: std::mem::take(&mut self.tool_calls),
            breakpoint: false,
            // Timing is filled by the gateway (Ctx) after take_record.
            start_ns: 0,
            end_ns: 0,
            usage: self.usage.take(),
            error: None,
            model_name: std::mem::take(&mut self.model_name),
            user_id: std::mem::take(&mut self.user_id),
            // WS turns carry no harness attribution yet (v0.3.0 scope).
            harness: String::new(),
            dialect: String::new(),
            client_ua: String::new(),
            api_key_fp: String::new(),
            cancelled: false,
            drain_timed_out: false,
            harness_candidates: Vec::new(),
            harness_anomaly: false,
            harness_enrich: Vec::new(),
            session_synthetic: false,
            completion_start_ns: None,
        }
    }
}
