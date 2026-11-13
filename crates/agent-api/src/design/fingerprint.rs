//! The draft fingerprint (spec B2 §2.7): a short hash of the job's change keys
//! ([`super::ledger::keys`]), so a saved test or trace can tell which draft it ran against (B4 `test_case.draft_fp`,
//! B8 `draft_test_run.draft_fp`). A pure port of `web/src/lib/journey/J17Validation.ts` `draftFingerprint` /
//! `shortHash`: the keys sorted by UTF-16 code units (JS `Array.prototype.sort`), joined with `|`, hashed with djb2
//! over UTF-16 code units (32-bit wrapping), as 8 hex digits cut to 7.

/// Fixture `shortHash`.
fn short_hash(s: &str) -> String {
    let mut h: u32 = 5381;
    for unit in s.encode_utf16() {
        h = (h << 5).wrapping_add(h).wrapping_add(u32::from(unit));
    }
    let mut hex = format!("{h:08x}");
    hex.truncate(7);
    hex
}

/// Fixture `draftFingerprint(keys)`: order-independent.
pub fn fingerprint(keys: &[String]) -> String {
    let mut sorted: Vec<(Vec<u16>, &str)> = keys
        .iter()
        .map(|k| (k.encode_utf16().collect(), k.as_str()))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let joined: Vec<&str> = sorted.into_iter().map(|(_, k)| k).collect();
    short_hash(&joined.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(ks: &[&str]) -> Vec<String> {
        ks.iter().map(|k| k.to_string()).collect()
    }

    /// The expected values were computed by the fixture's own `draftFingerprint` (node, 2026-10-20).
    #[test]
    #[allow(non_snake_case)]
    fn fingerprint_equals_fixture_draftFingerprint() {
        assert_eq!(fingerprint(&[]), "0000150");
        let a = keys(&[
            r#"rule:RET-01@{"v":{"window_days":"30"}}"#,
            "gate:GM:request@null",
        ]);
        let b = keys(&[
            "gate:GM:request@null",
            r#"rule:RET-01@{"v":{"window_days":"30"}}"#,
        ]);
        assert_eq!(fingerprint(&a), "7117434");
        assert_eq!(fingerprint(&b), "7117434");
        // UTF-16 order and code units: an astral character sorts after every BMP letter and hashes as two units
        let c = keys(&[r#"entity:Ördér@{"v":"é"}"#, "a", "Z", "😀x"]);
        assert_eq!(fingerprint(&c), "68cac24");
    }
}
