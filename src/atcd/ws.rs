//! WebSocket V2 透传桥（D7，docs/atcd-design.md §12）。
//!
//! 桥零协议语义：Text/Binary/Close 帧 payload 原样转发（消息层字节保真），
//! 不解析帧内容；Ping/Pong 是连接级控制帧，由 tungstenite 读路径在本侧
//! 自动应答（RFC 6455 要求 Pong 走同连接），不跨连接转发。
//! 身份面与 HTTP 同一套函数：strip_inbound + apply（B4——installation
//! 头投影 + turn 元数据 JSON 字段投影；帧投影为空集，见设计 §12.1-4）。
//!
//! 上游连接：ATCD_UPSTREAM 按 codex normalize_realtime_path 规则归一 +
//! 入站 query 原样透传；persona.proxy_url 有值时先建 SOCKS5 隧道
//! （socks5 本地解析 / socks5h 代理端解析，RFC 1929 userpass），TLS 由
//! tokio-tungstenite 的默认 rustls（webpki-roots）在隧道上完成。

use std::io;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::upgrade::Upgraded;
use hyper::{HeaderMap, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use sha1_smol::Sha1;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{tungstenite, MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::atcd::persona::Persona;
use crate::atcd::proxy::{json_error, now_unix_ms, HyperResponse, ProxyApp};
use crate::atcd::refresh::RefreshError;
use crate::atcd::rewrite::{self, RewriteInput};

/// RFC 6455 §1.3 官方向量即单测，握手应答键。
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// 上游意外断开时给对侧的 close code（1011 internal error）。
const CLOSE_INTERNAL: CloseCode = CloseCode::Iana(1011);

// ── 握手编解码 ────────────────────────────────────────────────────────

pub fn websocket_accept_key(sec_websocket_key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(sec_websocket_key.as_bytes());
    sha.update(WS_GUID.as_bytes());
    BASE64.encode(sha.digest().bytes())
}

/// 入站是否为 WS 升级请求（GET + upgrade: websocket + sec-websocket-key）。
pub fn is_websocket_upgrade(req: &Request<hyper::body::Incoming>) -> bool {
    req.method() == hyper::Method::GET
        && req
            .headers()
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
        && req.headers().contains_key("sec-websocket-key")
}

// ── 上游 URL：codex normalize_realtime_path 的逐行移植 ────────────────

/// codex methods.rs:1179-1201 非 frameless 分支（v1/v2 共用）。
fn normalize_realtime_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        return "/v1/realtime".to_string();
    }
    if path.ends_with("/realtime") {
        return path.to_string();
    }
    if path.ends_with("/realtime/") {
        return path.trim_end_matches('/').to_string();
    }
    if path.ends_with("/v1") {
        return format!("{path}/realtime");
    }
    if path.ends_with("/v1/") {
        return format!("{path}realtime");
    }
    path.to_string()
}

/// 上游 WS URL = ATCD_UPSTREAM 归一 + 入站 query 原样透传（D7 §12.2-D7.3）。
/// v3（frameless bidi）的 /v1/live 路径显式拒绝。
pub fn upstream_ws_url(
    upstream_base: &str,
    inbound_path: &str,
    inbound_query: Option<&str>,
) -> Result<String, String> {
    let base = Url::parse(upstream_base).map_err(|e| format!("bad upstream base url: {e}"))?;
    if inbound_path == "/v1/live" || inbound_path.starts_with("/v1/live/") {
        return Err("frameless bidi (realtime v3) is not supported".to_string());
    }
    let scheme = match base.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" => "ws",
        "wss" => "wss",
        other => return Err(format!("unsupported upstream scheme: {other}")),
    };
    let normalized = normalize_realtime_path(base.path());
    let host = match base.port() {
        Some(port) => format!("{}:{port}", base.host_str().unwrap_or_default()),
        None => base.host_str().unwrap_or_default().to_string(),
    };
    let mut out = format!("{scheme}://{host}{normalized}");
    if let Some(q) = inbound_query {
        if !q.is_empty() {
            out.push('?');
            out.push_str(q);
        }
    }
    Ok(out)
}

// ── SOCKS5 出口隧道 ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct SocksProxy {
    /// socks5 = 本地解析目标域名；socks5h = 代理端解析。
    remote_dns: bool,
    host: String,
    port: u16,
    auth: Option<(String, String)>,
}

fn parse_socks_url(raw: &str) -> Result<SocksProxy, String> {
    let url = Url::parse(raw).map_err(|e| format!("bad proxy url: {e}"))?;
    let remote_dns = match url.scheme() {
        "socks5" => false,
        "socks5h" => true,
        other => {
            return Err(format!(
                "unsupported proxy scheme (socks5/socks5h only): {other}"
            ))
        }
    };
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "proxy url missing host".to_string())?
        .to_string();
    let auth = match (url.username(), url.password()) {
        ("", None) => None,
        (user, pass) => Some((
            percent_decode(user).ok_or_else(|| "bad proxy userinfo".to_string())?,
            pass.map(|p| percent_decode(p).unwrap_or_else(|| p.to_string()))
                .unwrap_or_default(),
        )),
    };
    Ok(SocksProxy {
        remote_dns,
        host,
        port: url.port().unwrap_or(1080),
        auth,
    })
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// SOCKS5 CONNECT 隧道。返回的流已连通 dest_host:dest_port。
async fn socks5_dial(proxy: &SocksProxy, dest_host: &str, dest_port: u16) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;

    // 握手：声明支持 no-auth(00)，有凭据时追加 userpass(02)。
    let mut methods: Vec<u8> = vec![0x00];
    if proxy.auth.is_some() {
        methods.push(0x02);
    }
    let mut greeting = vec![0x05u8, methods.len() as u8];
    greeting.extend_from_slice(&methods);
    stream.write_all(&greeting).await?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 0x05 {
        return Err(io::Error::other(format!(
            "socks5 proxy sent bad version: {:#x}",
            reply[0]
        )));
    }
    match reply[1] {
        0x00 => {}
        0x02 => {
            let Some((user, pass)) = &proxy.auth else {
                return Err(io::Error::other(
                    "socks5 proxy demands auth but url has none",
                ));
            };
            let mut sub = vec![0x01u8, user.len() as u8];
            sub.extend_from_slice(user.as_bytes());
            sub.push(pass.len() as u8);
            sub.extend_from_slice(pass.as_bytes());
            stream.write_all(&sub).await?;
            let mut verdict = [0u8; 2];
            stream.read_exact(&mut verdict).await?;
            if verdict[1] != 0x00 {
                return Err(io::Error::other("socks5 userpass auth rejected"));
            }
        }
        0xFF => {
            return Err(io::Error::other(
                "socks5 proxy accepts none of our auth methods",
            ))
        }
        other => {
            return Err(io::Error::other(format!(
                "socks5 proxy chose unknown method {other:#x}"
            )))
        }
    }

    // CONNECT：socks5h 直接发域名；socks5 本地解析后按 IP 发。
    let mut req = vec![0x05u8, 0x01, 0x00];
    if proxy.remote_dns {
        req.push(0x03);
        req.push(dest_host.len() as u8);
        req.extend_from_slice(dest_host.as_bytes());
    } else {
        let addr = tokio::net::lookup_host((dest_host, dest_port))
            .await?
            .next()
            .ok_or_else(|| io::Error::other("dns resolution returned no address"))?;
        match addr {
            std::net::SocketAddr::V4(v4) => {
                req.push(0x01);
                req.extend_from_slice(&v4.ip().octets());
            }
            std::net::SocketAddr::V6(v6) => {
                req.push(0x04);
                req.extend_from_slice(&v6.ip().octets());
            }
        }
    }
    req.extend_from_slice(&dest_port.to_be_bytes());
    stream.write_all(&req).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(io::Error::other(format!(
            "socks5 CONNECT failed, rep={:#x}",
            head[1]
        )));
    }
    let skip = match head[3] {
        0x01 => 4,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        0x04 => 16,
        other => {
            return Err(io::Error::other(format!(
                "socks5 reply bad atyp {other:#x}"
            )))
        }
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).await?;
    Ok(stream)
}

// ── 入口：hyper 升级处理 ──────────────────────────────────────────────

impl ProxyApp {
    pub async fn ws_upgrade(
        self: Arc<Self>,
        mut req: Request<hyper::body::Incoming>,
    ) -> HyperResponse {
        let Some(key) = req
            .headers()
            .get("sec-websocket-key")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "websocket upgrade missing sec-websocket-key",
            );
        };

        // 会话键链与 HTTP 相同（D7 §12.2-D7.5）：realtime 恒带 session-id /
        // x-session-id；均缺则拒绝——WS 无 body 兜底，B3 粘性必须有键。
        let Some(session_key) = super::proxy::extract_session_key(req.headers()) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "realtime websocket requires session-id or x-session-id header",
            );
        };
        let downstream_installation = super::proxy::extract_downstream_installation(req.headers());
        let thread_hint = req
            .headers()
            .get("thread-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();

        // 绑定/放置：与 HTTP 同表同键——同会话 HTTP+WS 必然同账号。
        let Some(binding) =
            self.bind_session(&session_key, /*codex_native*/ true, &thread_hint)
        else {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no available account in pool",
            );
        };
        let mut account = match self.store.get_account(&binding.account_id) {
            Ok(Some(a)) => a,
            _ => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "binding to missing account",
                )
            }
        };
        match self.ensure_token(&account).await {
            Ok(()) => {}
            Err(RefreshError::Terminal(_)) => {
                let _ = self
                    .store
                    .set_state(&account.account_id, crate::atcd::store::ACCOUNT_COOLING);
                return json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "account entered cooling: refresh failed terminally",
                );
            }
            Err(e) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("token refresh failed: {e}"),
                )
            }
        }
        account = self
            .store
            .get_account(&account.account_id)
            .ok()
            .flatten()
            .unwrap_or(account);
        let Some(access_token) = account.access_token.clone() else {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "account has no access token",
            );
        };

        // 并发名额连接期持有（移入桥任务）；连接建立 = 一回合开始，
        // 帧级不加节奏间隔（音频流语义，D7 §12.2-D7.6）。
        let slot = self.gate.slot(&binding.account_id).await;
        self.gate.wait_turn(&binding.account_id).await;

        let persona = Persona {
            account_id: account.account_id.clone(),
            installation_id: account.installation_id.clone(),
            version_pin: account.version_pin.clone(),
            ua: account.user_agent.clone(),
            originator: account.originator.clone(),
            proxy_url: account.proxy_url.clone(),
        };

        // 出站握手头 = 入站 −（逐跳 + sec-websocket-* + 身份/凭据）+ persona。
        // 与 HTTP 路径同一套 strip_inbound + apply。
        let inbound_turn_metadata = req.headers().get("x-codex-turn-metadata").cloned();
        let mut headers = req.headers().clone();
        rewrite::strip_inbound(&mut headers);
        rewrite::apply(
            &mut headers,
            &RewriteInput {
                persona: &persona,
                access_token: &access_token,
                downstream_installation: downstream_installation.as_deref(),
                inbound_turn_metadata: inbound_turn_metadata.as_ref(),
            },
        );

        let ws_url = match upstream_ws_url(&self.upstream, req.uri().path(), req.uri().query()) {
            Ok(u) => u,
            Err(e) => return json_error(StatusCode::BAD_REQUEST, &e),
        };

        // 上游握手；401/403 刷新一次重拨一次（镜像 HTTP 401 自愈）。
        let (up_stream, _up_resp) = match dial_upstream(&persona, headers.clone(), &ws_url).await {
            Ok(v) => v,
            Err(DialError::Upstream(WsError::Http(resp)))
                if matches!(
                    resp.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
            {
                if let Err(e) = self.ensure_token(&account).await {
                    return json_error(
                        StatusCode::BAD_GATEWAY,
                        &format!("token refresh failed: {e}"),
                    );
                }
                let token = self
                    .store
                    .get_account(&account.account_id)
                    .ok()
                    .flatten()
                    .and_then(|a| a.access_token)
                    .unwrap_or(access_token);
                rewrite::apply(
                    &mut headers,
                    &RewriteInput {
                        persona: &persona,
                        access_token: &token,
                        downstream_installation: downstream_installation.as_deref(),
                        inbound_turn_metadata: inbound_turn_metadata.as_ref(),
                    },
                );
                match dial_upstream(&persona, headers, &ws_url).await {
                    Ok(v) => v,
                    Err(e) => return upstream_dial_error(e),
                }
            }
            Err(e) => return upstream_dial_error(e),
        };

        // 101 + 升级后的本地半条连接交给桥任务。
        let accept = websocket_accept_key(&key);
        let on_upgrade = hyper::upgrade::on(&mut req);
        let response = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-accept", accept)
            .body(UnsyncBoxBody::new(
                Full::new(Bytes::new()).map_err(io::Error::other),
            ))
            .expect("static 101 response");

        let app = self.clone();
        let account_id = binding.account_id.clone();
        let bind_key = session_key.clone();
        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(u) => u,
                Err(_) => return, // 客户端在 101 后未完成握手；连接已死
            };
            let down = WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                Some(WebSocketConfig::default()),
            )
            .await;
            bridge(down, up_stream).await;
            // 记账一次：连接视作一回合（D7 §12.2-D7.6）。
            app.gate.record_turn(&account_id);
            let now = now_unix_ms();
            let _ = app.store.touch_account_turn(&account_id, now);
            let _ = app.store.touch_binding(&bind_key, now);
            drop(slot);
        });

        response
    }
}

enum DialError {
    Upstream(WsError),
    Io(io::Error),
    Config(String),
}

fn upstream_dial_error(e: DialError) -> HyperResponse {
    let msg = match e {
        DialError::Upstream(e) => format!("upstream websocket handshake failed: {e}"),
        DialError::Io(e) => format!("upstream connect failed: {e}"),
        DialError::Config(e) => e,
    };
    json_error(StatusCode::BAD_GATEWAY, &msg)
}

/// 上游 WS 连接：SOCKS 隧道（可选）→ TLS（wss，tokio-tungstenite 默认
/// rustls webpki-roots）→ WS 握手。WebSocketConfig::default 与 codex
/// realtime 一致（无 permessage-deflate，设计 §12.1-3）。
async fn dial_upstream(
    persona: &Persona,
    headers: HeaderMap,
    ws_url: &str,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tungstenite::handshake::client::Response,
    ),
    DialError,
> {
    let url = Url::parse(ws_url).map_err(|e| DialError::Config(format!("bad ws url: {e}")))?;
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| DialError::Config("ws url missing host".into()))?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| DialError::Config("ws url missing port".into()))?;

    let tcp = match &persona.proxy_url {
        Some(raw) => {
            let proxy = parse_socks_url(raw).map_err(DialError::Config)?;
            socks5_dial(&proxy, &host, port)
                .await
                .map_err(DialError::Io)?
        }
        None => TcpStream::connect((host.as_str(), port))
            .await
            .map_err(DialError::Io)?,
    };

    let mut request = ws_url
        .into_client_request()
        .map_err(|e| DialError::Config(format!("ws request build failed: {e}")))?;
    // 追加而非替换：tungstenite 已生成 sec-websocket-key/version/connection/
    // upgrade/host，我们只叠加 persona 身份头（strip 后的入站集不含这些键）。
    request.headers_mut().extend(headers);

    let (stream, resp) = tokio_tungstenite::client_async_tls_with_config(
        request,
        tcp,
        Some(WebSocketConfig::default()),
        /*connector*/ None,
    )
    .await
    .map_err(DialError::Upstream)?;
    Ok((stream, resp))
}

/// 双向转发桥：Text/Binary/Close 原样过桥；Ping/Pong 本侧自动应答不转发
/// （D7 §12.2-D7.1）。任一侧终止 → 对侧 Close(1011)/原样 Close。
async fn bridge(
    down: WebSocketStream<TokioIo<Upgraded>>,
    up: WebSocketStream<MaybeTlsStream<TcpStream>>,
) {
    let (mut down_sink, mut down_stream) = down.split();
    let (mut up_sink, mut up_stream) = up.split();
    loop {
        tokio::select! {
            frame = down_stream.next() => {
                match frame {
                    Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => {
                        if up_sink.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let _ = up_sink.send(Message::Close(frame)).await;
                        break;
                    }
                    // tungstenite 读路径已自动回 Pong；Ping/Pong/Frame 不跨连接。
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Err(_)) => {
                        let _ = up_sink.send(close_internal("downstream error")).await;
                        break;
                    }
                    None => {
                        let _ = up_sink.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
            frame = up_stream.next() => {
                match frame {
                    Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => {
                        if down_sink.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let _ = down_sink.send(Message::Close(frame)).await;
                        break;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Err(_)) => {
                        let _ = down_sink.send(close_internal("upstream error")).await;
                        break;
                    }
                    None => {
                        let _ = down_sink.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    }
    let _ = down_sink.flush().await;
    let _ = up_sink.flush().await;
}

fn close_internal(reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code: CLOSE_INTERNAL,
        reason: reason.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6455 §1.3 官方向量。
    #[test]
    fn accept_key_rfc6455_example() {
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    // 对拍 codex methods.rs:1179-1201 与其测试预期值（1966-2110）。
    #[test]
    fn realtime_path_normalization_matches_codex() {
        assert_eq!(normalize_realtime_path(""), "/v1/realtime");
        assert_eq!(normalize_realtime_path("/"), "/v1/realtime");
        assert_eq!(normalize_realtime_path("/v1"), "/v1/realtime");
        assert_eq!(normalize_realtime_path("/v1/"), "/v1/realtime");
        assert_eq!(normalize_realtime_path("/v1/realtime"), "/v1/realtime");
        assert_eq!(normalize_realtime_path("/v1/realtime/"), "/v1/realtime");
        assert_eq!(normalize_realtime_path("/openai/v1"), "/openai/v1/realtime");
        assert_eq!(
            normalize_realtime_path("/openai/v1/"),
            "/openai/v1/realtime"
        );
        // 非 v1/realtime 结尾的既有路径原样保持（methods.rs 的 fallback）。
        assert_eq!(
            normalize_realtime_path("/backend-api/codex"),
            "/backend-api/codex"
        );
    }

    #[test]
    fn upstream_url_maps_scheme_and_passes_query_verbatim() {
        // https 基址 → wss；归一路径 + query 字节级透传。
        let url = upstream_ws_url(
            "https://chatgpt.com/backend-api/codex",
            "/v1/realtime",
            Some("model=rt-m&trace=1&call_id=abc"),
        )
        .unwrap();
        assert_eq!(
            url,
            "wss://chatgpt.com/backend-api/codex?model=rt-m&trace=1&call_id=abc"
        );
        // /v1 → /v1/realtime；无 query 不带 '?'。
        let url = upstream_ws_url("https://api.openai.com/v1", "/v1/realtime", None).unwrap();
        assert_eq!(url, "wss://api.openai.com/v1/realtime");
        // ws 基址保持 ws；端口保留。
        let url = upstream_ws_url("http://127.0.0.1:8011", "/v1/realtime", Some("a=b")).unwrap();
        assert_eq!(url, "ws://127.0.0.1:8011/v1/realtime?a=b");
        // v3 拒绝。
        assert!(upstream_ws_url("https://x.example", "/v1/live/call-1", None).is_err());
    }

    #[test]
    fn socks_url_parsing() {
        let p = parse_socks_url("socks5://10.0.0.9:1080").unwrap();
        assert_eq!(
            p,
            SocksProxy {
                remote_dns: false,
                host: "10.0.0.9".into(),
                port: 1080,
                auth: None
            }
        );
        let p = parse_socks_url("socks5h://u:p%40ss@proxy.lan").unwrap();
        assert_eq!(
            p,
            SocksProxy {
                remote_dns: true,
                host: "proxy.lan".into(),
                port: 1080,
                auth: Some(("u".into(), "p@ss".into())),
            }
        );
        assert!(parse_socks_url("http://proxy:3128").is_err());
    }
}
