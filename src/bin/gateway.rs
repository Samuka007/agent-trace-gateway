//! agent-trace-gateway entrypoint.
//! Env: ATG_LISTEN (default 127.0.0.1:6180), ATG_UPSTREAM (required),
//! ATG_OTLP_ENDPOINT (optional), ATG_CAPTURE_MAX_BYTES, ATG_STITCH_CAPACITY,
//! ATG_STITCH_TTL_MS, ATG_APIKEY_SALT (API-key fingerprint salt; default
//! "atg-apikey-fp-salt-v1").
fn main() {
    // `atg key-fp <key>`: recompute the trace fingerprint of an API key —
    // EXACTLY the runtime code path (ops verifiable recomputation: run it,
    // paste the 16 hex chars into a Langfuse client_key_fp filter). The
    // salt comes from ATG_APIKEY_SALT, same as the gateway runtime.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("key-fp") {
        let Some(key) = args.get(2) else {
            eprintln!("usage: gateway key-fp <api-key>");
            std::process::exit(2);
        };
        println!("{}", agent_trace_gateway::gateway_app::api_key_fp(key));
        return;
    }

    let listen = std::env::var("ATG_LISTEN").unwrap_or_else(|_| "127.0.0.1:6180".to_string());
    let upstream = match std::env::var("ATG_UPSTREAM") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            eprintln!("agent-trace-gateway: ATG_UPSTREAM is required (host:port)");
            std::process::exit(2);
        }
    };
    agent_trace_gateway::gateway_app::run(&listen, &upstream);
}
