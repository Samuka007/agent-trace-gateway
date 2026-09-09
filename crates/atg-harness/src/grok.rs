//! grok harness: header-level — `x-grok-conv-id` rides only the grok
//! route (openai.responses legality; a hit elsewhere is mimicry → anomaly
//! tag). The header mount is declared by the openai.responses protocol
//! table itself, so no harness session rule is needed.
pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "grok",
    protocols: &["openai.responses"],
    identify: &[crate::Identifier {
        kind: crate::IdentKind::HeaderPresent(crate::GROK_CONV_HEADER),
        strength: 5,
    }],
    session: None,
    session_header_mounts: &[],
    enrich: &[],
};
