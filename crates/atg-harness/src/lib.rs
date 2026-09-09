//! Harness layer: WHO fills the protocol layer's session mount points, and
//! in what shape. Four descriptors (claude-code, codex, grok, opencode) —
//! empirically grounded in `.tmp-atg-harness-layer-design.md` §1; observed
//! UA families beyond these (zcode/dsh/…) are deliberately not designed.
//!
//! Identification and session extraction are ORTHOGONAL (design §0):
//! - `identify` attributes a harness name from ordered evidence
//!   (body/header fingerprints ≥3 first, then UA prefix);
//! - session body rules run on SHAPE HITS (any matched body fingerprint),
//!   never on final attribution — classification failure never degrades
//!   session extraction (F2 red line).
//!
//! Legality of a (harness, protocol) pair is declared, not enforced: an
//! out-of-pair hit records `protocol_anomaly` (mimicry is noise, not an
//! error).
use atg_protocol::mounts;
use serde_json::Value;

pub mod claude_code;
pub mod codex;
pub mod grok;
pub mod opencode;
#[cfg(test)]
mod tests;

pub const UNKNOWN: &str = "unknown";

/// All registered harness descriptors; a new harness is one file + one line.
pub static HARNESSES: &[&HarnessDescriptor] = &[
    &claude_code::DESCRIPTOR,
    &codex::DESCRIPTOR,
    &grok::DESCRIPTOR,
    &opencode::DESCRIPTOR,
];

/// One identification evidence item.
#[derive(Clone, Copy)]
pub enum IdentKind {
    /// Request User-Agent starts with the prefix (ATG-canary-only signal:
    /// the main sub2api OTLP path records no UA).
    UaPrefix(&'static str),
    /// A named header mount is present and non-empty.
    HeaderPresent(&'static str),
    /// A harness-specific body shape test on the parsed request body.
    Body(fn(&Value) -> bool),
}

pub struct Identifier {
    pub kind: IdentKind,
    /// Evidence strength: fingerprints 3-5 outrank UA (4) only when both
    /// are shape-level; ordering is (strength desc, table order).
    pub strength: u8,
}

pub struct HarnessDescriptor {
    pub name: &'static str,
    /// Legal (harness, protocol) combinations — declaration, not enforcement.
    pub protocols: &'static [&'static str],
    pub identify: &'static [Identifier],
    /// Session value from the request body, harness-specific shape
    /// (claude-code envelope/legacy; None for header-only harnesses).
    pub session: Option<fn(&Value) -> Option<String>>,
    /// Session header mounts this harness reads (after the protocol's own
    /// header sources; e.g. opencode's x-session-* affinity family).
    pub session_header_mounts: &'static [&'static str],
    /// Extra identity facts: (metadata key, extractor) — emitted as
    /// langfuse.trace.metadata.<key> (cc_account, codex_installation).
    pub enrich: &'static [Enrich],
}

/// One enrichment fact: (metadata key, extractor from the request body).
pub type Enrich = (&'static str, fn(&Value) -> Option<String>);

/// Classification result: attribution facts (never affects extraction).
pub struct HarnessFacts {
    /// Winner by (strength desc, table order); UNKNOWN when no evidence.
    pub name: &'static str,
    /// All harnesses with at least one evidence hit at the winner's
    /// strength — a conflict signal (e.g. CC header + codex body).
    pub candidates: Vec<&'static str>,
    /// Winner outside its declared legal protocols.
    pub protocol_anomaly: bool,
    /// Harnesses whose BODY shape test matched — their session rules run
    /// regardless of the attribution winner (shape-gated extraction).
    pub body_shape_hits: Vec<&'static str>,
}

/// Attribute the harness from ordered evidence. `ua` is the User-Agent
/// header value (main path: usually None — no UA recorded there).
pub fn identify(
    protocol: &str,
    req: Option<&Value>,
    ua: Option<&str>,
    header_get: &dyn Fn(&str) -> Option<String>,
) -> HarnessFacts {
    struct Hit {
        name: &'static str,
        strength: u8,
        body: bool,
    }
    let mut hits: Vec<Hit> = Vec::new();
    for h in HARNESSES {
        let mut best: Option<u8> = None;
        let mut body = false;
        for id in h.identify {
            let matched = match id.kind {
                IdentKind::UaPrefix(p) => ua.is_some_and(|u| u.starts_with(p)),
                IdentKind::HeaderPresent(name) => {
                    header_get(name).is_some_and(|v| !v.trim().is_empty())
                }
                IdentKind::Body(test) => req.is_some_and(test),
            };
            if matched {
                if best.is_none_or(|s| id.strength > s) {
                    best = Some(id.strength);
                }
                if matches!(id.kind, IdentKind::Body(_)) {
                    body = true;
                }
            }
        }
        if let Some(strength) = best {
            hits.push(Hit {
                name: h.name,
                strength,
                body,
            });
        }
    }
    if hits.is_empty() {
        return HarnessFacts {
            name: UNKNOWN,
            candidates: Vec::new(),
            protocol_anomaly: false,
            body_shape_hits: Vec::new(),
        };
    }
    let top = hits.iter().map(|h| h.strength).max().unwrap();
    let candidates: Vec<&'static str> = hits
        .iter()
        .filter(|h| h.strength == top)
        .map(|h| h.name)
        .collect();
    // Winner: strongest, table order as the deterministic tiebreak.
    let winner = *candidates.first().unwrap_or(&UNKNOWN);
    let descriptor = HARNESSES.iter().find(|h| h.name == winner);
    let anomaly = match descriptor {
        Some(d) => !d.protocols.is_empty() && !d.protocols.contains(&protocol),
        None => false,
    };
    HarnessFacts {
        name: winner,
        candidates,
        protocol_anomaly: anomaly,
        body_shape_hits: hits.iter().filter(|h| h.body).map(|h| h.name).collect(),
    }
}

/// Harness session value from the request body: session rules of every
/// SHAPE-matched harness, strongest-first (extraction keyed on shape, not
/// attribution — classification failure never degrades session extraction).
pub fn session_from_body(facts: &HarnessFacts, req: &Value) -> Option<String> {
    for name in &facts.body_shape_hits {
        let d = HARNESSES.iter().find(|h| h.name == *name)?;
        if let Some(rule) = d.session {
            if let Some(sid) = rule(req) {
                return Some(sid);
            }
        }
    }
    None
}

/// Harness session header mounts (opencode family) — consulted after the
/// protocol's own header sources; gated on attribution (UA-level noise
/// control: an unidentified client cannot mint sessions from these).
pub fn session_from_headers(
    facts: &HarnessFacts,
    get: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let d = HARNESSES.iter().find(|h| h.name == facts.name)?;
    for mount in d.session_header_mounts {
        if let Some(v) = get(mount).filter(|s| !s.trim().is_empty()) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Enrichment facts for the ATTRIBUTED harness (account/installation ids):
/// (metadata key, value) pairs; empty when the attributed harness is
/// unknown or the shapes are absent.
pub fn enrich(facts: &HarnessFacts, req: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(d) = HARNESSES.iter().find(|h| h.name == facts.name) else {
        return out;
    };
    for (key, extract) in d.enrich {
        if let Some(v) = extract(req) {
            out.push(((*key).to_string(), v));
        }
    }
    out
}

/// Convenience: the claude-code session header mount name (referenced by
/// the anthropic protocol table already — kept here for symmetry/tests).
pub const CC_SESSION_HEADER: &str = mounts::HDR_CC_SESSION;
pub const GROK_CONV_HEADER: &str = mounts::HDR_GROK_CONV;
