//! Validation questions (B8 T12 add; T12a F-50 edit, soft delete, CSV import). Live data (D69): nothing here is a
//! design write. A question needs a question, the expected answer and the reference passage the answer must cite;
//! `activity` (`stage:activity`) is optional. At most [`MAX_LIVE`] live questions per job.
use axum::http::StatusCode;
use serde_json::json;

use crate::error::{ApiError, ApiResult};

/// Live questions a job may hold, and rows one import may carry.
pub const MAX_LIVE: usize = 500;
/// The largest CSV an import reads.
pub const MAX_IMPORT_BYTES: usize = 4 * 1024 * 1024;

/// A question's fields, trimmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fields {
    pub question: String,
    pub expected: String,
    pub reference: String,
    pub activity: String,
}

/// Why a field set is refused (`None` = ok).
pub fn problem(f: &Fields) -> Option<&'static str> {
    if f.question.is_empty() || f.expected.is_empty() || f.reference.is_empty() {
        return Some("fields_required");
    }
    if f.question.chars().count() > 2000
        || f.expected.chars().count() > 4000
        || f.reference.chars().count() > 500
    {
        return Some("too_long");
    }
    if !f.activity.is_empty() && !activity_ok(&f.activity) {
        return Some("invalid_activity");
    }
    None
}

/// `stage:activity`, each a lower-snake slug (0172 CHECK).
pub fn activity_ok(a: &str) -> bool {
    let slug = |s: &str, max: usize| {
        let mut ch = s.chars();
        matches!(ch.next(), Some('a'..='z'))
            && s.len() <= max + 1
            && ch.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    match a.split_once(':') {
        Some((st, ac)) => slug(st, 40) && slug(ac, 60),
        None => false,
    }
}

/// The 422 of [`problem`].
pub fn refuse(code: &'static str) -> ApiError {
    let msg = match code {
        "fields_required" => "Question, expected answer and reference citation are all required.",
        "too_long" => {
            "The question is at most 2000 characters, the answer 4000 and the reference 500."
        }
        _ => "The activity is stage:activity in lower_snake_case.",
    };
    ApiError::Unprocessable(code, msg.into())
}

/// 409 `no_release`.
pub fn no_release() -> ApiError {
    ApiError::Conflict(
        "no_release",
        "No release yet: a question is graded on a release cut.".into(),
    )
}

/// 413 `too_many_questions`.
pub fn too_many() -> ApiError {
    ApiError::TooLarge(
        "too_many_questions",
        format!("A job holds at most {MAX_LIVE} questions."),
    )
}

/// 404 `question_not_found`.
pub fn not_found() -> ApiError {
    ApiError::Missing("question_not_found", "No such question.".into())
}

/// Parse an import CSV (header `question,expected,reference[,activity]`, any order, case-insensitive; `,` `;` or
/// tab). All or nothing: any bad row → 422 `invalid_rows` with the file's line numbers; more than [`MAX_LIVE`] rows
/// → 413 `too_many_questions`.
pub fn parse_csv(bytes: &[u8]) -> ApiResult<Vec<Fields>> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(empty_file());
    }
    let delimiter = (*b",;\t")
        .into_iter()
        .max_by_key(|d| {
            bytes
                .split(|b| *b == b'\n')
                .next()
                .unwrap_or_default()
                .iter()
                .filter(|b| *b == d)
                .count()
        })
        .unwrap_or(b',');
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(bytes);
    let header: Vec<String> = rdr
        .headers()
        .map_err(|_| unreadable())?
        .iter()
        .map(|h| h.trim().to_lowercase())
        .collect();
    let col = |name: &str| header.iter().position(|h| h == name);
    let (Some(qi), Some(ei), Some(ri)) = (col("question"), col("expected"), col("reference"))
    else {
        return Err(ApiError::Unprocessable(
            "missing_columns",
            "The CSV needs the columns question, expected and reference (activity is optional)."
                .into(),
        ));
    };
    let ai = col("activity");
    let mut out = Vec::new();
    let mut bad: Vec<u64> = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        let line = i as u64 + 2;
        let Ok(rec) = rec else {
            bad.push(line);
            continue;
        };
        if rec.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        if out.len() + bad.len() >= MAX_LIVE {
            return Err(too_many());
        }
        let get = |k: Option<usize>| k.and_then(|k| rec.get(k)).unwrap_or("").trim().to_string();
        let f = Fields {
            question: get(Some(qi)),
            expected: get(Some(ei)),
            reference: get(Some(ri)),
            activity: get(ai),
        };
        if problem(&f).is_some() {
            bad.push(line);
        } else {
            out.push(f);
        }
    }
    if !bad.is_empty() {
        return Err(ApiError::detailed(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_rows",
            "Some rows are not valid: fix them and import the file again (nothing was imported).",
        )
        .with("rows", json!(bad)));
    }
    if out.is_empty() {
        return Err(empty_file());
    }
    Ok(out)
}

fn empty_file() -> ApiError {
    ApiError::Unprocessable("empty_file", "The file has no questions.".into())
}

fn unreadable() -> ApiError {
    ApiError::Unprocessable("unsupported_file", "The file is not a readable CSV.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_question_validates_fields() {
        let mut f = Fields {
            question: "q".into(),
            expected: "e".into(),
            reference: "r".into(),
            activity: String::new(),
        };
        assert_eq!(problem(&f), None);
        f.activity = "after_sales:return_item".into();
        assert_eq!(problem(&f), None);
        f.activity = "Return".into();
        assert_eq!(problem(&f), Some("invalid_activity"));
        f.activity.clear();
        f.expected.clear();
        assert_eq!(problem(&f), Some("fields_required"));
    }

    #[test]
    fn csv_header_any_order_and_bad_rows_reject_whole_file() {
        let ok = parse_csv(b"Reference,question,expected\npolicy \xC2\xA72,How long?,30 days\n")
            .unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].reference, "policy §2");
        let e = parse_csv(b"question,expected\nq,e\n").unwrap_err();
        assert_eq!(e.code(), "missing_columns");
        let e = parse_csv(b"question;expected;reference;activity\nq;e;r;\n;e;r;\nq;e;r;Bad\n")
            .unwrap_err();
        assert_eq!(e.code(), "invalid_rows");
        assert_eq!(parse_csv(b"  \n").unwrap_err().code(), "empty_file");
    }

    #[test]
    fn import_500_ok_501_413() {
        let mut csv = String::from("question,expected,reference\n");
        for i in 0..MAX_LIVE {
            csv.push_str(&format!("q{i},e,r\n"));
        }
        assert_eq!(parse_csv(csv.as_bytes()).unwrap().len(), MAX_LIVE);
        csv.push_str("one,more,row\n");
        assert_eq!(
            parse_csv(csv.as_bytes()).unwrap_err().code(),
            "too_many_questions"
        );
    }
}
