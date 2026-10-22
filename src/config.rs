//! The TOML configuration file (`atg.toml`) and its loading semantics
//! (v0.4.0 contract, tickets #22/#23/#24/#25).
//!
//! The file is the ONLY configuration surface: the file is complete (no
//! implicit defaults, no `serde(default)` anywhere), unknown keys are
//! rejected at every level (`deny_unknown_fields`), and `version = 1` is
//! mandatory — an unknown version is a hard failure. Any config error at
//! startup is fatal (exit 2, one stderr line); every `ATG_*` environment
//! variable is retired and silently ignored.
//!
//! Hot vs structural keys live in the config-store layer
//! ([`crate::config_store`]); this module owns the schema, the parse and
//! the semantic validation (the 1.2 key table).
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};

/// The one supported schema version.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub server: Server,
    pub trace: Trace,
    pub drain: Drain,
    pub capture: Capture,
    pub store: Store,
    pub stitch: Stitch,
    pub api_key: ApiKey,
    pub export: Export,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: String,
    pub upstream: String,
    pub sni: String,
    pub worker_threads: u32,
    pub max_ws_frame_payload: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trace {
    pub tag: String,
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Drain {
    pub on_cancel: bool,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Store {
    pub max_records: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stitch {
    pub capacity: u64,
    pub ttl_ms: u64,
}

/// The API-key fingerprint salt — a secret. Its `Debug`/`Serialize` views
/// render `***` so the value can never leak through logs, records or the
/// `/__atg/config` endpoint (the 1.2 table: 视图/日志不得输出值).
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKey {
    salt: String,
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey").field("salt", &"***").finish()
    }
}

impl Serialize for ApiKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Redacted<'a> {
            salt: &'a str,
        }
        Redacted { salt: "***" }.serialize(serializer)
    }
}

impl ApiKey {
    pub fn salt(&self) -> &str {
        &self.salt
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Export {
    pub otlp_endpoint: String,
    pub max_inflight: u64,
    pub workers: u64,
}

/// One configuration failure mode. `Display` renders the `<detail>` part;
/// the `file: <path>` suffix is appended by the caller that knows it wants
/// the full one-line form (`agent-trace-gateway: config error: <detail>
/// (file: <path>)`).
#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        line: usize,
        col: usize,
        msg: String,
    },
    Version {
        path: PathBuf,
        found: u32,
    },
    Invalid {
        path: PathBuf,
        key: &'static str,
        reason: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { source, .. } => write!(f, "cannot read file: {source}"),
            Self::Parse { line, col, msg, .. } => {
                write!(f, "TOML parse error at line {line} column {col}: {msg}")
            }
            Self::Version { found, .. } => write!(
                f,
                "unsupported config version {found} (this binary supports version {SCHEMA_VERSION})"
            ),
            Self::Invalid { key, reason, .. } => write!(f, "invalid {key}: {reason}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl ConfigError {
    /// The file this error refers to (for the one-line startup form).
    pub fn path(&self) -> &Path {
        match self {
            Self::Io { path, .. } | Self::Parse { path, .. } => path,
            Self::Version { path, .. } | Self::Invalid { path, .. } => path,
        }
    }

    /// Attach the file a semantic error was found in (the parse step knows
    /// the path, the value-domain step does not).
    fn at_path(self, path: &Path) -> Self {
        match self {
            Self::Version { found, .. } => Self::Version {
                path: path.to_path_buf(),
                found,
            },
            Self::Invalid { key, reason, .. } => Self::Invalid {
                path: path.to_path_buf(),
                key,
                reason,
            },
            other => other,
        }
    }
}

/// The file contents plus the facts every audit line and self-attestation
/// needs (sha256 over the raw bytes, size, path).
pub struct Loaded {
    pub config: Config,
    pub sha256: [u8; 32],
    pub path: PathBuf,
    pub len: u64,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("sha256", &hex::encode(self.sha256))
            .field("path", &self.path)
            .field("len", &self.len)
            .finish()
    }
}

/// Load, parse and validate `path`. Every failure carries the path and is
/// fatal at startup (the caller exits 2 with the one-line stderr form).
pub fn load(path: &Path) -> Result<Loaded, ConfigError> {
    let bytes = std::fs::read(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let sha256: [u8; 32] = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        hasher.finalize().into()
    };
    let len = bytes.len() as u64;
    let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::Parse {
        path: path.to_path_buf(),
        line: 1,
        col: 1,
        msg: "file is not valid UTF-8".to_string(),
    })?;
    let config = parse_toml::<Config>(text).map_err(|(line, col, msg)| ConfigError::Parse {
        path: path.to_path_buf(),
        line,
        col,
        msg,
    })?;
    if config.version != SCHEMA_VERSION {
        return Err(ConfigError::Version {
            path: path.to_path_buf(),
            found: config.version,
        });
    }
    validate(&config).map_err(|err| err.at_path(path))?;
    Ok(Loaded {
        config,
        sha256,
        path: path.to_path_buf(),
        len,
    })
}

/// Parse a TOML document into `T`, mapping the error into the one-line
/// `(line, col, msg)` form. `toml::de::Error` carries a byte span; the line
/// and column are derived by counting newlines up to the span start.
fn parse_toml<T: DeserializeOwned>(text: &str) -> Result<T, (usize, usize, String)> {
    toml::from_str(text).map_err(|err: toml::de::Error| {
        let (line, col) = match err.span() {
            Some(span) => {
                let mut line: usize = 1;
                let mut col: usize = 1;
                for (i, b) in text.bytes().enumerate() {
                    if i >= span.start {
                        break;
                    }
                    if b == b'\n' {
                        line = line.saturating_add(1);
                        col = 1;
                    } else {
                        col = col.saturating_add(1);
                    }
                }
                (line, col)
            }
            None => (1, 1),
        };
        (line, col, err.message().to_string())
    })
}

/// Semantic validation (the 1.2 key table). Type errors are parse errors
/// (serde); this pass owns the value-domain rules.
pub fn validate(cfg: &Config) -> Result<(), ConfigError> {
    invalid(cfg.version == SCHEMA_VERSION, "version", || {
        format!("must be {SCHEMA_VERSION} (found {})", cfg.version)
    })?;
    // server.listen: non-empty host:port, port 1..=65535. Accepted shapes
    // mirror what pingora's add_tcp accepts: host[:port] with the host an
    // IPv4/IPv6/hostname; a bracketed IPv6 literal needs `]:port`.
    invalid(
        !cfg.server.listen.trim().is_empty(),
        "server.listen",
        || "must be a non-empty host:port".to_string(),
    )?;
    invalid(valid_listen(&cfg.server.listen), "server.listen", || {
        format!(
            "must be host:port with port in 1..=65535 (found {:?})",
            cfg.server.listen
        )
    })?;
    invalid(
        !cfg.server.upstream.trim().is_empty(),
        "server.upstream",
        || "must be non-empty (http(s)://… or host[:port][/path])".to_string(),
    )?;
    invalid(
        valid_upstream(&cfg.server.upstream),
        "server.upstream",
        || {
            format!(
                "must be http(s)://… or host[:port][/path] (found {:?})",
                cfg.server.upstream
            )
        },
    )?;
    // server.sni: optional (empty = derive from upstream) — always valid.
    invalid(
        cfg.server.worker_threads >= 1,
        "server.worker_threads",
        || format!("must be >= 1 (found {})", cfg.server.worker_threads),
    )?;
    // server.max_ws_frame_payload: u64 >= 0 — any value valid (0 = unlimited).
    invalid(!cfg.trace.tag.trim().is_empty(), "trace.tag", || {
        "must be non-empty after trimming".to_string()
    })?;
    invalid(valid_trace_mode(&cfg.trace.mode), "trace.mode", || {
        if cfg!(feature = "bench-trace-mode") {
            format!(
                "must be \"full\" (or \"off\" in bench builds) (found {:?})",
                cfg.trace.mode
            )
        } else {
            format!("must be \"full\" (found {:?})", cfg.trace.mode)
        }
    })?;
    // drain.on_cancel: bool — always valid.
    invalid(cfg.drain.timeout_secs >= 1, "drain.timeout_secs", || {
        format!("must be >= 1 (found {})", cfg.drain.timeout_secs)
    })?;
    invalid(cfg.capture.max_bytes >= 1, "capture.max_bytes", || {
        format!("must be >= 1 (found {})", cfg.capture.max_bytes)
    })?;
    invalid(cfg.store.max_records >= 1, "store.max_records", || {
        format!("must be >= 1 (found {})", cfg.store.max_records)
    })?;
    invalid(cfg.store.max_bytes >= 1, "store.max_bytes", || {
        format!("must be >= 1 (found {})", cfg.store.max_bytes)
    })?;
    // stitch.capacity / stitch.ttl_ms: >= 1 — 0 is no longer accepted
    // (v0.4.0 contract change vs. the old env knobs, which passed 0 through).
    invalid(cfg.stitch.capacity >= 1, "stitch.capacity", || {
        format!("must be >= 1 (found {})", cfg.stitch.capacity)
    })?;
    invalid(cfg.stitch.ttl_ms >= 1, "stitch.ttl_ms", || {
        format!("must be >= 1 (found {})", cfg.stitch.ttl_ms)
    })?;
    invalid(
        !cfg.api_key.salt().trim().is_empty(),
        "api_key.salt",
        || "must be non-empty".to_string(),
    )?;
    // export.otlp_endpoint: optional (empty = export disabled) — always valid.
    invalid(cfg.export.max_inflight >= 1, "export.max_inflight", || {
        format!("must be >= 1 (found {})", cfg.export.max_inflight)
    })?;
    invalid(cfg.export.workers >= 1, "export.workers", || {
        format!("must be >= 1 (found {})", cfg.export.workers)
    })?;
    Ok(())
}

/// Build the `Invalid` error with the reason computed lazily.
fn invalid(
    ok: bool,
    key: &'static str,
    reason: impl FnOnce() -> String,
) -> Result<(), ConfigError> {
    if ok {
        Ok(())
    } else {
        Err(ConfigError::Invalid {
            path: PathBuf::new(),
            key,
            reason: reason(),
        })
    }
}

/// `host:port` with port 1..=65535. The host may be an IPv4/IPv6/hostname;
/// a bracketed IPv6 literal (`[::1]:8443`) is accepted. This is a shape
/// check, not a DNS resolution — pingora's listener surfaces resolution
/// failures separately at startup.
fn valid_listen(listen: &str) -> bool {
    let (host, port) = if let Some(v6) = listen.strip_prefix('[') {
        match v6.split_once(']') {
            Some((h, tail)) => match tail.strip_prefix(':') {
                Some(p) => (h, p),
                None => return false,
            },
            None => return false,
        }
    } else {
        match listen.rsplit_once(':') {
            Some((h, p)) => (h, p),
            None => return false,
        }
    };
    !host.trim().is_empty() && port.parse::<u16>().is_ok_and(|p| (1..=65535).contains(&p))
}

/// `http(s)://…` or bare `host[:port][/path]` (the existing parse_upstream
/// contract). The host part must be non-empty and the port, when present,
/// must be 1..=65535.
fn valid_upstream(upstream: &str) -> bool {
    let rest = match upstream.split_once("://") {
        Some((scheme, rest)) => {
            if scheme != "http" && scheme != "https" {
                return false;
            }
            rest
        }
        None => upstream,
    };
    let authority = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        match v6.split_once(']') {
            Some((h, tail)) => match tail.strip_prefix(':') {
                Some(p) => (h, Some(p)),
                None => (h, None),
            },
            None => return false,
        }
    } else {
        match authority.rsplit_once(':') {
            // An explicit colon means the port part is MANDATORY — "host:"
            // (empty port) is a shape error, not a default-port case.
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    !host.trim().is_empty()
        && match port {
            None => true,
            Some(p) => p.parse::<u16>().is_ok_and(|v| (1..=65535).contains(&v)),
        }
}

/// `trace.mode` value domain: prod builds only accept `full`; bench builds
/// (the `bench-trace-mode` feature) also accept `off` (capture-off
/// passthrough, the functional-debug isolation knob).
fn valid_trace_mode(mode: &str) -> bool {
    if cfg!(feature = "bench-trace-mode") {
        mode == "full" || mode == "off"
    } else {
        mode == "full"
    }
}

/// Parsed upstream: (scheme, host, port, base_path). "host:port" defaults
/// to http with the port present; scheme prefixes override; a missing port
/// defaults to 80/443 by scheme. An optional base path is preserved
/// (leading '/', trailing slashes trimmed) — the relay's earlier
/// construction only trimmed trailing slashes, and base-path upstreams must
/// keep resolving to the same URLs. Lives here because BOTH the semantic
/// validation of `server.upstream` and the request-path relay read it.
pub fn parse_upstream(upstream: &str) -> (String, String, u16, String) {
    let (scheme, rest) = match upstream.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("http".to_string(), upstream),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_string()),
        None => (rest, String::new()),
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        // Bracketed IPv6: [::1] or [::1]:8443.
        match v6.split_once(']') {
            Some((h, tail)) => (
                h,
                tail.strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok())
                    .unwrap_or(default_port),
            ),
            None => (v6, default_port),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse::<u16>().unwrap_or(default_port))
            }
            _ => (authority, default_port),
        }
    };
    (scheme, host.to_string(), port, path)
}

/// The redacted JSON view of the config (the `/__atg/config` `effective`
/// object): the 18 current effective values with `api_key.salt` as `***`.
pub fn redacted_json(cfg: &Config) -> serde_json::Value {
    // The derived Serialize of every section is field-faithful; ApiKey's
    // manual Serialize carries the redaction.
    serde_json::to_value(cfg).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A complete, valid minimal file — the fixture every config test and
    //! (later) the integration harness start from.
    use super::Config;

    pub const MINIMAL_TOML: &str = r#"
version = 1
[server]
listen = "127.0.0.1:6180"
upstream = "127.0.0.1:17000"
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

    /// Parse MINIMAL_TOML (no file IO — the shape-level fixture).
    pub fn parse_minimal() -> Config {
        toml::from_str(MINIMAL_TOML).expect("minimal fixture parses")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> Result<Config, String> {
        toml::from_str::<Config>(toml_text)
            .map_err(|e| format!("parse: {e}"))
            .and_then(|c| {
                validate(&c)
                    .map(|_| c)
                    .map_err(|e| format!("validate: {e}"))
            })
    }

    fn parse_fail(toml_text: &str) -> String {
        match parse(toml_text) {
            Ok(_) => panic!("expected failure for:\n{toml_text}"),
            Err(msg) => msg,
        }
    }

    /// Missing-key matrix: every key must be REQUIRED — removing it is a
    /// hard failure (the file is complete; no implicit defaults).
    #[test]
    fn missing_key_fails() {
        for key in [
            "version",
            "server.listen",
            "server.upstream",
            "server.sni",
            "server.worker_threads",
            "server.max_ws_frame_payload",
            "trace.tag",
            "trace.mode",
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
            "export.workers",
        ] {
            let text = test_support::MINIMAL_TOML
                .lines()
                .filter(|l| !l.contains(&format!("{} =", key.rsplit('.').next().unwrap())))
                .collect::<Vec<_>>()
                .join("\n");
            let err = parse_fail(&text);
            assert!(
                err.contains("missing field"),
                "removing {key} must fail as a missing field, got: {err}"
            );
        }
    }

    #[test]
    fn unknown_key_fails_at_every_level() {
        // top level
        let err = parse_fail(&format!("{}\nunknown = 1", test_support::MINIMAL_TOML));
        assert!(err.contains("unknown field `unknown`"), "{err}");
        // one nested level: insert inside the EXISTING [server] section
        // (re-opening the table at the end would be a redefinition error,
        // not an unknown-key error).
        let text =
            test_support::MINIMAL_TOML.replacen("[server]\n", "[server]\nextra_key = true\n", 1);
        let err = parse_fail(&text);
        assert!(err.contains("unknown field `extra_key`"), "{err}");
    }

    #[test]
    fn version_two_fails() {
        let text = test_support::MINIMAL_TOML.replacen("version = 1", "version = 2", 1);
        let err = parse_fail(&text);
        assert!(err.contains("must be 1 (found 2)"), "{err}");
    }

    /// The FILE-level version failure rides the dedicated `Version` variant
    /// (the one-line startup form names the found version).
    #[test]
    fn load_rejects_version_two() {
        let dir = std::env::temp_dir().join("atg-config-test-version");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v2.toml");
        let text = test_support::MINIMAL_TOML.replacen("version = 1", "version = 2", 1);
        std::fs::write(&path, text).unwrap();
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Version { found: 2, .. }));
        assert!(
            err.to_string().contains("unsupported config version 2"),
            "{err}"
        );
    }

    /// The full 1.2 out-of-range matrix: every semantic bound is enforced.
    #[test]
    fn out_of_range_values_fail() {
        let cases: Vec<(&str, &str)> = vec![
            ("listen = \"\"", "server.listen"),
            ("listen = \"hostonly\"", "server.listen"),
            ("listen = \"1.2.3.4:0\"", "server.listen"),
            ("listen = \"1.2.3.4:65536\"", "server.listen"),
            ("upstream = \"\"", "server.upstream"),
            ("upstream = \":\"", "server.upstream"),
            ("worker_threads = 0", "server.worker_threads"),
            ("tag = \"   \"", "trace.tag"),
            ("mode = \"off\"", "trace.mode"),
            ("timeout_secs = 0", "drain.timeout_secs"),
            ("max_bytes = 0", "capture.max_bytes"),
            ("max_records = 0", "store.max_records"),
            ("max_bytes = 0", "store.max_bytes"),
            ("capacity = 0", "stitch.capacity"),
            ("ttl_ms = 0", "stitch.ttl_ms"),
            ("salt = \"\"", "api_key.salt"),
            ("max_inflight = 0", "export.max_inflight"),
            ("workers = 0", "export.workers"),
        ];
        for (replacement, key) in cases {
            // Section-aware replacement: `max_bytes` exists in BOTH capture
            // and store — only the named key's section is touched.
            let (section, leaf) = key.split_once('.').unwrap_or(("", key));
            let value = replacement
                .split_once('=')
                .map(|(_, v)| v.trim())
                .unwrap_or("");
            let mut current_section = String::new();
            let text = test_support::MINIMAL_TOML
                .lines()
                .map(|l| {
                    let trimmed = l.trim_start();
                    if trimmed.starts_with('[') {
                        current_section =
                            trimmed.trim_matches(|c| c == '[' || c == ']').to_string();
                        return l.to_string();
                    }
                    if current_section == section && trimmed.starts_with(&format!("{leaf} =")) {
                        format!("{leaf} = {value}")
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if key == "trace.mode" && cfg!(feature = "bench-trace-mode") {
                // Bench builds accept "off"; the prod arm of this matrix is
                // covered by trace_mode_domain_matches_build.
                parse(&text).expect("bench build accepts mode = off");
                continue;
            }
            let err = parse_fail(&text);
            assert!(
                err.contains(key),
                "replacing {key} must fail naming {key}, got: {err}"
            );
        }
    }

    /// Accepted corner values: empty optional keys, boundary minimums.
    #[test]
    fn accepted_values_pass() {
        let cfg = test_support::parse_minimal();
        assert_eq!(cfg.server.sni, "", "empty sni = derive from upstream");
        assert_eq!(cfg.export.otlp_endpoint, "", "empty endpoint = export off");
        assert_eq!(cfg.server.max_ws_frame_payload, 0, "0 = unlimited frames");

        let text = test_support::MINIMAL_TOML
            .lines()
            .map(|l| match l.trim_start() {
                "worker_threads = 1" => "worker_threads = 1".to_string(),
                "timeout_secs = 60" => "timeout_secs = 1".to_string(),
                "max_inflight = 8" => "max_inflight = 1".to_string(),
                "workers = 4" => "workers = 1".to_string(),
                "capacity = 100000" => "capacity = 1".to_string(),
                "ttl_ms = 86400000" => "ttl_ms = 1".to_string(),
                "max_bytes = 16777216" => "max_bytes = 1".to_string(),
                "max_records = 1000" => "max_records = 1".to_string(),
                "max_bytes = 268435456" => "max_bytes = 1".to_string(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        parse(&text).expect("boundary minimums must pass");
    }

    /// trace.mode accepts `off` only in bench builds; this assertion runs on
    /// the actual feature set so both worlds are covered by one matrix.
    #[test]
    fn trace_mode_domain_matches_build() {
        if cfg!(feature = "bench-trace-mode") {
            let mut cfg = test_support::parse_minimal();
            cfg.trace.mode = "off".to_string();
            validate(&cfg).expect("bench build accepts off");
        } else {
            // covered by out_of_range_values_fail ("off" → trace.mode)
        }
    }

    /// The salt must never appear in any serialized view.
    #[test]
    fn salt_is_redacted_in_views() {
        let cfg = test_support::parse_minimal();
        let debug = format!("{cfg:?}");
        assert!(
            !debug.contains("atg-apikey-fp-salt-v1"),
            "Debug leaked salt"
        );
        assert!(debug.contains("***"), "Debug redacts: {debug}");
        let json = redacted_json(&cfg).to_string();
        assert!(!json.contains("atg-apikey-fp-salt-v1"), "JSON leaked salt");
        assert!(json.contains("\"salt\":\"***\""), "JSON redacts: {json}");
        let full_json = serde_json::to_string(&cfg).expect("serialize");
        assert!(
            !full_json.contains("atg-apikey-fp-salt-v1"),
            "raw Serialize leaked salt"
        );
    }

    #[test]
    fn listen_shapes() {
        let mut cfg = test_support::parse_minimal();
        for good in [
            "127.0.0.1:6180",
            "[::1]:6180",
            "0.0.0.0:6180",
            "example.com:6180",
            "[2001:db8::1]:443",
        ] {
            cfg.server.listen = good.to_string();
            validate(&cfg).unwrap_or_else(|e| panic!("{good} must pass: {e}"));
        }
        for bad in [
            "",
            "1.2.3.4",
            "1.2.3.4:0",
            "1.2.3.4:65536",
            "[::1]",
            "[::1]:0",
        ] {
            cfg.server.listen = bad.to_string();
            assert!(validate(&cfg).is_err(), "{bad} must fail");
        }
    }

    #[test]
    fn upstream_shapes() {
        let mut cfg = test_support::parse_minimal();
        for good in [
            "1.2.3.4:8443",
            "https://api.example.com",
            "http://127.0.0.1:17000",
            "http://[::1]:8443",
            "https://gw.example.com/llm",
            "api.example.com",
            "api.example.com:8443",
        ] {
            cfg.server.upstream = good.to_string();
            validate(&cfg).unwrap_or_else(|e| panic!("{good} must pass: {e}"));
        }
        for bad in [
            "",
            ":",
            "http://",
            "ftp://host",
            "http://host:0",
            "http://host:99999",
        ] {
            cfg.server.upstream = bad.to_string();
            assert!(validate(&cfg).is_err(), "{bad} must fail");
        }
    }

    /// The sha256 of `load` is stable: the same file hashes identically on
    /// two loads, and one flipped byte changes the digest.
    #[test]
    fn load_sha256_stable() {
        let dir = std::env::temp_dir().join("atg-config-test-sha");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stable.toml");
        std::fs::write(&path, test_support::MINIMAL_TOML).unwrap();
        let first = load(&path).expect("minimal file loads");
        let second = load(&path).expect("minimal file loads");
        assert_eq!(first.sha256, second.sha256, "same bytes → same digest");
        assert_eq!(first.len, second.len);

        let mut flipped = test_support::MINIMAL_TOML.to_string();
        flipped.replace_range(0..1, " ");
        let path2 = dir.join("flipped.toml");
        std::fs::write(&path2, flipped).unwrap();
        let other = load(&path2).expect("flipped file loads");
        assert_ne!(first.sha256, other.sha256, "one flipped byte → new digest");
    }

    #[test]
    fn load_reports_missing_file_as_io_error() {
        let err = load(Path::new("/nonexistent/atg/nope.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
        assert!(err.to_string().contains("cannot read file"));
    }

    #[test]
    fn load_reports_syntax_error_with_line_and_col() {
        let dir = std::env::temp_dir().join("atg-config-test-syntax");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.toml");
        std::fs::write(&path, "version = 1\n[server\nlisten = \"x\"\n").unwrap();
        let err = load(&path).unwrap_err();
        match err {
            ConfigError::Parse {
                line,
                col,
                msg,
                path: p,
            } => {
                assert!(line >= 2, "error on line {line}");
                assert!(col >= 1);
                assert!(!msg.is_empty());
                assert_eq!(p, path);
            }
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    /// End-to-end `load` on the real minimal fixture.
    #[test]
    fn load_minimal_file() {
        let dir = std::env::temp_dir().join("atg-config-test-load");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("minimal.toml");
        std::fs::write(&path, test_support::MINIMAL_TOML).unwrap();
        let loaded = load(&path).expect("minimal file loads");
        assert_eq!(loaded.config.version, 1);
        assert_eq!(loaded.len, test_support::MINIMAL_TOML.len() as u64);
        assert_eq!(loaded.config.server.listen, "127.0.0.1:6180");
        assert_eq!(loaded.config.export.workers, 4);
    }
}
