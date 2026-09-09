//! Shared session/identity mount points: the header (and body) locations
//! harnesses and clients may fill. Descriptors declare which mounts their
//! protocol accepts, in priority order; the harness layer declares who
//! fills them and in what shape. Centralised here so the two layers never
//! drift on spelling.
pub const HDR_SESSION_ID: &str = "session-id";
pub const HDR_SESSION_ID_ALT: &str = "session_id";
pub const HDR_CC_SESSION: &str = "x-claude-code-session-id";
pub const HDR_GROK_CONV: &str = "x-grok-conv-id";
