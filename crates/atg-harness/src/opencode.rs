//! opencode harness: UA-level client (`opencode/1.x ai-sdk/…`) whose
//! affinity family (X-Session-Id / X-Session-Affinity / X-Opencode-Session
//! / X-Conversation-ID, openai_gateway_scheduling.go:22-36) carried zero
//! session rows in the 7d window — declared here as harness-scoped header
//! mounts, consulted ONLY for opencode-attributed traffic (an unidentified
//! client cannot mint sessions from these headers).
pub static DESCRIPTOR: crate::HarnessDescriptor = crate::HarnessDescriptor {
    name: "opencode",
    protocols: &[
        "openai.responses",
        "openai.chat_completions",
        "anthropic.messages",
    ],
    identify: &[
        crate::Identifier {
            kind: crate::IdentKind::UaPrefix("opencode/"),
            strength: 4,
        },
        crate::Identifier {
            kind: crate::IdentKind::HeaderPresent(atg_protocol::mounts::HDR_OPENCODE_SESSION),
            strength: 4,
        },
    ],
    session: None,
    session_header_mounts: &[
        atg_protocol::mounts::HDR_SESSION,
        atg_protocol::mounts::HDR_SESSION_AFFINITY,
        atg_protocol::mounts::HDR_OPENCODE_SESSION,
        atg_protocol::mounts::HDR_CONVERSATION_ID,
    ],
    enrich: &[],
};
