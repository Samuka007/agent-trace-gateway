//! Full-file TOML config fixtures (v0.4.0 contract): the file is the ONLY
//! configuration surface, so tests build a complete `atg.toml` from the base
//! template (v0.3.13 production defaults), apply dotted-key overrides
//! textually, write it to a unique temp file and go through the real
//! `config::load` → `ConfigStore::new` → `gateway_app::run(store)` chain.
//! No `ATG_*` environment variables anywhere (the one positive test that
//! sets them lives in tests/legacy_env_ignored.rs).
//!
//! Included two ways:
//! - `tests/common/mod.rs` (`pub mod cfg;`) for files that pull the shared
//!   stack anyway;
//! - `#[path = "common/cfg.rs"] mod cfg;` for files that spawn their own
//!   instance and must not pull the stack helpers (dead_code under clippy).
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_trace_gateway::config_store::ConfigStore;

/// The base template: the v0.3.13 production defaults (the canary instance's
/// measured baseline; see /tmp/atg-dispatch/atg-toml-draft/atg-canary.toml),
/// with `{LISTEN}` / `{UPSTREAM}` placeholders substituted by [`base`].
/// Exactly the 18 keys of the v0.4.0 schema — no defaults exist outside the
/// file, so this IS the fixture.
const BASE_TEMPLATE: &str = r#"# agent-trace-gateway test fixture config (complete file; full-file contract)
version = 1

[server]
listen = "{LISTEN}"
upstream = "{UPSTREAM}"
sni = ""
worker_threads = 1
max_ws_frame_payload = 0

[trace]
tag = "line:atg"
mode = "full"

[drain]
on_cancel = false
timeout_secs = 60

[capture]
max_bytes = 16777216

[store]
max_records = 1000
max_bytes = 268435456

[stitch]
capacity = 100000
ttl_ms = 86400000

[api_key]
salt = "atg-apikey-fp-salt-v1"

[export]
otlp_endpoint = ""
max_inflight = 8
workers = 4
"#;

/// The default salt the base template ships (the v0.3.13 default; tests that
/// recompute fingerprints use this constant instead of a retired env read).
pub const DEFAULT_SALT: &str = "atg-apikey-fp-salt-v1";

/// Base file with listen/upstream substituted.
pub fn base(listen: &str, upstream: &str) -> String {
    BASE_TEMPLATE
        .replace("{LISTEN}", listen)
        .replace("{UPSTREAM}", upstream)
}

/// A quoted TOML basic-string literal. Rust's `{:?}` for a `&str` produces
/// exactly that for ASCII values (every value these tests substitute is).
pub fn q(s: &str) -> String {
    debug_assert!(s.is_ascii(), "fixture string values must stay ASCII: {s:?}");
    format!("{s:?}")
}

/// Apply one dotted-key override to the TOML text: rewrite the `leaf = …`
/// line inside `[section]`. Panics on an unknown key — a test overriding a
/// nonexistent key is a bug in the test, not a silent no-op.
pub fn apply(text: &str, dotted: &str, value_literal: &str) -> String {
    let (section, leaf) = dotted
        .split_once('.')
        .unwrap_or_else(|| panic!("override key must be dotted: {dotted}"));
    let mut out = String::with_capacity(text.len() + value_literal.len() + 2);
    let mut in_section = false;
    let mut replaced = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_section = &t[1..t.len() - 1] == section;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let key = t.split('=').next().unwrap_or("").trim();
        if in_section && key == leaf {
            out.push_str(&format!("{leaf} = {value_literal}\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    assert!(
        replaced,
        "config override {dotted} matched no line in:\n{text}"
    );
    out
}

/// Base + several dotted overrides.
pub fn build(listen: &str, upstream: &str, overrides: &[(&str, String)]) -> String {
    let mut text = base(listen, upstream);
    for (k, v) in overrides {
        text = apply(&text, k, v);
    }
    text
}

/// Rewrite a config file IN PLACE (same path, same inode — the reload
/// contract: never rename, never swap directories).
pub fn overwrite(path: &PathBuf, text: &str) {
    std::fs::write(path, text).unwrap_or_else(|e| panic!("rewrite {}: {e}", path.display()));
}

static FILE_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Write a complete config file at a unique temp path (per-process pid +
/// monotonic counter — parallel test binaries never collide).
pub fn write_file(tag: &str, text: &str) -> PathBuf {
    let n = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("atg-test-{}-{n}-{tag}.toml", std::process::id()));
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    path
}

/// Write → `config::load` → `ConfigStore::new`, spawn `run(store)` on a
/// dedicated thread (pingora's `run_forever` blocks it for the process
/// lifetime), return the store. The caller waits for the listen port and
/// keeps the returned Arc when it plans to `reload()`: the file lives at
/// `store.snapshot().path`, rewritten IN PLACE via [`overwrite`].
pub fn spawn_gateway(tag: &str, text: &str) -> std::sync::Arc<ConfigStore> {
    let path = write_file(tag, text);
    let loaded =
        agent_trace_gateway::config::load(&path).expect("test fixture config must load+validate");
    let store = ConfigStore::new(loaded);
    let runner = store.clone();
    std::thread::Builder::new()
        .name(format!("atg-test-gw-{tag}"))
        .spawn(move || agent_trace_gateway::gateway_app::run(runner))
        .expect("spawn gateway thread");
    store
}
