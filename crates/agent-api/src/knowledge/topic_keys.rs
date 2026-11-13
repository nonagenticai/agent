//! Platform topic keys: the one validator for every list the agent sends to strata as `topic_keys` or `supersedes`
//! (PK-8 / PK-9). The limits are strata_rag's, which strata_ingest #15 shares: at most [`MAX_KEYS`] keys, each 1 to
//! [`MAX_KEY_CHARS`] characters of `[a-z0-9_./-]`. strata answers 400 past them; the agent refuses first with a 422
//! naming the rule, so nothing invalid reaches strata (tenant `supersedes` on sources and uploads, platform
//! `topic_keys` in curation).

/// Most topic keys one document, source or upload names (strata's limit).
pub const MAX_KEYS: usize = 32;
/// Longest topic key (strata's limit; the charset is ASCII, so bytes = characters).
pub const MAX_KEY_CHARS: usize = 128;

/// A topic key: 1 to [`MAX_KEY_CHARS`] characters of `[a-z0-9_./-]`.
pub fn valid(k: &str) -> bool {
    (1..=MAX_KEY_CHARS).contains(&k.len())
        && k.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'/' | b'-')
        })
}

/// `keys` trimmed, checked, sorted and de-duplicated: at most [`MAX_KEYS`] (counted after de-duplication, i.e. what
/// strata receives), each [`valid`]. `Err` is a message naming the broken rule; `what` names the field in it
/// (`supersedes`, `topic_keys`). An empty list is allowed here; a caller that needs one key checks that itself.
pub fn normalize<'a>(
    what: &str,
    keys: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = keys.into_iter().map(|k| k.trim().to_string()).collect();
    if let Some(bad) = out.iter().find(|k| !valid(k)) {
        let shown: String = bad.chars().take(80).collect();
        return Err(format!(
            "{what}: {shown:?} is not a topic key: 1 to {MAX_KEY_CHARS} characters of [a-z0-9_./-]."
        ));
    }
    out.sort_unstable();
    out.dedup();
    if out.len() > MAX_KEYS {
        return Err(format!("{what} lists at most {MAX_KEYS} topic keys."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strata_limits_and_charset() {
        assert_eq!(
            normalize("supersedes", [" returns/policy ", "hours", "hours"]).unwrap(),
            ["hours", "returns/policy"]
        );
        assert!(normalize("supersedes", []).unwrap().is_empty());
        assert!(valid("hr/leave.v2_x-y") && valid(&"k".repeat(MAX_KEY_CHARS)));
        let long = "k".repeat(MAX_KEY_CHARS + 1);
        for bad in ["", "Hours", "a b", "a\tb", "é", "hr:leave", long.as_str()] {
            assert!(!valid(bad), "{bad:?}");
            assert!(
                normalize("topic_keys", [bad])
                    .unwrap_err()
                    .contains("[a-z0-9_./-]"),
                "{bad:?}"
            );
        }
        let max: Vec<String> = (0..MAX_KEYS).map(|i| format!("k{i}")).collect();
        assert_eq!(
            normalize("supersedes", max.iter().map(String::as_str))
                .unwrap()
                .len(),
            MAX_KEYS
        );
        let over: Vec<String> = (0..=MAX_KEYS).map(|i| format!("k{i}")).collect();
        assert!(
            normalize("supersedes", over.iter().map(String::as_str))
                .unwrap_err()
                .contains("at most 32")
        );
        // 33 entries that de-duplicate to 32 are what strata receives: allowed
        let mut dup = max.clone();
        dup.push("k0".into());
        assert!(normalize("supersedes", dup.iter().map(String::as_str)).is_ok());
    }
}
