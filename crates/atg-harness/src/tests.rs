//! Harness-layer behavior pins (two-tier: identity vs dialect, v0.3.2).
use crate::*;
use serde_json::Value;

fn body(json: &str) -> Value {
    serde_json::from_str(json).unwrap()
}

fn hdrs<'a>(map: &'a [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name: &str| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.to_string())
    }
}

const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";

fn legacy_user_id() -> String {
    format!(
        "user_{}{}_account_acc-123_session_{UUID}",
        "0123456789abcdef".repeat(4),
        ""
    )
}

/// Design pin 1a: omp UA + CC header → identity=omp (dialect borrowed),
/// session carried by the CC header via the protocol mount.
#[test]
fn omp_ua_with_cc_dialect_header_attributed_as_omp() {
    let req = body(r#"{"messages":[]}"#);
    let get = hdrs(&[(crate::CC_SESSION_HEADER, "omp-sess-1")]);
    let facts = identify("anthropic.messages", Some(&req), Some("omp/18.1.0"), &get);
    assert_eq!(facts.identity, Some("omp"), "omp UA is identity-exclusive");
    assert_eq!(
        facts.dialect, "claude-code",
        "CC header signals the dialect"
    );
    assert_eq!(facts.harness_label(), "omp");
    assert!(!facts.protocol_anomaly, "omp is legal on every protocol");
    // Pipeline order: dialect body rule (no CC body shapes) -> protocol
    // headers -> the CC header value.
    assert!(session_from_body(&facts, &req).is_none());
    let d = atg_protocol::ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
    assert_eq!(
        d.session_from_headers(&get).as_deref(),
        Some("omp-sess-1"),
        "session still extracts through the protocol mount"
    );
}

/// Production-bug arbitration pin (anthropic line): the omp UA — ANY
/// casing — is identity evidence and must beat every claude-code SHAPE
/// (CC header, envelope) AND the legacy identity fingerprint at its lower
/// strength. The production misattribution showed candidates=['claude-code']
/// alone, i.e. the UA evidence never matched — casing variance is one
/// concrete way that happens.
#[test]
fn omp_ua_beats_cc_shapes_and_legacy_identity_any_case() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":"{}"}}}}"#,
        legacy_user_id()
    ));
    let get = hdrs(&[(crate::CC_SESSION_HEADER, "cc-s")]);
    for ua in ["omp/18.1.16", "OMP/18.1.16", "Omp/18.2.0"] {
        let facts = identify("anthropic.messages", Some(&req), Some(ua), &get);
        assert_eq!(facts.identity, Some("omp"), "UA evidence must win: {ua}");
        assert_eq!(
            facts.dialect, "claude-code",
            "CC shapes still signal the dialect"
        );
        // The legacy fingerprint DID match — but as a lower-strength
        // identity, it stays out of the top-strength candidate set.
        assert_eq!(
            facts.candidates,
            vec!["omp"],
            "single top-strength identity"
        );
        assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
        // Sub-path (count_tokens) session carriage is protocol-level and
        // identical across the line — dialect consistency.
        let d = atg_protocol::ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
        assert_eq!(
            d.session_from_headers(&get).as_deref(),
            Some("cc-s"),
            "sub-path sessions carry the same dialect carriage"
        );
    }
    // Negative control: without any omp UA the legacy fingerprint DOES
    // claim the identity (claude-code, s=3) — the documented design.
    let facts = identify("anthropic.messages", Some(&req), None, &get);
    assert_eq!(facts.identity, Some("claude-code"));
    assert_eq!(facts.dialect, "claude-code");
    // Multibyte UA whose byte-4 falls inside a character: the matcher
    // must not PANIC (client-controlled input in the logging hook) and
    // must not match omp — reaching this assertion at all proves no
    // panic; the legacy fingerprint still claims claude-code (this req
    // carries it), which is the documented no-UA behavior.
    for ua in ["日éx/1.0 omp", "ÖMP/18.1.0"] {
        let facts = identify("anthropic.messages", Some(&req), Some(ua), &hdrs(&[]));
        assert_ne!(
            facts.identity,
            Some("omp"),
            "multibyte UA must not match: {ua}"
        );
        assert_eq!(facts.identity, Some("claude-code"), "legacy fallback: {ua}");
    }
}

/// Design pin 1b: omp UA + CC envelope body → dialect session extracted,
/// identity stays omp (the misattribution case from production, inverted).
#[test]
fn omp_ua_with_cc_envelope_extracts_via_dialect() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":"{{\"device_id\":\"d\",\"session_id\":\"{UUID}\"}}"}}}}"#
    ));
    let facts = identify(
        "anthropic.messages",
        Some(&req),
        Some("omp/18.1.0"),
        &hdrs(&[]),
    );
    assert_eq!(facts.identity, Some("omp"));
    assert_eq!(facts.dialect, "claude-code");
    assert_eq!(
        session_from_body(&facts, &req).as_deref(),
        Some(UUID),
        "borrowed dialect rules extract the session"
    );
}

/// Design pin 2: no UA + CC header → downgraded claude-code-compatible
/// (shape-only assertion, not the identity); extraction unaffected.
#[test]
fn no_ua_cc_header_is_compatible_not_identity() {
    let req = body(r#"{"messages":[]}"#);
    let get = hdrs(&[(crate::CC_SESSION_HEADER, "cc-sess-9")]);
    let facts = identify("anthropic.messages", Some(&req), None, &get);
    assert_eq!(facts.identity, None, "no identity evidence");
    assert_eq!(facts.dialect, "claude-code");
    assert_eq!(facts.harness_label(), "claude-code-compatible");
    let d = atg_protocol::ProtocolDescriptor::detect_by_name("anthropic.messages").unwrap();
    assert_eq!(d.session_from_headers(&get).as_deref(), Some("cc-sess-9"));
}

/// Shape-only envelope (no UA): compatible label, session STILL extracted
/// through the dialect rule (extraction never degrades).
#[test]
fn no_ua_envelope_is_compatible_and_extracts() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":"{{\"session_id\":\"{UUID}\"}}"}}}}"#
    ));
    let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.identity, None);
    assert_eq!(facts.harness_label(), "claude-code-compatible");
    assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
}

/// v0.3.1 pin restored for the dialect era (BLOCK-G1, §5 row 6): the
/// object user_id form {session_id} extracts through the dialect rule.
/// Under the two-tier model it no longer claims the IDENTITY (compatible
/// label) — the design's tightening; extraction semantics unchanged.
#[test]
fn object_user_id_variant_extracts_via_dialect() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":{{"session_id":"{UUID}"}}}}}}"#
    ));
    let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.identity, None, "object form is dialect-only");
    assert_eq!(facts.dialect, "claude-code");
    assert_eq!(facts.harness_label(), "claude-code-compatible");
    assert_eq!(
        session_from_body(&facts, &req).as_deref(),
        Some(UUID),
        "the dialect rule's object branch must extract"
    );
}

/// v0.3.1 pin restored for the dialect era (BLOCK-G1, §5 row 4 — a REAL
/// historical traffic form): metadata itself as a JSON envelope string
/// {"user_id": …} reaches the same extraction.
#[test]
fn metadata_envelope_string_form_extracts_via_dialect() {
    let inner = legacy_user_id();
    let req = body(&format!(r#"{{"metadata":"{{\"user_id\":\"{inner}\"}}"}}"#));
    let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.dialect, "claude-code");
    assert_eq!(facts.harness_label(), "claude-code-compatible");
    assert_eq!(
        session_from_body(&facts, &req).as_deref(),
        Some(UUID),
        "the dialect rule's metadata-envelope branch must extract"
    );
}

/// NIT: declared dialects must reference registered dialect tables (the
/// field's validation surface — otherwise it is dead data).
#[test]
fn declared_dialects_are_registered() {
    for h in HARNESSES {
        for name in h.dialects {
            assert!(
                DIALECTS.iter().any(|d| d.name == *name),
                "{} declares unregistered dialect {name}",
                h.name
            );
        }
    }
}

/// Design pin 3: claude-cli UA → the claude-code identity proper.
#[test]
fn claude_cli_ua_is_identity() {
    let req = body(r#"{"messages":[]}"#);
    let facts = identify(
        "anthropic.messages",
        Some(&req),
        Some("claude-cli/2.1.230"),
        &hdrs(&[]),
    );
    assert_eq!(facts.identity, Some("claude-code"));
    assert_eq!(facts.harness_label(), "claude-code");
}

/// Design pin 4 (CC tightening): the legacy composite is identity evidence
/// on its own (CC ≤2.1.114 fingerprint); a conflicting UA wins over it.
#[test]
fn legacy_shape_alone_is_identity() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":"{}"}}}}"#,
        legacy_user_id()
    ));
    let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.identity, Some("claude-code"), "legacy = identity");
    assert_eq!(facts.dialect, "claude-code");
    assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
    assert_eq!(
        enrich(&facts, &req),
        vec![("cc_account".to_string(), "acc-123".to_string())]
    );
    // Conflicting identity evidence outranks the legacy shape.
    let facts = identify(
        "anthropic.messages",
        Some(&req),
        Some("omp/18.1"),
        &hdrs(&[]),
    );
    assert_eq!(
        facts.identity,
        Some("omp"),
        "UA identity wins over legacy body fingerprint"
    );
}

/// Design pin 5: codex unaffected — body fingerprint is identity evidence,
/// and a CC header alongside is a dialect signal, not a conflict.
#[test]
fn codex_identity_survives_cc_dialect_shapes() {
    let req = body(
        r#"{"client_metadata":{"session_id":"01JULID","x-codex-turn-metadata":"{}","x-codex-installation-id":"inst-9"}}"#,
    );
    let facts = identify("openai.responses", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.identity, Some("codex"));
    assert_eq!(facts.dialect, "");
    assert_eq!(
        enrich(&facts, &req),
        vec![("codex_installation".to_string(), "inst-9".to_string())]
    );
    let get = hdrs(&[(crate::CC_SESSION_HEADER, "cc-mixed")]);
    let facts = identify("openai.responses", Some(&req), None, &get);
    assert_eq!(facts.identity, Some("codex"), "identity beats shape class");
    assert_eq!(
        facts.dialect, "claude-code",
        "CC header still signals dialect"
    );
    assert!(
        session_from_body(&facts, &req).is_none(),
        "cm session is a protocol mount"
    );
}

/// Same-strength IDENTITY conflicts still record candidates.
#[test]
fn conflicting_identity_evidence_records_candidates() {
    let req = body(
        r#"{"metadata":{"user_id":"user_x"},"client_metadata":{"x-codex-turn-metadata":"{}"}}"#,
    );
    let facts = identify(
        "openai.responses",
        Some(&req),
        Some("claude-cli/2.1.230"),
        &hdrs(&[]),
    );
    assert_eq!(facts.identity, Some("claude-code"), "table-order tiebreak");
    assert!(facts.candidates.contains(&"claude-code"));
    assert!(facts.candidates.contains(&"codex"));
    assert!(
        facts.protocol_anomaly,
        "CC identity is illegal on responses"
    );
}

/// grok: own header convention is identity; off-route is an anomaly.
#[test]
fn grok_header_identity_and_anomaly_off_route() {
    let get = hdrs(&[(crate::GROK_CONV_HEADER, UUID)]);
    let facts = identify("openai.responses", None, None, &get);
    assert_eq!(facts.identity, Some("grok"));
    assert!(!facts.protocol_anomaly);
    let facts = identify("openai.chat_completions", None, None, &get);
    assert_eq!(facts.identity, Some("grok"));
    assert!(facts.protocol_anomaly, "grok conv id off the grok route");
}

/// opencode: UA identity unlocks the x-session-* mounts; unidentified
/// traffic cannot mint sessions from them.
#[test]
fn opencode_ua_unlocks_session_header_mounts() {
    let req = body(r#"{"input":"x"}"#);
    let get = hdrs(&[("x-session-id", "oc-42")]);
    let facts = identify(
        "openai.responses",
        Some(&req),
        Some("opencode/1.0 ai-sdk/5"),
        &get,
    );
    assert_eq!(facts.identity, Some("opencode"));
    assert_eq!(
        session_from_headers(&facts, &get).as_deref(),
        Some("oc-42"),
        "opencode identity unlocks the x-session-* family"
    );
    let facts = identify("openai.responses", Some(&req), None, &get);
    assert_eq!(facts.identity, None);
    assert!(session_from_headers(&facts, &get).is_none());
}

/// Unknown: no evidence at all → empty label, no dialect.
#[test]
fn unknown_when_no_evidence() {
    let req = body(r#"{"input":"x"}"#);
    let facts = identify(
        "openai.responses",
        Some(&req),
        Some("some-sdk/1"),
        &hdrs(&[]),
    );
    assert_eq!(facts.identity, None);
    assert_eq!(facts.dialect, "");
    assert_eq!(facts.harness_label(), "");
    assert!(session_from_body(&facts, &req).is_none());
}

/// Malformed CC shapes neither identify nor extract (ported rejection
/// matrix; the envelope-without-session_id case is now VALID JSON — the
/// pre-port version was vacuously invalid).
#[test]
fn cc_malformed_shapes_do_not_identify_or_extract() {
    let mk = |core: &str| body(&format!(r#"{{"metadata":{{"user_id":"{core}"}}}}"#));
    let cases = [
        mk(&format!(
            "user_{}_account_abc_session_{UUID}",
            &"0123456789abcdef".repeat(4)[1..]
        )),
        mk(&format!(
            "user_g{}_account_abc_session_{UUID}",
            &"0123456789abcdef".repeat(4)[1..]
        )),
        mk(&format!(
            "user_{}a_account_abc",
            "0123456789abcdef".repeat(4)
        )),
        mk(&format!(
            "user_{}a_account_abc_session_{}",
            "0123456789abcdef".repeat(4),
            &UUID[..35]
        )),
        mk(r#"{\"user_id\":\"someone\"}"#),
    ];
    for req in cases {
        let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
        assert_eq!(facts.identity, None, "shape violation must not identify");
        assert!(
            session_from_body(&facts, &req).is_none(),
            "must not extract from {req}"
        );
    }
}
