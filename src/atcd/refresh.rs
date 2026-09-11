//! 凭据刷新：与 codex CLI 逐字一致的 refresh grant。
//!
//! wire 形状逐字取自 openai/codex `codex-rs/login/src/auth/manager.rs`
//! （main，2026-09）：
//! - `POST https://auth.openai.com/oauth/token`，`Content-Type: application/json`
//! - body: `{"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": ...}`
//! - `CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"`
//! - 200 响应字段：`id_token` / `access_token` / `refresh_token`（均可选、可能轮换）
//!
//! codex 响应里没有 expires_in：过期时间从 id_token 的 JWT `exp` claim
//! 解析（codex 同样如此，见其 token_data::parse_jwt_expiration）。

use base64::Engine;
use serde::Deserialize;

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";

#[derive(Debug)]
pub struct RefreshedTokens {
    pub id_token: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// unix 秒。来自 id_token JWT exp；id_token 缺失时为 None（调用方保守处理）。
    pub expires_at: Option<i64>,
}

#[derive(Debug)]
pub enum RefreshError {
    /// refresh_token 已失效（invalid_grant 等）——账号进入冷却/报废，重试无意义。
    Terminal(String),
    /// 网络/5xx——可退避重试。
    Transient(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::Terminal(m) => write!(f, "terminal: {m}"),
            RefreshError::Transient(m) => write!(f, "transient: {m}"),
        }
    }
}

#[derive(serde::Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

pub fn refresh_body(refresh_token: &str) -> String {
    serde_json::to_string(&RefreshRequest {
        client_id: CODEX_CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token,
    })
    .expect("serialize refresh request")
}

pub async fn refresh(
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<RefreshedTokens, RefreshError> {
    let resp = http
        .post(REFRESH_URL)
        .header("Content-Type", "application/json")
        .body(refresh_body(refresh_token))
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let err = format!("{status}: {text}");
        if status == reqwest::StatusCode::BAD_REQUEST && text.contains("invalid_grant") {
            return Err(RefreshError::Terminal(err));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(RefreshError::Terminal(err));
        }
        return Err(RefreshError::Transient(err));
    }

    let parsed: RefreshResponse =
        serde_json::from_str(&text).map_err(|e| RefreshError::Transient(e.to_string()))?;
    let expires_at = parsed.id_token.as_deref().and_then(jwt_exp);
    Ok(RefreshedTokens {
        id_token: parsed.id_token,
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at,
    })
}

/// 解析 JWT（未验证签名——我们只读上游刚发回来的 exp，不做信任判断）。
pub fn jwt_exp(token: &str) -> Option<i64> {
    let mid = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(mid)
        .ok()?;
    let val: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    val.get("exp")?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_matches_codex_wire_shape() {
        let b = refresh_body("rt-1");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["client_id"], CODEX_CLIENT_ID);
        assert_eq!(v["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(v["grant_type"], "refresh_token");
        assert_eq!(v["refresh_token"], "rt-1");
        // 不应有多余字段
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn jwt_exp_parses_unverified_claim() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = serde_json::json!({"exp": 1_800_000_000i64, "sub": "x"});
        let enc = URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = format!("aaa.{enc}.bbb");
        assert_eq!(jwt_exp(&token), Some(1_800_000_000));
        assert_eq!(jwt_exp("not-a-jwt"), None);
    }
}
