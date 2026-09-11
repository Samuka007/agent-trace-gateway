//! Harness layer: WHO fills the protocol layer's session mount points, and
//! in what shape — with DIALECT and IDENTITY decoupled (two-tier
//! attribution, v0.3.2):
//!
//! - **Identity** (metadata.harness, tags harness:*): WHO is sending —
//!   attributed from identity-exclusive evidence only (UA prefixes,
//!   codex's x-codex-* body fingerprint, claude-code's legacy composite).
//! - **Dialect** (metadata.dialect): HOW sessions are carried — the named
//!   session-extraction rule set whose shapes matched (claude-code
//!   envelope/legacy/header family). Borrowed dialects are the norm: omp
//!   speaks the claude-code dialect while remaining harness=omp.
//! - No identity evidence but dialect shapes matched → downgraded
//!   "<dialect>-compatible" assertion (not the real thing).
//!
//! Session extraction stays keyed on SHAPES (dialect), never on
//! attribution — classification failure never degrades extraction (F2 red
//! line). Legality of an (identity, protocol) pair is declared, not
//! enforced: out-of-pair hits record `protocol_anomaly`.
use atg_protocol::mounts;
use serde_json::Value;

pub mod claude_code;
pub mod codex;
pub mod grok;
pub mod omp;
pub mod opencode;
#[cfg(test)]
mod tests;

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

impl IdentKind {
    /// UA prefixes match case-insensitively (client SDKs vary "omp/" vs
    /// "OMP/" — identity evidence must not hinge on casing).
    fn ua_matches(prefix: &str, ua: Option<&str>) -> bool {
        ua.is_some_and(|u| {
            u.len() >= prefix.len() && u[..prefix.len()].eq_ignore_ascii_case(prefix)
        })
    }
}

/// Evidence semantics: IDENTITY items attribute the sender and outrank
/// every SHAPE item regardless of strength; SHAPE items only signal a
/// dialect (how sessions are carried), never an identity on their own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EvidenceClass {
    Identity,
    Shape,
}

pub struct Identifier {
    pub kind: IdentKind,
    /// Evidence strength WITHIN its class (ordering is class first, then
    /// strength desc, then table order).
    pub strength: u8,
    pub class: EvidenceClass,
}

/// A named session-extraction rule set (the "how sessions are carried").
pub struct Dialect {
    pub name: &'static str,
    /// Body shape tests signalling this dialect.
    pub body_shapes: &'static [fn(&Value) -> bool],
    /// Header mounts signalling this dialect.
    pub header_shapes: &'static [&'static str],
    /// The dialect's session body rule (shape-conditional internally).
    pub session_body: Option<fn(&Value) -> Option<String>>,
    /// The harness that natively speaks it (enrich fallback when only
    /// shapes matched — the "<dialect>-compatible" case).
    pub owner: &'static str,
}

/// All registered dialects; a new dialect is one file + one line.
pub static DIALECTS: &[&Dialect] = &[&claude_code::DIALECT];

pub struct HarnessDescriptor {
    pub name: &'static str,
    /// Legal (harness, protocol) combinations — declaration, not enforcement.
    pub protocols: &'static [&'static str],
    /// Identity-exclusive evidence (UA prefixes, identity fingerprints).
    pub identity: &'static [Identifier],
    /// Dialects this harness natively speaks or borrows (omp borrows the
    /// claude-code dialect) — documentation + validation surface; dialect
    /// DETECTION is always shape-driven, independent of the identity.
    pub dialects: &'static [&'static str],
    /// Session header mounts this harness reads (after the protocol's own
    /// header sources; e.g. opencode's x-session-* affinity family) —
    /// identity-gated.
    pub session_header_mounts: &'static [&'static str],
    /// Extra identity facts: (metadata key, extractor) — emitted as
    /// langfuse.trace.metadata.<key> (cc_account, codex_installation).
    pub enrich: &'static [Enrich],
}

/// One enrichment fact: (metadata key, extractor from the request body).
pub type Enrich = (&'static str, fn(&Value) -> Option<String>);

/// All registered harness descriptors; a new harness is one file + one line.
pub static HARNESSES: &[&HarnessDescriptor] = &[
    &claude_code::DESCRIPTOR,
    &codex::DESCRIPTOR,
    &grok::DESCRIPTOR,
    &opencode::DESCRIPTOR,
    &omp::DESCRIPTOR,
];

/// Classification result: identity attribution + dialect detection
/// (never affects extraction).
pub struct HarnessFacts {
    /// Registered identity attribution (None = no identity evidence).
    pub identity: Option<&'static str>,
    /// Matched dialect name ("" = none) — drives the session body rule and
    /// the metadata.dialect wire field.
    pub dialect: &'static str,
    /// Identity harnesses tied at the winner's strength — a conflict signal.
    pub candidates: Vec<&'static str>,
    /// Winner identity outside its declared legal protocols.
    pub protocol_anomaly: bool,
}

impl HarnessFacts {
    /// Wire label for metadata.harness / tags harness:*: the identity, or
    /// the downgraded "<dialect>-compatible" assertion when only dialect
    /// shapes matched, or empty when nothing matched (unannotated).
    pub fn harness_label(&self) -> String {
        match self.identity {
            Some(n) => n.to_string(),
            None if !self.dialect.is_empty() => format!("{}-compatible", self.dialect),
            None => String::new(),
        }
    }
}

/// Attribute the harness (identity layer) and detect the dialect (shape
/// layer). `ua` is the User-Agent header value (main path: usually None —
/// no UA recorded there).
pub fn identify(
    protocol: &str,
    req: Option<&Value>,
    ua: Option<&str>,
    header_get: &dyn Fn(&str) -> Option<String>,
) -> HarnessFacts {
    // Identity layer: identity-class evidence only, strongest first.
    struct Hit {
        name: &'static str,
        strength: u8,
    }
    let mut identity_hits: Vec<Hit> = Vec::new();
    for h in HARNESSES {
        let mut best: Option<u8> = None;
        for id in h.identity {
            let matched = match id.kind {
                IdentKind::UaPrefix(p) => IdentKind::ua_matches(p, ua),
                IdentKind::HeaderPresent(name) => {
                    header_get(name).is_some_and(|v| !v.trim().is_empty())
                }
                IdentKind::Body(test) => req.is_some_and(test),
            };
            if matched
                && id.class == EvidenceClass::Identity
                && best.is_none_or(|s| id.strength > s)
            {
                best = Some(id.strength);
            }
        }
        if let Some(strength) = best {
            identity_hits.push(Hit {
                name: h.name,
                strength,
            });
        }
    }
    // Dialect layer: shape-driven, independent of identity.
    let dialect = DIALECTS
        .iter()
        .find(|d| {
            d.body_shapes.iter().any(|test| req.is_some_and(test))
                || d.header_shapes
                    .iter()
                    .any(|name| header_get(name).is_some_and(|v| !v.trim().is_empty()))
        })
        .map(|d| d.name)
        .unwrap_or("");

    if identity_hits.is_empty() {
        return HarnessFacts {
            identity: None,
            dialect,
            candidates: Vec::new(),
            protocol_anomaly: false,
        };
    }
    let top = identity_hits.iter().map(|h| h.strength).max().unwrap();
    let candidates: Vec<&'static str> = identity_hits
        .iter()
        .filter(|h| h.strength == top)
        .map(|h| h.name)
        .collect();
    // Winner: strongest identity, table order as the deterministic tiebreak.
    let winner = candidates.first().copied().unwrap_or_default();
    let anomaly = HARNESSES
        .iter()
        .find(|h| h.name == winner)
        .is_some_and(|d| !d.protocols.is_empty() && !d.protocols.contains(&protocol));
    HarnessFacts {
        identity: Some(winner),
        dialect,
        candidates,
        protocol_anomaly: anomaly,
    }
}

/// Dialect session value from the request body — shape-keyed, never
/// attribution-keyed (classification failure never degrades extraction).
pub fn session_from_body(facts: &HarnessFacts, req: &Value) -> Option<String> {
    let d = DIALECTS.iter().find(|d| d.name == facts.dialect)?;
    if let Some(rule) = d.session_body {
        return rule(req);
    }
    None
}

/// Harness session header mounts (opencode family) — consulted after the
/// protocol's own header sources; gated on IDENTITY attribution (UA-level
/// noise control: an unidentified client cannot mint sessions from these).
pub fn session_from_headers(
    facts: &HarnessFacts,
    get: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let name = facts.identity?;
    let d = HARNESSES.iter().find(|h| h.name == name)?;
    for mount in d.session_header_mounts {
        if let Some(v) = get(mount).filter(|s| !s.trim().is_empty()) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Enrichment facts: the ATTRIBUTED identity's extractors; when only
/// dialect shapes matched (the -compatible case), the dialect owner's —
/// e.g. a legacy account uuid is shape-derived, not identity-derived.
pub fn enrich(facts: &HarnessFacts, req: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let owner = facts.identity.or_else(|| {
        DIALECTS
            .iter()
            .find(|d| d.name == facts.dialect)
            .map(|d| d.owner)
    });
    let Some(owner) = owner else {
        return out;
    };
    let Some(d) = HARNESSES.iter().find(|h| h.name == owner) else {
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
