// Per-request cost model for the production long-stream shape (ATG#5 follow-up).
//
// Measures the coefficients of the production code paths on realistic
// payloads — no exotic configuration, just the functions the request path
// actually calls:
//
//   capture bound()        lib.rs push_record path -> trace/capture.rs bound()
//   request parse          trace/unpack.rs parse_body + atg_harness::identify + turn_facts
//   SSE reassembly         trace/unpack.rs reassemble_sse   (events × c2 + bytes × c3)
//   store.push deep copy   lib.rs:132 store.push(record.clone())
//   exporter submit copy   trace/export.rs:80 try_send(record.clone()) — the
//                          same deep clone as above, so it is measured by the
//                          same term (no endpoint is configured for this test)
//
// Run on demand:  cargo test --release --test turn_cost_model -- --ignored --nocapture
use agent_trace_gateway::trace::capture::CaptureCap;
use agent_trace_gateway::trace::unpack;

const PROTOCOL: &str = "anthropic.messages";

/// One valid anthropic SSE text-delta frame of about `frame_bytes`.
fn frame(frame_bytes: usize) -> String {
    let head = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"";
    let tail = "\"}}\n\n";
    let filler = frame_bytes.saturating_sub(head.len() + tail.len()).max(1);
    format!("{head}{}{tail}", "x".repeat(filler))
}

fn sse_body(total_bytes: usize, frame_bytes: usize) -> String {
    let mut out = String::with_capacity(total_bytes + frame_bytes);
    while out.len() < total_bytes {
        out.push_str(&frame(frame_bytes));
    }
    out
}

fn request_body(user_bytes: usize) -> String {
    format!(
        r#"{{"model":"m","max_tokens":1024,"messages":[{{"role":"system","content":"sys"}},{{"role":"user","content":"{}"}}]}}"#,
        "y".repeat(user_bytes)
    )
}

fn time_us(iters: u32, mut f: impl FnMut()) -> f64 {
    let start = std::time::Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_secs_f64() * 1e6 / f64::from(iters)
}

#[test]
#[ignore = "cost model instrument: run with --ignored --nocapture"]
fn per_request_cost_coefficients() {
    let cap = CaptureCap::new();
    let header_get = |_: &str| -> Option<String> { None };

    let iters = 20;
    println!("protocol={PROTOCOL} iters={iters}");
    println!(
        "{:>10} {:>10} {:>12} {:>12} {:>12} {:>12}",
        "resp_MiB", "frame_B", "bound_us", "parse_us", "sse_us", "clone_us"
    );

    for resp_mib in [1usize, 8] {
        let resp_bytes = resp_mib << 20;
        for frame_bytes in [119usize, 842, resp_bytes / 4] {
            let resp = sse_body(resp_bytes, frame_bytes);
            let req = request_body(4096);
            let resp_raw = resp.as_bytes();

            let t_bound = time_us(iters, || {
                let _ = std::hint::black_box(cap.bound(resp_raw));
            });

            let t_parse = time_us(iters, || {
                let parsed = unpack::parse_body(req.as_bytes());
                let hfacts =
                    atg_harness::identify(PROTOCOL, parsed.as_ref(), Some("omp/1"), &header_get);
                let _ = parsed
                    .as_ref()
                    .and_then(|r| unpack::turn_facts(PROTOCOL, r, &header_get, &hfacts));
            });

            let t_sse = time_us(iters, || {
                let _ = std::hint::black_box(unpack::reassemble_sse(PROTOCOL, resp_raw));
            });

            // A record shaped like the real one: the payload lives in the raw
            // captures (the fields store.push/estimate walk).
            let record = atg_model::TurnRecord {
                protocol: PROTOCOL.to_string(),
                raw_request: req.clone(),
                raw_response: resp.clone(),
                user_input: "u".repeat(32),
                final_output: "o".repeat(256),
                ..Default::default()
            };
            let t_clone = time_us(iters, || {
                let _ = std::hint::black_box(record.clone());
            });

            println!(
                "{:>10} {:>10} {:>12.1} {:>12.1} {:>12.1} {:>12.1}",
                resp_mib, frame_bytes, t_bound, t_parse, t_sse, t_clone
            );
            // Per-MiB coefficients for the two payload-linear terms.
            let mb = resp_mib as f64;
            println!(
                "           -> per MiB: bound={:.2} ms, clone={:.2} ms (=每记录两次深拷贝的一项); sse per MiB={:.2} ms, per frame={:.1} us",
                t_bound / 1000.0 / mb,
                t_clone / 1000.0 / mb,
                t_sse / 1000.0 / mb,
                t_sse * 1000.0 / (mb * (1 << 20) as f64 / frame_bytes as f64)
            );
        }
    }
}
