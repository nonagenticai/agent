//! A job's canned replies (SW T7, migration 0193 `support_canned_reply`): saved texts a staff member inserts into the
//! composer. `CannedReply = {id, title, body, created_by, updated_at}`.
//!
//! - `GET /api/jobs/{job}/canned-replies`: any member of the job's tenant; oldest first.
//! - `POST /api/jobs/{job}/canned-replies` `{title, body}` (`support_manager`): 201 `CannedReply`. Both are trimmed;
//!   a title of 1–80 and a body of 1–4000 characters, else 422 `invalid_canned_reply`.
//! - `DELETE /api/jobs/{job}/canned-replies/{id}` (`support_manager`): 204; 404 `canned_reply_not_found`.
//!
//! Both writes are audited (`canned_reply.create` with the title's length, `canned_reply.delete`); never the text.
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::conversations::{iso, store};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, role};

/// The longest title (characters).
pub const TITLE_MAX: usize = 80;
/// The longest body (characters).
pub const BODY_MAX: usize = 4000;

/// One canned reply (the web's `CannedReply`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CannedReply {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub created_by: String,
    pub updated_at: String,
}

const COLS: &str = "id, title, body, created_by, updated_at";

fn of_row(r: &sqlx::postgres::PgRow) -> CannedReply {
    CannedReply {
        id: r.get("id"),
        title: r.get("title"),
        body: r.get("body"),
        created_by: r.get("created_by"),
        updated_at: iso(r.get("updated_at")),
    }
}

fn invalid(msg: &str) -> ApiError {
    ApiError::Unprocessable("invalid_canned_reply", msg.to_string())
}

fn not_found() -> ApiError {
    ApiError::Missing(
        "canned_reply_not_found",
        "No such saved reply in this job.".into(),
    )
}

/// The trimmed `(title, body)` of a new reply, else 422 `invalid_canned_reply`.
pub fn checked(title: &str, body: &str) -> ApiResult<(String, String)> {
    store::require_clean([title, body])?;
    let (title, body) = (title.trim(), body.trim());
    let n = title.chars().count();
    if n == 0 || n > TITLE_MAX {
        return Err(invalid("A title of 1 to 80 characters."));
    }
    let n = body.chars().count();
    if n == 0 || n > BODY_MAX {
        return Err(invalid("A reply of 1 to 4000 characters."));
    }
    Ok((title.to_string(), body.to_string()))
}

/// `GET /api/jobs/{job}/canned-replies`.
pub async fn canned_replies(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<CannedReply>>> {
    let mut tx = j.begin(&st.db).await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM support_canned_reply WHERE job_id = $1 ORDER BY updated_at, id"
    )))
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(rows.iter().map(of_row).collect()))
}

#[derive(Deserialize)]
pub struct NewCanned {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

/// `POST /api/jobs/{job}/canned-replies`.
pub async fn create_canned_reply(
    JobRequires(j, _): JobRequires<role::SupportManager>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<NewCanned>,
) -> ApiResult<(StatusCode, Json<CannedReply>)> {
    let (title, body) = checked(&b.title, &b.body)?;
    let mut tx = j.begin(&st.db).await?;
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO support_canned_reply (job_id, title, body, created_by) VALUES ($1, $2, $3, $4) \
         RETURNING {COLS}"
    )))
    .bind(j.job_id)
    .bind(&title)
    .bind(&body)
    .bind(&j.t.caller.sub)
    .fetch_one(&mut *tx)
    .await?;
    let made = of_row(&r);
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("canned_reply.create", "canned_reply", made.id.to_string())
            .job(&j.job_key)
            .detail("title_chars", made.title.chars().count()),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(made)))
}

/// `DELETE /api/jobs/{job}/canned-replies/{id}`.
pub async fn delete_canned_reply(
    JobRequires(j, _): JobRequires<role::SupportManager>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    let id = Uuid::parse_str(id.trim()).map_err(|_| not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let done = sqlx::query("DELETE FROM support_canned_reply WHERE id = $1 AND job_id = $2")
        .bind(id)
        .bind(j.job_id)
        .execute(&mut *tx)
        .await?;
    if done.rows_affected() == 0 {
        return Err(not_found());
    }
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("canned_reply.delete", "canned_reply", id.to_string()).job(&j.job_key),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_bodies_are_trimmed_and_bounded() {
        assert_eq!(
            checked("  Hi ", " Body ").unwrap(),
            ("Hi".to_string(), "Body".to_string())
        );
        assert_eq!(
            checked(" ", "b").unwrap_err().code(),
            "invalid_canned_reply"
        );
        assert!(checked(&"t".repeat(80), "b").is_ok());
        assert!(checked(&"t".repeat(81), "b").is_err());
        assert!(checked("t", &"b".repeat(4000)).is_ok());
        assert!(checked("t", &"b".repeat(4001)).is_err());
    }
}
