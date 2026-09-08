//! WebSocket frame parsing and turn assembly for upgraded connections.
//! Client->server frames are masked per RFC 6455; server frames are not.
//! Turn boundaries come from protocol frames (response.create /
//! response.completed), never from connection lifetime.
use crate::trace::store::{ToolCall, TurnRecord};

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

    /// Feed raw bytes; returns complete text-frame payloads (split frames are
    /// buffered until whole).
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
            // Binary/control frames are ignored for trace purposes.
        }
        out
    }
}

/// One in-progress WS turn.
#[derive(Default)]
pub struct WsTurnState {
    pub input: Option<String>,
    pub session_id: String,
    pub output: String,
    pub tool_calls: Vec<ToolCall>,
    /// Verbatim client frame payload of the turn (response.create).
    pub raw_request: String,
    /// Verbatim server frame payloads accumulated for the turn.
    pub raw_response: String,
}

impl WsTurnState {
    pub fn active(&self) -> bool {
        self.input.is_some()
    }

    /// Client frames: a response.create frame starts a new turn; the session id
    /// is extracted from the frame's client_metadata at that moment.
    pub fn apply_client_frame(&mut self, payload: &[u8]) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return;
        };
        if v["type"] == "response.create" {
            let text = String::from_utf8_lossy(payload).to_string();
            self.input = Some(text.clone());
            self.raw_request = text;
            self.raw_response.clear();
            self.session_id = v["client_metadata"]["session_id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            self.output.clear();
            self.tool_calls.clear();
        }
    }

    /// Server frames: accumulate output/tool calls; response.completed ends the
    /// turn and returns its record.
    pub fn apply_server_frame(&mut self, payload: &[u8]) -> Option<TurnRecord> {
        self.raw_response
            .push_str(&String::from_utf8_lossy(payload));
        self.raw_response.push('\n');
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return None;
        };
        match v["type"].as_str() {
            Some("response.tool_call") => {
                self.tool_calls.push(ToolCall {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    arguments: v["arguments"].as_str().unwrap_or("").to_string(),
                });
            }
            Some("response.output_text.delta") => {
                if let Some(d) = v["delta"].as_str() {
                    self.output.push_str(d);
                }
            }
            Some("response.completed") => return Some(self.take_record()),
            _ => {}
        }
        None
    }

    pub fn take_record(&mut self) -> TurnRecord {
        TurnRecord {
            protocol: "openai_responses_ws".to_string(),
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
        }
    }
}
