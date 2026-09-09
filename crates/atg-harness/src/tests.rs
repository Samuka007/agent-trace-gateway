//! Harness-layer behavior pins (design §1/§2 shapes, F2 acceptance).
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

#[test]
fn cc_envelope_identifies_and_extracts_session() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":"{{\"device_id\":\"d\",\"session_id\":\"{UUID}\"}}"}}}}"#
    ));
    let get = hdrs(&[]);
    let facts = identify("anthropic.messages", Some(&req), None, &get);
    assert_eq!(facts.name, "claude-code");
    assert!(facts.candidates.contains(&"claude-code"));
    assert!(!facts.protocol_anomaly);
    assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
    // Envelope form carries no account segment.
    assert!(enrich(&facts, &req).is_empty());
}

#[test]
fn cc_legacy_restores_session_and_account() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":"{}"}}}}"#,
        legacy_user_id()
    ));
    let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.name, "claude-code");
    assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
    let enrichments = enrich(&facts, &req);
    assert_eq!(
        enrichments,
        vec![("cc_account".to_string(), "acc-123".to_string())],
        "legacy account segment must surface as cc_account metadata"
    );
}

#[test]
fn cc_metadata_envelope_string_form_still_extracts() {
    // metadata itself is a JSON string {"user_id": "<legacy>"} — the
    // pre-v0.3.0 fourth table row, preserved as a CC body shape.
    let inner = legacy_user_id();
    let req = body(&format!(r#"{{"metadata":"{{\"user_id\":\"{inner}\"}}"}}"#));
    let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.name, "claude-code");
    assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
}

#[test]
fn cc_object_user_id_variant_identifies() {
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":{{"session_id":"{UUID}"}}}}}}"#
    ));
    let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.name, "claude-code");
    assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
}

#[test]
fn cc_ua_alone_identifies_without_session() {
    let req = body(r#"{"messages":[]}"#);
    let facts = identify(
        "anthropic.messages",
        Some(&req),
        Some("claude-cli/2.1.230"),
        &hdrs(&[]),
    );
    assert_eq!(facts.name, "claude-code");
    assert!(session_from_body(&facts, &req).is_none());
}

/// Ported from the protocol session tests: legacy-shape violations (63
/// hex digits / non-hex run / missing session / truncated uuid / envelope
/// without session_id) must neither identify nor extract.
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
        // JSON envelope without session_id (properly escaped so the outer
        // body is valid JSON — the pre-port version was vacuously invalid).
        mk(r#"{\"user_id\":\"someone\"}"#),
    ];
    for req in cases {
        let facts = identify("anthropic.messages", Some(&req), None, &hdrs(&[]));
        assert_eq!(facts.name, UNKNOWN, "shape violation must not identify");
        assert!(
            session_from_body(&facts, &req).is_none(),
            "must not extract from {req}"
        );
    }
}

#[test]
fn cc_on_openai_protocol_is_an_anomaly_not_an_error() {
    // Legality is a declaration: CC fingerprint on openai.* records the
    // anomaly (mimicry is noise); session still extracts from the shape.
    let req = body(&format!(
        r#"{{"metadata":{{"user_id":"{{\"session_id\":\"{UUID}\"}}"}}}}"#
    ));
    let facts = identify("openai.chat_completions", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.name, "claude-code");
    assert!(
        facts.protocol_anomaly,
        "CC shape on openai.* must be flagged"
    );
    // Shape-gated extraction still runs (classification does not gate it).
    assert_eq!(session_from_body(&facts, &req).as_deref(), Some(UUID));
}

#[test]
fn codex_body_fingerprint_with_installation_enrich() {
    let req = body(
        r#"{"client_metadata":{"session_id":"01JULID","x-codex-turn-metadata":"{}","x-codex-installation-id":"inst-9"}}"#,
    );
    let facts = identify("openai.responses", Some(&req), None, &hdrs(&[]));
    assert_eq!(facts.name, "codex");
    assert!(!facts.protocol_anomaly);
    assert!(
        session_from_body(&facts, &req).is_none(),
        "cm session is a protocol mount"
    );
    assert_eq!(
        enrich(&facts, &req),
        vec![("codex_installation".to_string(), "inst-9".to_string())]
    );
}

#[test]
fn grok_header_fingerprint_and_anomaly_off_route() {
    let get = hdrs(&[("x-grok-conv-id", UUID)]);
    let facts = identify("openai.responses", None, None, &get);
    assert_eq!(facts.name, "grok");
    assert!(!facts.protocol_anomaly);
    let facts = identify("openai.chat_completions", None, None, &get);
    assert_eq!(facts.name, "grok");
    assert!(facts.protocol_anomaly, "grok conv id off the grok route");
}

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
    assert_eq!(facts.name, "opencode");
    assert_eq!(
        session_from_headers(&facts, &get).as_deref(),
        Some("oc-42"),
        "opencode attribution unlocks the x-session-* family"
    );
    // Unidentified traffic must NOT mint sessions from the same header.
    let facts = identify("openai.responses", Some(&req), None, &get);
    assert_eq!(facts.name, UNKNOWN);
    assert!(session_from_headers(&facts, &get).is_none());
}

#[test]
fn unknown_when_no_evidence_and_session_unaffected() {
    let req = body(r#"{"input":"x"}"#);
    let facts = identify(
        "openai.responses",
        Some(&req),
        Some("some-sdk/1"),
        &hdrs(&[]),
    );
    assert_eq!(facts.name, UNKNOWN);
    assert!(facts.candidates.is_empty());
    assert!(!facts.protocol_anomaly);
    assert!(session_from_body(&facts, &req).is_none());
}

#[test]
fn conflicting_same_strength_evidence_records_candidates() {
    // CC header (s=4) + codex body (s=4): conflict signal — both recorded,
    // deterministic winner by table order.
    let req = body(r#"{"client_metadata":{"x-codex-turn-metadata":"{}"}}"#);
    let get = hdrs(&[("x-claude-code-session-id", "cc-s")]);
    let facts = identify("openai.responses", Some(&req), None, &get);
    assert!(facts.candidates.contains(&"claude-code"));
    assert!(facts.candidates.contains(&"codex"));
    assert_eq!(facts.name, "claude-code", "table-order tiebreak");
    assert!(facts.protocol_anomaly, "CC header is illegal on responses");
}

#[test]
fn higher_strength_beats_lower_regardless_of_order() {
    // grok header (s=5) beats a CC UA (s=4).
    let get = hdrs(&[("x-grok-conv-id", UUID)]);
    let facts = identify("openai.responses", None, Some("claude-cli/2.1.230"), &get);
    assert_eq!(facts.name, "grok");
    assert_eq!(facts.candidates, vec!["grok"]);
}
