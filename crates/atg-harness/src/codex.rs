//! codex harness: openai.responses client whose identity fingerprint lives
//! in the request body — `client_metadata` carrying x-codex-* keys (turn
//! metadata / installation id). Its session value needs no dialect rule:
//! client_metadata.session_id is already an openai.responses body mount.
// PANIC-AUDIT v0.3.8: serde_json Value key-index in the harness shape
// matchers is panic-free for the audited shapes (miss → Null; shapes are
// JSON objects) — the indexing_slicing lint is syntax-broad over
// Value::index. Tracked in the PanicAudit issue.
#![allow(clippy::indexing_slicing)]
use serde_json::Value;

/// Identity body fingerprint: any x-codex-* key inside client_metadata.
fn codex_metadata_shape(req: &Value) -> bool {
    req["client_metadata"]
        .as_object()
        .is_some_and(|cm| cm.keys().any(|k| k.starts_with("x-codex-")))
}

/// installation-id → langfuse.trace.metadata.codex_installation.
fn installation_id(req: &Value) -> Option<String> {
    req["client_metadata"]["x-codex-installation-id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "codex",
    protocols: &["openai.responses", "openai.live"],
    identity: &[
        crate::Identifier {
            kind: crate::IdentKind::Body(codex_metadata_shape),
            strength: 4,
            class: crate::EvidenceClass::Identity,
        },
        crate::Identifier {
            kind: crate::IdentKind::UaPrefix("codex"),
            strength: 4,
            class: crate::EvidenceClass::Identity,
        },
    ],
    dialects: &[],
    // No session mounts: cm.session_id is a protocol body mount already.
    session_header_mounts: &[],
    enrich: &[("codex_installation", installation_id)],
};
