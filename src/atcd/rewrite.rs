//! 出站身份处理："只换必须换的"。
//!
//! 下游是真实 codex CLI，它发来的 session-id / thread-id / window-id /
//! turn 元数据是真实客户端工件（uuid v7、真实时戳、真实终端形态）——
//! **一律透传**，任何改写都是信息损失与签名风险。
//!
//! 必须改写的只有账号级身份：
//! - `x-codex-installation-id`（头）→ 账号人设的 installation；
//! - `x-codex-turn-metadata` JSON 里的 `installation_id` 字段（其余字段保留）；
//! - body 中 client_metadata 的 installation 投影——由调用方对 body 做
//!   下游 installation 字符串的**外科替换**（见 `surgical_installation_replace`），
//!   其余字节不动；
//! - authorization / chatgpt-account-id → 账号凭据；
//! - user-agent / originator → 账号人设（同一 installation 不能跨请求
//!   呈现多种 OS/版本——设备不会变换自己的操作系统）。
//!
//! 之所以替换 body 中的 installation：headers 与 body 的 client_metadata
//! 是同一身份的两个投影，只换头不换 body 会制造"身份分裂"签名；字符串
//! 级替换保证其余字节逐位不动。

use crate::atcd::persona::Persona;

/// 入站需要剥离的头：逐跳头 + 我们要重新写入的身份/凭据头。
/// 其余一切（包括全部 x-codex-* 客户端特征头）透传。
pub fn strip_inbound(headers: &mut http::HeaderMap) {
    const STRIP: &[&str] = &[
        "host",
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "content-length",
        "authorization",
        "chatgpt-account-id",
        "x-codex-installation-id",
        "originator",
        "user-agent",
    ];
    for name in STRIP {
        headers.remove(*name);
    }
}

pub struct RewriteInput<'a> {
    pub persona: &'a Persona,
    pub access_token: &'a str,
    /// 下游自身的 installation id（从头或 turn 元数据提取）。
    /// 与人设不同时，turn 元数据里的该字段被替换。
    pub downstream_installation: Option<&'a str>,
    /// 剥离前捕获的入站 turn 元数据（None = 下游未携带，不发明）。
    pub inbound_turn_metadata: Option<&'a http::HeaderValue>,
}

pub fn apply(headers: &mut http::HeaderMap, input: &RewriteInput<'_>) {
    let p = input.persona;
    headers.insert("originator", p.originator.parse().unwrap());
    headers.insert("user-agent", p.user_agent().parse().unwrap());
    headers.insert(
        "authorization",
        format!("Bearer {}", input.access_token).parse().unwrap(),
    );
    headers.insert("chatgpt-account-id", p.account_id.parse().unwrap());
    headers.insert(
        "x-codex-installation-id",
        p.installation_id.parse().unwrap(),
    );

    if let Some(raw) = input.inbound_turn_metadata {
        if let Some(out) = turn_metadata_with_installation(raw, p.installation_id.as_str()) {
            headers.insert("x-codex-turn-metadata", out);
        }
    }
}

/// 对 turn 元数据 JSON 只替换 installation_id 字段。
pub fn turn_metadata_with_installation(
    raw: &http::HeaderValue,
    installation_id: &str,
) -> Option<http::HeaderValue> {
    let mut v: serde_json::Value = serde_json::from_slice(raw.as_bytes()).ok()?;
    let obj = v.as_object_mut()?;
    obj.insert("installation_id".into(), installation_id.into());
    let out = serde_json::to_string(&v).ok()?;
    http::HeaderValue::from_str(&out).ok()
}

/// 第三方 responses 客户端（omp/opencode）：铸造 v7 会话/线程 id。
/// 与 codex 的 `SessionId::new` / `ThreadId::new` 同一原语。
pub fn mint_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub fn mint_thread_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 逐轮合成 turn 元数据：turn_id 新 v7、时戳取真实发送时刻，
/// window 挂在 thread 名下（thread:0），thread_source 如实为 user。
pub fn mint_turn_metadata(
    installation_id: &str,
    session_id: &str,
    thread_id: &str,
    now_unix_ms: i64,
) -> http::HeaderValue {
    let v = serde_json::json!({
        "installation_id": installation_id,
        "session_id": session_id,
        "thread_id": thread_id,
        "window_id": format!("{thread_id}:0"),
        "turn_id": uuid::Uuid::now_v7().to_string(),
        "turn_started_at_unix_ms": now_unix_ms,
        "thread_source": "user",
    });
    http::HeaderValue::from_str(&v.to_string()).expect("turn metadata header")
}

/// 第三方客户端的完整身份头：铸造的身份树 + 账号人设壳。
/// instructions/工具表保留客户端自己的——第三方 ChatGPT 登录客户端
/// 是官方容忍的自洽家族，不伪装成 codex。
pub fn apply_third_party(
    headers: &mut http::HeaderMap,
    persona: &Persona,
    access_token: &str,
    session_id: &str,
    thread_id: &str,
    now_unix_ms: i64,
) {
    apply(
        headers,
        &RewriteInput {
            persona,
            access_token,
            downstream_installation: None,
            inbound_turn_metadata: None,
        },
    );
    headers.insert("session-id", session_id.parse().unwrap());
    headers.insert("thread-id", thread_id.parse().unwrap());
    headers.insert(
        "x-codex-window-id",
        format!("{thread_id}:0").parse().unwrap(),
    );
    headers.insert("x-client-request-id", thread_id.parse().unwrap());
    headers.insert(
        "x-codex-turn-metadata",
        mint_turn_metadata(
            persona.installation_id.as_str(),
            session_id,
            thread_id,
            now_unix_ms,
        ),
    );
}

/// body 外科替换：把下游 installation id 的全部出现换成账号人设的。
/// 其余字节逐位不动。两者相同时原样返回。
pub fn surgical_installation_replace(
    body: bytes::Bytes,
    downstream_installation: Option<&str>,
    persona_installation: &str,
) -> bytes::Bytes {
    match downstream_installation {
        Some(down) if down != persona_installation && !down.is_empty() => {
            let text = String::from_utf8_lossy(&body);
            if text.contains(down) {
                bytes::Bytes::from(text.replace(down, persona_installation))
            } else {
                body
            }
        }
        _ => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn persona() -> Persona {
        Persona {
            account_id: "acc-1".into(),
            installation_id: "our-install-uuid".into(),
            version_pin: "0.154.0".into(),
            originator: "codex_cli_rs".into(),
            ua: "codex_cli_rs/0.154.0 (Ubuntu 22.4.0; x86_64) xterm-256color".into(),
            proxy_url: None,
        }
    }

    #[test]
    fn apply_replaces_only_account_level_identity() {
        let p = persona();
        let mut h = http::HeaderMap::new();
        h.insert("user-agent", "codex_cli_rs/9.9.9 (Mac OS 15; arm64) iTerm.app/3.5".parse().unwrap());
        h.insert("x-codex-turn-metadata", serde_json::json!({"installation_id":"their-install"}).to_string().parse().unwrap());
        h.insert("authorization", "Bearer downstream".parse().unwrap());
        h.insert("session-id", "their-real-session".parse().unwrap());
        h.insert("thread-id", "their-real-thread".parse().unwrap());
        h.insert("x-codex-window-id", "their-window".parse().unwrap());
        apply(
            &mut h,
            &RewriteInput {
                persona: &p,
                access_token: "at",
                downstream_installation: None,
                inbound_turn_metadata: None,
            },
        );

        // 账号级：替换
        assert_eq!(h.get("x-codex-installation-id").unwrap(), "our-install-uuid");
        assert_eq!(h.get("authorization").unwrap(), "Bearer at");
        assert_eq!(h.get("chatgpt-account-id").unwrap(), "acc-1");
        assert_eq!(
            h.get("user-agent").unwrap(),
            "codex_cli_rs/0.154.0 (Ubuntu 22.4.0; x86_64) xterm-256color"
        );
        // 客户端工件：透传
        assert_eq!(h.get("session-id").unwrap(), "their-real-session");
        assert_eq!(h.get("thread-id").unwrap(), "their-real-thread");
        assert_eq!(h.get("x-codex-window-id").unwrap(), "their-window");
    }

    #[test]
    fn strip_removes_credentials_and_hop_by_hop_keeps_client_artifacts() {
        let mut h = http::HeaderMap::new();
        h.insert("host", "relay.internal".parse().unwrap());
        h.insert("connection", "keep-alive".parse().unwrap());
        h.insert("authorization", "Bearer sk-downstream".parse().unwrap());
        h.insert("x-codex-installation-id", "their-install".parse().unwrap());
        h.insert("x-codex-turn-metadata", "{}".parse().unwrap());
        h.insert("session-id", "their-session".parse().unwrap());
        h.insert("x-custom-keep", "keepme".parse().unwrap());
        strip_inbound(&mut h);
        for gone in ["host", "connection", "authorization", "x-codex-installation-id"] {
            assert!(h.get(gone).is_none(), "{gone} 应被剥离");
        }
        // 客户端工件保留
        assert!(h.get("x-codex-turn-metadata").is_some());
        assert_eq!(h.get("session-id").unwrap(), "their-session");
        assert_eq!(h.get("x-custom-keep").unwrap(), "keepme");
    }

    #[test]
    fn turn_metadata_only_installation_replaced() {
        let raw = serde_json::json!({
            "installation_id": "their-install",
            "session_id": "their-session",
            "thread_id": "their-thread",
            "window_id": "their-window",
            "turn_id": "0198-uuid-turn",
            "turn_started_at_unix_ms": 12345,
            "sandbox": "workspace-write",
            "thread_source": "user"
        });
        let hv = http::HeaderValue::from_str(&raw.to_string()).unwrap();
        let out = turn_metadata_with_installation(&hv, "our-install").unwrap();
        let v: serde_json::Value = serde_json::from_str(out.to_str().unwrap()).unwrap();
        assert_eq!(v["installation_id"], "our-install");
        // 其余全部保留——包括时戳与 id（真实工件）
        assert_eq!(v["session_id"], "their-session");
        assert_eq!(v["thread_id"], "their-thread");
        assert_eq!(v["window_id"], "their-window");
        assert_eq!(v["turn_id"], "0198-uuid-turn");
        assert_eq!(v["turn_started_at_unix_ms"], 12345);
        assert_eq!(v["sandbox"], "workspace-write");
        assert_eq!(v["thread_source"], "user");
    }

    #[test]
    fn surgical_replace_swaps_all_installation_projections() {
        let downstream = "11111111-2222-3333-4444-555555555555";
        let ours = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let body = format!(
            r#"{{"client_metadata":{{"installation_id":"{downstream}","x-codex-installation-id":"{downstream}","session_id":"s"}},"input":[]}}"#
        );
        let out = surgical_installation_replace(
            bytes::Bytes::from(body.clone()),
            Some(downstream),
            ours,
        );
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains(downstream));
        assert_eq!(s.matches(ours).count(), 2);
        // 其余内容逐位不动
        assert!(s.contains(r#""session_id":"s""#));
    }

    #[test]
    fn surgical_replace_noop_when_same_or_absent() {
        let body = bytes::Bytes::from(r#"{"a":1}"#);
        let p = persona();
        let out = surgical_installation_replace(
            body.clone(),
            Some(p.installation_id.as_str()),
            p.installation_id.as_str(),
        );
        assert_eq!(out, body);
        let out = surgical_installation_replace(body.clone(), None, p.installation_id.as_str());
        assert_eq!(out, body);
    }

    #[test]
    fn no_header_name_collisions() {
        let p = persona();
        let mut h = http::HeaderMap::new();
        apply(
            &mut h,
            &RewriteInput { persona: &p, access_token: "at", downstream_installation: None, inbound_turn_metadata: None },
        );
        let names: HashSet<_> = h.keys().map(|k| k.as_str().to_string()).collect();
        assert_eq!(names.len(), h.keys_len());
    }

    #[test]
    fn inbound_turn_metadata_flows_through_with_installation_swapped() {
        let p = persona();
        let raw = serde_json::json!({
            "installation_id": "their-install",
            "session_id": "their-session",
            "turn_id": "0198-uuid-turn",
            "sandbox": "read-only"
        });
        let hv = http::HeaderValue::from_str(&raw.to_string()).unwrap();
        let mut h = http::HeaderMap::new();
        h.insert("x-codex-turn-metadata", hv.clone());
        let inbound = h.get("x-codex-turn-metadata").cloned();
        apply(
            &mut h,
            &RewriteInput {
                persona: &p,
                access_token: "at",
                downstream_installation: Some("their-install"),
                inbound_turn_metadata: inbound.as_ref(),
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(h.get("x-codex-turn-metadata").unwrap().to_str().unwrap())
                .unwrap();
        assert_eq!(v["installation_id"], "our-install-uuid");
        assert_eq!(v["session_id"], "their-session");
        assert_eq!(v["turn_id"], "0198-uuid-turn");
        assert_eq!(v["sandbox"], "read-only");
        // 下游未携带时不发明
        let mut h2 = http::HeaderMap::new();
        apply(
            &mut h2,
            &RewriteInput { persona: &p, access_token: "at", downstream_installation: None, inbound_turn_metadata: None },
        );
        assert!(h2.get("x-codex-turn-metadata").is_none());
    }
}
