//! Explicit session id extraction, ported from sub2api modeltrace/session.go
//! priority rules: body wins over header; per-protocol body paths.
use serde_json::Value;

/// Claude-Code legacy `metadata.user_id` shape
/// (`user_<64hex>_account_<hex/uuid>_session_<uuid36>`); returns the session
/// uuid. Mirrors modeltrace/session.go claudeCodeLegacyUserIDPattern.
/// Hand-rolled matcher instead of a regex crate: the pattern is fixed, so a
/// slice-scan keeps the dependency tree untouched.
fn claude_code_legacy_user_id_session(user_id: &str) -> Option<String> {
    let rest = user_id.strip_prefix("user_")?;
    let (_hex64, rest) = split_at_hex(rest, 64)?;
    let rest = rest.strip_prefix("_account_")?;
    // Account token: hex/dashes only (empty allowed by the M regex's `*`).
    let acct_len = rest
        .find(|c: char| !c.is_ascii_hexdigit() && c != '-')
        .unwrap_or(rest.len());
    let rest = &rest[acct_len..];
    let rest = rest.strip_prefix("_session_")?;
    // The Go {36} class is `[0-9a-fA-F-]` — dashes count toward the 36.
    let (uuid36, tail) = split_at_class(rest, 36, true)?;
    if !tail.is_empty() || !uuid36.bytes().any(|b| b == b'-') {
        return None;
    }
    Some(uuid36.to_ascii_lowercase())
}

/// Take `n` hex-digit chars off `s`; returns (taken, remainder) or None.
fn split_at_hex(s: &str, n: usize) -> Option<(&str, &str)> {
    split_at_class(s, n, false)
}

/// Take `n` chars off `s`, hex digits plus (when `allow_dash`) dashes — the
/// Go regex class shape; returns (taken, remainder) or None.
fn split_at_class(s: &str, n: usize, allow_dash: bool) -> Option<(&str, &str)> {
    let end = s
        .char_indices()
        .find(|(i, c)| i >= n || !(c.is_ascii_hexdigit() || (allow_dash && *c == '-')))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end != n {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// Extract the explicit session id from one request.
/// `header_get` returns a request header value by (case-insensitive) name.
pub fn extract_session_id(
    protocol: &str,
    request_body: &[u8],
    header_get: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Some(s) = extract_body_session(protocol, request_body) {
        return Some(s);
    }
    extract_header_session(protocol, header_get)
}

fn extract_body_session(protocol: &str, request_body: &[u8]) -> Option<String> {
    if request_body.is_empty() {
        return None;
    }
    let Ok(v) = serde_json::from_slice::<Value>(request_body) else {
        return None;
    };
    if let Some(s) = body_string(&v, &["session_id"]) {
        return Some(s);
    }
    if let Some(s) = body_string(&v, &["conversation_id"]) {
        return Some(s);
    }
    match protocol {
        "anthropic.messages" => {
            if let Some(s) = body_string(&v, &["metadata", "session_id"]) {
                return Some(s);
            }
            metadata_user_id_session(&v)
        }
        "openai.responses" | "openai.live" => body_string(&v, &["client_metadata", "session_id"]),
        _ => body_string(&v, &["metadata", "session_id"]),
    }
}

/// metadata.user_id may be a JSON envelope string holding session_id
/// (Claude-Code legacy and shaped forms).
fn metadata_user_id_session(v: &Value) -> Option<String> {
    let metadata = v.get("metadata")?;
    let user_id = match metadata {
        Value::Object(m) => m.get("user_id")?,
        Value::String(s) => {
            let parsed = serde_json::from_str::<Value>(s).ok()?;
            let user_id = parsed.get("user_id")?.clone();
            return session_from_user_id_value(&user_id);
        }
        _ => return None,
    };
    session_from_user_id_value(user_id)
}

fn session_from_user_id_value(user_id: &Value) -> Option<String> {
    match user_id {
        Value::String(s) => {
            // May itself be a JSON envelope: {"session_id": "..."}
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                if let Some(sid) = parsed.get("session_id").and_then(|x| x.as_str()) {
                    let trimmed = sid.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            // Legacy source 6: only a plain (non-JSON) user_id string reaches
            // the matcher, matching session.go's gjson.Valid short-circuit.
            claude_code_legacy_user_id_session(s)
        }
        Value::Object(_) => user_id
            .get("session_id")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn extract_header_session(
    protocol: &str,
    header_get: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let standard = || {
        header_get("session-id")
            .or_else(|| header_get("session_id"))
            .filter(|s| !s.trim().is_empty())
    };
    let claude = || header_get("x-claude-code-session-id").filter(|s| !s.trim().is_empty());
    if protocol == "anthropic.messages" {
        return claude().or_else(standard);
    }
    let grok = || header_get("x-grok-conv-id").filter(|s| !s.trim().is_empty());
    // modeltrace gates this source on `grokRoute` (the request's API-key
    // group platform == "grok", middleware.go:411-415), which ATG cannot see
    // — ATG sits before sub2api's key resolution. Protocol-shape
    // approximation: responses/live requests only.
    if protocol == "openai.responses" || protocol == "openai.live" {
        return standard().or_else(claude).or_else(grok);
    }
    standard().or_else(claude)
}

fn body_string(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + '_ {
        move |name: &str| {
            map.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        }
    }

    const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    /// Source 6 (legacy user_id regex): hit.
    #[test]
    fn legacy_user_id_extracts_session_uuid() {
        let body = format!(
            r#"{{"metadata":{{"user_id":"user_{}{}_account_abc_session_{}"}}}}"#,
            "0123456789abcdef".repeat(4),
            UUID
        );
        assert_eq!(
            extract_session_id("anthropic.messages", body.as_bytes(), &hdr(&[])),
            Some(UUID.to_string())
        );
        // Envelope-string metadata form reaches the same matcher.
        let envelope = format!(
            r#"{{"metadata":"{{\"user_id\":\"user_{}{}_account__session_{}\"}}"}}"#,
            "0123456789abcdef".repeat(4),
            UUID
        );
        assert_eq!(
            extract_session_id("anthropic.messages", envelope.as_bytes(), &hdr(&[])),
            Some(UUID.to_string())
        );
    }

    /// Source 6: misses (shape violations) must not extract anything.
    #[test]
    fn legacy_user_id_rejects_malformed_shapes() {
        let mk = |core: String| format!(r#"{{"metadata":{{"user_id":"{core}"}}}}"#);
        let cases = [
            // 63 hex digits.
            mk(format!(
                "user_{}_account_abc_session_{UUID}",
                "0123456789abcdef".repeat(4)[1..]
            )),
            // Non-hex inside the 64-digit run.
            mk(format!(
                "user_g{}_account_abc_session_{UUID}",
                "0123456789abcdef".repeat(4)[1..]
            )),
            // Missing session segment.
            mk(format!(
                "user_{}a_account_abc",
                "0123456789abcdef".repeat(4)
            )),
            // Truncated uuid.
            mk(format!(
                "user_{}a_account_abc_session_{}",
                "0123456789abcdef".repeat(4),
                &UUID[..35]
            )),
            // JSON envelope without session_id falls through to the matcher and fails.
            mk(r#"{"user_id":"someone"}"#.to_string()),
        ];
        for body in cases {
            assert_eq!(
                extract_session_id("anthropic.messages", body.as_bytes(), &hdr(&[])),
                None,
                "must not extract from {body}"
            );
        }
    }

    /// Source 9 (X-Grok-Conv-Id): hit on responses protocol, after the
    /// standard and claude headers.
    #[test]
    fn grok_conv_id_hit_on_responses() {
        let body = br#"{"model":"m","input":"x"}"#;
        let get = hdr(&[("x-grok-conv-id", UUID)]);
        assert_eq!(
            extract_session_id("openai.responses", body, &get),
            Some(UUID.to_string())
        );
        assert_eq!(
            extract_session_id("openai.live", body, &get),
            Some(UUID.to_string())
        );
        // Priority: standard header wins over grok.
        let both = hdr(&[("session-id", "std-1"), ("x-grok-conv-id", UUID)]);
        assert_eq!(
            extract_session_id("openai.responses", body, &both),
            Some("std-1".to_string())
        );
        let claude_first = hdr(&[("x-claude-code-session-id", "cc-1"), ("x-grok-conv-id", UUID)]);
        assert_eq!(
            extract_session_id("openai.responses", body, &claude_first),
            Some("cc-1".to_string())
        );
    }

    /// Source 9: absent on protocols without the grok route.
    #[test]
    fn grok_conv_id_ignored_on_other_protocols() {
        let body = br#"{"model":"m"}"#;
        let get = hdr(&[("x-grok-conv-id", UUID)]);
        assert_eq!(
            extract_session_id("openai.chat_completions", body, &get),
            None
        );
        assert_eq!(extract_session_id("anthropic.messages", body, &get), None);
    }
}
