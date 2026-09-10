//! 出站身份改写："换证件"。
//!
//! 原则（与代码结构一致，不是口号）：
//! - body 永不经过本模块——body 字节不动是结构保证；
//! - 只写有证据的头：installation/session/thread/window/turn 元数据、
//!   originator、UA、authorization、chatgpt-account-id；
//! - turn 元数据里的身份字段替换、时间戳取真实发送时刻（服务端看得到
//!   到达时间，编造会在对照下暴露）；inbound 没有的字段不发明；
//! - 其余 x-codex-* 头（如 beta 特性标记）是客户端真实特征，透传。

use crate::atcd::persona::Persona;

/// 入站需要剥离的头：逐跳头 + 会暴露下游/上游混杂身份的头。
/// authorization / chatgpt-account-id / 身份族由 `apply` 重新写入。
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
        "session-id",
        "session_id",
        "thread-id",
        "x-codex-installation-id",
        "x-codex-window-id",
        "x-client-request-id",
        "x-codex-turn-metadata",
        "originator",
        "user-agent",
    ];
    for name in STRIP {
        headers.remove(*name);
    }
}

pub struct RewriteInput<'a> {
    pub persona: &'a Persona,
    /// 本进程签发、绑定表持有的会话身份（非下游原值）。
    pub session_id: &'a str,
    pub thread_id: &'a str,
    pub access_token: &'a str,
    /// 真实发送时刻（unix 毫秒）。
    pub now_unix_ms: i64,
    /// 剥离前捕获的入站 turn 元数据（None = 下游未携带，不发明）。
    pub inbound_turn_metadata: Option<&'a http::HeaderValue>,
}

pub fn apply(headers: &mut http::HeaderMap, input: &RewriteInput<'_>) {
    let p = input.persona;
    headers.insert("session-id", input.session_id.parse().unwrap());
    if let Ok(v) = input.session_id.parse() {
        headers.insert("session_id", v); // 兼容下划线变体的读取方
    }
    headers.insert("thread-id", input.thread_id.parse().unwrap());
    headers.insert(
        "x-codex-window-id",
        format!("{}:0", input.thread_id).parse().unwrap(),
    );
    headers.insert(
        "x-codex-installation-id",
        p.installation_id.parse().unwrap(),
    );
    headers.insert("x-client-request-id", input.thread_id.parse().unwrap());
    headers.insert("originator", p.originator.parse().unwrap());
    headers.insert("user-agent", p.user_agent().parse().unwrap());
    headers.insert(
        "authorization",
        format!("Bearer {}", input.access_token).parse().unwrap(),
    );
    headers.insert("chatgpt-account-id", p.account_id.parse().unwrap());

    if let Some(out) = rewrite_turn_metadata(
        input.inbound_turn_metadata,
        input.session_id,
        input.thread_id,
        p.installation_id.as_str(),
        input.now_unix_ms,
    ) {
        headers.insert("x-codex-turn-metadata", out);
    }
}

/// 就地改写 x-codex-turn-metadata 的身份字段。
/// 独立成函数以便单测：保留 inbound 携带的非身份字段（sandbox、
/// thread_source、turn_id 等），只换身份四元组 + 时间戳。
pub fn rewrite_turn_metadata(
    raw: Option<&http::HeaderValue>,
    session_id: &str,
    thread_id: &str,
    installation_id: &str,
    now_unix_ms: i64,
) -> Option<http::HeaderValue> {
    let raw = raw?;
    let bytes = raw.as_bytes();
    let mut v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let obj = v.as_object_mut()?;
    obj.insert("installation_id".into(), installation_id.into());
    obj.insert("session_id".into(), session_id.into());
    obj.insert("thread_id".into(), thread_id.into());
    obj.insert("window_id".into(), format!("{thread_id}:0").into());
    obj.insert("turn_started_at_unix_ms".into(), now_unix_ms.into());
    let out = serde_json::to_string(&v).ok()?;
    http::HeaderValue::from_str(&out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn persona() -> Persona {
        Persona::mint("acc-1", Some("0.154.0".into()), None, None, None)
    }

    fn input<'a>(p: &'a Persona, at: &'a str) -> RewriteInput<'a> {
        RewriteInput {
            persona: p,
            session_id: "our-session",
            thread_id: "our-thread",
            access_token: at,
            now_unix_ms: 1_700_000_000_000,
            inbound_turn_metadata: None,
        }
    }

    #[test]
    fn apply_sets_full_identity_header_set() {
        let p = persona();
        let mut h = http::HeaderMap::new();
        h.insert("user-agent", "codex-tui/9.9.9 (Mac; arm64)".parse().unwrap());
        h.insert("authorization", "Bearer downstream".parse().unwrap());
        apply(&mut h, &input(&p, "at"));

        assert_eq!(h.get("session-id").unwrap(), "our-session");
        assert_eq!(h.get("thread-id").unwrap(), "our-thread");
        assert_eq!(h.get("x-codex-window-id").unwrap(), "our-thread:0");
        assert_eq!(h.get("x-codex-installation-id").unwrap(), p.installation_id.as_str());
        assert_eq!(h.get("x-client-request-id").unwrap(), "our-thread");
        assert_eq!(h.get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(
            h.get("user-agent").unwrap(),
            "codex-tui/0.154.0 (Ubuntu 22.04; x86_64) xterm-256color"
        );
        assert_eq!(h.get("authorization").unwrap(), "Bearer at");
        assert_eq!(h.get("chatgpt-account-id").unwrap(), "acc-1");
    }

    #[test]
    fn strip_removes_downstream_identity_and_hop_by_hop() {
        let mut h = http::HeaderMap::new();
        h.insert("host", "relay.internal".parse().unwrap());
        h.insert("connection", "keep-alive".parse().unwrap());
        h.insert("authorization", "Bearer sk-downstream".parse().unwrap());
        h.insert("session-id", "their-session".parse().unwrap());
        h.insert("x-codex-installation-id", "their-install".parse().unwrap());
        h.insert("x-custom-keep", "keepme".parse().unwrap());
        strip_inbound(&mut h);
        for gone in ["host", "connection", "authorization", "session-id", "x-codex-installation-id"] {
            assert!(h.get(gone).is_none(), "{gone} 应被剥离");
        }
        assert_eq!(h.get("x-custom-keep").unwrap(), "keepme");
    }

    #[test]
    fn turn_metadata_identity_replaced_content_preserved() {
        let raw = serde_json::json!({
            "installation_id": "their-install",
            "session_id": "their-session",
            "thread_id": "their-thread",
            "window_id": "their-thread:3",
            "turn_id": "0198-UUID",
            "turn_started_at_unix_ms": 1,
            "sandbox": "workspace-write",
            "thread_source": "user"
        });
        let hv = http::HeaderValue::from_str(&raw.to_string()).unwrap();
        let out = rewrite_turn_metadata(Some(&hv), "s", "t", "i", 42).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.to_str().unwrap()).unwrap();
        assert_eq!(v["installation_id"], "i");
        assert_eq!(v["session_id"], "s");
        assert_eq!(v["thread_id"], "t");
        assert_eq!(v["window_id"], "t:0");
        assert_eq!(v["turn_started_at_unix_ms"], 42);
        // 非身份字段保留
        assert_eq!(v["turn_id"], "0198-UUID");
        assert_eq!(v["sandbox"], "workspace-write");
        assert_eq!(v["thread_source"], "user");
    }

    #[test]
    fn turn_metadata_garbage_returns_none() {
        let hv = http::HeaderValue::from_str("not json").unwrap();
        assert!(rewrite_turn_metadata(Some(&hv), "s", "t", "i", 1).is_none());
        assert!(rewrite_turn_metadata(None, "s", "t", "i", 1).is_none());
    }

    #[test]
    fn no_header_name_collisions() {
        let p = persona();
        let mut h = http::HeaderMap::new();
        apply(&mut h, &input(&p, "at"));
        let names: HashSet<_> = h.keys().map(|k| k.as_str().to_string()).collect();
        assert_eq!(names.len(), h.keys_len());
    }

    #[test]
    fn inbound_turn_metadata_flows_through_apply() {
        let p = persona();
        let raw = serde_json::json!({
            "installation_id": "their-install",
            "session_id": "their-session",
            "thread_id": "their-thread",
            "turn_id": "turn-9",
            "sandbox": "read-only"
        });
        let hv = http::HeaderValue::from_str(&raw.to_string()).unwrap();
        let mut inp = input(&p, "at");
        inp.inbound_turn_metadata = Some(&hv);
        let mut h = http::HeaderMap::new();
        apply(&mut h, &inp);
        let v: serde_json::Value =
            serde_json::from_str(h.get("x-codex-turn-metadata").unwrap().to_str().unwrap())
                .unwrap();
        assert_eq!(v["session_id"], "our-session");
        assert_eq!(v["installation_id"], p.installation_id);
        assert_eq!(v["turn_id"], "turn-9");
        assert_eq!(v["sandbox"], "read-only");
        // 下游未携带时不发明
        let mut h2 = http::HeaderMap::new();
        apply(&mut h2, &input(&p, "at"));
        assert!(h2.get("x-codex-turn-metadata").is_none());
    }
}
