//! openai.live — Realtime API (WebSocket) descriptor plus the WS wire
//! assembly. https://developers.openai.com/api/reference/resources/realtime
//! Turn boundaries come from protocol frames (turn_markers), never from
//! connection lifetime; the text/tool event names are sourced from this
//! descriptor's sse_rules (the same knowledge the SSE engine would use).
// PANIC-AUDIT v0.3.8: serde_json Value key-index in apply_client_frame /
// apply_server_frame is panic-free for the audited shapes (miss → Null;
// frames are JSON objects) — the indexing_slicing lint is syntax-broad
// over Value::index. Tracked in the PanicAudit issue.
// The two expects below are static descriptor-contract asserts: the table
// is a compile-time constant whose sse_rules are fixed in this file.
#![allow(clippy::indexing_slicing, clippy::expect_used)]
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
    /// Optional single-frame payload cap in bytes — 0 (the default) means
    /// UNLIMITED: a declared length is only waited on, memory grows with
    /// the bytes actually received (DoS accepted by the operator). A
    /// non-zero cap refuses frames declaring beyond it.
    max_payload: usize,
}

impl WsFrameParser {
    pub fn new(expect_masked: bool) -> Self {
        Self {
            buf: Vec::new(),
            expect_masked,
            max_payload: 0,
        }
    }

    /// Cap-enabled constructor: frames whose DECLARED payload exceeds
    /// `max_payload` bytes are refused (parse buffer dropped; later bytes
    /// restart a fresh parse). `max_payload` 0 = unlimited.
    pub fn with_max_payload(expect_masked: bool, max_payload: usize) -> Self {
        Self {
            buf: Vec::new(),
            expect_masked,
            max_payload,
        }
    }

    /// Feed raw bytes; returns complete text-frame payloads.
    ///
    /// Panic-free by construction over ARBITRARY remote bytes (v0.3.8 WS
    /// hardening): the 64-bit declared frame length is remote-controlled —
    /// checked arithmetic replaces the overflowing add (that guard is
    /// against an arithmetic panic, not a size limit), and frames whose
    /// payload exceeds the configured cap are refused (parse buffer
    /// dropped; later bytes restart a fresh parse — invalid frames are
    /// ignored by the turn state, the stream itself keeps flowing).
    /// A frame declaring an absurd length simply waits for bytes that
    /// never come — memory stays bounded by the bytes actually received.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while let Some(&[b0, b1]) = self.buf.first_chunk::<2>() {
            let opcode = b0 & 0x0f;
            let masked = b1 & 0x80 != 0;
            let mut len = u64::from(b1 & 0x7f);
            let mut hdr = 2usize;
            if len == 126 {
                let Some(h4) = self.buf.first_chunk::<4>() else {
                    break;
                };
                len = u16::from_be_bytes([h4[2], h4[3]]) as u64;
                hdr = 4;
            } else if len == 127 {
                let Some(h10) = self.buf.first_chunk::<10>() else {
                    break;
                };
                let &[_, _, a, b, c, d, e, f, g, h] = h10;
                len = u64::from_be_bytes([a, b, c, d, e, f, g, h]);
                hdr = 10;
            }
            let mask_len: u64 = if masked { 4 } else { 0 };
            let Some(total) = (hdr as u64)
                .checked_add(mask_len)
                .and_then(|t| t.checked_add(len))
            else {
                // Overflow impossible even in principle (len < 2^64): the
                // arithmetic above already bounds it — kept for rigor.
                self.buf.clear();
                return out;
            };
            if self.max_payload > 0 && len > self.max_payload as u64 {
                // Remote-declared oversized frame: refuse. The frame's wire
                // bytes cannot be skipped without trusting `len`, so the
                // parse buffer is dropped — transparent forward is
                // unaffected, turn parsing restarts on a clean slate.
                self.buf.clear();
                return out;
            }
            let total = total as usize;
            if self.buf.len() < total {
                break;
            }
            let payload_start = hdr.saturating_add(mask_len as usize);
            let Some(payload_bytes) = self.buf.get(payload_start..total) else {
                break; // unreachable: total <= buf.len() by the check above
            };
            let mut payload = payload_bytes.to_vec();
            if masked {
                let Some(mask) = self.buf.get(hdr..hdr.saturating_add(4)) else {
                    break; // unreachable: total >= hdr + 4 when masked
                };
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i & 3]; // mask length 4: & 3 == % 4
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // v0.3.8 WS hardening: push() must be total over ARBITRARY remote
    // bytes — including frames declaring lengths around u64::MAX, where
    // the pre-fix arithmetic overflowed and reversed-sliced (exit 101 on
    // any WS-upgraded connection, both directions).
    proptest! {
        #[test]
        fn ws_push_never_panics(data in proptest::collection::vec(proptest::num::u8::ANY, 0..512)) {
            let mut p = WsFrameParser::new(false);
            let _ = p.push(&data);
            let mut masked = WsFrameParser::new(true);
            let _ = masked.push(&data);
        }

        #[test]
        fn ws_push_u64max_lengths_never_panics(
            len in proptest::num::u64::ANY,
            prefix in proptest::collection::vec(proptest::num::u8::ANY, 0..16),
            tail in proptest::collection::vec(proptest::num::u8::ANY, 0..64),
            opcode in 0x00u8..=0x0f,
            masked in proptest::bool::ANY,
        ) {
            // 127-form frame header declaring `len` — the hostile space.
            let mut frame = vec![opcode, 0x7f | if masked { 0x80 } else { 0 }];
            frame.extend_from_slice(&len.to_be_bytes());
            frame.extend_from_slice(&tail);
            let mut p = WsFrameParser::new(false);
            let _ = p.push(&prefix);
            let _ = p.push(&frame);
            // A follow-up push after any buffer state must stay safe too.
            let _ = p.push(b"\x81\x05hello");
        }
    }

    /// Cap semantics (v0.3.8 final ruling): cap 0 (the default) never
    /// refuses — a fully-delivered frame of ANY declared size parses; a
    /// non-zero cap refuses frames declaring beyond it and the parser
    /// restarts clean on the next push.
    #[test]
    fn ws_cap_zero_accepts_and_nonzero_refuses() {
        // A real 1 MiB text frame: parsed under an unlimited cap.
        let payload = vec![b'x'; 1 << 20];
        let mut frame = vec![0x81u8, 0x7f];
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        frame.extend_from_slice(&payload);
        let mut p = WsFrameParser::new(false);
        assert_eq!(p.push(&frame), vec![payload.clone()], "cap 0 must accept");

        // cap 1 MiB: a frame declaring beyond it is refused on the declared
        // length alone (empty), and the parser restarts cleanly on the next
        // push.
        let mut capped = WsFrameParser::with_max_payload(false, 1 << 20);
        let mut over = vec![0x81u8, 0x7f];
        over.extend_from_slice(&((payload.len() + 1) as u64).to_be_bytes());
        over.extend_from_slice(&payload[..64]);
        assert!(capped.push(&over).is_empty(), "oversized frame refused");
        assert_eq!(
            capped.push(b"\x81\x05hello"),
            vec![b"hello".to_vec()],
            "the parser restarts on a clean slate after a refusal"
        );
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
