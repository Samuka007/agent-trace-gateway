//! The single protocol interpretation engine: consumes a `ProtocolDescriptor`
//! and produces turn facts. This is the only place allowed to branch on
//! wire-format knowledge; protocol details themselves live in the const
//! descriptor tables.
use atg_model::ToolCall;
use atg_protocol::{resolve_path, ProtocolDescriptor, SseAction, SseRule, ToolCallStrategy};

/// Split an SSE body into `data:` payloads per the SSE spec. Line-driven
/// frame separation works identically for LF-LF and CRLF-CRLF delimiters
/// (each line sheds its trailing CR). Zero-copy on the UTF-8-valid fast
/// path: frames and single data lines borrow from the body; allocation only
/// for multi-data-line frames or lossy replacement of invalid bytes.
pub fn sse_data_frames(body: &[u8]) -> Vec<std::borrow::Cow<'_, str>> {
    // Fast path: valid UTF-8 borrows from the body — the lifetime of
    // from_utf8's &str is the body's, so frames borrow the caller's buffer.
    if let Ok(text) = std::str::from_utf8(body) {
        return collect_frames(text);
    }
    // Lossy fallback: frames own their strings (allocation only here).
    let owned = String::from_utf8_lossy(body).into_owned();
    collect_frames(&owned)
        .into_iter()
        .map(std::borrow::Cow::into_owned)
        .map(std::borrow::Cow::Owned)
        .collect()
}

/// Frame splitter over a &str body (borrowed frames; allocation only when a
/// frame has multiple data lines).
fn collect_frames<'a>(text: &'a str) -> Vec<std::borrow::Cow<'a, str>> {
    let mut out = Vec::new();
    let mut data: Option<std::borrow::Cow<'a, str>> = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            // Blank line = frame boundary; flush any pending multi-line join.
            if let Some(d) = data.take() {
                out.push(d);
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        match &mut data {
            Some(d) => {
                d.to_mut().push('\n');
                d.to_mut().push_str(rest);
            }
            None => data = Some(std::borrow::Cow::Borrowed(rest)),
        }
    }
    // Trailing frame without a blank-line terminator.
    if let Some(d) = data.take() {
        out.push(d);
    }
    out
}

/// Streaming accumulator state driven by the descriptor's sse_rules.
#[derive(Default)]
pub struct SseAccum {
    pub text: String,
    pub usage: Option<atg_model::TurnUsage>,
    /// Data frames that failed to parse as JSON (observability counter).
    pub frame_errors: u32,
    /// Terminal-frame error marker (response.failed / response.incomplete /
    /// anthropic event:error) — protocol error text when present.
    pub error: Option<String>,
    tools: Vec<ToolCall>,
    pending: Option<(u64, ToolCall)>,
    chat_tools: Vec<(u64, ToolCall)>,
}

/// Full result of one streaming pass.
pub struct SseOutcome {
    pub text: String,
    pub usage: Option<atg_model::TurnUsage>,
    pub tools: Vec<ToolCall>,
    pub frame_errors: u32,
    pub error: Option<String>,
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
                                acc.chat_tools.push((
                                    idx,
                                    ToolCall {
                                        name: String::new(),
                                        arguments: String::new(),
                                    },
                                ));
                                acc.chat_tools.last_mut().unwrap()
                            }
                        };
                        // First hit wins: compatible gateways may resend the
                        // function name on later chunks — appending would
                        // duplicate it. Arguments stay append-only (they are
                        // genuinely chunked).
                        if let Some(n) = tc["function"]["name"].as_str() {
                            if entry.1.name.is_empty() {
                                entry.1.name = n.to_string();
                            }
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
pub fn stream_response(d: &ProtocolDescriptor, body: &[u8]) -> SseOutcome {
    let mut acc = SseAccum::default();
    for data in sse_data_frames(body) {
        feed_frame(&mut acc, d, &data);
    }
    finish_accum(acc, d)
}

/// Parse one SSE data frame into the accumulator — the exact per-frame path
/// stream_response has always used (parse → error marker → rules → usage).
fn feed_frame(acc: &mut SseAccum, d: &ProtocolDescriptor, data: &str) {
    // Protocol-legal non-payload frames — skipped, never counted as
    // errors: `:`-comment keep-alives produce empty payloads (the
    // comment lines themselves never yield a data frame), and
    // `data: [DONE]` is the OpenAI chat termination sentinel.
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        acc.frame_errors += 1;
        return;
    };
    // slop#6: terminal error frames mark the turn errored instead of
    // recording a silently-successful half turn.
    if let Some(err_text) = error_marker(d.name, &v) {
        acc.error = Some(err_text);
    }
    let event = v["type"].as_str();
    apply_sse_rule(acc, d, event, &v);
    // Usage harvest is FRAME-driven (descriptor usage_frames), never
    // rule-gated: anthropic carries no Usage SSE action (its usage
    // rides message_start/message_delta events) — gating on an action
    // silently dropped streaming anthropic usage pre-v0.3.0.
    if let Some(u) = atg_protocol::usage::usage_from_sse_frame(d, &v) {
        atg_model::merge_usage(&mut acc.usage, u);
    }
}

/// Finish an incrementally-fed accumulator (drain_feed) into the same
/// outcome shape stream_response returns.
pub fn finish_accum(mut acc: SseAccum, d: &ProtocolDescriptor) -> SseOutcome {
    let text = std::mem::take(&mut acc.text);
    let usage = acc.usage.take();
    let error = acc.error.take();
    let frame_errors = acc.frame_errors;
    let mut tools = std::mem::take(&mut acc.tools);
    if let Some((_, call)) = acc.pending.take() {
        tools.push(call);
    }
    if d.tool_calls == ToolCallStrategy::ChunkedToolCalls {
        acc.chat_tools.sort_by_key(|(i, _)| *i);
        tools.extend(acc.chat_tools.into_iter().map(|(_, c)| c));
    }
    SseOutcome {
        text,
        usage,
        tools,
        frame_errors,
        error,
    }
}

/// Incremental drain feed (v0.3.6): append one raw body chunk to `buf` and
/// parse every COMPLETE SSE frame into `acc` — the identical per-frame path
/// as stream_response, executed chunk-boundary-safely. `buf` retains the
/// incomplete tail as RAW BYTES (possibly a half frame or half delimiter):
/// chunk boundaries may fall mid-frame and mid-multibyte-UTF-8 without
/// corruption, because the complete prefix always ends on a delimiter — an
/// ASCII byte, hence a valid UTF-8 boundary — before it is decoded.
pub fn drain_feed(acc: &mut SseAccum, d: &ProtocolDescriptor, buf: &mut Vec<u8>, chunk: &[u8]) {
    buf.extend_from_slice(chunk);
    let Some(cut) = last_frame_boundary(buf) else {
        return;
    };
    let complete = String::from_utf8_lossy(&buf[..cut]).into_owned();
    buf.drain(..cut);
    for data in collect_frames(&complete) {
        feed_frame(acc, d, &data);
    }
}

/// Drain end (v0.3.6): flush the retained tail (a trailing frame without a
/// blank-line terminator is legal SSE) and finish the accumulator — the
/// outcome equals stream_response on the same concatenated body.
pub fn drain_finish(mut acc: SseAccum, d: &ProtocolDescriptor, buf: &mut Vec<u8>) -> SseOutcome {
    let tail = std::mem::take(buf);
    let text = String::from_utf8_lossy(&tail).into_owned();
    for data in collect_frames(&text) {
        feed_frame(&mut acc, d, &data);
    }
    finish_accum(acc, d)
}

/// End offset of the LAST complete SSE frame boundary in `body` — a blank
/// line, "\n\n" or CRLF-CRLF ("\n\r\n"; the lines' trailing CRs are shed by
/// the line splitter). None while only a partial frame has arrived.
fn last_frame_boundary(body: &[u8]) -> Option<usize> {
    let end_of = |pat: &[u8]| {
        if body.len() < pat.len() {
            None
        } else {
            body.windows(pat.len())
                .rposition(|w| w == pat)
                .map(|p| p + pat.len())
        }
    };
    match (end_of(b"\n\r\n"), end_of(b"\n\n")) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (found_a, found_b) => found_a.or(found_b),
    }
}

/// Detect protocol error markers on terminal/exception frames: returns the
/// protocol error text when the frame signals failure.
fn error_marker(protocol: &str, v: &serde_json::Value) -> Option<String> {
    match protocol {
        "openai.responses" | "openai.live" => {
            if matches!(
                v["type"].as_str(),
                Some("response.failed") | Some("response.incomplete")
            ) {
                return Some(v["response"]["error"].to_string());
            }
            None
        }
        "anthropic.messages" => {
            if v["type"].as_str() == Some("error") {
                return Some(v["error"].to_string());
            }
            None
        }
        _ => None,
    }
}

/// Non-streaming: user_input/final_output/usage in one pass over the
/// already-parsed bodies.
pub fn nonstreaming(
    d: &ProtocolDescriptor,
    req: &serde_json::Value,
    resp: &serde_json::Value,
) -> Option<atg_model::TurnRecord> {
    let user_input = d.user_input(req)?;
    let final_output = d.final_output(resp).unwrap_or_default();
    let usage = atg_protocol::usage::usage_from_nonstreaming(d, resp);
    let model_name = req["model"].as_str().unwrap_or_default().to_string();
    let user_id = d.end_user(req).unwrap_or_default();
    // G2: complete tool items sit in the response body at rest — the
    // descriptor's extractor mirrors the streaming strategies.
    let tool_calls = d.nonstreaming_tools.map(|f| f(resp)).unwrap_or_default();
    Some(atg_model::TurnRecord {
        protocol: d.name.to_string(),
        user_input,
        final_output,
        usage,
        model_name,
        user_id,
        tool_calls,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counter semantics (user probe): protocol-legal non-payload frames
    /// must not inflate frame_errors — comment keep-alives and the
    /// OpenAI [DONE] sentinel are stream furniture, not parse failures.
    #[test]
    fn comment_and_done_frames_never_count_as_errors() {
        let d =
            atg_protocol::ProtocolDescriptor::detect_by_name("openai.chat_completions").unwrap();
        let body = concat!(
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            ": ping\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data:\n\n",
            "data: [DONE]\n\n",
        );
        let out = stream_response(d, body.as_bytes());
        assert_eq!(out.text, "hello");
        assert_eq!(
            out.frame_errors, 0,
            "comment frames, empty payloads and [DONE] are legal stream furniture"
        );
        // The anthropic flavor: same furniture, no [DONE].
        let d = atg_protocol::ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
        let body = concat!(
            ": подключение установлено\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"привет\"}}\n\n",
            ": ping\n\n",
        );
        let out = stream_response(d, body.as_bytes());
        assert_eq!(out.text, "привет");
        assert_eq!(out.frame_errors, 0);
        // Genuinely broken frames still count (the counter must stay honest).
        let body = "data: not-json\n\n";
        let out = stream_response(d, body.as_bytes());
        assert_eq!(out.frame_errors, 1);
    }

    /// Pre-v0.3.0 gap pin: streaming anthropic usage rides
    /// message_start/message_delta (split frames, last-wins merge) — the
    /// harvest must be frame-driven, not gated on a Usage SSE action
    /// (anthropic's table carries none).
    #[test]
    fn anthropic_stream_usage_accumulates_across_frames() {
        let d = atg_protocol::ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":3}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
        );
        let out = stream_response(d, body.as_bytes());
        assert_eq!(out.text, "hi");
        let u = out
            .usage
            .expect("streaming anthropic usage must accumulate");
        assert_eq!(u.input_tokens, Some(12));
        assert_eq!(u.cache_read_tokens, Some(3));
        assert_eq!(u.output_tokens, Some(7));
    }

    fn chat_chunk(name: &str, arguments: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "function": {"name": name, "arguments": arguments}
                    }]}
                }]
            })
        )
    }

    /// Incidental (R3 #4): a chat tool name re-sent on later chunks must be
    /// recorded once (first hit wins) — appending would duplicate it on
    /// name-resending compatible gateways. Chunked arguments still
    /// concatenate.
    #[test]
    fn chat_tool_name_first_hit_wins() {
        let d =
            atg_protocol::ProtocolDescriptor::detect_by_name("openai.chat_completions").unwrap();
        let body =
            chat_chunk("read_file", r#"{"pa"#) + &chat_chunk("read_file", r#"th":"/tmp/x"}"#);
        let out = stream_response(d, body.as_bytes());
        assert_eq!(out.tools.len(), 1);
        assert_eq!(
            out.tools[0].name, "read_file",
            "re-sent name must not concatenate: {:?}",
            out.tools[0].name
        );
        assert_eq!(out.tools[0].arguments, r#"{"path":"/tmp/x"}"#);
    }

    /// Drain equivalence (v0.3.6): feeding a stream through drain_feed one
    /// byte at a time must produce EXACTLY the outcome of a whole-body
    /// stream_response pass — chunk boundaries may land anywhere, including
    /// inside frames and inside CRLF delimiters.
    #[test]
    fn drain_feed_byte_split_equals_whole_body_parse() {
        let d = atg_protocol::ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
        let body = concat!(
            ": keep-alive\r\n\r\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\r\n\r\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"he\"}}\n\n",
            "event: x\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"y\"}}\r\n\r\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\r\n",
            "data: [DONE]\n\n",
        );
        let whole = stream_response(d, body.as_bytes());
        assert_eq!(whole.text, "hey");
        let u = whole.usage.as_ref().expect("usage");
        assert_eq!(u.input_tokens, Some(9));
        assert_eq!(u.output_tokens, Some(4));

        // Every single-byte split position must agree with the whole-body pass.
        for split in 0..body.len() {
            let mut acc = SseAccum::default();
            let mut buf = Vec::new();
            drain_feed(&mut acc, d, &mut buf, &body.as_bytes()[..split]);
            drain_feed(&mut acc, d, &mut buf, &body.as_bytes()[split..]);
            let out = drain_finish(acc, d, &mut buf);
            assert_eq!(out.text, whole.text, "split at {split}");
            assert_eq!(out.usage, whole.usage, "split at {split}");
            assert_eq!(out.frame_errors, whole.frame_errors, "split at {split}");
        }

        // Multibyte text: chunk boundaries may fall inside a code point.
        let body = concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"привет\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
        );
        let whole = stream_response(d, body.as_bytes());
        assert_eq!(whole.text, "привет");
        for split in 0..body.len() {
            let mut acc = SseAccum::default();
            let mut buf = Vec::new();
            drain_feed(&mut acc, d, &mut buf, &body.as_bytes()[..split]);
            drain_feed(&mut acc, d, &mut buf, &body.as_bytes()[split..]);
            let out = drain_finish(acc, d, &mut buf);
            assert_eq!(out.text, whole.text, "multibyte split at {split}");
        }
    }
}
