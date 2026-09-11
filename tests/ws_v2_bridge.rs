// Behavior: atcd WebSocket V2 透传桥（D7）——v2 帧双向字节级保真、身份头
// persona 替换、SOCKS5 出口绑定、Close 透传。
// [Requirement: R4 WebSocket V2; B1 wire 透传; B4 installation 三投影]
use std::sync::Arc;

use agent_trace_gateway::atcd::persona::Persona;
use agent_trace_gateway::atcd::placement::LruPlacement;
use agent_trace_gateway::atcd::proxy::{ProxyApp, ProxyService};
use agent_trace_gateway::atcd::scheduler::{PacingConfig, PacingGate};
use agent_trace_gateway::atcd::store::{AccountRow, Store, ACCOUNT_ACTIVE};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderMap, Request, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async_with_config, MaybeTlsStream, WebSocketStream};

// ── 脚本帧（含 unicode / 转义 / 未知 type——桥必须原样过桥） ───────────
const CLIENT_SESSION_UPDATE: &str = r#"{"type":"session.update","session":{"type":"realtime","instructions":"You are 面试官 \"strict\" — stay terse.\n声调必须标准","audio":{"input":{"format":{"type":"audio/pcm","rate":24000}}}}}"#;
const CLIENT_RESPONSE_CREATE: &str = r#"{"type":"response.create"}"#;
const SERVER_SESSION_UPDATED: &str =
    r#"{"type":"session.updated","session":{"id":"sess_mock","instructions":"backend"}}"#;
const SERVER_RESPONSE_CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1"}}"#;
const SERVER_AUDIO_DELTA: &str =
    r#"{"type":"response.output_audio.delta","delta":"AQID//48","output_index":0}"#;
const SERVER_UNKNOWN_TYPE: &str =
    r#"{"type":"bridge.fidelity.probe","note":"未知类型也必须透传 ✓","esc":"a\nb"}"#;

const CLIENT_BINARY: &[u8] = &[0x00, 0xFF, 0xFE, 0x7F, 0x80, b'a', b'b'];
const CLOSE_CODE: u16 = 4000;
const CLOSE_REASON: &str = "mock-done";

const SESSION_KEY: &str = "sess-ws-v2-018f";
const THREAD_ID: &str = "thread-ws-v2-018f";
const REALTIME_SESSION: &str = "realtime-sess-1";
const DOWNSTREAM_INSTALLATION: &str = "downstream-installation-uuid";
const TURN_METADATA: &str = r#"{"installation_id":"downstream-installation-uuid","agent_name":"/root","turn_id":"turn-018f"}"#;

#[derive(Clone, Default)]
struct MockLog {
    uri: Option<String>,
    handshake_headers: Option<HeaderMap>,
    received_text: Vec<String>,
    received_binary: Vec<Vec<u8>>,
    received_close: Option<(u16, String)>,
}

/// mock 上游：记录握手与全部收帧，主动下发脚本帧，最后 Close(4000)。
#[allow(clippy::result_large_err)] // accept_hdr 回调签名固定，Err 是库类型
async fn run_mock_upstream(listener: TcpListener, log: Arc<Mutex<MockLog>>) {
    let (stream, _) = listener.accept().await.expect("mock accept");
    let log2 = log.clone();
    let ws = accept_hdr_async_with_config(
        stream,
        move |req: &Request<()>, resp| {
            let mut l = log2.lock();
            l.uri = Some(req.uri().to_string());
            l.handshake_headers = Some(req.headers().clone());
            Ok(resp)
        },
        Some(WebSocketConfig::default()),
    )
    .await
    .expect("mock handshake");
    run_mock_session(ws, log).await;
}

async fn run_mock_session(mut ws: WebSocketStream<TcpStream>, log: Arc<Mutex<MockLog>>) {
    // 主动下发脚本帧（桥必须逐字节送到客户端）。
    for frame in [
        SERVER_SESSION_UPDATED,
        SERVER_RESPONSE_CREATED,
        SERVER_AUDIO_DELTA,
        SERVER_UNKNOWN_TYPE,
    ] {
        ws.send(Message::Text(frame.into()))
            .await
            .expect("mock send script");
    }
    // 记录客户端方向的所有帧；客户端把它的三帧发完后即以脚本 Close 收尾。
    let want_binary = CLIENT_BINARY.to_vec();
    let mut got_binary = false;
    loop {
        let Some(msg) = ws.next().await else { break };
        let Ok(msg) = msg else { break };
        match msg {
            Message::Text(t) => log.lock().received_text.push(t.to_string()),
            Message::Binary(b) => {
                got_binary = b == want_binary;
                log.lock().received_binary.push(b.to_vec());
            }
            Message::Close(frame) => {
                log.lock().received_close = frame.map(|f| (f.code.into(), f.reason.to_string()));
                break;
            }
            _ => {}
        }
        if got_binary && log.lock().received_text.len() >= 2 {
            break;
        }
    }
    // 以自定义 code/reason 关闭：验证 Close 透传。
    let _ = ws
        .close(Some(CloseFrame {
            code: CloseCode::from(CLOSE_CODE),
            reason: CLOSE_REASON.into(),
        }))
        .await;
    // 排干 close 握手直至对端断开。
    while let Some(Ok(msg)) = ws.next().await {
        if matches!(
            msg,
            Message::Close(_) | Message::Text(_) | Message::Binary(_)
        ) {
            break;
        }
    }
}

struct Stack {
    atcd_port: u16,
    mock_port: u16,
    persona: Persona,
    mock_log: Arc<Mutex<MockLog>>,
}

async fn start_ws_stack(persona_proxy: Option<String>) -> Stack {
    let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_port = mock_listener.local_addr().unwrap().port();
    let mock_log = Arc::new(Mutex::new(MockLog::default()));
    tokio::spawn(run_mock_upstream(mock_listener, mock_log.clone()));

    let store = Arc::new(Store::open_in_memory().unwrap());
    let persona = Persona::mint(
        "acct-ws-v2",
        None,
        None,
        None,
        None,
        None,
        persona_proxy.clone(),
    );
    store
        .upsert_account(
            &AccountRow {
                account_id: persona.account_id.clone(),
                label: "ws-test".into(),
                refresh_token: "rt-test".into(),
                access_token: Some("persona-token-jwt".into()),
                expires_at: None, // 不过期：不触发刷新
                installation_id: persona.installation_id.clone(),
                version_pin: persona.version_pin.clone(),
                user_agent: persona.user_agent(),
                originator: persona.originator.clone(),
                proxy_url: persona_proxy,
                state: ACCOUNT_ACTIVE.into(),
                last_turn_at: 0,
                primary_used_percent: None,
                secondary_used_percent: None,
                quota_updated_at: None,
            },
            1,
        )
        .unwrap();

    let app = Arc::new(ProxyApp {
        store: store.clone(),
        gate: Arc::new(PacingGate::new(PacingConfig {
            min_turn_gap: Duration::ZERO,
            jitter: Duration::ZERO,
            concurrency_per_account: 4,
        })),
        placement: Arc::new(LruPlacement {
            max_sessions_per_account: 12,
            quota_ceiling_percent: 85.0,
        }),
        http: reqwest::Client::builder().build().unwrap(),
        upstream: format!("http://127.0.0.1:{mock_port}"),
        refresh_locks: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        per_proxy: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let atcd_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let service = ProxyService(app.clone());
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                let _ = builder.serve_connection_with_upgrades(io, service).await;
            });
        }
    });
    Stack {
        atcd_port,
        mock_port,
        persona,
        mock_log,
    }
}

async fn wait_port(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..100 {
        if TcpStream::connect(&addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("port {port} never came up");
}

/// codex 形态的下游客户端：握手头带完整身份工件 + 升级 query。
async fn connect_client(atcd_port: u16) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let url = format!("ws://127.0.0.1:{atcd_port}/v1/realtime?model=gpt-realtime&call_id=call-1");
    let mut req = url.into_client_request().unwrap();
    let headers = req.headers_mut();
    headers.insert("session-id", SESSION_KEY.parse().unwrap());
    headers.insert("thread-id", THREAD_ID.parse().unwrap());
    headers.insert("x-session-id", REALTIME_SESSION.parse().unwrap());
    headers.insert(
        "x-codex-installation-id",
        DOWNSTREAM_INSTALLATION.parse().unwrap(),
    );
    headers.insert("x-codex-turn-metadata", TURN_METADATA.parse().unwrap());
    headers.insert("authorization", "Bearer downstream-junk".parse().unwrap());
    headers.insert("originator", "codex_cli_rs".parse().unwrap());
    let (ws, resp) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(req),
    )
    .await
    .expect("client connect timeout")
    .expect("client handshake");
    assert_eq!(
        resp.status(),
        StatusCode::SWITCHING_PROTOCOLS,
        "atcd 必须 101"
    );
    ws
}

/// 客户端全脚本：两帧 Text + 一帧 Binary，然后收对向脚本直至 Close。
async fn run_client(client: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) {
    client
        .send(Message::Text(CLIENT_SESSION_UPDATE.into()))
        .await
        .unwrap();
    client
        .send(Message::Text(CLIENT_RESPONSE_CREATE.into()))
        .await
        .unwrap();
    client
        .send(Message::Binary(CLIENT_BINARY.to_vec().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn ws_v2_bridge_byte_fidelity_and_identity() {
    let stack = start_ws_stack(None).await;
    wait_port(stack.atcd_port).await;

    let mut client = connect_client(stack.atcd_port).await;
    run_client(&mut client).await;

    // 收上游方向全部脚本帧 + Close(4000, mock-done)。
    let mut got_text: Vec<String> = Vec::new();
    let mut close: Option<(u16, String)> = None;
    while let Some(msg) = client.next().await {
        match msg.expect("client frame ok") {
            Message::Text(t) => got_text.push(t.to_string()),
            Message::Close(frame) => {
                close = frame.map(|f| (f.code.into(), f.reason.to_string()));
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        got_text,
        vec![
            SERVER_SESSION_UPDATED.to_string(),
            SERVER_RESPONSE_CREATED.to_string(),
            SERVER_AUDIO_DELTA.to_string(),
            SERVER_UNKNOWN_TYPE.to_string(),
        ],
        "上游→客户端方向必须逐字节保真（含未知 type 帧）"
    );
    assert_eq!(
        close,
        Some((CLOSE_CODE, CLOSE_REASON.to_string())),
        "Close code/reason 必须透传"
    );

    // mock 侧：收到的客户端帧必须逐字节相等。
    let log = stack.mock_log.lock().clone();
    assert_eq!(
        log.uri.as_deref(),
        Some("/v1/realtime?model=gpt-realtime&call_id=call-1"),
        "上游路径=ATCD_UPSTREAM 归一 + query 原样透传"
    );
    assert_eq!(
        log.received_text,
        vec![
            CLIENT_SESSION_UPDATE.to_string(),
            CLIENT_RESPONSE_CREATE.to_string()
        ],
        "客户端→上游方向必须逐字节保真"
    );
    assert_eq!(log.received_binary, vec![CLIENT_BINARY.to_vec()]);
    assert!(
        log.received_close.is_none(),
        "mock 应是主动关闭方，不应先收到客户端 Close"
    );

    // 身份面断言（B4）：persona 替换 + 工件透传。
    let headers = log.handshake_headers.expect("upstream handshake captured");
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer persona-token-jwt"),
        "上游凭据必须是 persona token"
    );
    assert_eq!(
        headers
            .get("chatgpt-account-id")
            .and_then(|v| v.to_str().ok()),
        Some("acct-ws-v2")
    );
    assert_eq!(
        headers
            .get("x-codex-installation-id")
            .and_then(|v| v.to_str().ok()),
        Some(stack.persona.installation_id.as_str()),
        "installation 头投影 → persona（非下游值）"
    );
    assert_eq!(
        headers.get("originator").and_then(|v| v.to_str().ok()),
        Some(stack.persona.originator.as_str())
    );
    assert_eq!(
        headers.get("user-agent").and_then(|v| v.to_str().ok()),
        Some(stack.persona.user_agent().as_str()),
        "UA → persona（设备不换操作系统）"
    );
    let tm: serde_json::Value = serde_json::from_str(
        headers
            .get("x-codex-turn-metadata")
            .and_then(|v| v.to_str().ok())
            .expect("turn metadata passthrough"),
    )
    .unwrap();
    assert_eq!(
        tm["installation_id"],
        stack.persona.installation_id.as_str(),
        "turn 元数据 installation 投影 → persona"
    );
    assert_eq!(tm["agent_name"], "/root", "turn 元数据其余字段透传");
    assert_eq!(tm["turn_id"], "turn-018f");
    assert_eq!(
        headers.get("session-id").and_then(|v| v.to_str().ok()),
        Some(SESSION_KEY),
        "session-id 工件透传"
    );
    assert_eq!(
        headers.get("thread-id").and_then(|v| v.to_str().ok()),
        Some(THREAD_ID)
    );
    assert_eq!(
        headers.get("x-session-id").and_then(|v| v.to_str().ok()),
        Some(REALTIME_SESSION),
        "x-session-id（realtime 会话工件）透传"
    );
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap();
    assert_eq!(
        host,
        format!("127.0.0.1:{}", stack.mock_port),
        "host 必须指向上游而非 atcd"
    );
}

/// SOCKS5 出口绑定（台账 R4 已知缺口的关闭证明）：persona.proxy_url 指向
/// 进程内 mini SOCKS5 服务，流量必须经它到达 mock 上游。
#[tokio::test]
async fn ws_bridge_dials_upstream_through_socks5_exit() {
    // mini SOCKS5：接受 no-auth，解析 CONNECT，转发到目标。
    let socks_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socks_port = socks_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = socks_listener.accept().await.unwrap();
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).await.unwrap();
        let nmethods = head[1] as usize;
        let mut methods = vec![0u8; nmethods];
        stream.read_exact(&mut methods).await.unwrap();
        assert!(methods.contains(&0x00), "client must offer no-auth");
        stream.write_all(&[0x05, 0x00]).await.unwrap();
        // CONNECT 请求
        let mut fixed = [0u8; 4];
        stream.read_exact(&mut fixed).await.unwrap();
        assert_eq!(fixed[1], 0x01, "CMD must be CONNECT");
        let target: std::net::SocketAddr = match fixed[3] {
            0x01 => {
                let mut ip = [0u8; 4];
                stream.read_exact(&mut ip).await.unwrap();
                let mut port = [0u8; 2];
                stream.read_exact(&mut port).await.unwrap();
                std::net::SocketAddr::new(ip.into(), u16::from_be_bytes(port))
            }
            0x03 => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).await.unwrap();
                let mut host = vec![0u8; len[0] as usize];
                stream.read_exact(&mut host).await.unwrap();
                let mut port = [0u8; 2];
                stream.read_exact(&mut port).await.unwrap();
                format!(
                    "{}:{}",
                    String::from_utf8(host).unwrap(),
                    u16::from_be_bytes(port)
                )
                .parse()
                .unwrap()
            }
            other => panic!("unexpected atyp {other}"),
        };
        stream
            .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut upstream = TcpStream::connect(target).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
    });

    let stack = start_ws_stack(Some(format!("socks5://127.0.0.1:{socks_port}"))).await;
    wait_port(stack.atcd_port).await;

    let mut client = connect_client(stack.atcd_port).await;
    run_client(&mut client).await;

    let mut seen_script = false;
    while let Some(msg) = client.next().await {
        match msg.expect("frame ok") {
            Message::Text(t) if t == SERVER_SESSION_UPDATED => seen_script = true,
            Message::Close(_) => break,
            _ => {}
        }
    }
    assert!(seen_script, "WS 流量必须经 SOCKS5 出口往返");
    let log = stack.mock_log.lock().clone();
    assert_eq!(
        log.received_text,
        vec![
            CLIENT_SESSION_UPDATE.to_string(),
            CLIENT_RESPONSE_CREATE.to_string()
        ],
        "客户端帧经 SOCKS 出口到达上游，字节不变"
    );
    assert_eq!(log.received_binary, vec![CLIENT_BINARY.to_vec()]);
}
