//! Descriptor-engine micro-benchmarks (std Instant, 3 rounds, median) —
//! performance gate for the protocol-descriptor rewrite. Run:
//! `cargo test --test sse_bench --release -- --nocapture`
use agent_trace_gateway::trace::descriptor::ProtocolDescriptor;
use agent_trace_gateway::trace::engine::stream_response;
use std::time::Instant;

fn make_sse_body(n_frames: usize, text_per_frame: usize) -> Vec<u8> {
    let chunk = "x".repeat(text_per_frame);
    let mut body = String::with_capacity(n_frames * (text_per_frame + 90));
    for _ in 0..n_frames {
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{chunk}\"}}\n\n"
        ));
    }
    body.into_bytes()
}

fn make_anthropic_tool_body(frames: usize, tools: usize) -> Vec<u8> {
    // Prebuild argument fragments: {"k":"v<block>-<i>"} per delta frame.
    let mut body = String::new();
    let per_block = frames / tools.max(1);
    let mut block = 0usize;
    for i in 0..frames {
        if i % per_block == 0 && block < tools {
            block += 1;
            let start = format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{block},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_{block}\",\"name\":\"tool_{block}\"}}}}\n\n"
            );
            body.push_str(&start);
        }
        let fragment = "{\"k\":\"v".to_string() + &format!("{block}-{i}") + "\"}";
        let delta = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{block},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{fragment}\"}}}}\n\n"
        );
        body.push_str(&delta);
    }
    body.into_bytes()
}

fn median_of3<T: Ord + Copy>(mut f: impl FnMut() -> T) -> T {
    let mut v = [f(), f(), f()];
    v.sort();
    v[1]
}

/// Release-mode only: run with `--release`. Debug builds are ~8x slower by
/// nature and would false-fail the timing gate.
#[test]
#[cfg_attr(debug_assertions, ignore = "bench gate is release-mode only")]
fn bench_sse_reassembly_256k() {
    let d = ProtocolDescriptor::detect_by_name("openai.responses").unwrap();
    let body = make_sse_body(256, 1024); // ~272KB
    let run = || {
        let start = Instant::now();
        let out = stream_response(d, &body);
        (start.elapsed(), out.text.len())
    };
    let (elapsed, text_len) = median_of3(run);
    println!("SSE 264KB reassembly: {elapsed:?} (text {text_len} bytes)");
    assert!(text_len > 0);
    // Hard gate: <= 1.7ms (v0.2.0 baseline); target 0.8ms.
    assert!(
        elapsed <= std::time::Duration::from_millis(1),
        "SSE reassembly regressed: {elapsed:?}"
    );
}

/// Release-mode only (see bench_sse_reassembly_256k).
#[test]
#[cfg_attr(debug_assertions, ignore = "bench gate is release-mode only")]
fn bench_anthropic_tool_stream() {
    let d = ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
    let body = make_anthropic_tool_body(3000, 50);
    let run = || {
        let start = Instant::now();
        let out = stream_response(d, &body);
        (start.elapsed(), out.tools.len())
    };
    let (elapsed, tools) = median_of3(run);
    println!("anthropic 3000-frame/50-tool stream: {elapsed:?} ({tools} tools)");
    assert_eq!(tools, 50);
}
