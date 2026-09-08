use std::time::Duration;

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
    let fixture_listen = format!("127.0.0.1:{}", fixture_port());
    tokio::spawn(async move {
        let _ = agent_trace_gateway::harness::fixture_server::serve(&fixture_listen).await;
    });
    let gw_listen = format!("127.0.0.1:{}", gateway_port());
    let upstream = format!("127.0.0.1:{}", fixture_port());
    let extra: Vec<(String, String)> = extra
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    std::thread::spawn(move || {
        for (k, v) in &extra {
            std::env::set_var(k, v);
        }
        agent_trace_gateway::gateway_app::run(&gw_listen, &upstream);
    });
    wait_port(fixture_port()).await;
    wait_port(gateway_port()).await;
}
