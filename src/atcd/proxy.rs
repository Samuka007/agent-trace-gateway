//! 数据面：hyper 服务，把六件事串成一个请求路径。
//!
//! 路径：提取会话键 → 绑定/放置 → 确保令牌（必要时刷新）→ 节奏门 →
//! 剥离入站身份 → 写出站身份 → body 字节透传 → SSE 流式回传。
//! 所有错误以 JSON 形式返回；业务失败不改变绑定表（失败轮不记账）。

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{HeaderMap, Request, Response, StatusCode};
use tokio::sync::Mutex as AsyncMutex;

use crate::atcd::placement::Placement;
use crate::atcd::refresh::{self, RefreshError};
use crate::atcd::rewrite::{self, RewriteInput};
use crate::atcd::scheduler::PacingGate;
use crate::atcd::store::{self, BindingRow, Store, ACCOUNT_COOLING};
use crate::atcd::persona::Persona;

pub type RespBody = UnsyncBoxBody<Bytes, io::Error>;
pub type HyperResponse = Response<RespBody>;

const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// 提前刷新余量：access_token 剩余寿命低于该值即刷新。
const TOKEN_REFRESH_MARGIN_SECS: i64 = 300;

pub struct ProxyApp {
    pub store: Arc<Store>,
    pub gate: Arc<PacingGate>,
    pub placement: Arc<dyn Placement>,
    pub http: reqwest::Client,
    pub upstream: String,
    pub refresh_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// 每出口代理一个 reqwest Client（连接池与出口绑定）。
    pub per_proxy: AsyncMutex<HashMap<String, reqwest::Client>>,
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    now_unix_ms() / 1000
}

fn json_error(status: StatusCode, message: &str) -> HyperResponse {
    let body = serde_json::json!({"error": {"message": message}});
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(UnsyncBoxBody::new(
            Full::new(Bytes::from(body.to_string())).map_err(io::Error::other),
        ))
        .expect("static response")
}

fn full_response(status: StatusCode, text: &str) -> HyperResponse {
    Response::builder()
        .status(status)
        .body(UnsyncBoxBody::new(
            Full::new(Bytes::from(text.to_string())).map_err(io::Error::other),
        ))
        .expect("static response")
}

fn strip_response_hop_by_hop(headers: &mut HeaderMap) {
    for name in ["connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade"] {
        headers.remove(name);
    }
}

impl ProxyApp {
    pub async fn handle(&self, req: Request<Incoming>) -> HyperResponse {
        if req.uri().path() == "/healthz" {
            return full_response(StatusCode::OK, "ok");
        }
        if req.method() != hyper::Method::POST {
            return json_error(StatusCode::METHOD_NOT_ALLOWED, "only POST is supported");
        }

        // 捕获入站工件（剥离前）：turn 元数据原样、下游 installation。
        let inbound_turn_metadata = req.headers().get("x-codex-turn-metadata").cloned();
        let downstream_installation = req
            .headers()
            .get("x-codex-installation-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                // 头缺失时从 turn 元数据 JSON 提取
                let raw = inbound_turn_metadata.as_ref()?;
                let v: serde_json::Value = serde_json::from_slice(raw.as_bytes()).ok()?;
                v.get("installation_id")?.as_str().map(str::to_string)
            });
        let session_key = req
            .headers()
            .get("session-id")
            .or_else(|| req.headers().get("session_id"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let (parts, body) = req.into_parts();
        let body = match BodyExt::collect(body).await {
            Ok(collected) => collected.to_bytes(),
            Err(e) => return json_error(StatusCode::BAD_REQUEST, &format!("body read failed: {e}")),
        };
        if body.len() > MAX_BODY_BYTES {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }

        // ── 绑定（仅路由粘性；身份是下游真实工件，不替换） ───────────
        let existing = match &session_key {
            Some(key) => self.store.binding(key).ok().flatten(),
            None => None,
        };
        let account_id = match existing {
            Some(b) => b.account_id,
            None => {
                let Some(account_id) = self.placement.place(&self.store) else {
                    return json_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "no available account in pool",
                    );
                };
                if let Some(key) = &session_key {
                    let thread_id = parts
                        .headers
                        .get("thread-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let _ = self.store.insert_binding(&BindingRow {
                        session_key: key.clone(),
                        account_id: account_id.clone(),
                        thread_id,
                        session_id: key.clone(),
                        turns: 0,
                        last_seen: now_unix_ms(),
                    });
                }
                account_id
            }
        };

        // ── 确保令牌 ────────────────────────────────────────────────
        let mut account = match self.store.get_account(&account_id) {
            Ok(Some(a)) => a,
            _ => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "binding to missing account"),
        };
        match self.ensure_token(&account).await {
            Ok(()) => {}
            Err(e) => {
                if matches!(e, RefreshError::Terminal(_)) {
                    let _ = self.store.set_state(&account_id, ACCOUNT_COOLING);
                    return json_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &format!("account {account_id} entered cooling: refresh failed terminally"),
                    );
                }
                return json_error(StatusCode::BAD_GATEWAY, &format!("token refresh failed: {e}"));
            }
        }
        // ensure_token 可能已更新令牌
        account = self.store.get_account(&account_id).ok().flatten().unwrap_or(account);
        let access_token = account
            .access_token
            .clone()
            .unwrap_or_default();

        // ── 节奏门 ──────────────────────────────────────────────────
        let _slot = self.gate.slot(&account_id).await;
        self.gate.wait_turn(&account_id).await;

        // ── 出站请求 ────────────────────────────────────────────────
        let persona = Persona {
            account_id: account.account_id.clone(),
            installation_id: account.installation_id.clone(),
            version_pin: account.version_pin.clone(),
            ua: account.user_agent.clone(),
            originator: account.originator.clone(),
            proxy_url: account.proxy_url.clone(),
        };
        let mut headers = parts.headers.clone();
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

        // body 外科替换：仅 installation 投影，其余字节不动
        let body = rewrite::surgical_installation_replace(
            body,
            downstream_installation.as_deref(),
            persona.installation_id.as_str(),
        );

        let url = format!("{}{}", self.upstream.trim_end_matches('/'), parts.uri.path());
        let client = self.client_for(persona.proxy_url.as_deref()).await;

        let mut resp = match client
            .post(&url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                self.record_turn(&account_id, &session_key);
                return json_error(StatusCode::BAD_GATEWAY, &format!("upstream error: {e}"));
            }
        };

        // 401：刷新一次并重试一次（凭据过期是最常见的可自愈故障）。
        if resp.status() == StatusCode::UNAUTHORIZED {
            if let Ok(()) = self.ensure_token(&account).await {
                let account = self.store.get_account(&account_id).ok().flatten();
                if let Some(account) = account {
                    if let Some(token) = account.access_token.as_deref() {
                        let mut headers = parts.headers.clone();
                        rewrite::strip_inbound(&mut headers);
                        rewrite::apply(
                            &mut headers,
                            &RewriteInput {
                                persona: &persona,
                                access_token: token,
                                downstream_installation: downstream_installation.as_deref(),
                                inbound_turn_metadata: inbound_turn_metadata.as_ref(),
                            },
                        );
                        if let Ok(retried) =
                            client.post(&url).headers(headers).body(body).send().await
                        {
                            resp = retried;
                        }
                    }
                }
            }
        }

        self.record_turn(&account_id, &session_key);

        // ── 回程：状态与头透传，SSE 流式 ────────────────────────────
        let mut builder = Response::builder().status(resp.status());
        {
            let mut upstream_headers = resp.headers().clone();
            strip_response_hop_by_hop(&mut upstream_headers);
            for (k, v) in upstream_headers.iter() {
                builder = builder.header(k, v);
            }
        }
        let stream = resp
            .bytes_stream()
            .map(|r| r.map(Frame::data).map_err(io::Error::other));
        match builder.body(UnsyncBoxBody::new(StreamBody::new(stream))) {
            Ok(r) => r,
            Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("build response: {e}")),
        }
    }

    fn record_turn(&self, account_id: &str, session_key: &Option<String>) {
        self.gate.record_turn(account_id);
        let now = now_unix();
        let _ = self.store.touch_account_turn(account_id, now);
        if let Some(key) = session_key {
            let _ = self.store.touch_binding(key, now_unix_ms());
        }
    }

    /// 令牌双检：未过期直接用；过期则每账号串行刷新并落库。
    async fn ensure_token(&self, account: &store::AccountRow) -> Result<(), RefreshError> {
        let fresh = match (account.access_token.as_deref(), account.expires_at) {
            (Some(_), Some(exp)) => exp - TOKEN_REFRESH_MARGIN_SECS > now_unix(),
            (Some(_), None) => true, // 无过期信息：姑且使用，401 路径会自愈
            (None, _) => false,
        };
        if fresh {
            return Ok(());
        }

        let lock = {
            let mut locks = self.refresh_locks.lock().await;
            locks
                .entry(account.account_id.clone())
                .or_default()
                .clone()
        };
        let _guard = lock.lock().await;

        // 双检：等锁期间可能已被并发刷新
        if let Ok(Some(latest)) = self.store.get_account(&account.account_id) {
            let fresh = match (latest.access_token.as_deref(), latest.expires_at) {
                (Some(_), Some(exp)) => exp - TOKEN_REFRESH_MARGIN_SECS > now_unix(),
                (Some(_), None) => true,
                (None, _) => false,
            };
            if fresh {
                return Ok(());
            }
        }

        let tokens = refresh::refresh(&self.http, &account.refresh_token).await?;
        let access = tokens
            .access_token
            .ok_or_else(|| RefreshError::Transient("refresh returned no access_token".into()))?;
        // id_token 缺失时保留旧过期时间，避免写成"永不过期"
        let expires_at = tokens.expires_at.or(account.expires_at);
        self.store
            .update_tokens(
                &account.account_id,
                &access,
                tokens.id_token.as_deref(),
                tokens.refresh_token.as_deref().unwrap_or(&account.refresh_token),
                expires_at,
            )
            .map_err(|e| RefreshError::Transient(e.to_string()))?;
        Ok(())
    }

    async fn client_for(&self, proxy_url: Option<&str>) -> reqwest::Client {
        match proxy_url {
            None => self.http.clone(),
            Some(url) => {
                {
                    let map = self.per_proxy.lock().await;
                    if let Some(c) = map.get(url) {
                        return c.clone();
                    }
                }
                let mut builder = reqwest::Client::builder();
                if let Ok(p) = reqwest::Proxy::all(url) {
                    builder = builder.proxy(p);
                }
                let client = builder.build().unwrap_or_else(|_| self.http.clone());
                self.per_proxy.lock().await.insert(url.to_string(), client.clone());
                client
            }
        }
    }
}

/// hyper Service 包装：handle 永不返回 Err。
#[derive(Clone)]
pub struct ProxyService(pub Arc<ProxyApp>);

impl hyper::service::Service<Request<Incoming>> for ProxyService {
    type Response = HyperResponse;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let app = self.0.clone();
        Box::pin(async move { Ok(app.handle(req).await) })
    }
}
