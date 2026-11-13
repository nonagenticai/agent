//! Card-data redaction (spec 17 N-03), step 0 of every turn: before a customer message is stored, published or sent
//! to the qualifier, a PAN-shaped digit run (13–19 digits, single spaces or dashes allowed between them) that passes
//! the Luhn check is replaced by [`CARD_REMOVED`], together with a CVV or an expiry date that follows it. A digit run
//! that fails Luhn (an order number, a phone number) is kept. The redacted text is the only text the runtime ever
//! holds: the chat log, the case events, the bus and the qualifier request all see it.

/// What replaces a card number (and the CVV / expiry right after it).
pub const CARD_REMOVED: &str = "[card removed]";

/// How far after a card number a CVV or an expiry is looked for (characters).
const WINDOW: usize = 48;

/// Words that may stand between a card number and its CVV / expiry (matched case-insensitively, whole words).
const LABELS: [&str; 10] = [
    "security code",
    "valid thru",
    "expires",
    "expiry",
    "exp.",
    "cvv2",
    "cvv",
    "cvc",
    "csc",
    "exp",
];

/// The redacted text; `changed` when a card number was removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub changed: bool,
}

/// Remove every Luhn-valid card number from `text` (see the module doc).
pub fn redact(text: &str) -> Redacted {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut copied = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let starts_run =
            chars[i].1.is_ascii_digit() && (i == 0 || !chars[i - 1].1.is_ascii_digit());
        if !starts_run {
            i += 1;
            continue;
        }
        let (end, digits) = digit_run(&chars, i);
        if (13..=19).contains(&digits.len()) && luhn(&digits) {
            let stop = trailing_secrets(&chars, end);
            out.push_str(&text[copied..chars[i].0]);
            out.push_str(CARD_REMOVED);
            copied = byte_at(&chars, stop, text.len());
            changed = true;
            i = stop;
        } else {
            i = end.max(i + 1);
        }
    }
    out.push_str(&text[copied..]);
    Redacted { text: out, changed }
}

/// The byte offset of char index `i` (`len` past the end).
fn byte_at(chars: &[(usize, char)], i: usize, len: usize) -> usize {
    chars.get(i).map_or(len, |(b, _)| *b)
}

/// The digits from `start` on, with single spaces or dashes between digits; returns the char index after the run.
fn digit_run(chars: &[(usize, char)], start: usize) -> (usize, String) {
    let mut digits = String::new();
    let mut i = start;
    while let Some((_, c)) = chars.get(i) {
        if c.is_ascii_digit() {
            digits.push(*c);
            i += 1;
            continue;
        }
        let joins = matches!(*c, ' ' | '-')
            && chars.get(i + 1).is_some_and(|(_, n)| n.is_ascii_digit())
            && !digits.is_empty();
        if !joins {
            break;
        }
        i += 1;
    }
    (i, digits)
}

/// The Luhn check over a string of ASCII digits.
pub fn luhn(digits: &str) -> bool {
    let mut sum = 0u32;
    for (i, c) in digits.bytes().rev().enumerate() {
        let mut d = u32::from(c - b'0');
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    !digits.is_empty() && sum.is_multiple_of(10)
}

/// After a card number ending at `end`: up to two CVV / expiry tokens, with only separators and [`LABELS`] between
/// them, within [`WINDOW`] characters. Returns the char index after the last one found (`end` when none).
fn trailing_secrets(chars: &[(usize, char)], end: usize) -> usize {
    let mut i = end;
    let mut last = end;
    let mut found = 0;
    while i < chars.len() && i - end <= WINDOW && found < 2 {
        let c = chars[i].1;
        if c.is_whitespace() || matches!(c, ',' | ';' | ':' | '.' | '(' | ')' | '-') {
            i += 1;
            continue;
        }
        if let Some(n) = label_at(chars, i) {
            i += n;
            continue;
        }
        if let Some(n) = expiry_at(chars, i).or_else(|| cvv_at(chars, i)) {
            i += n;
            last = i;
            found += 1;
            continue;
        }
        break;
    }
    last
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

/// A label word at `i` (whole word, any case): its length in chars.
fn label_at(chars: &[(usize, char)], i: usize) -> Option<usize> {
    LABELS.iter().find_map(|label| {
        let n = label.chars().count();
        let hit = label.chars().enumerate().all(|(k, l)| {
            chars
                .get(i + k)
                .is_some_and(|(_, c)| c.to_ascii_lowercase() == l)
        });
        let whole = !chars.get(i + n).is_some_and(|(_, c)| is_word(*c));
        (hit && whole).then_some(n)
    })
}

/// The number of ASCII digits from `i` on.
fn digits_from(chars: &[(usize, char)], i: usize) -> usize {
    chars[i..]
        .iter()
        .take_while(|(_, c)| c.is_ascii_digit())
        .count()
}

/// `M/YY`, `MM/YY` or `MM/YYYY` at `i`, not followed by a digit: its length in chars.
fn expiry_at(chars: &[(usize, char)], i: usize) -> Option<usize> {
    let month = digits_from(chars, i);
    if !(1..=2).contains(&month) || chars.get(i + month).map(|(_, c)| *c) != Some('/') {
        return None;
    }
    let year = digits_from(chars, i + month + 1);
    matches!(year, 2 | 4).then_some(month + 1 + year)
}

/// A 3- or 4-digit CVV at `i`, not followed by a digit or a letter: its length in chars.
fn cvv_at(chars: &[(usize, char)], i: usize) -> Option<usize> {
    let n = digits_from(chars, i);
    let whole = !chars.get(i + n).is_some_and(|(_, c)| is_word(*c));
    ((3..=4).contains(&n) && whole).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_vectors() {
        assert!(luhn("4111111111111111"));
        assert!(luhn("5555555555554444"));
        assert!(luhn("378282246310005"));
        assert!(!luhn("1234567890123456"));
        assert!(!luhn(""));
    }

    #[test]
    fn a_valid_card_is_removed_with_spaces_or_dashes() {
        for raw in [
            "4111 1111 1111 1111",
            "4111-1111-1111-1111",
            "4111111111111111",
        ] {
            let r = redact(&format!("my card is {raw} thanks"));
            assert_eq!(r.text, "my card is [card removed] thanks", "{raw}");
            assert!(r.changed);
        }
    }

    #[test]
    fn cvv_and_expiry_after_the_card_go_too() {
        let r = redact("pay with 4111 1111 1111 1111 exp 12/27 cvv 123 please");
        assert_eq!(r.text, "pay with [card removed] please");
        let r = redact("5555555555554444, 09/2028, 4321.");
        assert_eq!(r.text, "[card removed].");
    }

    #[test]
    fn a_luhn_invalid_order_number_is_kept() {
        let text = "order 1234 5678 9012 3456 has not arrived";
        let r = redact(text);
        assert_eq!(r.text, text);
        assert!(!r.changed);
        let r = redact("ORD-58213 and 06 1234 5678");
        assert!(!r.changed);
    }

    #[test]
    fn non_ascii_text_around_a_card_survives() {
        let r = redact("café € 4111111111111111 — ok");
        assert_eq!(r.text, "café € [card removed] — ok");
    }
}
