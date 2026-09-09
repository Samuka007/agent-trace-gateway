use std::time::Duration;

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
/// process-global env vars; the first caller wins and later ones reuse it.
/// The mutex guards only synchronous startup decisions — never held across
/// an await.
static STACK_START: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

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
/// `extra` adds gateway env vars (e.g. stitcher capacity/TTL, OTLP endpoint).
pub async fn start_stack(extra: &[(&str, String)]) {
    start_stack_inner(extra).await;
}

async fn start_stack_inner(extra: &[(&str, String)]) {
    let need_start = {
        let _guard = STACK_START.lock();
        let up = wait_port_instant(fixture_port()) && wait_port_instant(gateway_port());
        !up
    };
    if !need_start {
        return;
    }
    let extra: Vec<(String, String)> = extra
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let fixture_listen = format!("127.0.0.1:{}", fixture_port());
    stack_runtime().spawn(async move {
        if let Err(e) = super::fixture_server::serve(&fixture_listen).await {
            eprintln!("FIXTURE: server exited: {e}");
        }
    });
    let gw_listen = format!("127.0.0.1:{}", gateway_port());
    let upstream = format!("127.0.0.1:{}", fixture_port());
    let gw_listen2 = gw_listen.clone();
    stack_runtime().spawn_blocking(move || {
        for (k, v) in &extra {
            std::env::set_var(k, v);
        }
        agent_trace_gateway::gateway_app::run(&gw_listen2, &upstream);
    });
    wait_port(fixture_port()).await;
    wait_port(gateway_port()).await;
}

/// Non-blocking port check for "is the shared stack already up".
fn wait_port_instant(port: u16) -> bool {
    std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok()
}
