//! Explicit session id extraction, ported from sub2api modeltrace/session.go
//! priority rules: body wins over header; per-protocol body paths.
use serde_json::Value;

/// Claude-Code legacy `metadata.user_id` shape
/// (`user_<64hex>_account_<hex/uuid>_session_<uuid36>`); returns the session
/// uuid. Mirrors modeltrace/session.go claudeCodeLegacyUserIDPattern.
/// Hand-rolled matcher instead of a regex crate: the pattern is fixed, so a
/// slice-scan keeps the dependency tree untouched.
/// metadata.user_id may carry session_id in three shapes (Claude Code legacy
/// and shaped forms): a JSON envelope string {"session_id": ...}, a plain
/// composite legacy string, or an object with session_id. Returns the
/// session uuid when recognisable.
pub fn metadata_user_id_session(user_id: &Value) -> Option<String> {
    match user_id {
        Value::String(s) => {
            // JSON envelope: {"session_id": "..."} — extraction wins first.
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                if let Some(sid) = parsed.get("session_id").and_then(|x| x.as_str()) {
                    let trimmed = sid.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            // Plain (non-JSON) string: legacy composite matcher.
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

pub fn claude_code_legacy_user_id_session(user_id: &str) -> Option<String> {
    claude_code_legacy_parts(user_id).map(|(session, _)| session)
}

/// Full legacy-shape read: returns (session uuid, account token) — the
/// account segment restores the Claude Code account identity (harness
/// enrich); it is None only for the empty-account form the Go regex allows.
pub fn claude_code_legacy_parts(user_id: &str) -> Option<(String, Option<String>)> {
    let rest = user_id.strip_prefix("user_")?;
    let (_hex64, rest) = split_at_hex(rest, 64)?;
    let rest = rest.strip_prefix("_account_")?;
    // Account token: hex/dashes only (empty allowed by the M regex's `*`).
    let acct_len = rest
        .find(|c: char| !c.is_ascii_hexdigit() && c != '-')
        .unwrap_or(rest.len());
    let account = if acct_len > 0 {
        Some(rest[..acct_len].to_string())
    } else {
        None
    };
    let rest = &rest[acct_len..];
    let rest = rest.strip_prefix("_session_")?;
    // The Go {36} class is `[0-9a-fA-F-]` — dashes count toward the 36.
    let (uuid36, tail) = split_at_class(rest, 36, true)?;
    if !tail.is_empty() || !uuid36.bytes().any(|b| b == b'-') {
        return None;
    }
    Some((uuid36.to_ascii_lowercase(), account))
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
        .find(|(i, c)| *i >= n || !(c.is_ascii_hexdigit() || (allow_dash && *c == '-')))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end != n {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// metadata itself may be a JSON envelope string: {"user_id": "..."} —
/// extracts the user_id then applies the three-form reader.
pub fn metadata_envelope_transform(metadata_str: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(metadata_str).ok()?;
    let user_id = parsed.get("user_id")?.as_str()?;
    let inner: Value = serde_json::from_str(user_id).unwrap_or(Value::String(user_id.to_string()));
    metadata_user_id_session(&inner)
}

#[cfg(test)]
mod tests {
    // The claude-code shape pins (envelope/legacy hit + malformed
    // rejection) moved to atg-harness (tests.rs) together with the CC
    // session rules; what stays here pins the protocol tables' generic
    // header evaluation (grok mount on responses/live only).

    fn hdr<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            map.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        }
    }

    const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    /// Source 9 (X-Grok-Conv-Id): hit on responses/live, after the
    /// standard and claude headers.
    #[test]
    fn grok_conv_id_hit_on_responses() {
        let get = hdr(&[("x-grok-conv-id", UUID)]);
        let responses = crate::ProtocolDescriptor::detect_by_name("openai.responses").unwrap();
        assert_eq!(responses.session_from_headers(&get), Some(UUID.to_string()));
        let live = crate::ProtocolDescriptor::detect_by_name("openai.live").unwrap();
        assert_eq!(live.session_from_headers(&get), Some(UUID.to_string()));
        // Priority: standard header wins over grok.
        let both = hdr(&[("session-id", "std-1"), ("x-grok-conv-id", UUID)]);
        assert_eq!(
            responses.session_from_headers(&both),
            Some("std-1".to_string())
        );
        let claude_first = hdr(&[
            ("x-claude-code-session-id", "cc-1"),
            ("x-grok-conv-id", UUID),
        ]);
        assert_eq!(
            responses.session_from_headers(&claude_first),
            Some("cc-1".to_string())
        );
    }

    /// Source 9: absent on protocols without the grok route.
    #[test]
    fn grok_conv_id_ignored_on_other_protocols() {
        let get = hdr(&[("x-grok-conv-id", UUID)]);
        for name in ["openai.chat_completions", "anthropic.messages"] {
            let d = crate::ProtocolDescriptor::detect_by_name(name).unwrap();
            assert_eq!(d.session_from_headers(&get), None, "{name}");
        }
    }
}
