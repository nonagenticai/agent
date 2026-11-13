//! Discovery import rows in the database (T14a upload, T15a delete). The parsing is crate::discovery::rows; the
//! classification crate::discovery::worker.
use sqlx::PgConnection;

use super::rows::{Collected, Format};
use crate::error::{ApiError, ApiResult};
use crate::tenancy::JobCtx;

/// Longest file name stored (characters).
const MAX_FILE_CHARS: usize = 255;

pub fn import_not_found() -> ApiError {
    ApiError::Missing(
        "import_not_found",
        "This job has no discovery import with that id.".into(),
    )
}

/// The stored file name: the last path segment, control characters dropped, at most 255 characters.
pub fn file_label(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_FILE_CHARS)
        .collect();
    let clean = clean.trim().to_string();
    if clean.is_empty() {
        "upload".into()
    } else {
        clean
    }
}

/// Insert the import (`classifying`) and its distinct rows; returns its id (`imp-NNNN`).
pub async fn insert(
    c: &mut PgConnection,
    job: &JobCtx,
    file: &str,
    format: Format,
    got: &Collected,
) -> ApiResult<String> {
    let distinct = i32::try_from(got.distinct.len()).unwrap_or(i32::MAX);
    let id: String = sqlx::query_scalar(
        "INSERT INTO discovery_import (tenant_id, job_id, file, rows, distinct_rows, uploaded_by, format, \
                                       text_column, count_column) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .bind(file)
    .bind(got.rows.max(distinct))
    .bind(distinct)
    .bind(&job.t.caller.sub)
    .bind(format.as_str())
    .bind(&got.text_column)
    .bind(got.count_column.as_deref())
    .fetch_one(&mut *c)
    .await?;
    let hashes: Vec<&str> = got.distinct.iter().map(|d| d.text_hash.as_str()).collect();
    let excerpts: Vec<&str> = got.distinct.iter().map(|d| d.excerpt.as_str()).collect();
    let weights: Vec<i32> = got.distinct.iter().map(|d| d.weight).collect();
    let ords: Vec<i32> = (0..distinct).collect();
    sqlx::query(
        "INSERT INTO discovery_row (tenant_id, job_id, import_id, text_hash, ord, excerpt, weight) \
         SELECT $1, $2, $3, u.h, u.o, nullif(u.e, ''), u.w \
           FROM UNNEST($4::text[], $5::integer[], $6::text[], $7::integer[]) AS u(h, o, e, w)",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .bind(&id)
    .bind(&hashes)
    .bind(&ords)
    .bind(&excerpts)
    .bind(&weights)
    .execute(&mut *c)
    .await?;
    Ok(id)
}

/// Delete the import (rows and clusters cascade, 0178; decisions stay). Returns the status it had: a running worker
/// sees the row gone before its next batch and stops. 404 `import_not_found`.
pub async fn delete(c: &mut PgConnection, job: &JobCtx, id: &str) -> ApiResult<String> {
    let status: Option<String> = sqlx::query_scalar(
        "DELETE FROM discovery_import WHERE tenant_id = $1 AND job_id = $2 AND id = $3 RETURNING status",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .bind(id)
    .fetch_optional(&mut *c)
    .await?;
    status.ok_or_else(import_not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_label_keeps_the_base_name() {
        assert_eq!(file_label("C:\\exports\\zendesk.csv"), "zendesk.csv");
        assert_eq!(file_label("a/b/tickets.json"), "tickets.json");
        assert_eq!(file_label("  "), "upload");
        assert_eq!(file_label(&"x".repeat(300)).chars().count(), 255);
    }
}
