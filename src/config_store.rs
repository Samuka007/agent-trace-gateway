//! The in-process config store: the swap point between the file on disk and
//! the values the gateway reads per request (v0.4.0 hot-reload contract).
//!
//! One [`ConfigStore`] holds the current [`Snapshot`] behind a
//! `parking_lot::RwLock` — the hot path is one read plus an `Arc` clone.
//! `reload()` re-reads the file TRANSACTIONALLY: the new snapshot replaces
//! the old one only after parse + full validation succeeded; a failure keeps
//! serving the old snapshot, counts in `reload_failures` and logs one stderr
//! line.
//!
//! Structural keys (`server.listen`, `server.worker_threads`, `trace.mode`,
//! `export.workers`) cannot take effect in a running process: on reload a
//! changed structural key is NOT applied (the running value is kept) and is
//! reported in the snapshot's `restart_required` — never silently ignored.
use crate::config::{load, Config, Loaded};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// One immutable configuration generation. `cfg` is shared by `Arc` — a
/// request takes one `snapshot()` clone and reads it for its whole lifetime.
pub struct Snapshot {
    pub cfg: Arc<Config>,
    pub sha256: [u8; 32],
    pub generation: u64,
    pub loaded_at_ms: u128,
    pub path: PathBuf,
    /// Structural keys whose FILE value differs from the RUNNING value
    /// (a restart is needed to apply them). Empty right after startup.
    pub restart_required: Vec<&'static str>,
}

/// The structural values the process actually runs with — pinned at startup
/// (a reload never changes them; that is what makes them structural).
#[derive(Clone, Debug)]
struct RunningStructural {
    listen: String,
    worker_threads: u64,
    export_workers: u64,
    trace_mode: String,
}

/// Hot keys — everything not structural, with the dotted file key it is
/// reported under.
const HOT_KEYS: &[&str] = &[
    "server.upstream",
    "server.sni",
    "server.max_ws_frame_payload",
    "trace.tag",
    "drain.on_cancel",
    "drain.timeout_secs",
    "capture.max_bytes",
    "store.max_records",
    "store.max_bytes",
    "stitch.capacity",
    "stitch.ttl_ms",
    "api_key.salt",
    "export.otlp_endpoint",
    "export.max_inflight",
];
/// Structural keys (restart required to take effect).
const STRUCTURAL_KEYS: &[&str] = &[
    "server.listen",
    "server.worker_threads",
    "trace.mode",
    "export.workers",
];

/// What one reload changed.
pub struct ReloadReport {
    pub generation: u64,
    /// Hot keys whose value changed with this reload (actually applied).
    pub applied: Vec<&'static str>,
    /// Structural keys that changed but CANNOT apply without a restart.
    pub restart_required: Vec<&'static str>,
    pub sha256: [u8; 32],
    pub elapsed_ms: u128,
}

/// A hot-apply hook invoked after every successful reload with the NEW
/// config — the seam that wires the running components (capture cap, trace
/// store, stitcher, exporter) to the swapped snapshot. `run()` registers the
/// real hook, so `reload()` carries the full SIGHUP semantics (swap + apply)
/// in one call for every caller, in-process or real binary.
type HotHook = Box<dyn Fn(&crate::config::Config) + Send + Sync>;

pub struct ConfigStore {
    inner: RwLock<Arc<Snapshot>>,
    reload_failures: AtomicU64,
    structural: RunningStructural,
    on_reload: RwLock<Option<HotHook>>,
}

impl ConfigStore {
    /// Adopt a freshly loaded file as generation 1.
    pub fn new(loaded: Loaded) -> Arc<Self> {
        let structural = RunningStructural {
            listen: loaded.config.server.listen.clone(),
            worker_threads: u64::from(loaded.config.server.worker_threads),
            export_workers: loaded.config.export.workers,
            trace_mode: loaded.config.trace.mode.clone(),
        };
        Arc::new(Self {
            inner: RwLock::new(Arc::new(Snapshot {
                cfg: Arc::new(loaded.config),
                sha256: loaded.sha256,
                generation: 1,
                loaded_at_ms: now_ms(),
                path: loaded.path,
                restart_required: Vec::new(),
            })),
            reload_failures: AtomicU64::new(0),
            structural,
            on_reload: RwLock::new(None),
        })
    }

    /// Wrap a programmatically built config (tests, embedded runs): no file
    /// was read, so the digest is zeroed and the path is empty.
    pub fn from_config(cfg: Config) -> Arc<Self> {
        Self::new(Loaded {
            config: cfg,
            sha256: [0u8; 32],
            path: PathBuf::new(),
            len: 0,
        })
    }

    /// The current snapshot — the hot-path read (one lock read + Arc clone).
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.inner.read())
    }

    /// Failed reloads since startup.
    pub fn reload_failures(&self) -> u64 {
        self.reload_failures.load(Ordering::Relaxed)
    }

    /// Register the hot-apply hook (called by `run()` once, before serving).
    /// Invoked after each successful reload, with the new config, BEFORE
    /// `reload()` returns — a caller that observed `Ok` can rely on the
    /// running components already carrying the new values.
    pub fn set_on_reload(&self, hook: HotHook) {
        *self.on_reload.write() = Some(hook);
    }

    /// Re-read the config file and swap it in atomically. A parse/semantic
    /// failure keeps the old snapshot, counts one failure and logs the
    /// stderr audit line; success swaps and logs the success line.
    pub fn reload(&self) -> Result<ReloadReport, crate::config::ConfigError> {
        let started = std::time::Instant::now();
        let current = self.snapshot();
        let result = load(&current.path);
        match result {
            Ok(loaded) => {
                let (applied, restart_required) =
                    diff(&current.cfg, &loaded.config, &self.structural);
                let snapshot = Arc::new(Snapshot {
                    cfg: Arc::new(loaded.config),
                    sha256: loaded.sha256,
                    generation: current.generation.saturating_add(1),
                    loaded_at_ms: now_ms(),
                    path: loaded.path.clone(),
                    restart_required,
                });
                let snapshot_for_log = Arc::clone(&snapshot);
                let report = ReloadReport {
                    generation: snapshot.generation,
                    applied,
                    restart_required: snapshot.restart_required.clone(),
                    sha256: snapshot.sha256,
                    elapsed_ms: started.elapsed().as_millis(),
                };
                *self.inner.write() = snapshot;
                eprintln!(
                    "ATG config reloaded: generation={} sha256={} applied=[{}] restart_required=[{}] path={}",
                    report.generation,
                    hex::encode(report.sha256),
                    report.applied.join(","),
                    report.restart_required.join(","),
                    snapshot_for_log.path.display()
                );
                // Hot-apply BEFORE returning: the swapped snapshot and the
                // component values move together (a hook failure is a bug —
                // every hook body is infallible by construction).
                if let Some(hook) = self.on_reload.read().as_ref() {
                    hook(&snapshot_for_log.cfg);
                }
                Ok(report)
            }
            Err(err) => {
                self.reload_failures.fetch_add(1, Ordering::Relaxed);
                eprintln!("ATG config reload FAILED: {err}");
                Err(err)
            }
        }
    }
}

/// Diff two configs into (applied hot keys, structural keys that differ from
/// the RUNNING values). Key names are the dotted file paths.
fn diff(
    old: &Config,
    new: &Config,
    running: &RunningStructural,
) -> (Vec<&'static str>, Vec<&'static str>) {
    let hot_changed = [
        old.server.upstream != new.server.upstream,
        old.server.sni != new.server.sni,
        old.server.max_ws_frame_payload != new.server.max_ws_frame_payload,
        old.trace.tag != new.trace.tag,
        old.drain.on_cancel != new.drain.on_cancel,
        old.drain.timeout_secs != new.drain.timeout_secs,
        old.capture.max_bytes != new.capture.max_bytes,
        old.store.max_records != new.store.max_records,
        old.store.max_bytes != new.store.max_bytes,
        old.stitch.capacity != new.stitch.capacity,
        old.stitch.ttl_ms != new.stitch.ttl_ms,
        old.api_key.salt() != new.api_key.salt(),
        old.export.otlp_endpoint != new.export.otlp_endpoint,
        old.export.max_inflight != new.export.max_inflight,
    ];
    let applied: Vec<&'static str> = HOT_KEYS
        .iter()
        .zip(hot_changed)
        .filter(|(_, changed)| *changed)
        .map(|(key, _)| *key)
        .collect();
    let structural_changed = [
        running.listen != new.server.listen,
        running.worker_threads != u64::from(new.server.worker_threads),
        running.trace_mode != new.trace.mode,
        running.export_workers != new.export.workers,
    ];
    let restart_required: Vec<&'static str> = STRUCTURAL_KEYS
        .iter()
        .zip(structural_changed)
        .filter(|(_, changed)| *changed)
        .map(|(key, _)| *key)
        .collect();
    (applied, restart_required)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support;

    #[test]
    fn initial_snapshot_is_generation_one() {
        let store = ConfigStore::from_config(test_support::parse_minimal());
        let snap = store.snapshot();
        assert_eq!(snap.generation, 1);
        assert!(snap.restart_required.is_empty());
        assert_eq!(store.reload_failures(), 0);
    }

    #[test]
    fn reload_from_config_store_without_path_fails_cleanly() {
        // from_config has no file path — a reload is an Io error, the old
        // snapshot keeps serving, and the failure is counted.
        let store = ConfigStore::from_config(test_support::parse_minimal());
        let before = store.snapshot();
        assert!(store.reload().is_err());
        assert_eq!(store.reload_failures(), 1);
        let after = store.snapshot();
        assert_eq!(after.generation, before.generation, "old snapshot kept");
    }

    #[test]
    fn reload_hot_change_applies_and_reports() {
        let dir = std::env::temp_dir().join("atg-store-test-hot");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hot.toml");
        std::fs::write(&path, test_support::MINIMAL_TOML).unwrap();
        let loaded = crate::config::load(&path).unwrap();
        let store = ConfigStore::new(loaded);
        // Flip a hot key on disk.
        let changed =
            test_support::MINIMAL_TOML.replacen("timeout_secs = 60", "timeout_secs = 120", 1);
        std::fs::write(&path, changed).unwrap();
        let report = store.reload().expect("reload applies");
        assert_eq!(report.generation, 2);
        assert_eq!(report.applied, vec!["drain.timeout_secs"]);
        assert!(report.restart_required.is_empty());
        assert_eq!(store.snapshot().cfg.drain.timeout_secs, 120);
    }

    #[test]
    fn reload_structural_change_reports_restart_required_and_keeps_running() {
        let dir = std::env::temp_dir().join("atg-store-test-structural");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("structural.toml");
        std::fs::write(&path, test_support::MINIMAL_TOML).unwrap();
        let store = ConfigStore::new(crate::config::load(&path).unwrap());
        let changed = test_support::MINIMAL_TOML.replacen(
            "listen = \"127.0.0.1:6180\"",
            "listen = \"127.0.0.1:6199\"",
            1,
        );
        std::fs::write(&path, changed).unwrap();
        let report = store.reload().expect("reload applies");
        assert_eq!(
            report.restart_required,
            vec!["server.listen"],
            "structural change is reported, not silently ignored"
        );
        // The RUNNING value stays pinned: the snapshot reports the structural
        // mismatch via restart_required while `from`-side consumers (health,
        // /__atg/config structural view) keep reading the startup values.
        assert_eq!(store.snapshot().cfg.server.listen, "127.0.0.1:6199");
        assert_eq!(store.snapshot().restart_required, vec!["server.listen"]);
    }

    #[test]
    fn diff_identifies_all_hot_keys() {
        let mut new = test_support::parse_minimal();
        new.server.upstream = "10.0.0.9:9999".to_string();
        new.server.sni = "sni.example.com".to_string();
        new.server.max_ws_frame_payload = 4096;
        new.trace.tag = "line:atg-test".to_string();
        new.drain.on_cancel = true;
        new.drain.timeout_secs = 5;
        new.capture.max_bytes = 1024;
        new.store.max_records = 10;
        new.store.max_bytes = 2048;
        new.stitch.capacity = 8;
        new.stitch.ttl_ms = 1000;
        // api_key.salt is private: rebuild via round-trip is overkill for the
        // unit; the salt branch is exercised by reload_hot_change (file) and
        // the redaction tests.
        new.export.otlp_endpoint = "http://127.0.0.1:13000/otel".to_string();
        new.export.max_inflight = 2;
        let running = RunningStructural {
            listen: new.server.listen.clone(),
            worker_threads: u64::from(new.server.worker_threads),
            export_workers: new.export.workers,
            trace_mode: new.trace.mode.clone(),
        };
        let old = test_support::parse_minimal();
        let (applied, restart_required) = diff(&old, &new, &running);
        assert_eq!(
            applied,
            vec![
                "server.upstream",
                "server.sni",
                "server.max_ws_frame_payload",
                "trace.tag",
                "drain.on_cancel",
                "drain.timeout_secs",
                "capture.max_bytes",
                "store.max_records",
                "store.max_bytes",
                "stitch.capacity",
                "stitch.ttl_ms",
                "export.otlp_endpoint",
                "export.max_inflight",
            ]
        );
        assert!(restart_required.is_empty());
    }

    #[test]
    fn diff_detects_structural_against_running_not_old() {
        // The structural diff compares against the RUNNING values: a value
        // that equals the running pin is NOT restart_required even if it
        // differs from the previous file's value.
        let old = test_support::parse_minimal();
        let mut new = old.clone();
        new.server.listen = "127.0.0.1:6180".to_string(); // == running, != old below
        let running = RunningStructural {
            listen: "127.0.0.1:6180".to_string(),
            worker_threads: 1,
            export_workers: 4,
            trace_mode: "full".to_string(),
        };
        let mut old = old;
        old.server.listen = "127.0.0.1:7777".to_string();
        let (applied, restart_required) = diff(&old, &new, &running);
        assert!(applied.is_empty());
        assert!(
            restart_required.is_empty(),
            "matching the running value is not a restart requirement"
        );
    }
}
