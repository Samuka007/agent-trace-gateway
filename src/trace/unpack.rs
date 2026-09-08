//! Protocol unpacking: request/response bytes -> turn facts.
//! Slice 2.1 scope: non-streaming user_input + final_output for the three
//! model protocols. SSE/WS reassembly lands in later slices.
use crate::trace::store::TurnRecord;

pub fn detect_protocol(path: &str) -> Option<&'static str> {
    crate::trace::descriptor::ProtocolDescriptor::detect(path).map(|d| d.name)
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
    Option<crate::trace::adaptor::TurnUsage>,
    Vec<crate::trace::store::ToolCall>,
    Option<String>,
) {
    let Some(d) = crate::trace::descriptor::ProtocolDescriptor::detect_by_name(protocol) else {
        return (String::new(), None, Vec::new(), None);
    };
    let out = crate::trace::engine::stream_response(d, response_body);
    (out.text, out.usage, out.tools, out.error)
}

/// Extract user input + final output from one non-streaming request/response
/// pair via the descriptor engine. Returns None when the protocol is unknown
/// or the request carries no user turn.
pub fn unpack_nonstreaming(
    protocol: &str,
    request_body: &[u8],
    response_body: &[u8],
) -> Option<TurnRecord> {
    let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name(protocol)?;
    let req: serde_json::Value = serde_json::from_slice(request_body).ok()?;
    let resp: serde_json::Value = serde_json::from_slice(response_body).ok()?;
    crate::trace::engine::nonstreaming(d, &req, &resp)
}

/// Extract only the user input from a request body (streaming path; response
/// reassembly is handled separately).
pub fn extract_user_input(protocol: &str, request_body: &[u8]) -> Option<String> {
    let d = crate::trace::descriptor::ProtocolDescriptor::detect_by_name(protocol)?;
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
