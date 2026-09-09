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
    request_body: &[u8],
    response_body: &[u8],
) -> Option<TurnRecord> {
    let d = atg_protocol::ProtocolDescriptor::detect_by_name(protocol)?;
    let req: serde_json::Value = serde_json::from_slice(request_body).ok()?;
    let resp: serde_json::Value = serde_json::from_slice(response_body).ok()?;
    crate::engine::nonstreaming(d, &req, &resp)
}

/// Parse one request body once — the lib logging hook shares this parsed
/// Value with every extractor (no repeated req_buf traversal, C14).
pub fn parse_body(request_body: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice(request_body).ok()
}

/// &Value entry: session id from body sources, then the descriptor's header
/// sources (body wins over header — same priority as the bytes entry).
pub fn session_from_parsed(
    protocol: &str,
    req: &Value,
    header_get: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let d = atg_protocol::ProtocolDescriptor::detect_by_name(protocol)?;
    d.session_from_body(req)
        .or_else(|| d.session_from_headers(header_get))
}

/// Extract only the user input from a request body (streaming path; response
/// reassembly is handled separately).
pub fn extract_user_input(protocol: &str, request_body: &[u8]) -> Option<String> {
    let d = atg_protocol::ProtocolDescriptor::detect_by_name(protocol)?;
    let req: serde_json::Value = serde_json::from_slice(request_body).ok()?;
    d.user_input(&req)
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
