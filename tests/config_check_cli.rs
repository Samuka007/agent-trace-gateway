// `gateway config check <path>` (ticket #29): the offline deploy pre-flight.
// Nails:
//   (1) a valid file → exit 0, JSON carrying path + sha256 + effective values
//       (salt redacted) and NOTHING is bound/served;
//   (2) an invalid file → exit 2 with the standard one-line stderr form
//       `agent-trace-gateway: config error: … (file: <path>)`;
//   (3) a missing path → exit 2 with the usage block.
#[path = "common/cfg.rs"]
mod cfg;

fn gateway_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gateway")
}

#[test]
fn config_check_validates_and_reports_without_serving() {
    let path = cfg::write_file(
        "check-ok",
        &cfg::build("127.0.0.1:24660", "127.0.0.1:24661", &[]),
    );
    let out = std::process::Command::new(gateway_bin())
        .args(["config", "check", path.to_str().unwrap()])
        .output()
        .expect("run config check");
    assert!(
        out.status.success(),
        "config check must succeed: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let view: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("config check prints JSON");
    assert_eq!(
        view["effective"]["api_key"]["salt"], "***",
        "salt must stay redacted: {view}"
    );
    assert_eq!(
        view["path"].as_str().unwrap(),
        path.to_str().unwrap(),
        "the checked file must be named: {view}"
    );
    assert_eq!(
        view["sha256"].as_str().unwrap().len(),
        64,
        "sha256 is a 32-byte hex digest: {view}"
    );
    // Nothing served: the port the file names never opened (the command
    // exits immediately; a listener would have kept it alive).
    assert!(
        std::net::TcpStream::connect("127.0.0.1:24660").is_err(),
        "config check must not bind the configured listen address"
    );
}

#[test]
fn config_check_rejects_with_the_one_row_form() {
    let path = cfg::write_file("check-bad", "version = 2\n");
    let out = std::process::Command::new(gateway_bin())
        .args(["config", "check", path.to_str().unwrap()])
        .output()
        .expect("run config check");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("agent-trace-gateway: config error:"),
        "the one-line form is the contract: {stderr}"
    );
    assert!(
        stderr.contains(path.to_str().unwrap()),
        "the failing file must be named: {stderr}"
    );
}

#[test]
fn config_check_requires_a_path() {
    let out = std::process::Command::new(gateway_bin())
        .args(["config", "check"])
        .output()
        .expect("run config check");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage:"),
        "missing path prints the usage block"
    );
}
