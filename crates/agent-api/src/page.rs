//! Paging (D1: every list is `{items, next}`; `next` is the cursor of the next, older page, `null` at the end).
//! Audit lists page by the bigint row id (`before=<id>`); the other lists of B1 are short and answer one page.
use serde_json::{Value, json};

use crate::error::{ApiError, ApiResult};

/// `{items, next}`.
pub fn paged(items: Vec<Value>, next: Option<String>) -> Value {
    json!({"items": items, "next": next})
}

/// `limit` from the query: absent or empty → `default`; else an integer in `1..=max`, or 422 `invalid_limit`.
pub fn limit(raw: Option<&str>, default: i64, max: i64) -> ApiResult<i64> {
    match raw.map(str::trim) {
        None | Some("") => Ok(default),
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=max).contains(n))
            .ok_or_else(|| {
                ApiError::Unprocessable(
                    "invalid_limit",
                    format!("limit must be an integer from 1 to {max}"),
                )
            }),
    }
}

/// A bigint id cursor (`before=<id>`): absent or empty → `None`; else a positive integer, or 422 `invalid_cursor`.
pub fn id_cursor(raw: Option<&str>) -> ApiResult<Option<i64>> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| {
                ApiError::Unprocessable(
                    "invalid_cursor",
                    "before must be a cursor returned as next by this list".into(),
                )
            }),
    }
}

/// A query flag: `true`/`1` → true; absent, empty, `false`/`0` → false; anything else 422 `invalid_query`.
pub fn flag(raw: Option<&str>, name: &str) -> ApiResult<bool> {
    match raw.map(str::trim) {
        None | Some("" | "false" | "0") => Ok(false),
        Some("true" | "1") => Ok(true),
        Some(_) => Err(ApiError::Unprocessable(
            "invalid_query",
            format!("{name} must be true or false"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_cursors_and_flags() {
        assert_eq!(limit(None, 50, 200).unwrap(), 50);
        assert_eq!(limit(Some("200"), 50, 200).unwrap(), 200);
        assert_eq!(
            limit(Some("0"), 50, 200).unwrap_err().code(),
            "invalid_limit"
        );
        assert_eq!(
            limit(Some("x"), 50, 200).unwrap_err().code(),
            "invalid_limit"
        );
        assert_eq!(id_cursor(Some("17")).unwrap(), Some(17));
        assert_eq!(id_cursor(Some("")).unwrap(), None);
        assert_eq!(id_cursor(Some("-1")).unwrap_err().code(), "invalid_cursor");
        assert!(flag(Some("true"), "x").unwrap());
        assert!(!flag(None, "x").unwrap());
        assert_eq!(flag(Some("yes"), "x").unwrap_err().code(), "invalid_query");
        assert_eq!(paged(vec![], None), json!({"items": [], "next": null}));
    }
}
