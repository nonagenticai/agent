//! Canonical JSON and content hashes (configurator spec section 5.3, ported verbatim).

use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Canonical JSON: object keys sorted recursively by UTF-16 code units (RFC 8785, as care-bus sorts them), no
/// whitespace. Numbers and strings are printed exactly as `serde_json` prints them.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_string(s: &str, out: &mut String) {
    // Serializing a `str` cannot fail.
    out.push_str(&serde_json::to_string(s).unwrap_or_default());
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // UTF-16 order differs from UTF-8 byte order once a key holds a character above
            // U+FFFF next to one in U+E000..U+FFFF.
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(&map[k], out);
            }
            out.push('}');
        }
    }
}

/// Lower-case hex SHA-256 of `s`.
pub fn sha256_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `"sha256:" + hex(SHA-256(canonical JSON of fields))`: a layer row's `fields_hash`, the `base_hash` a tenant
/// refinement records (B2) and the hash `tag_diff` compares.
pub fn fields_hash(fields: &Value) -> String {
    format!("sha256:{}", sha256_hex(&canonical_json(fields)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys() {
        let a = json!({"b": {"y": 1, "x": [true, null, {"d": 1, "c": "q\"z"}]}, "a": 1.5});
        assert_eq!(
            canonical_json(&a),
            r#"{"a":1.5,"b":{"x":[true,null,{"c":"q\"z","d":1}],"y":1}}"#
        );
        let b: Value =
            serde_json::from_str(r#"{"a":1.5,"b":{"x":[true,null,{"c":"q\"z","d":1}],"y":1}}"#)
                .expect("json");
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn keys_sort_by_utf16_code_units() {
        // U+1F600 is F0.. in UTF-8 but D83D DE00 in UTF-16; U+E000 is EE.. / E000. UTF-8 byte
        // order puts U+E000 first, UTF-16 (care-bus, RFC 8785) puts the emoji first.
        let v = json!({"\u{e000}": 1, "\u{1f600}": 2, "a": 0});
        assert_eq!(
            canonical_json(&v),
            "{\"a\":0,\"\u{1f600}\":2,\"\u{e000}\":1}"
        );
        let mut utf8: Vec<&str> = vec!["\u{e000}", "\u{1f600}"];
        utf8.sort_unstable();
        assert_eq!(
            utf8,
            ["\u{e000}", "\u{1f600}"],
            "UTF-8 order really differs"
        );
    }

    #[test]
    fn fields_hash_is_prefixed_and_order_free() {
        let a = json!({"label": "Order", "synonyms": ["purchase"]});
        let b: Value =
            serde_json::from_str(r#"{"synonyms":["purchase"],"label":"Order"}"#).expect("json");
        let h = fields_hash(&a);
        assert!(h.starts_with("sha256:"));
        assert_eq!(h.len(), "sha256:".len() + 64);
        assert_eq!(h, fields_hash(&b));
        assert_ne!(
            h,
            fields_hash(&json!({"label": "Purchase", "synonyms": ["purchase"]}))
        );
        assert_eq!(
            fields_hash(&json!({})),
            format!("sha256:{}", sha256_hex("{}"))
        );
    }
}
