//! What audit details (and provisioning payloads and errors) may not carry: a token, a secret, a password, an
//! email address. Ported from care-bus-agents@96c608c `secrets.rs` `looks_like_secret`, plus the email, JWT and
//! bearer rules of B1 X3. A hit is refused (fail loud), never silently dropped; the error names the field only.

use serde_json::Value;

/// Key fragments that mark a secret- or person-bearing field (compared ignoring case, anywhere in the key).
const SECRET_KEY_PARTS: [&str; 8] = [
    "token",
    "secret",
    "password",
    "authorization",
    "api_key",
    "apikey",
    "cookie",
    "email",
];

/// Keys that contain a fragment above but carry no secret: the domain of an email address is recorded instead
/// of the address.
const ALLOWED_KEYS: [&str; 1] = ["email_domain"];

/// Does `k` name a secret- or person-bearing field?
pub fn is_secret_key(k: &str) -> bool {
    let lower = k.to_ascii_lowercase();
    !ALLOWED_KEYS.contains(&lower.as_str()) && SECRET_KEY_PARTS.iter().any(|p| lower.contains(p))
}

/// `local@domain.tld`: one `@`, a non-empty local part, a dotted domain, no whitespace.
pub fn looks_like_email(s: &str) -> bool {
    let t = s.trim();
    match t.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !local.contains(char::is_whitespace)
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains('@')
                && !domain.contains(char::is_whitespace)
        }
        None => false,
    }
}

/// A JWT (`eyJ….….…`), a bearer credential, an API key, a PEM block, or a long high-entropy string
/// (≥ 32 characters, upper- and lowercase and digits, no `.`, `:` or whitespace). A uuid is not (no uppercase).
pub fn looks_like_secret(s: &str) -> bool {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    if t.starts_with("eyJ") || t.starts_with("sk-") || t.contains("-----BEGIN") {
        return true;
    }
    if lower.starts_with("bearer ") || lower.starts_with("basic ") {
        return true;
    }
    if t.len() < 32
        || t.contains("://")
        || t.chars().any(|c| c.is_whitespace() || c == ':' || c == '.')
    {
        return false;
    }
    let classes = [
        t.chars().any(|c| c.is_ascii_lowercase()),
        t.chars().any(|c| c.is_ascii_uppercase()),
        t.chars().any(|c| c.is_ascii_digit()),
    ];
    classes.iter().filter(|b| **b).count() >= 3
}

/// A string that must not be stored: secret-shaped or an email address.
pub fn forbidden_value(s: &str) -> bool {
    looks_like_secret(s) || looks_like_email(s)
}

/// Walk `v` (object keys included). `Err(field)` names the first refused field (a secret key, or a secret- or
/// email-shaped string) without echoing its value.
pub fn check(v: &Value) -> Result<(), String> {
    fn name(key: &str) -> String {
        if key.len() > 64 || forbidden_value(key) {
            "(unnamed field)".into()
        } else {
            key.into()
        }
    }
    fn walk(path: &str, v: &Value) -> Result<(), String> {
        match v {
            Value::String(s) if forbidden_value(s) => Err(name(path)),
            Value::Object(m) => m.iter().try_for_each(|(k, v)| {
                if is_secret_key(k) || forbidden_value(k) {
                    return Err(name(k));
                }
                walk(k, v)
            }),
            Value::Array(a) => a.iter().try_for_each(|v| walk(path, v)),
            _ => Ok(()),
        }
    }
    walk("(value)", v)
}

/// `text` with every whitespace-separated word that is secret- or email-shaped replaced by `[redacted]`, cut to
/// at most `max` characters: for upstream error texts that are stored (`provision_outbox.last_error`).
pub fn scrub_text(text: &str, max: usize) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|w| {
            let bare = w.trim_matches(|c: char| "\"'(),;<>[]{}".contains(c));
            if forbidden_value(bare) || forbidden_value(w) {
                "[redacted]"
            } else {
                w
            }
        })
        .collect();
    words.join(" ").chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_values_and_texts() {
        assert!(is_secret_key("token"));
        assert!(is_secret_key("Client_Secret"));
        assert!(is_secret_key("email"));
        assert!(!is_secret_key("email_domain"));
        assert!(!is_secret_key("license_id"));
        assert!(looks_like_email("a@b.eu"));
        assert!(!looks_like_email("b.eu"));
        assert!(looks_like_secret(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.c2ln"
        ));
        assert!(looks_like_secret("Bearer abc"));
        assert!(!looks_like_secret("0195f48b-6c1e-7a3b-9d2e-0123456789ab"));
        assert!(!looks_like_secret("licences_unavailable"));
        assert!(check(&json!({"code": "x", "ids": ["lic-1"], "email_domain": "b.eu"})).is_ok());
        assert_eq!(check(&json!({"who": "a@b.eu"})), Err("who".to_string()));
        assert_eq!(check(&json!({"api_key": 1})), Err("api_key".to_string()));
        assert_eq!(
            scrub_text("refused for ann@globex.com with Bearer", 200),
            "refused for [redacted] with Bearer"
        );
        assert_eq!(scrub_text(&"x ".repeat(3000), 2000).chars().count(), 2000);
    }
}
