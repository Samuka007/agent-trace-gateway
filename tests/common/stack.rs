//! Process-wide shared test stack, config-file edition (v0.4.0): ONE
//! in-process gateway per test binary, started from a complete TOML file
//! (base template + dotted-key overrides written to a unique temp path and
//! loaded through the real `config::load`). The `Arc<ConfigStore>` and the
//! file path are kept in statics so reload tests can rewrite the SAME file
//! in place and call `store.reload()`.
//!
//! Tests that need their own instance (reload tests, drain_switch.rs, …)
//! spawn their own `run(store)` on unique ports via `common::cfg` instead.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use agent_trace_gateway::config_store::ConfigStore;

/// Process-wide tokio runtime hosting the fixture + gateway servers. Tests
/// run on their own per-test runtimes; a server spawned there dies when that
/// test's runtime drops, killing the shared stack out from under concurrently
/// running siblings (502/EOF flakes). Hosting on one process-lifetime runtime
/// keeps the shared stack alive for the whole test binary.
static STACK_RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> =
    std::sync::LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("stack runtime")
    });

fn stack_runtime() -> &'static tokio::runtime::Runtime {
    &STACK_RUNTIME
}

/// Serialize stack startup: concurrent callers race on the fixed ports and on
/// the shared config file path; the first caller wins and later ones reuse
/// the running stack. The mutex guards only synchronous startup decisions —
/// never held across an await.
static STACK_START: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The shared stack's store + file path, recorded at startup for reload
/// tests (`shared_store()` / `shared_config_path()`).
static SHARED_STORE: parking_lot::Mutex<Option<std::sync::Arc<ConfigStore>>> =
    parking_lot::Mutex::new(None);
static SHARED_PATH: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// Ports are derived from the process id so parallel test binaries never
/// collide on the same listener.
pub fn fixture_port() -> u16 {
    17000 + ((std::process::id() % 5000) as u16)
}

pub fn gateway_port() -> u16 {
    fixture_port() - 1
}

async fn wait_port(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("port {port} never came up");
}

/// Start the fixture upstream as a tokio task and the pingora gateway in a
/// background thread (both in-process: endpoint security kills freshly
/// compiled listener child processes, so tests embed the servers).
/// `overrides` are dotted config keys (`capture.max_bytes`, `stitch.ttl_ms`,
/// …) applied to the full-file template — the file is the only config
/// surface; there are no environment variables.
pub async fn start_stack(overrides: &[(&str, String)]) {
    let need_start = {
        let _guard = STACK_START.lock();
        let up = wait_port_instant(fixture_port()) && wait_port_instant(gateway_port());
        if !up {
            // Build the complete file, load it through the real chain and
            // record store + path for reload tests. First caller's overrides
            // win — identical semantics to the old env-var race.
            let listen = format!("127.0.0.1:{}", gateway_port());
            let upstream = format!("127.0.0.1:{}", fixture_port());
            let text = super::cfg::build(&listen, &upstream, overrides);
            let path = super::cfg::write_file("shared", &text);
            let loaded = agent_trace_gateway::config::load(&path)
                .expect("shared stack fixture config must load+validate");
            let store = ConfigStore::new(loaded);
            *SHARED_PATH.lock() = Some(path);
            *SHARED_STORE.lock() = Some(store.clone());

            stack_runtime().spawn(async move {
                let fixture_listen = format!("127.0.0.1:{}", fixture_port());
                if let Err(e) = super::fixture_server::serve(&fixture_listen).await {
                    eprintln!("FIXTURE: server exited: {e}");
                }
            });
            stack_runtime().spawn_blocking(move || {
                agent_trace_gateway::gateway_app::run(store);
            });
        }
        !up
    };
    let _ = need_start;
    wait_port(fixture_port()).await;
    wait_port(gateway_port()).await;
}

/// The shared stack's `Arc<ConfigStore>` — for tests that reload it. Panics
/// if the stack was never started in this process.
pub fn shared_store() -> std::sync::Arc<ConfigStore> {
    SHARED_STORE
        .lock()
        .clone()
        .expect("shared stack not started: call start_stack first")
}

/// The temp file the shared stack was loaded from. Rewrite it IN PLACE
/// (`common::cfg::overwrite`) before `shared_store().reload()`.
pub fn shared_config_path() -> PathBuf {
    SHARED_PATH
        .lock()
        .clone()
        .expect("shared stack not started: call start_stack first")
}

/// Non-blocking port check for "is the shared stack already up".
fn wait_port_instant(port: u16) -> bool {
    std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
}
