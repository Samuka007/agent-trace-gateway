//! agent-trace-gateway entrypoint.
//!
//! Configuration is FILE-ONLY (v0.4.0 contract): `--config <path>` supplies
//! a complete TOML file; every `ATG_*` environment variable is retired and
//! silently ignored. Config errors (unreadable file, syntax, unknown keys,
//! semantic violations) print one stderr line and exit 2:
//! `agent-trace-gateway: config error: <detail> (file: <path>)`
//!
//! Forms:
//! - `gateway --config <path>` — run the gateway
//! - `gateway config check <path>` — validate the file offline (pre-flight
//!   for deploys): prints path + sha256 + the effective values (salt
//!   redacted) as JSON, exit 0; any config error exits 2 with the standard
//!   one-line stderr form
//! - `gateway key-fp --config <path> <api-key>` — recompute the trace
//!   fingerprint of an API key with EXACTLY the runtime code path (same
//!   config salt — paste the 16 hex chars into a Langfuse client_key_fp
//!   filter to correlate keys without ever exposing them)
fn main() {
    const USAGE: &str = concat!(
        "usage: agent-trace-gateway --config <path>\n",
        "       agent-trace-gateway config check <path>\n",
        "       agent-trace-gateway key-fp --config <path> <api-key>"
    );
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("config") {
        // config check <path>: parse + validate without serving. The deploy
        // pre-flight — same code path the gateway would take, so a file that
        // passes here cannot fail a restart.
        if args.get(2).map(String::as_str) != Some("check") || args.len() != 4 {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        let path = std::path::PathBuf::from(arg(&args, 3, USAGE));
        let loaded =
            agent_trace_gateway::config::load(&path).unwrap_or_else(|err| config_fatal(&err));
        let view = serde_json::json!({
            "path": loaded.path.display().to_string(),
            "sha256": hex::encode(loaded.sha256),
            "len": loaded.len,
            "effective": agent_trace_gateway::config::redacted_json(&loaded.config),
        });
        match serde_json::to_string_pretty(&view) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("agent-trace-gateway: config check: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("key-fp") {
        // key-fp --config <path> <api-key>
        if args.get(2).map(String::as_str) != Some("--config") || args.len() != 5 {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        let path = std::path::PathBuf::from(arg(&args, 3, USAGE));
        let key = arg(&args, 4, USAGE);
        let loaded =
            agent_trace_gateway::config::load(&path).unwrap_or_else(|err| config_fatal(&err));
        println!(
            "{}",
            agent_trace_gateway::gateway_app::api_key_fp(key, loaded.config.api_key.salt())
        );
        return;
    }
    // --config <path> (serve)
    if args.get(1).map(String::as_str) != Some("--config") || args.len() != 3 {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let path = std::path::PathBuf::from(arg(&args, 2, USAGE));
    let loaded = agent_trace_gateway::config::load(&path).unwrap_or_else(|err| config_fatal(&err));
    agent_trace_gateway::gateway_app::run(agent_trace_gateway::config_store::ConfigStore::new(
        loaded,
    ));
}

/// Checked argv access (the panic-hardening gate forbids indexing in bins):
/// the caller has already validated the shape; a missing slot is a usage
/// error, not a panic.
fn arg<'a>(args: &'a [String], index: usize, usage: &str) -> &'a str {
    match args.get(index) {
        Some(value) => value.as_str(),
        None => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}

/// The one-line fatal form for every config error (v0.4.0 contract, 1.1):
/// `agent-trace-gateway: config error: <detail> (file: <path>)`, exit 2.
fn config_fatal(err: &agent_trace_gateway::config::ConfigError) -> ! {
    let path = err.path();
    let file = if path.as_os_str().is_empty() {
        "<unknown>"
    } else {
        &path.display().to_string()
    };
    eprintln!("agent-trace-gateway: config error: {err} (file: {file})");
    std::process::exit(2);
}
