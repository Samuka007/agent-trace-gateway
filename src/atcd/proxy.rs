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

use std::collections::HashMap as StdHashMap;

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

/// 非 codex 客户端的会话键：body 的 prompt_cache_key 优先，
/// 否则用 body 前缀（4KB）散列——多轮重放的前缀是稳定的。
fn derive_session_key(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(k) = v.get("prompt_cache_key").and_then(|x| x.as_str()) {
            if !k.trim().is_empty() {
                return format!("pck:{k}");
            }
        }
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&body[..body.len().min(4096)]);
    format!("pfx:{}", hex::encode(h.finalize())[..16].to_string())
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
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
        // 会话键提取链：codex 的 session-id → opencode 系的 x-session-id →
        // body prompt_cache_key / 前缀散列（见下方 derive_session_key）。
        let session_key = req
            .headers()
            .get("session-id")
            .or_else(|| req.headers().get("session_id"))
            .or_else(|| req.headers().get("x-session-id"))
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

        // ── 绑定（仅路由粘性） ──────────────────────────────────────
        // codex 下游（带 session-id 头）：身份是下游真实工件，透传不替换。
        // 其余 responses 客户端（omp/opencode）：铸造 v7 身份树并逐轮合成
        // turn 元数据——这是"第三方 ChatGPT 登录客户端"的自洽形态。
        let codex_native = session_key.is_some();
        let session_key = session_key.unwrap_or_else(|| derive_session_key(&body));
        let existing = self.store.binding(&session_key).ok().flatten();
        let (account_id, bound_session, bound_thread, bound_root_turn, bound_context_window) = match existing {
            Some(b) => (b.account_id, b.session_id, b.thread_id, b.root_turn_id, b.context_window_id),
            None => {
                let Some(account_id) = self.placement.place(&self.store) else {
                    return json_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "no available account in pool",
                    );
                };
                let bound_session = if codex_native {
                    session_key.clone()
                } else {
                    rewrite::mint_session_id()
                };
                let bound_thread = if codex_native {
                    parts
                        .headers
                        .get("thread-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string()
                } else {
                    rewrite::mint_thread_id()
                };
                // 跨轮稳定字段：root_turn 锚定首轮，context_window 在窗口期不变
                let root_turn_id = if codex_native { String::new() } else { uuid::Uuid::now_v7().to_string() };
                let context_window_id = if codex_native { String::new() } else { uuid::Uuid::now_v7().to_string() };
                let _ = self.store.insert_binding(&BindingRow {
                    session_key: session_key.clone(),
                    account_id: account_id.clone(),
                    thread_id: bound_thread.clone(),
                    session_id: bound_session.clone(),
                    root_turn_id: root_turn_id.clone(),
                    context_window_id: context_window_id.clone(),
                    turns: 0,
                    last_seen: now_unix_ms(),
                });
                (account_id, bound_session, bound_thread, root_turn_id, context_window_id)
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
        // 第三方路径：先用 codex 自身结构体铸造本轮身份（元数据头与 body
        // 投影共用同一份，保证 header/body 身份一致）。
        let third_party_meta = if codex_native {
            None
        } else {
            let identity = rewrite::TurnIdentity {
                installation_id: &persona.installation_id,
                session_id: &bound_session,
                thread_id: &bound_thread,
                root_turn_id: &bound_root_turn,
                context_window_id: &bound_context_window,
                agent_name: rewrite::pick_agent_name(&session_key),
                sandbox: rewrite::DEFAULT_SANDBOX,
                sandbox_mode: rewrite::DEFAULT_SANDBOX_MODE,
            };
            Some(rewrite::mint_metadata(&identity, now_unix_ms()))
        };
        if codex_native {
            rewrite::apply(
                &mut headers,
                &RewriteInput {
                    persona: &persona,
                    access_token: &access_token,
                    downstream_installation: downstream_installation.as_deref(),
                    inbound_turn_metadata: inbound_turn_metadata.as_ref(),
                },
            );
        } else {
            rewrite::apply_third_party(
                &mut headers,
                &persona,
                &access_token,
                third_party_meta.as_ref().expect("third-party metadata"),
                &bound_session,
            );
        }

        // body 处理：codex 下游做 installation 外科替换（其余字节不动）；
        // 第三方客户端做信封合成（store/include/prompt_cache_key/client_metadata
        // 对齐 codex 形状，instructions/input/tools 保留其自洽内容）。
        let body = if codex_native {
            rewrite::surgical_installation_replace(
                body,
                downstream_installation.as_deref(),
                persona.installation_id.as_str(),
            )
        } else {
            let cm: StdHashMap<String, String> = third_party_meta
                .as_ref()
                .map(|m| m.client_metadata())
                .unwrap_or_default();
            rewrite::codex_envelope_body(&body, &cm).unwrap_or(body)
        };

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
                self.record_turn(&account_id, &Some(session_key.clone()));
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
                        if codex_native {
                            rewrite::apply(
                                &mut headers,
                                &RewriteInput {
                                    persona: &persona,
                                    access_token: token,
                                    downstream_installation: downstream_installation.as_deref(),
                                    inbound_turn_metadata: inbound_turn_metadata.as_ref(),
                                },
                            );
                        } else {
                            rewrite::apply_third_party(
                                &mut headers,
                                &persona,
                                token,
                                third_party_meta.as_ref().expect("third-party metadata"),
                                &bound_session,
                            );
                        }
                        if let Ok(retried) =
                            client.post(&url).headers(headers).body(body).send().await
                        {
                            resp = retried;
                        }
                    }
                }
            }
        }

        self.record_turn(&account_id, &Some(session_key.clone()));

        // 配额观测：上游响应头里的窗口用量百分比落库，供放置过滤
        let qp = header_f64(resp.headers(), "x-codex-primary-used-percent");
        let qs = header_f64(resp.headers(), "x-codex-secondary-used-percent");
        if qp.is_some() || qs.is_some() {
            let _ = self.store.update_quota(&account_id, qp, qs, now_unix());
        }

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
