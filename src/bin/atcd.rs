//! atcd — 最简账号池 Responses 反代原型。
//!
//! 子命令：
//!   atcd serve                     启动代理（环境变量配置，见下）
//!   atcd import [file]             导入账号：stdin 或文件参数，每行一个 JSON
//!                                  {"account_id","refresh_token","access_token",
//!                                   "expires_at","label","proxy_url","version_pin"}
//!   atcd accounts                  富列表（状态/配额/凭据寿命/绑定数）
//!   atcd enable <id> / disable <id>  账号状态转换
//!   atcd login [--device]          OAuth 登录并入库
//!                                  默认：粘贴回调模式（浏览器可在任意设备，
//!                                  回调页打不开属预期，粘贴地址栏 URL）
//!                                  --device：codex 原生 device-code 流程
//!
//! 环境变量（serve）：
//!   ATCD_LISTEN      默认 127.0.0.1:8400（只绑回环：鉴权是 newapi 的事）
//!   ATCD_UPSTREAM    默认 https://chatgpt.com/backend-api/codex
//!   ATCD_DB          默认 ./atcd.db
//!   ATCD_MIN_TURN_GAP_MS / ATCD_JITTER_MS / ATCD_CONCURRENCY_PER_ACCOUNT
//!   ATCD_MAX_SESSIONS_PER_ACCOUNT   默认 12
//!   ATCD_QUOTA_CEILING_PERCENT      默认 85（5h 窗口用量天花板，临期号不接新会话）
//!   ATCD_VERSION_PIN / ATCD_OS_TYPE / ATCD_OS_VERSION / ATCD_ARCH / ATCD_TERMINAL
//!                                   人设默认值（未设则取本机真实值），导入可逐号覆盖

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_trace_gateway::atcd::oauth;
use agent_trace_gateway::atcd::persona::Persona;
use agent_trace_gateway::atcd::placement::LruPlacement;
use agent_trace_gateway::atcd::proxy::{ProxyApp, ProxyService};
use agent_trace_gateway::atcd::refresh::{self, CODEX_CLIENT_ID};
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

fn open_store() -> Store {
    let db = PathBuf::from(env_or("ATCD_DB", "./atcd.db"));
    Store::open(&db).expect("open sqlite db")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).cloned().unwrap_or_else(|| "serve".into());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(async move {
        match sub.as_str() {
            "serve" => serve().await,
            "import" => import(&args).await,
            "accounts" | "list" => accounts(),
            "enable" => set_state(args.get(2), store::ACCOUNT_ACTIVE),
            "disable" => set_state(args.get(2), store::ACCOUNT_DISABLED),
            "login" => login(&args).await,
            other => {
                eprintln!(
                    "unknown subcommand: {other} (serve|import|accounts|enable|disable|login)"
                );
                2
            }
        }
    });
    std::process::exit(code);
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn opt_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

// ── import ──────────────────────────────────────────────────────────

async fn import(args: &[String]) -> i32 {
    let store = open_store();
    let mut buf = String::new();
    use std::io::Read;
    match args.get(2) {
        Some(path) => {
            buf = std::fs::read_to_string(path).unwrap_or_else(|e| {
                eprintln!("read {path}: {e}");
                String::new()
            });
        }
        None => {
            let _ = std::io::stdin().read_to_string(&mut buf);
        }
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
            v["os_type"].as_str().map(str::to_string),
            v["os_version"].as_str().map(str::to_string),
            v["arch"].as_str().map(str::to_string),
            v["terminal"].as_str().map(str::to_string),
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
            user_agent: persona.user_agent(),
            originator: persona.originator.clone(),
            proxy_url: persona.proxy_url.clone(),
            state: store::ACCOUNT_ACTIVE.into(),
            last_turn_at: 0,
            primary_used_percent: None,
            secondary_used_percent: None,
            quota_updated_at: None,
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

// ── accounts / enable / disable ─────────────────────────────────────

fn accounts() -> i32 {
    let store = open_store();
    match store.list_accounts() {
        Ok(rows) => {
            println!(
                "{:<38} {:8} {:6} {:>5} {:>5} {:10} {:18}",
                "account", "state", "binds", "5h%", "7d%", "token_exp", "installation"
            );
            for a in rows {
                let binds = store.bindings_count(&a.account_id).unwrap_or(0);
                let exp = a
                    .expires_at
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<38} {:8} {:6} {:>5} {:>5} {:10} {:18}",
                    a.account_id,
                    a.state,
                    binds,
                    a.primary_used_percent
                        .map(|v| format!("{v:.0}"))
                        .unwrap_or_else(|| "-".into()),
                    a.secondary_used_percent
                        .map(|v| format!("{v:.0}"))
                        .unwrap_or_else(|| "-".into()),
                    exp,
                    a.installation_id,
                );
            }
            0
        }
        Err(e) => {
            eprintln!("accounts failed: {e}");
            1
        }
    }
}

fn set_state(id: Option<&String>, state: &str) -> i32 {
    let Some(id) = id else {
        eprintln!("usage: atcd {state} <account_id>");
        return 2;
    };
    let store = open_store();
    match store.set_state(id, state).and_then(|_| store.get_account(id)) {
        Ok(Some(_)) => {
            println!("{id} → {state}");
            0
        }
        Ok(None) => {
            eprintln!("account {id} not found");
            1
        }
        Err(e) => {
            eprintln!("failed: {e}");
            1
        }
    }
}

// ── login ───────────────────────────────────────────────────────────

async fn login(args: &[String]) -> i32 {
    let device = flag(args, "--device");
    let issuer = env_or("ATCD_ISSUER", oauth::ISSUER);
    let label = opt_value(args, "--label");
    let proxy_url = opt_value(args, "--proxy-url");
    let version_pin = opt_value(args, "--version-pin");

    let tokens = if device {
        match login_device().await {
            Ok(t) => t,
            Err(code) => return code,
        }
    } else {
        match login_paste(issuer.as_str()).await {
            Ok(t) => t,
            Err(code) => return code,
        }
    };

    let store = open_store();
    let account_id = oauth::chatgpt_account_id_from_id_token(&tokens.id_token).unwrap_or_else(|| {
        eprintln!("warning: id_token 无 chatgpt_account_id claim，请手工核对账号");
        format!("pending-{}", now_unix())
    });
    let expires_at = refresh::jwt_exp(&tokens.id_token);
    let persona = Persona::mint(&account_id, version_pin, None, None, None, None, proxy_url);
    let row = AccountRow {
        account_id: account_id.clone(),
        label: label.unwrap_or_else(|| account_id.clone()),
        refresh_token: tokens.refresh_token.clone(),
        access_token: Some(tokens.access_token.clone()),
        expires_at,
        installation_id: persona.installation_id.clone(),
        version_pin: persona.version_pin.clone(),
        user_agent: persona.user_agent(),
        originator: persona.originator.clone(),
        proxy_url: persona.proxy_url.clone(),
        state: store::ACCOUNT_ACTIVE.into(),
        last_turn_at: 0,
        primary_used_percent: None,
        secondary_used_percent: None,
        quota_updated_at: None,
    };
    if let Err(e) = store.upsert_account(&row, now_unix()) {
        eprintln!("upsert failed: {e}");
        return 1;
    }
    println!(
        "login ok: {account_id} installation={} expires_at={}",
        persona.installation_id,
        expires_at.map(|t| t.to_string()).unwrap_or_else(|| "?".into())
    );
    0
}

/// device 流程：codex-login 的公开导出完成 usercode 请求、轮询与交换
/// （令牌经 File store 落 staging codex_home 的 auth.json），我们收获进
/// sqlite 后删除 staging。
/// device 流程：codex 原生远程登录路径（browser 与交换端可在不同网络）。
/// wire 镜像 device_code_auth.rs：usercode → 轮询 → 以服务端回调
/// redirect_uri 交换（无 localhost 依赖）。
async fn login_device() -> Result<oauth::ExchangedTokens, i32> {
    let issuer = oauth::ISSUER;
    let device = oauth::request_device_code(issuer, CODEX_CLIENT_ID).await.map_err(|e| {
        eprintln!("device code request failed: {e}");
        1i32
    })?;
    println!(
        "1. 在任意设备浏览器打开: {}\n2. 输入代码: {}\n（等待授权中，最长 15 分钟……）",
        device.verification_url, device.user_code
    );
    let ex = oauth::poll_device_code(issuer, &device).await.map_err(|e| {
        eprintln!("device login failed: {e}");
        1i32
    })?;
    oauth::exchange_code(
        issuer,
        CODEX_CLIENT_ID,
        &oauth::device_redirect_uri(issuer),
        &ex.code_verifier,
        &ex.authorization_code,
    )
    .await
    .map_err(|e| {
        eprintln!("exchange failed: {e}");
        1i32
    })
}

/// 粘贴回调模式：浏览器在任意设备；回调页打不开属预期，粘贴地址栏 URL。
async fn login_paste(issuer: &str) -> Result<oauth::ExchangedTokens, i32> {
    let pkce = oauth::generate_pkce();
    let state = oauth::generate_state();
    let url = oauth::build_authorize_url(
        issuer,
        CODEX_CLIENT_ID,
        oauth::REDIRECT_URI,
        &pkce,
        &state,
        "codex_cli_rs",
    );
    println!("1. 在任意设备的浏览器打开（可用带代理的设备）:\n{url}");
    println!("2. 登录完成后浏览器会跳到 localhost:1455/auth/callback——本机没有服务，页面打不开是预期行为");
    println!("3. 把浏览器地址栏的完整 URL 原样粘贴到这里，回车:");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let code = oauth::parse_callback_url(&line, &state).map_err(|e| {
        eprintln!("{e}");
        1i32
    })?;
    oauth::exchange_code(
        issuer,
        CODEX_CLIENT_ID,
        oauth::REDIRECT_URI,
        &pkce.code_verifier,
        &code,
    )
    .await
    .map_err(|e| {
        eprintln!("exchange failed: {e}");
        1i32
    })
}

// ── serve ───────────────────────────────────────────────────────────

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
        quota_ceiling_percent: env_num("ATCD_QUOTA_CEILING_PERCENT", 85.0f64),
    });

    let app = Arc::new(ProxyApp {
        store: store.clone(),
        gate: Arc::new(PacingGate::new(pacing)),
        placement,
        http: reqwest::Client::builder().build().expect("build http client"),
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
            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}
