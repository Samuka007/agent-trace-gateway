//! Explicit session id extraction, ported from sub2api modeltrace/session.go
//! priority rules: body wins over header; per-protocol body paths.
use serde_json::Value;

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
        "anthropic_messages" => {
            if let Some(s) = body_string(&v, &["metadata", "session_id"]) {
                return Some(s);
            }
            metadata_user_id_session(&v)
        }
        "openai_responses" | "openai_responses_ws" => {
            body_string(&v, &["client_metadata", "session_id"])
        }
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
            None
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
    if protocol == "anthropic_messages" {
        return claude().or_else(standard);
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
