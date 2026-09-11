//! atcd — 最简账号池反代原型。
//!
//! 职责只有六件：
//! 1. Responses 透传代理：下游仅限 codex CLI（custom provider 指向本进程），
//!    body 字节不动，只换身份头；
//! 2. 会话绑定表（sqlite）：下游会话键 → 账号 + 本进程签发的身份，粘住不换号；
//! 3. 放置调度：新会话选号（健康/容量/LRU），放置后不再迁移；
//! 4. 节奏治理：每账号并发上限 + 相邻回合最小间隔（带抖动）；
//! 5. 凭据刷新：与 codex CLI 逐字一致的 refresh grant（见 `refresh` 模块说明）；
//! 6. 账号导入：refresh_token + account_id 进，铸造一次性人设出。
//!
//! 关于直接引用 codex crate 的决策：`codex-login` 的公开刷新入口是
//! `AuthManager`，它绑定 codex 的存储体系（auth.json / keyring 三种 store
//! mode）；采用它意味着接受其存储层，与本进程的 sqlite 单一状态源冲突。
//! 其依赖网（codex-protocol、otel、keyring、webbrowser）远超原型所需。
//! 因此本原型只引用 codex 的 wire 常量与请求形状（`refresh` 模块，逐字
//! 取自 codex-rs/login/src/auth/manager.rs），日后如需共享维护，切换到
//! `codex-login::AuthManager` 是一个孤立改动点。
pub mod oauth;
pub mod persona;
pub mod placement;
pub mod proxy;
pub mod refresh;
pub mod rewrite;
pub mod scheduler;
pub mod store;
pub mod ws;
