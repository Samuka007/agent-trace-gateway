//! 账号人设：导入时铸造一次、此后不再变的一组静态身份常量。
//!
//! 纪律：body 是下游客户端自己的；本模块只描述"账号壳"。壳除了版本
//! 升级外几乎不动，动的越少，错的越少。UA 模板按真实 codex CLI 的
//! Linux 形态拼装，OS/版本逐号可配（错峰升级由导入参数控制）。

use serde::{Deserialize, Serialize};

pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
pub const DEFAULT_VERSION_PIN: &str = "0.153.4";
pub const DEFAULT_OS_DESC: &str = "Ubuntu 22.04";
pub const DEFAULT_ARCH: &str = "x86_64";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub account_id: String,
    pub installation_id: String,
    pub version_pin: String,
    pub os_desc: String,
    pub arch: String,
    pub originator: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
}

impl Persona {
    /// 导入时调用一次：installation 用 uuid v4 随机铸造，此后与账号同生命周期。
    pub fn mint(
        account_id: &str,
        version_pin: Option<String>,
        os_desc: Option<String>,
        arch: Option<String>,
        proxy_url: Option<String>,
    ) -> Self {
        Self {
            account_id: account_id.to_string(),
            installation_id: uuid::Uuid::new_v4().to_string(),
            version_pin: version_pin.unwrap_or_else(|| DEFAULT_VERSION_PIN.to_string()),
            os_desc: os_desc.unwrap_or_else(|| DEFAULT_OS_DESC.to_string()),
            arch: arch.unwrap_or_else(|| DEFAULT_ARCH.to_string()),
            originator: DEFAULT_ORIGINATOR.to_string(),
            proxy_url,
        }
    }

    /// codex CLI Linux 形态：`codex-tui/<ver> (<os>; <arch>) xterm-256color`。
    /// 注意不要模仿已知错误签名（如 "Ubuntu 22.4.0"）。
    pub fn user_agent(&self) -> String {
        format!(
            "codex-tui/{} ({}; {}) xterm-256color",
            self.version_pin, self.os_desc, self.arch
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_matches_real_codex_shape() {
        let p = Persona::mint("acc", None, None, None, None);
        assert_eq!(
            p.user_agent(),
            "codex-tui/0.153.4 (Ubuntu 22.04; x86_64) xterm-256color"
        );
    }

    #[test]
    fn installation_is_unique_per_mint() {
        let a = Persona::mint("acc", None, None, None, None);
        let b = Persona::mint("acc", None, None, None, None);
        assert_ne!(a.installation_id, b.installation_id);
    }
}
