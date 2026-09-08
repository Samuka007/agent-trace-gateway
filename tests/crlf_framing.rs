//! BLOCK-A regression tests: CRLF frame separation in sse_data_frames /
//! stream_response (SSE-spec frames through a normalizing proxy).
//! Adopted from the RustGate reviewer probe.
use agent_trace_gateway::trace::descriptor::ProtocolDescriptor;
use agent_trace_gateway::trace::engine::{sse_data_frames, stream_response};

#[test]
fn crlf_multiframe_body_splits_into_frames() {
    // Two frames separated by CRLF CRLF (SSE spec-legal through a
    // normalizing proxy); content-type anthropic.
    let body = concat!(
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\r\n",
        "\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\r\n",
        "\r\n",
    );
    let frames = sse_data_frames(body.as_bytes());
    assert_eq!(
        frames.len(),
        2,
        "CRLF-separated frames must split: {frames:?}"
    );

    let d = ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
    let out = stream_response(d, body.as_bytes());
    assert_eq!(out.text, "hello world");
    assert_eq!(out.frame_errors, 0, "no frame may fail JSON parse");
}

#[test]
fn crlf_single_frame_parses() {
    let body = "data: {\"type\":\"ping\"}\r\n\r\n";
    let frames = sse_data_frames(body.as_bytes());
    assert_eq!(frames.len(), 1);
    assert_eq!(out_of(frames[0].as_ref()), r#"{"type":"ping"}"#);
}

#[test]
fn lf_multiframe_still_splits() {
    let body = concat!("data: {\"type\":\"a\"}\n\n", "data: {\"type\":\"b\"}\n\n",);
    assert_eq!(sse_data_frames(body.as_bytes()).len(), 2);
}

#[test]
fn multiline_data_joins_within_frame() {
    let body = "data: {\"a\":\r\ndata: 1}\r\n\r\n";
    let frames = sse_data_frames(body.as_bytes());
    assert_eq!(frames.len(), 1);
    assert_eq!(out_of(frames[0].as_ref()), "{\"a\":\n1}");
}

fn out_of(c: &str) -> &str {
    c
}
