//! grok harness: identity via its own header convention — x-grok-conv-id
//! rides only the grok route (openai.responses legality; a hit elsewhere
//! is mimicry → anomaly tag). The header mount is declared by the
//! openai.responses protocol table itself, so no dialect rule is needed.
pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "grok",
    protocols: &["openai.responses"],
    // grok's own convention — nobody else sends it, so it is identity
    // evidence (header-shaped, but exclusive to grok).
    identity: &[crate::Identifier {
        kind: crate::IdentKind::HeaderPresent(crate::GROK_CONV_HEADER),
        strength: 5,
        class: crate::EvidenceClass::Identity,
    }],
    dialects: &[],
    session_header_mounts: &[],
    enrich: &[],
};
