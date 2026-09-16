// Behavior: the OTLP export flush pool (ATG #13) bounds its concurrency,
// flushes batches in parallel, ships everything it accepts (including the tail
// batch at shutdown) and leaves the pool empty — the invariants the
// exported/failed/dropped counters are read against.
// [Requirement: 轨迹导出与故障恢复；Scenario: 导出并发冲刷池上界与守恒]
use agent_trace_gateway::trace::export::{ExportHealth, Exporter};
use atg_model::TurnRecord;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

/// Records per flush, mirrored from the exporter (unchanged by ATG #13).
const BATCH_MAX: u64 = 32;

#[derive(Default)]
struct SinkStats {
    spans: AtomicU64,
    batches: AtomicU64,
    concurrent: AtomicU64,
    peak: AtomicU64,
}

/// Fake OTLP/HTTP sink: answers 200 to every POST, counts the spans it
/// received, tracks the peak number of concurrent POSTs (the pool's real
/// in-flight maximum) and holds each POST open for `latency` so batches
/// genuinely overlap.
async fn sink(listener: TcpListener, stats: Arc<SinkStats>, latency: Duration) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let stats = Arc::clone(&stats);
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let stats = Arc::clone(&stats);
                        async move {
                            let now = stats.concurrent.fetch_add(1, Ordering::Relaxed) + 1;
                            stats.peak.fetch_max(now, Ordering::Relaxed);
                            let body = req.collect().await.unwrap().to_bytes();
                            let payload: serde_json::Value =
                                serde_json::from_slice(&body).expect("OTLP JSON body");
                            let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
                                .as_array()
                                .map_or(0, |s| s.len() as u64);
                            stats.spans.fetch_add(spans, Ordering::Relaxed);
                            stats.batches.fetch_add(1, Ordering::Relaxed);
                            tokio::time::sleep(latency).await;
                            stats.concurrent.fetch_sub(1, Ordering::Relaxed);
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(
                                Bytes::from("{}"),
                            )))
                        }
                    },
                );
                let _ = Builder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
}

fn record(i: u64) -> TurnRecord {
    TurnRecord {
        protocol: "openai.responses".to_string(),
        session_id: format!("sess-{i}"),
        user_input: format!("turn {i}"),
        ..Default::default()
    }
}

/// Wait until every record has been accounted for by the exporter.
async fn wait_exported(health: &ExportHealth, records: u64) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while health.exported.load(Ordering::Relaxed) < records {
        assert!(
            Instant::now() < deadline,
            "export did not finish: {:?}",
            health.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn bound_sink(latency: Duration) -> (u16, Arc<SinkStats>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let stats = Arc::new(SinkStats::default());
    sink(listener, Arc::clone(&stats), latency).await;
    (port, stats)
}

/// A burst (fit inside the 1024-slot queue, so a drop is impossible) against a
/// slow sink must fill the pool exactly to its bound and run batches in
/// parallel — one flusher at a time is the behavior ATG #13 removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_pool_bounds_concurrency_and_parallelizes_batches() {
    const RECORDS: u64 = 300;
    let (port, stats) = bound_sink(Duration::from_millis(60)).await;
    let exporter = Exporter::start(Some(format!("http://127.0.0.1:{port}/api/public/otel")));
    let bound = exporter.max_inflight();
    let health = Arc::clone(&exporter.health);
    assert!(bound >= 1, "export must be enabled in this test");

    for i in 0..RECORDS {
        exporter.submit(Arc::new(record(i)));
    }
    // The gauge must actually move while batches are open (and never past the
    // bound): a gauge that is always 0 would satisfy every final-state assert.
    let deadline = Instant::now() + Duration::from_secs(5);
    while health.inflight() == 0 {
        assert!(Instant::now() < deadline, "the in-flight gauge never moved");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let observed = health.inflight();
    assert!(
        observed <= bound as u64,
        "the gauge ({observed}) must respect the pool bound ({bound})"
    );
    // Closing the channel is the shutdown signal: drain + join the pool.
    drop(exporter);
    wait_exported(&health, RECORDS).await;

    let (exported, failed, dropped, panicked) = health.snapshot();
    assert_eq!(
        (failed, dropped, panicked),
        (0, 0, 0),
        "a burst the queue can hold must lose nothing: {:?}",
        health.snapshot()
    );
    assert_eq!(exported, RECORDS, "conservation: {:?}", health.snapshot());
    assert_eq!(
        stats.spans.load(Ordering::Relaxed),
        RECORDS,
        "every span must reach the sink"
    );
    let peak = stats.peak.load(Ordering::Relaxed);
    assert!(
        peak >= 2,
        "batches must flush in parallel (peak in-flight was {peak})"
    );
    assert_eq!(
        peak, bound as u64,
        "the semaphore is the enforced in-flight bound (peak {peak}, bound {bound})"
    );
    assert_eq!(
        health.inflight(),
        0,
        "the pool must drain to empty once the channel closes"
    );
}

/// A sustained run (about 1000 records/s, the shape the ticket measures) plus
/// the shutdown tail: nothing may be dropped, and the final partial batch must
/// still be shipped after the channel closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_pool_conserves_a_sustained_run_and_ships_the_tail() {
    const RECORDS: u64 = 2000;
    let (port, stats) = bound_sink(Duration::from_millis(2)).await;
    let exporter = Exporter::start(Some(format!("http://127.0.0.1:{port}/api/public/otel")));
    let health = Arc::clone(&exporter.health);

    for i in 0..RECORDS {
        exporter.submit(Arc::new(record(i)));
        if i % 50 == 49 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    drop(exporter);
    wait_exported(&health, RECORDS).await;

    let (exported, failed, dropped, panicked) = health.snapshot();
    assert_eq!(
        (failed, dropped, panicked),
        (0, 0, 0),
        "export at ~1000 records/s must not drop: {:?}",
        health.snapshot()
    );
    assert_eq!(exported + failed + dropped, RECORDS, "conservation");
    assert_eq!(
        stats.spans.load(Ordering::Relaxed),
        RECORDS,
        "the tail batch must be shipped after the channel closes ({} spans of {RECORDS})",
        stats.spans.load(Ordering::Relaxed)
    );
    assert!(
        stats.batches.load(Ordering::Relaxed) >= RECORDS.div_ceil(BATCH_MAX),
        "records must be coalesced into full batches: {} batches",
        stats.batches.load(Ordering::Relaxed)
    );
    assert_eq!(health.inflight(), 0, "no batch may be left in flight");
}
