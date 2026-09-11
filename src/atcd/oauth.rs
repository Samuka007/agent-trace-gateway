//! OAuth 登录流：镜像 codex-rs/login 的 wire（粘贴回调 URL 模式 + device 模式）。
//!
//! 为什么不直接引用 codex-login crate（决策记录）：
//! 1. 它不提供"粘贴回调 URL"模式——浏览器在远端设备、VPS 完成交换的
//!    web 登录形态（sub2api 式）只能自己实现交换调用；
//! 2. 其依赖 tokio-tungstenite 被 codex 工作区 patch 到私有 fork
//!    （带 proxy feature），与本地依赖解析冲突。
//! 因此本模块 wire 逐字镜像 codex 源码（main，2026-09）：
//! - PKCE 生成：codex-rs/login/src/pkce.rs（64 随机字节 → b64url verifier，
//!   challenge = b64url(SHA256(verifier))，S256）
//! - authorize 参数：login/src/server.rs:584-606（含 id_token_add_organizations、
//!   codex_cli_simplified_flow、originator）
//! - 交换：server.rs:809-843，POST {issuer}/oauth/token，
//!   form: grant_type=authorization_code & code & redirect_uri & client_id & code_verifier
//! - device：device_code_auth.rs——usercode: POST {issuer}/api/accounts/deviceauth/usercode
//!   （json {client_id}）；轮询：POST {issuer}/api/accounts/deviceauth/token
//!   （json {device_auth_id, user_code}，15 分钟上限）；device 交换的
//!   redirect_uri = {issuer}/deviceauth/callback（无 localhost 依赖——这就是
//!   codex 原生的远程登录路径）
//!
//! 两种模式都不绑定 IP：浏览器与交换端可以在不同网络（device 流程本身
//! 即如此设计）。建议登录走账号绑定的出口，保持"注册 IP = 服务 IP"。

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};


pub const ISSUER: &str = "https://auth.openai.com";
/// 与 codex CLI 的 Hydra redirect 白名单一致（login/src/server.rs，端口 1455/1457）。
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// device 模式的 redirect：服务端回调，不经过任何 localhost。
pub fn device_redirect_uri(issuer: &str) -> String {
    format!("{}/deviceauth/callback", issuer.trim_end_matches('/'))
}

#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

/// 逐字镜像 codex-rs/login/src/pkce.rs 的 generate_pkce。
pub fn generate_pkce() -> PkceCodes {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceCodes { code_verifier, code_challenge }
}

/// 逐字镜像 codex login/src/server.rs 的 generate_state。
pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 逐字镜像 codex login/src/server.rs:576-608 的 build_authorize_url。
pub fn build_authorize_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
    originator: &str,
) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        ("code_challenge", pkce.code_challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", originator),
    ];
    let qs = query
        .into_iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}/oauth/authorize?{qs}", issuer.trim_end_matches('/'))
}

/// 从 id_token（未验证签名，仅解析上游刚发回的 token）提取
/// chatgpt_account_id。claim 位于 "https://api.openai.com/auth" 命名空间
/// （codex login/src/token_data.rs 同路径）。
pub fn chatgpt_account_id_from_id_token(id_token: &str) -> Option<String> {
    let mid = id_token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(mid).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

#[derive(Debug, Clone)]
pub struct ExchangedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug)]
pub enum OAuthError {
    /// 凭据/授权被拒——重试无意义。
    Terminal(String),
    Transient(String),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthError::Terminal(m) => write!(f, "terminal: {m}"),
            OAuthError::Transient(m) => write!(f, "transient: {m}"),
        }
    }
}

fn post_form(url: &str, form: String) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
}

/// 逐字镜像 codex login/src/server.rs:809-843 的 exchange_code_for_tokens。
pub async fn exchange_code(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    code_verifier: &str,
    code: &str,
) -> Result<ExchangedTokens, OAuthError> {
    let token_endpoint = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(client_id),
        urlencoding::encode(code_verifier),
    );
    let resp = post_form(&token_endpoint, form)
        .send()
        .await
        .map_err(|e| OAuthError::Transient(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let err = format!("{status}: {text}");
        return Err(if status.as_u16() == 400 || status.as_u16() == 401 {
            OAuthError::Terminal(err)
        } else {
            OAuthError::Transient(err)
        });
    }
    #[derive(Deserialize)]
    struct TokenResponse {
        id_token: String,
        access_token: String,
        refresh_token: String,
    }
    let t: TokenResponse =
        serde_json::from_str(&text).map_err(|e| OAuthError::Transient(e.to_string()))?;
    Ok(ExchangedTokens {
        id_token: t.id_token,
        access_token: t.access_token,
        refresh_token: t.refresh_token,
    })
}

#[derive(Debug, Clone)]
pub struct DeviceCode {
    /// 用户在任意设备浏览器打开的验证页。
    pub verification_url: String,
    pub user_code: String,
    pub device_auth_id: String,
    pub interval_secs: u64,
}

/// 逐字镜像 codex login/src/device_code_auth.rs 的 request_user_code。
pub async fn request_device_code(
    issuer: &str,
    client_id: &str,
) -> Result<DeviceCode, OAuthError> {
    let base = issuer.trim_end_matches('/');
    let url = format!("{base}/api/accounts/deviceauth/usercode");
    let body = serde_json::json!({ "client_id": client_id }).to_string();
    let resp = reqwest::Client::new()
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| OAuthError::Transient(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(OAuthError::Transient(format!("{status}: {text}")));
    }
    #[derive(Deserialize)]
    struct UserCodeResp {
        device_auth_id: String,
        #[serde(alias = "user_code", alias = "usercode")]
        user_code: String,
        #[serde(default = "default_interval")]
        interval: u64,
    }
    fn default_interval() -> u64 {
        5
    }
    let uc: UserCodeResp =
        serde_json::from_str(&text).map_err(|e| OAuthError::Transient(e.to_string()))?;
    Ok(DeviceCode {
        verification_url: format!("{base}/codex/device"),
        user_code: uc.user_code,
        device_auth_id: uc.device_auth_id,
        interval_secs: uc.interval,
    })
}

#[derive(Debug, Clone)]
pub struct DeviceExchange {
    pub authorization_code: String,
    pub code_verifier: String,
}

/// 逐字镜像 codex login/src/device_code_auth.rs 的 poll_for_token
/// （15 分钟上限，服务端节流按 interval）。
pub async fn poll_device_code(
    issuer: &str,
    device: &DeviceCode,
) -> Result<DeviceExchange, OAuthError> {
    let base = issuer.trim_end_matches('/');
    let url = format!("{base}/api/accounts/deviceauth/token");
    let start = std::time::Instant::now();
    let max_wait = std::time::Duration::from_secs(15 * 60);
    loop {
        let body = serde_json::json!({
            "device_auth_id": device.device_auth_id,
            "user_code": device.user_code,
        })
        .to_string();
        let resp = reqwest::Client::new()
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthError::Transient(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 400 {
            // 用户尚未完成授权——按 interval 继续等
            tokio::time::sleep(std::time::Duration::from_secs(device.interval_secs.max(1))).await;
        } else if status.is_success() {
            #[derive(Deserialize)]
            struct CodeSuccessResp {
                authorization_code: String,
                code_verifier: String,
            }
            let c: CodeSuccessResp = serde_json::from_str(&text)
                .map_err(|e| OAuthError::Transient(e.to_string()))?;
            return Ok(DeviceExchange {
                authorization_code: c.authorization_code,
                code_verifier: c.code_verifier,
            });
        } else {
            return Err(OAuthError::Transient(format!("{status}: {text}")));
        }
        if start.elapsed() > max_wait {
            return Err(OAuthError::Transient("device login timed out (15m)".into()));
        }
    }
}

/// 从粘贴的回调 URL 解出 code 并校验 state。
pub fn parse_callback_url(url: &str, expected_state: &str) -> Result<String, OAuthError> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|e| OAuthError::Terminal(format!("回调 URL 无法解析: {e}")))?;
    let mut code = None;
    let mut state = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    let state = state.ok_or_else(|| OAuthError::Terminal("回调缺少 state".into()))?;
    if state != expected_state {
        return Err(OAuthError::Terminal("state 不匹配（防 CSRF 校验失败）".into()));
    }
    code.ok_or_else(|| OAuthError::Terminal("回调缺少 code".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let p = generate_pkce();
        assert!(p.code_verifier.len() >= 43);
        let digest = Sha256::digest(p.code_verifier.as_bytes());
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(p.code_challenge, expect);
    }

    #[test]
    fn authorize_url_contains_all_codex_params() {
        let pkce = PkceCodes {
            code_verifier: "v".into(),
            code_challenge: "c".into(),
        };
        let url = build_authorize_url(
            ISSUER,
            "app_EMoamEEZ73f0CkXaXp7hrann",
            REDIRECT_URI,
            &pkce,
            "st",
            "codex_cli_rs",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        for frag in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            &format!("redirect_uri={}", urlencoding::encode(REDIRECT_URI)),
            "scope=openid%20profile%20email%20offline_access",
            "code_challenge=c",
            "code_challenge_method=S256",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "state=st",
            "originator=codex_cli_rs",
        ] {
            assert!(url.contains(frag), "缺少 {frag}: {url}");
        }
    }

    #[test]
    fn callback_parse_validates_state_and_extracts_code() {
        let url = "http://localhost:1455/auth/callback?code=abc123&state=st";
        assert_eq!(parse_callback_url(url, "st").unwrap(), "abc123");
        assert!(parse_callback_url(url, "other").is_err());
        assert!(parse_callback_url("http://localhost:1455/auth/callback?state=st", "st").is_err());
    }
}
