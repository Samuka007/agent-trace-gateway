// v0.4.0 SIGHUP acceptance against the REAL binary
// (`env!("CARGO_BIN_EXE_gateway")`): the operator workflow — edit the file,
// `kill -HUP <pid>` — must hot-apply the change (generation +1, effective
// value moves), be a no-op for an unchanged file (sha stable), and survive
// a broken file (generation frozen, reload_failures +1, service answering).
// [Requirement: 配置热重载；Scenario: SIGHUP 真进程热重载]
#[path = "common/cfg.rs"]
mod cfg;

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GW_PORT: u16 = 24594;
const UPSTREAM_PORT: u16 = 24595;

/// Minimal JSON upstream — health/config probes never touch it, but the
/// config must name a valid upstream.
fn counting_upstream() {
    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{UPSTREAM_PORT}")).expect("bind");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match s.read(&mut tmp) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            let t = String::from_utf8_lossy(&buf);
                            if let Some(hdr_end) = t.find("\r\n\r\n") {
                                if let Some(len) = t[..hdr_end].lines().find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .map(|v| v.to_string())
                                }) {
                                    let want: usize = len.trim().parse().unwrap_or(0);
                                    if buf.len() >= hdr_end + 4 + want {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                let body = r#"{"output":[]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
}

/// Raw HTTP GET returning (status, body); `None` while the listener is not
/// up yet (the binary is still bootstrapping or between restarts).
fn http_get(_child: &mut std::process::Child, path: &str) -> Option<(u16, String)> {
    let mut s = match std::net::TcpStream::connect(format!("127.0.0.1:{GW_PORT}")) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let req = format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 65536];
    loop {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// One health read, retrying until the listener answers 200 JSON.
fn health_json(child: &mut std::process::Child) -> serde_json::Value {
    for _ in 0..100 {
        if let Some((200, body)) = http_get(child, "/__atg/health") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                return v;
            }
        }
        assert_child_alive(child);
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("health never answered 200");
}

/// The real binary must stay up for the whole scenario — an early exit is a
/// config or bind failure this test must surface, not paper over.
fn assert_child_alive(child: &mut std::process::Child) {
    if let Ok(Some(status)) = child.try_wait() {
        panic!("gateway binary exited early: {status}");
    }
}

/// Poll until `f` holds; returns the matching health snapshot.
fn wait_for(
    child: &mut std::process::Child,
    f: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let h = health_json(child);
        if f(&h) {
            return h;
        }
        assert!(
            Instant::now() < deadline,
            "condition never became true; last health: {h}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn hup(pid: u32) {
    let status = Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .expect("kill must run");
    assert!(status.success(), "kill -HUP failed");
}

/// Kill the child even when an assertion fails mid-test.
struct KillOnDrop(u32);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-9", &self.0.to_string()])
            .status();
    }
}

#[test]
fn sighup_hot_reloads_the_real_binary() {
    counting_upstream();

    // Full temp config: listen on a free-for-this-suite port, upstream at
    // the mock, a distinctive hot value to start from.
    let text = cfg::build(
        &format!("127.0.0.1:{GW_PORT}"),
        &format!("127.0.0.1:{UPSTREAM_PORT}"),
        &[("capture.max_bytes", "1024".to_string())],
    );
    let path = cfg::write_file("sighup", &text);

    let mut child = Command::new(env!("CARGO_BIN_EXE_gateway"))
        .args(["--config"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gateway binary");
    let pid = child.id();
    let _guard = KillOnDrop(pid);

    // Ready: generation 1, sha256 of the initial file.
    let h = wait_for(&mut child, |h| h["config_generation"] == 1);
    let sha0 = h["config_sha256"].as_str().unwrap().to_string();
    assert_eq!(h["config_reload_failures"], 0, "{h}");

    // 1. HUP with an UNCHANGED file: generation +1, sha stable.
    hup(pid);
    let h = wait_for(&mut child, |h| h["config_generation"] == 2);
    assert_eq!(
        h["config_sha256"].as_str(),
        Some(sha0.as_str()),
        "same file ⇒ same digest"
    );
    assert_eq!(h["config_reload_failures"], 0, "{h}");

    // 2. Hot-key edit + HUP: generation +1 AND the effective value moves.
    let new_text = cfg::apply(
        &std::fs::read_to_string(&path).unwrap(),
        "capture.max_bytes",
        "2048",
    );
    cfg::overwrite(&path, &new_text);
    hup(pid);
    let h = wait_for(&mut child, |h| h["config_generation"] == 3);
    assert_ne!(
        h["config_sha256"].as_str(),
        Some(sha0.as_str()),
        "file changed"
    );
    let (_, cfg_body) = http_get(&mut child, "/__atg/config").expect("config view");
    let view: serde_json::Value = serde_json::from_str(&cfg_body).unwrap();
    assert_eq!(
        view["effective"]["capture"]["max_bytes"], 2048,
        "the hot edit must be live: {view}"
    );

    // 3. Broken file + HUP: generation FROZEN, failure counted, service up.
    cfg::overwrite(&path, "version = 99\n[[[ not toml\n");
    hup(pid);
    // Give the reload a moment to fail, then require the frozen state.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let h = health_json(&mut child);
        let failures = h["config_reload_failures"].as_u64().unwrap_or(0);
        if failures >= 1 {
            assert_eq!(
                h["config_generation"], 3,
                "a failed reload must not advance the generation: {h}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reload failure never got counted; last health: {h}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let (status, _) = http_get(&mut child, "/__atg/config").expect("config view");
    assert_eq!(
        status, 200,
        "service must keep answering after a bad reload"
    );
    let _ = child.kill();
}
