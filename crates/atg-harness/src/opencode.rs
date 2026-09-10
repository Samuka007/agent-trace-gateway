//! opencode harness: identity via UA (`opencode/1.x ai-sdk/…`) or its own
//! session header; the x-session-* affinity family
//! (openai_gateway_scheduling.go:22-36) stays identity-gated — an
//! unidentified client cannot mint sessions from these headers.
use atg_protocol::mounts;

pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "opencode",
    protocols: &[
        "openai.responses",
        "openai.chat_completions",
        "anthropic.messages",
    ],
    identity: &[
        crate::Identifier {
            kind: crate::IdentKind::UaPrefix("opencode/"),
            strength: 4,
            class: crate::EvidenceClass::Identity,
        },
        crate::Identifier {
            kind: crate::IdentKind::HeaderPresent(mounts::HDR_OPENCODE_SESSION),
            strength: 4,
            class: crate::EvidenceClass::Identity,
        },
    ],
    dialects: &[],
    session_header_mounts: &[
        mounts::HDR_SESSION,
        mounts::HDR_SESSION_AFFINITY,
        mounts::HDR_OPENCODE_SESSION,
        mounts::HDR_CONVERSATION_ID,
    ],
    enrich: &[],
};
