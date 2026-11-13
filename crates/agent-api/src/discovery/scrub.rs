//! PII rules of discovery (plan "Global Constraints" PII, from P8 T9 / B-P8-6): what an import row or a shadow
//! observation may store.
//!
//! - [`must_drop`]: a row with a Luhn-valid card number (B4's `turn::redact`), a password (`password: …`,
//!   `my password is Summer2024!`) or a one-time code (`OTP 482913`, `verification code is 1234`) is never stored —
//!   not even its excerpt. A row that only *talks about* a password ("I forgot my password", "password is
//!   incorrect") is kept: the value after the keyword must look like a secret.
//! - [`excerpt`]: whitespace collapsed, every e-mail address → `[email]`, every digit run of ≥ 8 digits (single
//!   spaces or dashes between digits join a run) → `[number]`, cut to 1024 bytes at a character boundary.
//! - [`text_hash`]: the dedupe key, sha256 (hex) of the lowercased, whitespace-collapsed text.
use sha2::{Digest, Sha256};

use crate::audit::scrub::looks_like_email;
use crate::turn::redact;

/// Longest excerpt stored (bytes).
pub const MAX_EXCERPT_BYTES: usize = 1024;
/// What replaces a long digit run.
pub const NUMBER: &str = "[number]";
/// What replaces an e-mail address.
pub const EMAIL: &str = "[email]";
/// Digits that make a run a `[number]`.
const MIN_NUMBER_DIGITS: usize = 8;

/// Keywords after which a secret-looking value is a password (ASCII, matched as whole words, any case).
const PASSWORD_WORDS: [&str; 7] = [
    "password",
    "passwort",
    "wachtwoord",
    "mot de passe",
    "passcode",
    "passwd",
    "pwd",
];
/// Keywords after which a 4–8 digit value is a one-time code.
const CODE_WORDS: [&str; 10] = [
    "one-time code",
    "one time code",
    "one-time password",
    "otp",
    "verification code",
    "security code",
    "2fa code",
    "sms code",
    "pin",
    "tan",
];
/// Words that may stand between the keyword and its value.
const CONNECTIVES: [&str; 8] = ["is", "was", "ist", "war", "is:", "est", "es", "code"];

/// The lowercased, whitespace-collapsed text (the dedupe form).
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// sha256 (lowercase hex) of [`normalize`]d `text`.
pub fn text_hash(text: &str) -> String {
    hex::encode(Sha256::digest(normalize(text).as_bytes()))
}

/// The row carries a card number, a password or a one-time code: it is never stored.
pub fn must_drop(text: &str) -> bool {
    redact::redact(text).changed
        || after_keyword(text, &PASSWORD_WORDS, password_like)
        || after_keyword(text, &CODE_WORDS, code_like)
}

/// The scrubbed excerpt of `text` (module docs).
pub fn excerpt(text: &str) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| {
            let bare = w.trim_matches(|c: char| "\"'(),;:<>[]{}!?.".contains(c));
            if looks_like_email(bare) {
                w.replace(bare, EMAIL)
            } else {
                w.to_string()
            }
        })
        .collect();
    let masked = mask_numbers(&words.join(" "));
    cut(&masked, MAX_EXCERPT_BYTES).to_string()
}

/// `s` cut to at most `max` bytes at a character boundary.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Every digit run of ≥ [`MIN_NUMBER_DIGITS`] digits (single spaces or dashes between digits join it) → [`NUMBER`].
fn mask_numbers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        let mut digits = 0usize;
        while j < chars.len() {
            if chars[j].is_ascii_digit() {
                digits += 1;
                j += 1;
                continue;
            }
            let joins = matches!(chars[j], ' ' | '-')
                && chars.get(j + 1).is_some_and(|c| c.is_ascii_digit());
            if !joins {
                break;
            }
            j += 1;
        }
        if digits >= MIN_NUMBER_DIGITS {
            out.push_str(NUMBER);
        } else {
            out.extend(&chars[i..j]);
        }
        i = j;
    }
    out
}

/// The byte offsets just past every whole-word occurrence of the ASCII `word` in `lower` (ASCII-lowercased, so its
/// byte offsets are the original text's).
fn word_ends(lower: &str, word: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(p) = lower[from..].find(word) {
        let start = from + p;
        let end = start + word.len();
        let before = lower[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after = lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before && after {
            out.push(end);
        }
        // `word` starts with an ASCII byte, so `start + 1` is a character boundary
        from = start + 1;
    }
    out
}

/// The value token after a keyword ending at byte `end`: separators (`:`, `=`, `-`) and [`CONNECTIVES`] skipped,
/// trailing punctuation trimmed.
fn value_after(text: &str, end: usize) -> &str {
    let mut rest = &text[end..];
    for _ in 0..4 {
        let t = rest.trim_start();
        let t = t.trim_start_matches([':', '=', '-']).trim_start();
        let word = t.split_whitespace().next().unwrap_or_default();
        if CONNECTIVES.iter().any(|c| word.eq_ignore_ascii_case(c)) {
            rest = &t[word.len()..];
            continue;
        }
        rest = t;
        break;
    }
    rest.split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_end_matches(['.', ',', ';', '!', '?', ')', '"', '\''])
}

/// Some keyword of `words` is followed by a value `looks` accepts.
fn after_keyword(text: &str, words: &[&str], looks: fn(&str) -> bool) -> bool {
    let lower = text.to_ascii_lowercase();
    words.iter().any(|w| {
        word_ends(&lower, w)
            .into_iter()
            .any(|end| looks(value_after(text, end)))
    })
}

/// A password-shaped value: ≥ 4 characters with a digit, a symbol, or both cases.
fn password_like(v: &str) -> bool {
    if v.chars().count() < 4 {
        return false;
    }
    let digit = v.chars().any(|c| c.is_ascii_digit());
    let symbol = v.chars().any(|c| !c.is_alphanumeric());
    let upper = v.chars().any(char::is_uppercase);
    let lower = v.chars().any(char::is_lowercase);
    digit || symbol || (upper && lower)
}

/// A one-time code: 4–8 ASCII digits.
fn code_like(v: &str) -> bool {
    (4..=8).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_password_and_code_rows_are_dropped() {
        assert!(must_drop("my card 4111 1111 1111 1111 was charged twice"));
        assert!(must_drop("Login fails, password: Summer2024!"));
        assert!(must_drop("my password is hunter2"));
        assert!(must_drop("Mein Passwort ist Geheim#1"));
        assert!(must_drop("the OTP 482913 never worked"));
        assert!(must_drop("Verification code is 1234."));
        assert!(must_drop("my PIN: 0000"));
    }

    #[test]
    fn talking_about_secrets_is_kept() {
        assert!(!must_drop("I forgot my password"));
        assert!(!must_drop("password is incorrect"));
        assert!(!must_drop("Password: reset link not received"));
        assert!(!must_drop("where is my verification code?"));
        assert!(!must_drop("shipping pin board order"));
        assert!(!must_drop("order 1234567890123456 is late")); // fails Luhn: an order number
    }

    #[test]
    fn excerpt_masks_email_and_long_numbers() {
        assert_eq!(
            excerpt("Mail me at Anna.B@example.com, order 12345678!"),
            "Mail me at [email], order [number]!"
        );
        assert_eq!(excerpt("call 0151 2345 6789 now"), "call [number] now");
        assert_eq!(excerpt("order 1234567 ok"), "order 1234567 ok");
        assert_eq!(excerpt("  a \n\t b "), "a b");
    }

    #[test]
    fn excerpt_is_cut_at_a_char_boundary() {
        let e = excerpt(&"é".repeat(2000));
        assert!(e.len() <= MAX_EXCERPT_BYTES);
        assert!(e.chars().all(|c| c == 'é'));
    }

    #[test]
    fn hash_ignores_case_and_spacing() {
        assert_eq!(
            text_hash("Where is  my\nOrder"),
            text_hash("where is my order")
        );
        assert_ne!(text_hash("a"), text_hash("b"));
        assert_eq!(text_hash("x").len(), 64);
    }
}
