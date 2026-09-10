//! omp harness (v0.3.2): this very gateway's client family — observed UA
//! `omp/18.x` across anthropic/openai egress. omp speaks the
//! CLAUDE-CODE DIALECT (it sends x-claude-code-session-id and CC-shaped
//! metadata when proxying anthropic messages) while keeping its own
//! identity: the production misattribution that motivated the
//! dialect/identity split (omp traffic tagged harness:claude-code).
//!
//! No own session mounts: the borrowed dialect's shapes + the protocol
//! tables' mounts carry the session (extraction unchanged).
pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "omp",
    // Multi-protocol egress: everything ATG speaks.
    protocols: &[
        "anthropic.messages",
        "openai.responses",
        "openai.chat_completions",
        "openai.live",
    ],
    identity: &[crate::Identifier {
        kind: crate::IdentKind::UaPrefix("omp/"),
        strength: 4,
        class: crate::EvidenceClass::Identity,
    }],
    // Borrows the claude-code dialect (session carriage), identity stays omp.
    dialects: &["claude-code"],
    session_header_mounts: &[],
    enrich: &[],
};
