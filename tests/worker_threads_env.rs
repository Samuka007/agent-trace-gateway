// Behavior: ATG_WORKER_THREADS is READ and REACHES pingora — the requested
// count is both self-attested by /__atg/health and observable as that many
// OS worker threads. The second check is the point: a build whose env read
// silently fell back to the default (the failure mode that produced a whole
// invalid measurement matrix — an env string literal patched in place, the
// knob parsed to nothing, every "t8" cell really running one thread while
// still printing a complete-looking result) would pass a self-report-only
// test if the report were derived from the same broken value. Counting the
// threads the process actually has cannot be faked that way.
// [Requirement: ATG#1 Specification 5；Scenario: env 被读取且真正生效]
mod common;

use bytes::Bytes;
use common::stack::start_stack;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// The requested count for this test binary. Must differ from
/// `DEFAULT_WORKER_THREADS` (1) — otherwise an ignored env var and a
/// correctly-read one produce the same observation.
const REQUESTED_THREADS: usize = 3;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn get_json(path: &str) -> serde_json::Value {
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("endpoint");
    assert_eq!(resp.status(), 200, "{path} must be served");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("JSON body")
}

/// Worker threads of THIS process carrying pingora's runtime thread name.
/// `Runtime::new_steal` names them after the pingora service ("agent-trace-
/// gateway", `thread_name` in pingora-runtime); Linux `comm` truncates to 15
/// bytes, so the match is on the truncated prefix.
fn pingora_worker_threads() -> usize {
    let tasks = std::fs::read_dir("/proc/self/task").expect("task dir");
    tasks
        .filter_map(|e| e.ok())
        .filter(|e| {
            std::fs::read_to_string(e.path().join("comm"))
                .map(|c| c.trim_start().starts_with("agent-trace-gat"))
                .unwrap_or(false)
        })
        .count()
}

/// The stack starts with ATG_WORKER_THREADS=3 in the process environment —
/// the deployment knob under test, set exactly the way an operator would.
#[tokio::test]
async fn atg_worker_threads_env_is_read_and_reaches_pingora() {
    start_stack(&[("ATG_WORKER_THREADS", REQUESTED_THREADS.to_string())]).await;

    // 1. Self-attestation: the deployment can prove its shape from health.
    let h = get_json("/__atg/health").await;
    assert_eq!(
        h["worker_threads"], REQUESTED_THREADS,
        "health must attest the requested worker count: {h}"
    );

    // 2. Ground truth: the process really runs that many pingora workers.
    //    A silently-defaulted env read (threads=1) or a hard-coded constant
    //    (threads=8, the pre-ATG#1 behaviour) both fail here.
    let observed = pingora_worker_threads();
    assert_eq!(
        observed, REQUESTED_THREADS,
        "process must run {REQUESTED_THREADS} pingora worker threads, found {observed}"
    );
}
