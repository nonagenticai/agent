//! The rows of an uploaded discovery export (T14a, T15a F-52): CSV / XLSX through B3's `crate::tabular` whole-table
//! readers (R14: no calamine), JSON here. Pure functions over bytes; nothing is stored.
//!
//! - Format by extension: `.csv` / `.txt` → CSV, `.xlsx` → XLSX (first sheet, cell values only), `.json` → an array
//!   of strings (column `text`) or of objects (columns = the keys, first-seen order). Anything else → 422
//!   `unsupported_file`.
//! - Text column: the one chosen (`text_column`, 422 `unknown_column` when the file has no such column), else the
//!   first column named `text|body|message|description|subject` (any case), else the first column whose sample is
//!   mostly non-numeric, else the first column.
//! - Count column (F-52): optional; it multiplies the row's weight. A non-numeric cell → 422 `count_not_numeric`; a
//!   blank cell counts 1; 0 drops the row.
//! - Dedupe by [`scrub::text_hash`]; a row [`scrub::must_drop`] refuses is counted in `rows` but never kept.
//!   No usable row → 422 `empty_file`; more than [`MAX_ROWS`] rows → 413 `too_large`.
use std::collections::HashMap;

use serde_json::{Value, json};

use super::scrub;
use crate::error::ApiError;
use crate::tabular::{self, PreviewError};

/// Rows an import takes.
pub const MAX_ROWS: usize = 50_000;
/// Sample rows a preview returns.
pub const SAMPLE_ROWS: usize = 5;
/// Column names read as the ticket text, in order of preference.
const TEXT_NAMES: [&str; 5] = ["text", "body", "message", "description", "subject"];
/// Column names suggested as the count column.
const COUNT_NAMES: [&str; 7] = [
    "count",
    "tickets",
    "volume",
    "occurrences",
    "total",
    "n",
    "frequency",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
    Xlsx,
}

impl Format {
    /// From the file name's extension (case-insensitive).
    pub fn of(file: &str) -> Option<Format> {
        let ext = file.rsplit_once('.')?.1.to_ascii_lowercase();
        match ext.as_str() {
            "csv" | "txt" => Some(Format::Csv),
            "json" => Some(Format::Json),
            "xlsx" => Some(Format::Xlsx),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Xlsx => "xlsx",
        }
    }
}

/// A parsed file: its columns and every data row (padded to the columns).
#[derive(Debug)]
pub struct Table {
    pub format: Format,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// One distinct row to store.
#[derive(Debug, Clone, PartialEq)]
pub struct Distinct {
    pub text_hash: String,
    pub excerpt: String,
    pub weight: i32,
}

/// What an upload stores: `rows` input rows (dropped ones included), the distinct ones in first-seen order.
#[derive(Debug)]
pub struct Collected {
    pub rows: i32,
    pub distinct: Vec<Distinct>,
    pub text_column: String,
    pub count_column: Option<String>,
}

pub fn unsupported_file() -> ApiError {
    ApiError::Unprocessable(
        "unsupported_file",
        "Upload a .csv, .json or .xlsx export.".into(),
    )
}

pub fn empty_file() -> ApiError {
    ApiError::Unprocessable("empty_file", "The file has no rows with text.".into())
}

pub fn too_large() -> ApiError {
    ApiError::TooLarge(
        "too_large",
        format!("An import takes up to {MAX_ROWS} rows and 20 MB."),
    )
}

fn unknown_column(name: &str) -> ApiError {
    let shown = scrub::cut(name, 64);
    ApiError::Unprocessable(
        "unknown_column",
        format!("The file has no column \"{shown}\"."),
    )
}

fn count_not_numeric(column: &str) -> ApiError {
    let shown = scrub::cut(column, 64);
    ApiError::Unprocessable(
        "count_not_numeric",
        format!("Column \"{shown}\" holds a value that is not a count."),
    )
}

fn table_error(e: PreviewError) -> ApiError {
    match e {
        PreviewError::TooLarge(_) => too_large(),
        PreviewError::Empty => empty_file(),
        PreviewError::TooManyColumns
        | PreviewError::Unreadable(_)
        | PreviewError::SheetNotFound(_) => {
            ApiError::Unprocessable("unsupported_file", format!("{e}."))
        }
    }
}

/// Parse `bytes` as `format` (blocking work: call it off the async runtime).
pub fn parse(format: Format, bytes: &[u8]) -> Result<Table, ApiError> {
    let (columns, rows) = match format {
        Format::Csv => tabular::rows_csv(bytes, MAX_ROWS).map_err(table_error)?,
        Format::Xlsx => tabular::rows_xlsx(bytes, MAX_ROWS).map_err(table_error)?,
        Format::Json => json_rows(bytes)?,
    };
    if rows.is_empty() {
        return Err(empty_file());
    }
    Ok(Table {
        format,
        columns,
        rows,
    })
}

/// A JSON array of strings (column `text`) or of objects (columns = keys, first-seen order).
fn json_rows(bytes: &[u8]) -> Result<(Vec<String>, Vec<Vec<String>>), ApiError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| {
        ApiError::Unprocessable("unsupported_file", "The file is not valid JSON.".into())
    })?;
    let Value::Array(items) = v else {
        return Err(ApiError::Unprocessable(
            "unsupported_file",
            "A JSON export must be an array of strings or objects.".into(),
        ));
    };
    if items.len() > MAX_ROWS {
        return Err(too_large());
    }
    let cell = |v: &Value| -> String {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        scrub::cut(&s, tabular::MAX_CELL_BYTES).to_string()
    };
    if items.iter().all(Value::is_string) {
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|v| vec![cell(v)])
            .filter(|r| !r[0].trim().is_empty())
            .collect();
        return Ok((vec!["text".to_string()], rows));
    }
    let mut columns: Vec<String> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for it in &items {
        let Value::Object(m) = it else {
            return Err(ApiError::Unprocessable(
                "unsupported_file",
                "A JSON export must be an array of strings or an array of objects.".into(),
            ));
        };
        for k in m.keys() {
            if !index.contains_key(k) {
                if columns.len() == tabular::MAX_COLUMNS {
                    return Err(ApiError::Unprocessable(
                        "unsupported_file",
                        "the table has more than 1,000 columns.".into(),
                    ));
                }
                index.insert(k.clone(), columns.len());
                columns.push(k.clone());
            }
        }
    }
    let mut rows = Vec::with_capacity(items.len());
    for it in &items {
        let mut r = vec![String::new(); columns.len()];
        if let Value::Object(m) = it {
            for (k, v) in m {
                if let Some(&i) = index.get(k) {
                    r[i] = cell(v);
                }
            }
        }
        if r.iter().any(|c| !c.trim().is_empty()) {
            rows.push(r);
        }
    }
    Ok((columns, rows))
}

fn position(columns: &[String], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|c| c == name)
        .or_else(|| columns.iter().position(|c| c.eq_ignore_ascii_case(name)))
}

fn numeric(cell: &str) -> Option<f64> {
    let t = cell.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// The default text column (module docs).
pub fn suggested_text_column(t: &Table) -> usize {
    for n in TEXT_NAMES {
        if let Some(i) = t
            .columns
            .iter()
            .position(|c| c.trim().eq_ignore_ascii_case(n))
        {
            return i;
        }
    }
    let sample: Vec<&Vec<String>> = t.rows.iter().take(20).collect();
    (0..t.columns.len())
        .find(|&i| {
            let filled: Vec<&str> = sample
                .iter()
                .map(|r| r[i].trim())
                .filter(|c| !c.is_empty())
                .collect();
            let text = filled.iter().filter(|c| numeric(c).is_none()).count();
            !filled.is_empty() && text * 2 > filled.len()
        })
        .unwrap_or(0)
}

/// The suggested count column: a column named like a count whose sample is numeric.
pub fn suggested_count_column(t: &Table) -> Option<usize> {
    COUNT_NAMES.iter().find_map(|n| {
        let i = t
            .columns
            .iter()
            .position(|c| c.trim().eq_ignore_ascii_case(n))?;
        let numeric_sample = t
            .rows
            .iter()
            .take(20)
            .map(|r| r[i].trim())
            .filter(|c| !c.is_empty())
            .all(|c| numeric(c).is_some());
        numeric_sample.then_some(i)
    })
}

/// `previewDiscoveryImport` (F-52): `{format, columns, sample_rows, rows, suggested_text_column,
/// suggested_count_column}`. Sample cells are scrubbed excerpts, never raw text.
pub fn preview(t: &Table) -> Value {
    let sample: Vec<Vec<String>> = t
        .rows
        .iter()
        .filter(|r| !r.iter().any(|c| scrub::must_drop(c)))
        .take(SAMPLE_ROWS)
        .map(|r| r.iter().map(|c| scrub::excerpt(c)).collect())
        .collect();
    json!({
        "format": t.format.as_str(),
        "columns": t.columns,
        "sample_rows": sample,
        "rows": t.rows.len(),
        "suggested_text_column": t.columns.get(suggested_text_column(t)),
        "suggested_count_column": suggested_count_column(t).and_then(|i| t.columns.get(i)),
    })
}

/// The rows to store (module docs).
pub fn collect(
    t: &Table,
    text_column: Option<&str>,
    count_column: Option<&str>,
) -> Result<Collected, ApiError> {
    let ti = match text_column {
        Some(n) => position(&t.columns, n).ok_or_else(|| unknown_column(n))?,
        None => suggested_text_column(t),
    };
    let ci = match count_column {
        Some(n) => Some(position(&t.columns, n).ok_or_else(|| unknown_column(n))?),
        None => None,
    };
    let mut distinct: Vec<Distinct> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut rows = 0i32;
    for r in &t.rows {
        let text = r[ti].trim();
        if text.is_empty() {
            continue;
        }
        rows = rows.saturating_add(1);
        let count: i64 = match ci {
            None => 1,
            Some(i) => {
                let c = r[i].trim();
                if c.is_empty() {
                    1
                } else {
                    let f = numeric(c)
                        .filter(|f| *f >= 0.0)
                        .ok_or_else(|| count_not_numeric(&t.columns[i]))?;
                    // a fractional count rounds; an absurd one saturates
                    f.round().min(f64::from(i32::MAX)) as i64
                }
            }
        };
        if count == 0 || scrub::must_drop(text) {
            continue;
        }
        let hash = scrub::text_hash(text);
        match at.get(&hash) {
            Some(&i) => {
                let w = i64::from(distinct[i].weight).saturating_add(count);
                distinct[i].weight = i32::try_from(w).unwrap_or(i32::MAX);
            }
            None => {
                at.insert(hash.clone(), distinct.len());
                distinct.push(Distinct {
                    text_hash: hash,
                    excerpt: scrub::excerpt(text),
                    weight: i32::try_from(count).unwrap_or(i32::MAX),
                });
            }
        }
    }
    if distinct.is_empty() {
        return Err(empty_file());
    }
    Ok(Collected {
        rows,
        distinct,
        text_column: t.columns[ti].clone(),
        count_column: ci.map(|i| t.columns[i].clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csv(s: &str) -> Table {
        parse(Format::Csv, s.as_bytes()).unwrap()
    }

    #[test]
    fn formats_by_extension() {
        assert_eq!(Format::of("a.CSV"), Some(Format::Csv));
        assert_eq!(Format::of("a.json"), Some(Format::Json));
        assert_eq!(Format::of("a.xlsx"), Some(Format::Xlsx));
        assert_eq!(Format::of("a.pdf"), None);
        assert_eq!(Format::of("noext"), None);
    }

    #[test]
    fn default_text_column_prefers_known_names_then_text() {
        let t = csv("id,Subject,n\n1,where is my order,3\n2,refund please,1\n");
        assert_eq!(suggested_text_column(&t), 1);
        assert_eq!(suggested_count_column(&t), Some(2));
        let t = csv("id,note\n1,hello there\n2,refund\n");
        assert_eq!(suggested_text_column(&t), 1);
    }

    #[test]
    fn duplicates_merge_with_weight_and_counts_multiply() {
        let t = csv("text,count\nWhere is my order,2\nwhere is  my order,3\nrefund,1\n");
        let c = collect(&t, None, Some("count")).unwrap();
        assert_eq!(c.rows, 3);
        assert_eq!(c.distinct.len(), 2);
        assert_eq!(c.distinct[0].weight, 5);
        let c = collect(&t, Some("text"), None).unwrap();
        assert_eq!(c.distinct[0].weight, 2);
    }

    #[test]
    fn unknown_and_non_numeric_columns_are_refused() {
        let t = csv("text,count\nhello,x\n");
        let e = collect(&t, Some("body"), None).unwrap_err();
        assert!(matches!(e, ApiError::Unprocessable("unknown_column", _)));
        let e = collect(&t, None, Some("count")).unwrap_err();
        assert!(matches!(e, ApiError::Unprocessable("count_not_numeric", _)));
    }

    #[test]
    fn json_strings_and_objects_parse() {
        let t = parse(Format::Json, br#"["a", "b", ""]"#).unwrap();
        assert_eq!(t.columns, ["text"]);
        assert_eq!(t.rows.len(), 2);
        let t = parse(
            Format::Json,
            br#"[{"id": 1, "body": "x"}, {"body": "y", "n": 2}]"#,
        )
        .unwrap();
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.columns[suggested_text_column(&t)], "body");
        assert!(parse(Format::Json, br#"{"a": 1}"#).is_err());
        assert!(parse(Format::Json, b"[]").is_err());
    }

    #[test]
    fn dropped_rows_are_counted_not_kept() {
        let t = csv("text\nmy password is hunter2\nhello\n");
        let c = collect(&t, None, None).unwrap();
        assert_eq!(c.rows, 2);
        assert_eq!(c.distinct.len(), 1);
        let t = csv("text\nmy password is hunter2\n");
        assert!(collect(&t, None, None).is_err());
    }
}
