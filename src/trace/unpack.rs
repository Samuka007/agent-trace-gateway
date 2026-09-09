//! Protocol unpacking: request/response bytes -> turn facts.
//! Slice 2.1 scope: non-streaming user_input + final_output for the three
//! model protocols. SSE/WS reassembly lands in later slices.
use atg_model::TurnRecord;
use serde_json::Value;

pub fn detect_protocol(path: &str) -> Option<&'static str> {
    atg_protocol::ProtocolDescriptor::detect(path).map(|d| d.name)
}

/// Detect whether a captured response is an SSE stream (by content type).
pub fn looks_like_sse(content_type: &str) -> bool {
    content_type.contains("text/event-stream")
}

/// Single-entry streaming reassembly: text output, token usage, tool calls
/// and the parse error marker in ONE traversal of the response body (the lib
/// SSE arm calls this once and fills all TurnRecord fields).
pub fn reassemble_sse(
    protocol: &str,
    response_body: &[u8],
) -> (
    String,
    Option<atg_model::TurnUsage>,
    Vec<atg_model::ToolCall>,
    Option<String>,
    u32,
) {
    let Some(d) = atg_protocol::ProtocolDescriptor::detect_by_name(protocol) else {
        return (String::new(), None, Vec::new(), None, 0);
    };
    let out = crate::engine::stream_response(d, response_body);
    (out.text, out.usage, out.tools, out.error, out.frame_errors)
}

/// Extract user input + final output from one non-streaming request/response
/// pair via the descriptor engine. Returns None when the protocol is unknown
/// or the request carries no user turn.
pub fn unpack_nonstreaming(
    protocol: &str,
    request: Option<&Value>,
    response_body: &[u8],
) -> Option<TurnRecord> {
    let d = atg_protocol::ProtocolDescriptor::detect_by_name(protocol)?;
    let req = request?;
    let resp: serde_json::Value = serde_json::from_slice(response_body).ok()?;
    crate::engine::nonstreaming(d, req, &resp)
}

/// Parse one request body once — the lib logging hook shares this parsed
/// Value with every extractor (no repeated req_buf traversal, C14).
pub fn parse_body(request_body: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice(request_body).ok()
}

/// Request-side facts extracted in ONE descriptor lookup + ONE pass over
/// the already-parsed body (the logging hook's single entry — C14).
pub struct TurnFacts {
    pub session_id: Option<String>,
    pub user_input: String,
    pub model_name: String,
    pub user_id: String,
    /// The replayable messages array (stitch-eligible protocols only).
    pub messages: Option<Vec<serde_json::Value>>,
    /// Whether the prefix stitcher may run for this protocol (F3 ruling).
    pub stitch_eligible: bool,
}

/// Single entry: resolve the session (protocol mounts → harness shapes →
/// header mounts), user input, model, end-user identity and the messages
/// array from one parsed request body. `protocol` resolves to exactly one
/// descriptor lookup shared by every extractor.
pub fn turn_facts(
    protocol: &str,
    req: &Value,
    header_get: &dyn Fn(&str) -> Option<String>,
    facts: &atg_harness::HarnessFacts,
) -> Option<TurnFacts> {
    let d = atg_protocol::ProtocolDescriptor::detect_by_name(protocol)?;
    let session_id = d
        .session_from_body(req)
        .or_else(|| atg_harness::session_from_body(facts, req))
        .or_else(|| d.session_from_headers(header_get))
        .or_else(|| atg_harness::session_from_headers(facts, header_get));
    Some(TurnFacts {
        session_id,
        user_input: d.user_input(req).unwrap_or_default(),
        model_name: req["model"].as_str().unwrap_or_default().to_string(),
        user_id: d.end_user(req).unwrap_or_default(),
        messages: extract_messages(req),
        stitch_eligible: d.stitch_eligible,
    })
}

/// F2 session pipeline (single parse — the Value is shared):
/// 1. protocol body mounts (generic table rows),
/// 2. harness body shapes (claude-code envelope/legacy — shape-gated, so
///    attribution failure never degrades extraction),
/// 3. protocol header mounts,
/// 4. harness header mounts (opencode x-session-* family, attribution-gated).
///
/// Same effective priority as the pre-v0.3.0 anthropic table rows.
pub fn resolve_session(
    protocol: &str,
    req: &Value,
    header_get: &dyn Fn(&str) -> Option<String>,
    facts: &atg_harness::HarnessFacts,
) -> Option<String> {
    let d = atg_protocol::ProtocolDescriptor::detect_by_name(protocol)?;
    d.session_from_body(req)
        .or_else(|| atg_harness::session_from_body(facts, req))
        .or_else(|| d.session_from_headers(header_get))
        .or_else(|| atg_harness::session_from_headers(facts, header_get))
}

/// Extract only the user input from a request body (streaming path; response
/// reassembly is handled separately).
pub fn extract_user_input(protocol: &str, request_body: &[u8]) -> Option<String> {
    let d = atg_protocol::ProtocolDescriptor::detect_by_name(protocol)?;
    let req: serde_json::Value = serde_json::from_slice(request_body).ok()?;
    d.user_input(&req)
}

/// Extract the full messages array from a parsed request body for prefix
/// stitching. Returns None when no non-empty messages array exists.
pub fn extract_messages(req: &Value) -> Option<Vec<serde_json::Value>> {
    req["messages"]
        .as_array()
        .map(|arr| arr.to_vec())
        .filter(|v| !v.is_empty())
}
