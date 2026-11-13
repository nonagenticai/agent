//! The keys and names the job-design routes validate: the D61 context-space key (one scheme, shared with
//! agent-model's overlay kinds and the web `context-space-key.ts`) and the name patterns of B5's tenant rows (the
//! migrations' CHECKs and the web validators use the same ones).
pub use agent_model::kinds::overlay::{area_of_context_space, parse_context_space_key};

/// D61: `<industry>:<area>:<slug>` (web `isContextSpaceKey`).
pub fn is_context_space_key(key: &str) -> bool {
    parse_context_space_key(key).is_some()
}

/// `first` matches the first character, every character matches `rest`, and the length is `1..=max`.
fn pattern(s: &str, max: usize, first: fn(char) -> bool, rest: fn(char) -> bool) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(first) && chars.all(rest) && s.chars().count() <= max
}

/// An ontology entity name: PascalCase letters and digits, `^[A-Z][A-Za-z0-9]{0,63}$` (`GiftCard`).
pub fn is_entity_name(s: &str) -> bool {
    pattern(
        s,
        64,
        |c| c.is_ascii_uppercase(),
        |c| c.is_ascii_alphanumeric(),
    )
}

/// An attribute or procedure name: `^[a-z][a-z0-9_]{0,63}$` (`balance`, `start_return`).
pub fn is_snake_name(s: &str) -> bool {
    pattern(
        s,
        64,
        |c| c.is_ascii_lowercase(),
        |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_',
    )
}

/// A relation predicate: `^[a-z][A-Za-z0-9]{0,63}$` (`places`, `billedFor`).
pub fn is_predicate(s: &str) -> bool {
    pattern(
        s,
        64,
        |c| c.is_ascii_lowercase(),
        |c| c.is_ascii_alphanumeric(),
    )
}

/// A tenant gate-question key: `^[A-Z][A-Z0-9]{1,7}$` (`TQ1`).
pub fn is_gate_key(s: &str) -> bool {
    s.len() >= 2
        && pattern(
            s,
            8,
            |c| c.is_ascii_uppercase(),
            |c| c.is_ascii_uppercase() || c.is_ascii_digit(),
        )
}

/// A narrowed operation: `^[a-z_]+\.[a-z_]+(\{[^}]+\})?$` (`payment.create`, `case.create{return}`).
pub fn is_op_label(s: &str) -> bool {
    let word = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c == '_');
    let (head, variant) = match s.find('{') {
        Some(i) => (&s[..i], Some(&s[i..])),
        None => (s, None),
    };
    let head_ok = head
        .split_once('.')
        .is_some_and(|(object, verb)| word(object) && word(verb));
    let variant_ok =
        variant.is_none_or(|v| v.len() > 2 && v.ends_with('}') && !v[1..v.len() - 1].contains('}'));
    head_ok && variant_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(is_entity_name("GiftCard") && is_entity_name("A"));
        assert!(!is_entity_name("giftCard") && !is_entity_name("Gift card") && !is_entity_name(""));
        assert!(!is_entity_name(&format!("A{}", "b".repeat(64))));
        assert!(is_snake_name("start_return") && !is_snake_name("Start") && !is_snake_name("_x"));
        assert!(
            is_predicate("billedFor") && !is_predicate("BilledFor") && !is_predicate("billed_for")
        );
        assert!(
            is_gate_key("TQ1") && is_gate_key("GS") && !is_gate_key("G") && !is_gate_key("tq1")
        );
        assert!(!is_gate_key("ABCDEFGHI"));
    }

    #[test]
    fn op_labels() {
        for ok in [
            "payment.create",
            "case.create{return}",
            "rule.evaluate{RET-01}",
            "contact_point.update",
        ] {
            assert!(is_op_label(ok), "{ok}");
        }
        for bad in [
            "payment",
            "payment.",
            ".create",
            "Payment.create",
            "case.create{}",
            "case.create{a}b",
            "a.b.c",
        ] {
            assert!(!is_op_label(bad), "{bad}");
        }
    }
}
