//! 账号人设：导入时铸造一次、此后不再变的一组静态身份常量。
//!
//! 复用原则（本模块存在的理由）：
//! - UA 的 OS 段由 `os_info`（codex 同款库）生成，终端段由
//!   `codex-terminal-detection`（直接引用 codex crate）生成，拼装格式
//!   逐字取自 codex-rs/login/src/auth/default_client.rs 的
//!   `get_codex_user_agent`：`{originator}/{ver} ({os_type} {os_version};
//!   {arch}) {terminal}`。**注意**：os_info 会把 Ubuntu 22.04 渲染成
//!   "22.4.0"（Semantic 显示）——真实 codex 就是这样，不要"纠正"它。
//! - 会话/线程/回合 id 由 uuid v7 生成，与 codex 的
//!   `ThreadId::new` / `SessionId::new` / turn_id 同一原语。
//! - installation_id 是 uuid v4（codex 同样如此），铸造一次不再变。
//!
//! 人设字段默认取本机真实值（os_info + 终端探测），导入时可逐号覆盖
//! 以实现多号错峰；未覆盖的字段一律如实。

use serde::{Deserialize, Serialize};

pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
pub const DEFAULT_VERSION_PIN: &str = "0.153.4";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub account_id: String,
    pub installation_id: String,
    pub version_pin: String,
    pub originator: String,
    /// 铸造时按 codex 格式拼装定稿的完整 UA。
    pub ua: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
}

/// 本机真实快照（os_info + 终端探测），导入时的默认人设来源。
pub fn local_host_snapshot() -> (String, String, String, String) {
    let info = os_info::get();
    let os_type = info.os_type().to_string();
    let os_version = info.version().to_string();
    let arch = info.architecture().unwrap_or("unknown").to_string();
    let terminal = codex_terminal_detection::user_agent();
    (os_type, os_version, arch, terminal)
}

/// codex 真实 UA 形状（default_client.rs:168-175 的格式串）：
/// `{originator}/{ver} ({os_type} {os_version}; {arch}) {terminal}`
pub fn assemble_user_agent(
    originator: &str,
    version_pin: &str,
    os_type: &str,
    os_version: &str,
    arch: &str,
    terminal: &str,
) -> String {
    format!("{originator}/{version_pin} ({os_type} {os_version}; {arch}) {terminal}")
}

impl Persona {
    /// 导入时调用一次：installation 铸造一次不再变；OS/终端默认取本机真实值。
    #[allow(clippy::too_many_arguments)]
    pub fn mint(
        account_id: &str,
        version_pin: Option<String>,
        os_type: Option<String>,
        os_version: Option<String>,
        arch: Option<String>,
        terminal: Option<String>,
        proxy_url: Option<String>,
    ) -> Self {
        let (host_os_type, host_os_version, host_arch, host_terminal) = local_host_snapshot();
        let ua = assemble_user_agent(
            DEFAULT_ORIGINATOR,
            version_pin.as_deref().unwrap_or(DEFAULT_VERSION_PIN),
            os_type.as_deref().unwrap_or(&host_os_type),
            os_version.as_deref().unwrap_or(&host_os_version),
            arch.as_deref().unwrap_or(&host_arch),
            terminal.as_deref().unwrap_or(&host_terminal),
        );
        Self {
            account_id: account_id.to_string(),
            installation_id: uuid::Uuid::new_v4().to_string(),
            version_pin: version_pin.unwrap_or_else(|| DEFAULT_VERSION_PIN.to_string()),
            originator: DEFAULT_ORIGINATOR.to_string(),
            ua,
            proxy_url,
        }
    }

    pub fn user_agent(&self) -> String {
        self.ua.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persona(ua: &str) -> Persona {
        Persona {
            account_id: "acc".into(),
            installation_id: uuid::Uuid::new_v4().to_string(),
            version_pin: "0.153.4".into(),
            originator: DEFAULT_ORIGINATOR.into(),
            ua: ua.into(),
            proxy_url: None,
        }
    }

    #[test]
    fn user_agent_matches_codex_format_string() {
        let p = persona("codex_cli_rs/0.153.4 (Ubuntu 22.4.0; x86_64) xterm-256color");
        assert_eq!(
            p.user_agent(),
            "codex_cli_rs/0.153.4 (Ubuntu 22.4.0; x86_64) xterm-256color"
        );
    }

    #[test]
    fn assembly_preserves_terminal_and_os_rendering() {
        // os_info 会把 Ubuntu 22.04 渲染成 "22.4.0"；终端 token 随真实终端变化。
        // 拼装函数对两者都原样保留，不做"纠正"。
        let ua = assemble_user_agent(
            "codex_cli_rs", "0.154.0", "Mac OS", "15.5.0", "aarch64", "iTerm.app/3.5.11",
        );
        assert_eq!(ua, "codex_cli_rs/0.154.0 (Mac OS 15.5.0; aarch64) iTerm.app/3.5.11");
    }

    #[test]
    fn installation_is_unique_per_mint() {
        let a = Persona::mint("acc", None, None, None, None, None, None);
        let b = Persona::mint("acc", None, None, None, None, None, None);
        assert_ne!(a.installation_id, b.installation_id);
    }

    #[test]
    fn local_snapshot_produces_nonempty_fields() {
        let (os_type, os_version, arch, terminal) = local_host_snapshot();
        assert!(!os_type.is_empty());
        assert!(!os_version.is_empty());
        assert!(!arch.is_empty());
        assert!(!terminal.is_empty());
    }
}
