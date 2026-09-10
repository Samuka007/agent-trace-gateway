//! atcd — 最简账号池 Responses 反代原型。
//!
//! 子命令：
//!   atcd serve   启动代理（环境变量配置，见下）
//!   atcd import  导入账号：stdin 或文件参数，每行一个 JSON
//!                {"account_id":"...","refresh_token":"...","label":"...",
//!                 "proxy_url":null,"version_pin":null,"os_desc":null,"arch":null}
//!   atcd list    列出账号与绑定计数
//!
//! 环境变量（serve）：
//!   ATCD_LISTEN      默认 127.0.0.1:8400（只绑回环：鉴权是 newapi 的事）
//!   ATCD_UPSTREAM    默认 https://chatgpt.com/backend-api/codex
//!   ATCD_DB          默认 ./atcd.db
//!   ATCD_MIN_TURN_GAP_MS / ATCD_JITTER_MS / ATCD_CONCURRENCY_PER_ACCOUNT
//!   ATCD_MAX_SESSIONS_PER_ACCOUNT  默认 12
//!   ATCD_VERSION_PIN / ATCD_OS_DESC / ATCD_ARCH  默认人设，导入可逐号覆盖

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_trace_gateway::atcd::persona::Persona;
use agent_trace_gateway::atcd::placement::LruPlacement;
use agent_trace_gateway::atcd::proxy::{ProxyApp, ProxyService};
use agent_trace_gateway::atcd::scheduler::{PacingConfig, PacingGate};
use agent_trace_gateway::atcd::store::{self, AccountRow, Store};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() {
    let sub = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(async move {
        match sub.as_str() {
            "serve" => serve().await,
            "import" => import().await,
            "list" => list(),
            other => {
                eprintln!("unknown subcommand: {other} (expected serve|import|list)");
                2
            }
        }
    });
    std::process::exit(code);
}

fn open_store() -> Store {
    let db = PathBuf::from(env_or("ATCD_DB", "./atcd.db"));
    Store::open(&db).expect("open sqlite db")
}

async fn import() -> i32 {
    let store = open_store();
    let mut buf = String::new();
    use std::io::Read;
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.get(2) {
        buf = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("read {path}: {e}");
            String::new()
        });
    } else {
        let _ = std::io::stdin().read_to_string(&mut buf);
    }

    let mut imported = 0usize;
    for (idx, line) in buf.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("line {}: bad json: {e}", idx + 1);
                return 1;
            }
        };
        let (Some(account_id), Some(refresh_token)) = (
            v["account_id"].as_str().map(str::to_string),
            v["refresh_token"].as_str().map(str::to_string),
        ) else {
            eprintln!("line {}: need account_id and refresh_token", idx + 1);
            return 1;
        };
        let persona = Persona::mint(
            &account_id,
            v["version_pin"].as_str().map(str::to_string),
            v["os_desc"].as_str().map(str::to_string),
            v["arch"].as_str().map(str::to_string),
            v["proxy_url"].as_str().map(str::to_string),
        );
        let row = AccountRow {
            account_id: account_id.clone(),
            label: v["label"].as_str().unwrap_or(&account_id).to_string(),
            refresh_token,
            access_token: v["access_token"].as_str().map(str::to_string),
            expires_at: v["expires_at"].as_i64(),
            installation_id: persona.installation_id.clone(),
            version_pin: persona.version_pin.clone(),
            os_desc: persona.os_desc.clone(),
            arch: persona.arch.clone(),
            originator: persona.originator.clone(),
            proxy_url: persona.proxy_url.clone(),
            state: store::ACCOUNT_ACTIVE.into(),
            last_turn_at: 0,
        };
        if let Err(e) = store.upsert_account(&row, now_unix()) {
            eprintln!("line {}: upsert failed: {e}", idx + 1);
            return 1;
        }
        println!(
            "imported {account_id} installation={} version={}",
            persona.installation_id, persona.version_pin
        );
        imported += 1;
    }
    println!("{imported} account(s) imported");
    0
}

fn list() -> i32 {
    let store = open_store();
    match store.list_accounts() {
        Ok(accounts) => {
            println!("{:<38} {:8} {:8} {:36}", "account", "state", "bindings", "installation");
            for a in accounts {
                println!(
                    "{:<38} {:8} {:8} {:36}",
                    a.account_id, a.state, "-", a.installation_id
                );
            }
            0
        }
        Err(e) => {
            eprintln!("list failed: {e}");
            1
        }
    }
}

async fn serve() -> i32 {
    let store = Arc::new(open_store());
    let upstream = env_or("ATCD_UPSTREAM", "https://chatgpt.com/backend-api/codex");
    let listen = env_or("ATCD_LISTEN", "127.0.0.1:8400");
    let pacing = PacingConfig {
        min_turn_gap: Duration::from_millis(env_num("ATCD_MIN_TURN_GAP_MS", 1200u64)),
        jitter: Duration::from_millis(env_num("ATCD_JITTER_MS", 900u64)),
        concurrency_per_account: env_num("ATCD_CONCURRENCY_PER_ACCOUNT", 2usize),
    };
    let placement = Arc::new(LruPlacement {
        max_sessions_per_account: env_num("ATCD_MAX_SESSIONS_PER_ACCOUNT", 12i64),
    });

    let app = Arc::new(ProxyApp {
        store: store.clone(),
        gate: Arc::new(PacingGate::new(pacing)),
        placement,
        http: reqwest::Client::builder()
            .build()
            .expect("build http client"),
        upstream,
        refresh_locks: tokio::sync::Mutex::new(HashMap::new()),
        per_proxy: tokio::sync::Mutex::new(HashMap::new()),
    });

    let addr: std::net::SocketAddr = listen
        .parse()
        .unwrap_or_else(|e| panic!("bad ATCD_LISTEN {listen}: {e}"));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    let accounts = store.list_accounts().map(|v| v.len()).unwrap_or(0);
    println!("atcd serving {addr} → {} ({} account(s))", app.upstream, accounts);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("accept: {e}");
                continue;
            }
        };
        let service = ProxyService(app.clone());
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let builder = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new(),
            );
            if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}
