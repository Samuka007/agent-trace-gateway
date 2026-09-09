//! claude-code harness: the dominant anthropic.messages client.
//! Session shapes (design §1, 7d evidence): ≥~2.1.22x JSON envelope
//! `metadata.user_id` string `{"device_id":…,"session_id":…}` (474k rows),
//! ≤2.1.114 legacy composite `user_<hex64>_account_<uuid>_session_<uuid36>`
//! (13k rows, also restores the account identity), plus the header mount
//! `x-claude-code-session-id` (declared by the anthropic protocol table).
use atg_protocol::session::{
    claude_code_legacy_parts, metadata_envelope_transform, metadata_user_id_session,
};
use serde_json::Value;

/// Envelope shape: metadata.user_id is a JSON string object carrying a
/// session_id key (device_id is the common first key — bonus, not required;
/// loose matching survives key-set drift across CC versions, design §6 R3).
fn envelope_shape(req: &Value) -> bool {
    let Some(raw) = req["metadata"]["user_id"].as_str() else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    parsed.get("session_id").and_then(|v| v.as_str()).is_some()
}

/// Legacy composite shape: the fixed user_<hex64>_account_…_session_… form.
fn legacy_shape(req: &Value) -> bool {
    req["metadata"]["user_id"]
        .as_str()
        .is_some_and(claude_code_legacy_parts_is_hit)
}

fn claude_code_legacy_parts_is_hit(raw: &str) -> bool {
    claude_code_legacy_parts(raw).is_some()
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

/// Session rule: three-form reader on metadata.user_id (envelope / legacy /
/// object), then the metadata-as-envelope form — exactly the two
/// pre-v0.3.0 anthropic table rows, in the same order.
fn session(req: &Value) -> Option<String> {
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

pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "claude-code",
    protocols: &["anthropic.messages"],
    identify: &[
        crate::Identifier {
            kind: crate::IdentKind::Body(envelope_shape),
            strength: 3,
        },
        crate::Identifier {
            kind: crate::IdentKind::Body(legacy_shape),
            strength: 3,
        },
        // Production-zero compatibility variants (design §1 rows 4/6) —
        // rare shapes, still attributed for coverage.
        crate::Identifier {
            kind: crate::IdentKind::Body(object_shape),
            strength: 3,
        },
        crate::Identifier {
            kind: crate::IdentKind::Body(metadata_envelope_shape),
            strength: 3,
        },
        crate::Identifier {
            kind: crate::IdentKind::UaPrefix("claude-cli/"),
            strength: 4,
        },
        crate::Identifier {
            kind: crate::IdentKind::HeaderPresent(crate::CC_SESSION_HEADER),
            strength: 4,
        },
    ],
    session: Some(session),
    session_header_mounts: &[],
    enrich: &[("cc_account", account)],
};
