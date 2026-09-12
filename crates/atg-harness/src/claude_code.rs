//! claude-code harness: the dominant anthropic.messages client — and the
//! owner of the claude-code DIALECT (envelope/legacy/object/metadata-
//! envelope body shapes + the CC session header), which other harnesses
//! (omp) borrow while keeping their own identity.
//!
//! Identity evidence is TIGHT (v0.3.2 user ruling): the claude-cli UA, or
//! the legacy composite user_id shape (a real CC ≤2.1.114 fingerprint)
//! with no conflicting UA. The modern JSON envelope and the bare CC header
//! are DIALECT shapes only — hitting them without identity evidence yields
//! the downgraded "claude-code-compatible" assertion, not the identity.
// PANIC-AUDIT v0.3.8: serde_json Value key-index in the harness shape
// matchers is panic-free for the audited shapes (miss → Null; shapes are
// JSON objects) — the indexing_slicing lint is syntax-broad over
// Value::index. Tracked in the PanicAudit issue.
#![allow(clippy::indexing_slicing)]
use atg_protocol::session::{
    claude_code_legacy_parts, metadata_envelope_transform, metadata_user_id_session,
};
use serde_json::Value;

/// Envelope shape: metadata.user_id is a JSON string object carrying a
/// session_id key (device_id is the common first key — bonus, not required;
/// loose matching survives key-set drift across CC versions, design §6 R3).
pub(crate) fn envelope_shape(req: &Value) -> bool {
    let Some(raw) = req["metadata"]["user_id"].as_str() else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    parsed.get("session_id").and_then(|v| v.as_str()).is_some()
}

/// Legacy composite shape: the fixed user_<hex64>_account_…_session_… form.
pub(crate) fn legacy_shape(req: &Value) -> bool {
    req["metadata"]["user_id"]
        .as_str()
        .is_some_and(|raw| claude_code_legacy_parts(raw).is_some())
}

/// user_id object form {session_id}: a production-zero compatibility
/// variant of the envelope (design §1 row 6 — kept for shape drift).
fn object_shape(req: &Value) -> bool {
    req["metadata"]["user_id"]
        .as_object()
        .is_some_and(|o| o.contains_key("session_id"))
}

/// metadata itself as a JSON envelope string {"user_id": …} — a real
/// pre-v0.3.0 traffic form; same priority as the old table row.
fn metadata_envelope_shape(req: &Value) -> bool {
    req["metadata"].as_str().is_some_and(|raw| {
        serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| v.get("user_id").map(|u| u.as_str().is_some()))
            .unwrap_or(false)
    })
}

/// Dialect session rule: three-form reader on metadata.user_id (envelope /
/// legacy / object), then the metadata-as-envelope form — exactly the two
/// pre-v0.3.0 anthropic table rows, in the same order. Runs for ANY
/// claude-code-dialect traffic (CC itself, omp, or -compatible).
fn dialect_session(req: &Value) -> Option<String> {
    if let Some(raw) = req["metadata"]["user_id"].as_str() {
        let parsed: Value = serde_json::from_str(raw).unwrap_or(Value::String(raw.to_string()));
        if let Some(sid) = metadata_user_id_session(&parsed) {
            return Some(sid);
        }
    }
    if let Some(raw) = req["metadata"].as_str() {
        return metadata_envelope_transform(raw);
    }
    req["metadata"]["user_id"]
        .as_object()
        .and_then(|o| o.get("session_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Legacy account segment → langfuse.trace.metadata.cc_account (never
/// overwrites langfuse.user.id; the envelope form carries no account).
fn account(req: &Value) -> Option<String> {
    let raw = req["metadata"]["user_id"].as_str()?;
    claude_code_legacy_parts(raw).and_then(|(_, account)| account)
}

/// The claude-code session-carrying dialect.
pub static DIALECT: crate::Dialect = crate::Dialect {
    name: "claude-code",
    body_shapes: &[
        envelope_shape,
        legacy_shape,
        object_shape,
        metadata_envelope_shape,
    ],
    header_shapes: &[crate::CC_SESSION_HEADER],
    session_body: Some(dialect_session),
    owner: "claude-code",
};

pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "claude-code",
    protocols: &["anthropic.messages"],
    // Identity-exclusive evidence ONLY (v0.3.2 tightening): the UA, or the
    // legacy composite (CC ≤2.1.114 fingerprint). Envelope/header hits are
    // dialect shapes — see DIALECT and the -compatible downgrade.
    identity: &[
        crate::Identifier {
            kind: crate::IdentKind::Body(legacy_shape),
            strength: 3,
            class: crate::EvidenceClass::Identity,
        },
        crate::Identifier {
            kind: crate::IdentKind::UaPrefix("claude-cli/"),
            strength: 4,
            class: crate::EvidenceClass::Identity,
        },
    ],
    dialects: &["claude-code"],
    session_header_mounts: &[],
    enrich: &[("cc_account", account)],
};
