//! B7 T4: operation keys, exactly as TS `opKey` / `parseOpKey` / `sameOp` of `web/src/lib/connectors/ops.ts`:
//! `order.read`, `case.create{return}`; variant `*` (or empty) is left off.
use super::jsonpath::js_space;
use super::model::OpRef;

/// `object.verb` or `object.verb{variant}` (variant `*` or empty → no braces).
pub fn op_key(o: &OpRef) -> String {
    if o.variant.is_empty() || o.variant == "*" {
        format!("{}.{}", o.object, o.verb)
    } else {
        format!("{}.{}{{{}}}", o.object, o.verb, o.variant)
    }
}

/// TS `parseOpKey`: `^([a-z_]+)\.([a-z_]+)(?:\{([^}]+)\})?$` on the trimmed key; anything else yields the empty
/// reference `{object: "", verb: "", variant: "*"}`.
pub fn parse_op_key(key: &str) -> OpRef {
    parse_strict(key.trim_matches(js_space)).unwrap_or_else(|| OpRef {
        object: String::new(),
        verb: String::new(),
        variant: "*".to_owned(),
    })
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn parse_strict(key: &str) -> Option<OpRef> {
    let (head, variant) = match key.find('{') {
        Some(i) => {
            let rest = key.get(i + 1..)?.strip_suffix('}')?;
            if rest.is_empty() || rest.contains('}') {
                return None;
            }
            (key.get(..i)?, rest.to_owned())
        }
        None => (key, "*".to_owned()),
    };
    let (object, verb) = head.split_once('.')?;
    if !is_ident(object) || !is_ident(verb) {
        return None;
    }
    Some(OpRef {
        object: object.to_owned(),
        verb: verb.to_owned(),
        variant,
    })
}

fn norm(v: &str) -> &str {
    if v.is_empty() { "*" } else { v }
}

/// TS `sameOp`: same object and verb, variants equal with empty meaning `*`.
pub fn same_op(a: &OpRef, b: &OpRef) -> bool {
    a.object == b.object && a.verb == b.verb && norm(&a.variant) == norm(&b.variant)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(o: &str, v: &str, va: &str) -> OpRef {
        OpRef {
            object: o.into(),
            verb: v.into(),
            variant: va.into(),
        }
    }

    #[test]
    fn round_trips_like_ts() {
        assert_eq!(op_key(&r("order", "read", "*")), "order.read");
        assert_eq!(
            op_key(&r("case", "create", "return")),
            "case.create{return}"
        );
        assert_eq!(op_key(&r("case", "create", "")), "case.create");
        assert_eq!(
            parse_op_key(" case.create{return} "),
            r("case", "create", "return")
        );
        assert_eq!(
            parse_op_key("rule.evaluate{PAY-04}"),
            r("rule", "evaluate", "PAY-04")
        );
        assert_eq!(parse_op_key("order.read"), r("order", "read", "*"));
        assert_eq!(parse_op_key("Order.read"), r("", "", "*"));
        assert_eq!(parse_op_key("order.read{}"), r("", "", "*"));
        assert_eq!(parse_op_key("order.read{a}b"), r("", "", "*"));
        assert_eq!(parse_op_key("a.b.c"), r("", "", "*"));
        assert!(same_op(&r("a", "b", ""), &r("a", "b", "*")));
    }
}
