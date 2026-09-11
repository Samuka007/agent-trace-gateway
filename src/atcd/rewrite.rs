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

use sha2::Digest as _;

use codex_core::responses_metadata::{CodexResponsesMetadata, CodexResponsesRequestKind};
use codex_protocol::protocol::ThreadSource;

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
        // WS 升级工件（WS 桥复用本函数剥入站；上游握手自生成）。
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-protocol",
        "sec-websocket-extensions",
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

/// 语义映射参数：字段的"值从哪里来、何时变、何时不变"。
/// 字段正确性（名字/类型/序列化）由 codex 的 CodexResponsesMetadata 保证；
/// 语义正确性（值的来源与生命周期）由这里的映射规则声明：
/// - installation_id：账号人设，铸造后永不变
/// - session_id/thread_id：绑定创建时铸造（v7），对话期内不变
/// - root_turn_id：对话首轮 turn_id，此后锚定不变
/// - context_window_id：窗口期内稳定，压缩事件后才换新
/// - turn_id：每轮新 v7；turn_started_at_unix_ms：真实发送时刻
/// - agent_name：会话工作目录的人设呈现（按会话键确定性选取）
pub struct TurnIdentity<'a> {
    pub installation_id: &'a str,
    pub session_id: &'a str,
    pub thread_id: &'a str,
    pub root_turn_id: &'a str,
    pub context_window_id: &'a str,
    pub agent_name: &'a str,
    pub sandbox: &'a str,
    pub sandbox_mode: &'a str,
}

pub const DEFAULT_AGENT_NAME: &str = "/root";
pub const DEFAULT_SANDBOX: &str = "seccomp";
pub const DEFAULT_SANDBOX_MODE: &str = "read-only";

/// 会话工作目录人设池：按会话键确定性选取，模拟不同项目目录。
pub fn pick_agent_name(session_key: &str) -> &'static str {
    const POOL: &[&str] = &["/root", "/home/dev/work", "/home/dev/proj", "/srv/app"];
    let h = sha2::Sha256::digest(session_key.as_bytes());
    let idx = (h[0] as usize) % POOL.len();
    POOL[idx]
}

/// 用 codex 自身结构体铸造 turn 元数据（字段正确性 by construction）。
pub fn mint_metadata(id: &TurnIdentity<'_>, now_unix_ms: i64) -> CodexResponsesMetadata {
    let turn_id = uuid::Uuid::now_v7().to_string();
    let mut m = CodexResponsesMetadata::new(
        id.installation_id.to_string(),
        id.session_id.to_string(),
        id.thread_id.to_string(),
        format!("{}:0", id.thread_id),
    );
    m.turn_id = Some(turn_id.clone());
    m.root_turn_id = Some(id.root_turn_id.to_string());
    m.context_window_id = uuid::Uuid::parse_str(id.context_window_id).ok();
    m.agent_name = Some(id.agent_name.to_string());
    m.request_kind = Some(CodexResponsesRequestKind::Turn);
    m.thread_source = Some(ThreadSource::User);
    m.sandbox = Some(id.sandbox.to_string());
    m.sandbox_mode = Some(id.sandbox_mode.to_string());
    m.auto_review_enabled = Some(false);
    m.node_repl_auto_review_required = Some(false);
    m.node_repl_disabled = Some(false);
    m.window_number = Some(0);
    m.turn_started_at_unix_ms = Some(now_unix_ms);
    m
}

/// 第三方客户端的完整身份头：铸造的身份树 + 账号人设壳。
/// instructions/工具表保留客户端自己的——第三方 ChatGPT 登录客户端
/// 是官方容忍的自洽家族，不伪装成 codex。
pub fn apply_third_party(
    headers: &mut http::HeaderMap,
    persona: &Persona,
    access_token: &str,
    meta: &CodexResponsesMetadata,
    session_id: &str,
) {
    // 真实捕获（codex_exec 0.153.4）中 session_id == thread_id，
    // 因此铸造时二者同值，调用方只需给一个。
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
    headers.insert("thread-id", meta.thread_id.parse().unwrap());
    headers.insert("x-codex-window-id", meta.window_id.parse().unwrap());
    headers.insert("x-client-request-id", meta.thread_id.parse().unwrap());
    if let Some(json) = meta.turn_metadata_json() {
        headers.insert("x-codex-turn-metadata", json.parse().unwrap());
    }
}

/// codex body 信封的字段顺序（取自金样本 codex_exec_0.153.4）：
/// model, instructions, input, tools, tool_choice, parallel_tool_calls,
/// store, stream, include, prompt_cache_key, text, client_metadata。
const CODEX_BODY_FIELD_ORDER: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "store",
    "stream",
    "include",
    "prompt_cache_key",
    "text",
    "client_metadata",
];

/// 第三方 responses 客户端的 body 源信息写入：prompt_cache_key（= 铸造
/// session，对齐"cache key 派生自 session"的真实不变量）与 client_metadata
/// 身份投影。范围严格限于 D6 认定的源信息——store/include/
/// parallel_tool_calls 等请求参数属内容平面，原样透传不补齐。
pub fn codex_envelope_body(
    body: &[u8],
    client_metadata: &std::collections::HashMap<String, String>,
) -> Option<bytes::Bytes> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    if let Some(sid) = client_metadata.get("session_id") {
        obj.insert(
            "prompt_cache_key".into(),
            serde_json::Value::String(sid.clone()),
        );
    }
    let mut cm = serde_json::Map::new();
    for (k, v) in client_metadata {
        cm.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    obj.insert("client_metadata".into(), serde_json::Value::Object(cm));

    // 键序重排：codex 字段序在前，其余未知字段按原相对顺序跟在后面。
    let mut ordered = serde_json::Map::new();
    for key in CODEX_BODY_FIELD_ORDER {
        if let Some(val) = obj.remove(*key) {
            ordered.insert((*key).to_string(), val);
        }
    }
    for (k, val) in obj.iter() {
        ordered.insert(k.clone(), val.clone());
    }
    let out = serde_json::to_vec(&serde_json::Value::Object(ordered)).ok()?;
    Some(bytes::Bytes::from(out))
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
        h.insert(
            "user-agent",
            "codex_cli_rs/9.9.9 (Mac OS 15; arm64) iTerm.app/3.5"
                .parse()
                .unwrap(),
        );
        h.insert(
            "x-codex-turn-metadata",
            serde_json::json!({"installation_id":"their-install"})
                .to_string()
                .parse()
                .unwrap(),
        );
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
        assert_eq!(
            h.get("x-codex-installation-id").unwrap(),
            "our-install-uuid"
        );
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
        for gone in [
            "host",
            "connection",
            "authorization",
            "x-codex-installation-id",
        ] {
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
        let out =
            surgical_installation_replace(bytes::Bytes::from(body.clone()), Some(downstream), ours);
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
    fn envelope_body_synthesizes_codex_shape() {
        let third_party_body = br#"{"model":"gpt-5.5","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],"prompt_cache_key":"their-key","tools":[{"type":"function","name":"run_cmd"}]}"#;
        // 生产路径：proxy 从 codex 的 CodexResponsesMetadata::client_metadata()
        // 取投影 map；单测直接构造同形 map（D6：源信息 = session 投影）。
        let cm: std::collections::HashMap<String, String> = [
            ("session_id".to_string(), "minted-session".to_string()),
            (
                "x-codex-installation-id".to_string(),
                "our-install-uuid".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let out = codex_envelope_body(&third_party_body[..], &cm).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // 源信息写入；参数类字段不碰（D6）
        assert_eq!(v["prompt_cache_key"], "minted-session");
        assert_eq!(
            v["client_metadata"]["x-codex-installation-id"],
            "our-install-uuid"
        );
        assert_eq!(v["client_metadata"]["session_id"], "minted-session");
        // 其余保留
        assert_eq!(v["model"], "gpt-5.5");
        assert_eq!(v["tools"][0]["name"], "run_cmd");
        // 键序对齐金样本
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let expect: Vec<&str> = CODEX_BODY_FIELD_ORDER
            .iter()
            .copied()
            .filter(|k| v.get(k).is_some())
            .collect();
        let known: Vec<&str> = keys
            .iter()
            .filter(|k| CODEX_BODY_FIELD_ORDER.contains(k))
            .copied()
            .collect();
        assert_eq!(known, expect);
    }

    #[test]
    fn envelope_body_none_on_garbage() {
        let raw = b"not json";
        // 纯函数契约：非 JSON 输入返回 None；透传是 proxy 调用方的
        // .unwrap_or(body) 职责，不在本函数。
        let cm: std::collections::HashMap<String, String> =
            [("session_id".to_string(), "s".to_string())]
                .into_iter()
                .collect();
        assert!(codex_envelope_body(&raw[..], &cm).is_none());
    }

    #[test]
    fn no_header_name_collisions() {
        let p = persona();
        let mut h = http::HeaderMap::new();
        apply(
            &mut h,
            &RewriteInput {
                persona: &p,
                access_token: "at",
                downstream_installation: None,
                inbound_turn_metadata: None,
            },
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
            &RewriteInput {
                persona: &p,
                access_token: "at",
                downstream_installation: None,
                inbound_turn_metadata: None,
            },
        );
        assert!(h2.get("x-codex-turn-metadata").is_none());
    }
}
