//! codex harness: openai.responses client whose fingerprint lives in the
//! request body — `client_metadata` carrying x-codex-* keys (turn
//! metadata / installation id). Its session value needs no harness rule:
//! client_metadata.session_id is already an openai.responses body mount
//! (pck ≡ cm in the observed fleet, design §1).
use serde_json::Value;

/// Body fingerprint: any x-codex-* key inside client_metadata.
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
    identify: &[
        crate::Identifier {
            kind: crate::IdentKind::Body(codex_metadata_shape),
            strength: 4,
        },
        crate::Identifier {
            kind: crate::IdentKind::UaPrefix("codex"),
            strength: 4,
        },
    ],
    // No body session rule: cm.session_id is a protocol mount already.
    session: None,
    session_header_mounts: &[],
    enrich: &[("codex_installation", installation_id)],
};
