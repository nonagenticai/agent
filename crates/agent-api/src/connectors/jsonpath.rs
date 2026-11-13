//! B7 T4: the JSONPath subset mapping overrides use (`$`, `.key`, `['key']`, `[n]`, `[*]`, `.*`) — a port of TS
//! `jsonPath` of `web/src/lib/connectors/ops.ts` with identical results and error texts (golden-tested).
use serde_json::Value;

enum Token {
    Key(String),
    Index(usize),
    Star,
}

/// JS `\s` / `String.prototype.trim` whitespace (Rust's `White_Space` plus U+FEFF, minus U+0085).
pub(crate) fn js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// One token at byte `at` of `p` (the TS sticky regex
/// `\.([A-Za-z_][\w-]*)|\[(\d+)\]|\[\*\]|\['([^']+)'\]|\.\*`), with the byte after it.
fn token_at(p: &str, at: usize) -> Option<(Token, usize)> {
    let b = p.as_bytes();
    let rest = b.get(at..)?;
    match *rest.first()? {
        b'.' => {
            let first = *rest.get(1)?;
            if first.is_ascii_alphabetic() || first == b'_' {
                let mut end = 2;
                while rest.get(end).is_some_and(|&c| is_word(c) || c == b'-') {
                    end += 1;
                }
                let key = p.get(at + 1..at + end)?.to_owned();
                Some((Token::Key(key), at + end))
            } else if first == b'*' {
                Some((Token::Star, at + 2))
            } else {
                None
            }
        }
        b'[' => {
            let second = *rest.get(1)?;
            if second.is_ascii_digit() {
                let mut end = 1;
                while rest.get(end).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                }
                if rest.get(end) != Some(&b']') {
                    return None;
                }
                // A number past usize never indexes an array (TS: `t < c.length` is false).
                let n = p
                    .get(at + 1..at + end)?
                    .parse::<usize>()
                    .unwrap_or(usize::MAX);
                Some((Token::Index(n), at + end + 1))
            } else if second == b'*' {
                (rest.get(2) == Some(&b']')).then_some((Token::Star, at + 3))
            } else if second == b'\'' {
                let close = rest.get(2..)?.iter().position(|&c| c == b'\'')?;
                if close == 0 || rest.get(2 + close + 1) != Some(&b']') {
                    return None;
                }
                let key = p.get(at + 2..at + 2 + close)?.to_owned();
                Some((Token::Key(key), at + 2 + close + 2))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Evaluates `path` on `doc`. `Ok` carries the first match, or every match (as an array) once a `*` was crossed;
/// `Err` carries the TS error text.
pub fn json_path(doc: &Value, path: &str) -> Result<Value, String> {
    let p = path.trim_matches(js_space);
    if !p.starts_with('$') {
        return Err("A JSONPath starts with $.".to_owned());
    }
    let mut tokens = Vec::new();
    let mut at = 1;
    while at < p.len() {
        match token_at(p, at) {
            Some((t, next)) => {
                tokens.push(t);
                at = next;
            }
            None => {
                let rest = p.get(at..).unwrap_or_default();
                return Err(format!("Cannot read the path at “{rest}”."));
            }
        }
    }
    let mut cur: Vec<&Value> = vec![doc];
    let mut wild = false;
    for t in &tokens {
        let mut next = Vec::new();
        for c in cur {
            match (t, c) {
                (Token::Star, Value::Array(a)) => {
                    wild = true;
                    next.extend(a.iter());
                }
                (Token::Star, Value::Object(o)) => {
                    wild = true;
                    next.extend(o.values());
                }
                (Token::Index(n), Value::Array(a)) => {
                    if let Some(v) = a.get(*n) {
                        next.push(v);
                    }
                }
                (Token::Key(k), Value::Object(o)) => {
                    if let Some(v) = o.get(k) {
                        next.push(v);
                    }
                }
                _ => {}
            }
        }
        cur = next;
    }
    if cur.is_empty() {
        return Err("No match in the sample response.".to_owned());
    }
    if wild {
        Ok(Value::Array(cur.into_iter().cloned().collect()))
    } else {
        Ok(cur[0].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subset_like_ts() {
        let d = json!({"data": {"items": [{"id": 1}, {"id": 2}], "a-b": "x", "k y": 3}});
        assert_eq!(json_path(&d, "$.data.items[1].id"), Ok(json!(2)));
        assert_eq!(json_path(&d, "$.data.items[*].id"), Ok(json!([1, 2])));
        assert_eq!(json_path(&d, "$.data.a-b"), Ok(json!("x")));
        assert_eq!(json_path(&d, "$['data']['k y']"), Ok(json!(3)));
        assert_eq!(json_path(&d, " $ "), Ok(d.clone()));
        assert_eq!(
            json_path(&d, "data"),
            Err("A JSONPath starts with $.".to_owned())
        );
        assert_eq!(
            json_path(&d, "$.data..x"),
            Err("Cannot read the path at “..x”.".to_owned())
        );
        assert_eq!(
            json_path(&d, "$.data.nope"),
            Err("No match in the sample response.".to_owned())
        );
        assert_eq!(
            json_path(&d, "$.data.items[*].nope"),
            Err("No match in the sample response.".to_owned())
        );
        assert_eq!(
            json_path(&d, "$.data.items[9]"),
            Err("No match in the sample response.".to_owned())
        );
    }
}
