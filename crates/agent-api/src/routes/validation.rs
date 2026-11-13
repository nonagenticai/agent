//! The validation grid (J17; B8 T12, T12a):
//!
//! - `GET /api/jobs/{job}/validation` — any member: `Paged<ValidationCut>`, newest release first.
//! - `POST /api/jobs/{job}/validation/{release}/run` — `release_manager`: grade the cut now (B8-5: concurrency 8,
//!   20 s per question, 85 s in all; rows not reached keep their state). Writes a `manual` grid run; 409
//!   `grid_running` while one is active for the release, 409 `no_questions`, 404 `release_not_found`, 503
//!   `unavailable` when no question could be answered (nothing stored).
//! - `PUT /api/jobs/{job}/validation/{release}/rows/{row}` `{verdict, reason?}` — `release_manager`: a person's
//!   verdict (`judged_by` = their sub); 404 `release_not_found` / `row_not_found`, 409 `not_run`, 422
//!   `reason_required` / `invalid_verdict` / `invalid_reason`.
//! - `POST /api/jobs/{job}/validation/questions` — `job_designer`: 201 `Paged<ValidationCut>`; 422
//!   `fields_required`, 409 `no_release`, 413 `too_many_questions`.
//! - `PATCH /api/jobs/{job}/validation/questions/{id}` — `job_designer`: `Paged<ValidationCut>`; 404
//!   `question_not_found`, 422 `fields_required`.
//! - `DELETE /api/jobs/{job}/validation/questions/{id}` — `job_designer`: 204 (soft delete; earlier cuts keep it).
//! - `POST /api/jobs/{job}/validation/questions/import` (multipart CSV `file`) — `job_designer`: 201
//!   `{imported, import_batch}`; all or nothing (`questions::parse_csv`).
//! - `GET /api/jobs/{job}/validation/draft` — any member: the draft's test scenarios, runs and state (T13).
//! - `POST /api/jobs/{job}/validation/draft/run` — `job_designer`: probe every scenario on Production and on the
//!   draft (env test) and judge them; 409 `no_scenarios` / `design_changed`, 503 `unavailable` (nothing stored).
//! - `GET /api/jobs/{job}/releases/{id}/validation` — any member: the cut + its latest grid run (`grid`).
//!
//! Live data (D69): no write here is a design write. Audited per write (`validation.*`), never with question text.
use axum::extract::multipart::MultipartRejection;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::design::JobRef;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::page::paged;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};
use crate::validation::judge::{Limits, Outcome};
use crate::validation::questions::{self, Fields};
use crate::validation::{cuts, grid, judge};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/validation", get(list_cuts))
        // before `/validation/{release}/…`: the static segment must win (the draft is not a release)
        .route("/api/jobs/{job}/validation/draft", get(draft_validation))
        .route(
            "/api/jobs/{job}/validation/draft/run",
            post(run_draft_validation),
        )
        .route("/api/jobs/{job}/validation/questions", post(add_question))
        .route(
            "/api/jobs/{job}/validation/questions/import",
            post(import_questions).layer(DefaultBodyLimit::max(
                questions::MAX_IMPORT_BYTES + 64 * 1024,
            )),
        )
        .route(
            "/api/jobs/{job}/validation/questions/{id}",
            patch(update_question).delete(delete_question),
        )
        .route("/api/jobs/{job}/validation/{release}/run", post(run_cut))
        .route(
            "/api/jobs/{job}/validation/{release}/rows/{row}",
            put(set_verdict),
        )
        .route(
            "/api/jobs/{job}/releases/{id}/validation",
            get(release_validation),
        )
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

async fn cuts_page(c: &mut PgConnection, job: Uuid) -> ApiResult<Value> {
    Ok(paged(cuts::cuts_json(c, job).await?, None))
}

async fn list_cuts(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let v = cuts_page(&mut tx, j.job_id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `draftValidation`.
async fn draft_validation(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    Ok(Json(crate::validation::draft::read(&st, &j).await?))
}

/// `runDraftValidation`.
async fn run_draft_validation(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    Ok(Json(crate::validation::draft::run(&st, &j).await?))
}

/// `runValidationCut`.
async fn run_cut(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiPath((_job, release)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let rel = cuts::release(&mut tx, j.job_id, &release).await?;
    let rows = cuts::rows(&mut tx, j.job_id, &rel).await?;
    if rows.is_empty() {
        return Err(ApiError::Conflict(
            "no_questions",
            "Add a question first: this cut has nothing to grade.".into(),
        ));
    }
    let grid_id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO validation_grid_run (job_id, release_id, trigger, status, total, by, started_at) \
         VALUES ($1, $2, 'manual', 'running', $3, $4, now()) \
         ON CONFLICT (tenant_id, job_id, release_id) WHERE status IN ('queued', 'running') DO NOTHING \
         RETURNING id",
    )
    .bind(j.job_id)
    .bind(&rel.release_id)
    .bind(i32::try_from(rows.len()).unwrap_or(i32::MAX))
    .bind(&j.t.caller.sub)
    .fetch_optional(&mut *tx)
    .await?;
    let grid_id = grid_id.ok_or_else(grid::grid_running)?;
    tx.commit().await?;
    let ctx = cuts::RunCtx {
        tenant: j.t.tenant,
        job: job_ref(&j),
        release_id: rel.release_id.clone(),
        by: j.t.caller.sub.clone(),
    };
    let questions = cuts::questions_of(&rows);
    let graded: ApiResult<cuts::RunCounts> = async {
        let (answerer, judge) = cuts::upstreams(&st, &ctx).await?;
        let outcomes = judge::grade(&questions, answerer, judge, Limits::request()).await;
        if !outcomes.iter().any(|o| matches!(o, Outcome::Graded(_))) {
            return Err(ApiError::Unavailable(
                "unavailable",
                "The release's knowledge did not answer; try again shortly.".into(),
                None,
            ));
        }
        cuts::store(&st, &ctx, &questions, &outcomes).await
    }
    .await;
    let mut tx = j.begin(&st.db).await?;
    let (status, code, done) = match &graded {
        Ok(c) => ("done", None, c.graded + c.failed),
        Err(e) => ("failed", Some(e.code()), 0),
    };
    sqlx::query(
        "UPDATE validation_grid_run SET status = $2, error_code = $3, done = $4, finished_at = now() \
          WHERE id = $1",
    )
    .bind(grid_id)
    .bind(status)
    .bind(code)
    .bind(i32::try_from(done).unwrap_or(i32::MAX))
    .execute(&mut *tx)
    .await?;
    let counts = graded?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "validation.cut_run",
            "validation_cut",
            rel.release_id.clone(),
        )
        .job(&j.job_key)
        .detail("questions", counts.total)
        .detail("graded", counts.graded)
        .detail("not_reached", counts.not_reached),
    )
    .await?;
    let v = cuts::cut_json(&mut tx, j.job_id, &rel).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerdictInput {
    verdict: String,
    #[serde(default)]
    reason: Option<String>,
}

/// `setValidationVerdict`.
async fn set_verdict(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiPath((_job, release, row)): ApiPath<(String, String, String)>,
    ApiJson(input): ApiJson<VerdictInput>,
) -> ApiResult<Json<Value>> {
    let verdict = match input.verdict.as_str() {
        v @ ("pass" | "rejected" | "pending") => v,
        _ => {
            return Err(ApiError::Unprocessable(
                "invalid_verdict",
                "A verdict is pass, rejected or pending.".into(),
            ));
        }
    };
    let reason = match (verdict, input.reason.as_deref().map(str::trim)) {
        ("rejected", None | Some("")) => {
            return Err(ApiError::Unprocessable(
                "reason_required",
                "Say why it is rejected.".into(),
            ));
        }
        ("rejected", Some(r @ ("missing_citation" | "wrong_citation" | "incorrect"))) => Some(r),
        ("rejected", Some(_)) => {
            return Err(ApiError::Unprocessable(
                "invalid_reason",
                "A rejection reason is missing_citation, wrong_citation or incorrect.".into(),
            ));
        }
        _ => None,
    };
    let mut tx = j.begin(&st.db).await?;
    let rel = cuts::release(&mut tx, j.job_id, &release).await?;
    let rows = cuts::rows(&mut tx, j.job_id, &rel).await?;
    let row_not_found =
        || ApiError::Missing("row_not_found", "No such question on this cut.".into());
    let id = Uuid::parse_str(&row).map_err(|_| row_not_found())?;
    let r = rows.iter().find(|r| r.id == id).ok_or_else(row_not_found)?;
    if r.response.is_none() {
        return Err(ApiError::Conflict(
            "not_run",
            "Run the cut first: there is no response to judge.".into(),
        ));
    }
    sqlx::query(
        "UPDATE validation_result SET verdict = $4, reason = $5, judged_by = $6, judged_at = now() \
          WHERE job_id = $1 AND release_id = $2 AND question_id = $3",
    )
    .bind(j.job_id)
    .bind(&rel.release_id)
    .bind(id)
    .bind(verdict)
    .bind(reason)
    .bind(&j.t.caller.sub)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "validation.verdict_set",
            "validation_result",
            id.to_string(),
        )
        .job(&j.job_key)
        .detail("release", rel.release_id.clone())
        .detail("verdict", verdict)
        .detail("reason", reason),
    )
    .await?;
    let v = cuts::cut_json(&mut tx, j.job_id, &rel).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionInput {
    #[serde(default)]
    question: String,
    #[serde(default)]
    expected: String,
    #[serde(default)]
    reference: String,
    #[serde(default)]
    activity: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionPatch {
    question: Option<String>,
    expected: Option<String>,
    reference: Option<String>,
    activity: Option<String>,
}

async fn has_release(c: &mut PgConnection, job: Uuid) -> ApiResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM job_release WHERE job_id = $1)")
            .bind(job)
            .fetch_one(&mut *c)
            .await?,
    )
}

async fn live_count(c: &mut PgConnection, job: Uuid) -> ApiResult<usize> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM validation_question WHERE job_id = $1 AND deleted_at IS NULL",
    )
    .bind(job)
    .fetch_one(&mut *c)
    .await?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

/// Serialise question writes of one job (the 500 cap is checked, then inserted).
async fn lock_questions(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('validation_question:' || $1::text, 0))",
    )
    .bind(job)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// `addValidationQuestion`.
async fn add_question(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<QuestionInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let f = Fields {
        question: input.question.trim().to_string(),
        expected: input.expected.trim().to_string(),
        reference: input.reference.trim().to_string(),
        activity: input.activity.unwrap_or_default().trim().to_string(),
    };
    if let Some(code) = questions::problem(&f) {
        return Err(questions::refuse(code));
    }
    let mut tx = j.begin(&st.db).await?;
    if !has_release(&mut tx, j.job_id).await? {
        return Err(questions::no_release());
    }
    lock_questions(&mut tx, j.job_id).await?;
    if live_count(&mut tx, j.job_id).await? >= questions::MAX_LIVE {
        return Err(questions::too_many());
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO validation_question (job_id, question, expected, reference, activity, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(j.job_id)
    .bind(&f.question)
    .bind(&f.expected)
    .bind(&f.reference)
    .bind(&f.activity)
    .bind(&j.t.caller.sub)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "validation.question_add",
            "validation_question",
            id.to_string(),
        )
        .job(&j.job_key),
    )
    .await?;
    let v = cuts_page(&mut tx, j.job_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(v)))
}

#[derive(sqlx::FromRow)]
struct QuestionRow {
    question: String,
    expected: String,
    reference: String,
    activity: String,
}

fn parse_question_id(raw: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(raw).map_err(|_| questions::not_found())
}

/// `updateValidationQuestion` (F-50): fields trimmed, all three required after the patch.
async fn update_question(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
    ApiJson(p): ApiJson<QuestionPatch>,
) -> ApiResult<Json<Value>> {
    let id = parse_question_id(&id)?;
    let mut tx = j.begin(&st.db).await?;
    let cur: QuestionRow = sqlx::query_as(
        "SELECT question, expected, reference, activity FROM validation_question \
          WHERE id = $1 AND job_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(questions::not_found)?;
    let mut fields: Vec<&'static str> = Vec::new();
    let pick =
        |new: Option<String>, old: String, name: &'static str, fields: &mut Vec<&'static str>| {
            match new {
                Some(v) => {
                    fields.push(name);
                    v.trim().to_string()
                }
                None => old,
            }
        };
    let f = Fields {
        question: pick(p.question, cur.question, "question", &mut fields),
        expected: pick(p.expected, cur.expected, "expected", &mut fields),
        reference: pick(p.reference, cur.reference, "reference", &mut fields),
        activity: pick(p.activity, cur.activity, "activity", &mut fields),
    };
    if let Some(code) = questions::problem(&f) {
        return Err(questions::refuse(code));
    }
    sqlx::query(
        "UPDATE validation_question SET question = $2, expected = $3, reference = $4, activity = $5 \
          WHERE id = $1",
    )
    .bind(id)
    .bind(&f.question)
    .bind(&f.expected)
    .bind(&f.reference)
    .bind(&f.activity)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "validation.question_update",
            "validation_question",
            id.to_string(),
        )
        .job(&j.job_key)
        .detail("fields", fields),
    )
    .await?;
    let v = cuts_page(&mut tx, j.job_id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `deleteValidationQuestion` (F-50): soft delete.
async fn delete_question(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    let id = parse_question_id(&id)?;
    let mut tx = j.begin(&st.db).await?;
    let n = sqlx::query(
        "UPDATE validation_question SET deleted_at = now() \
          WHERE id = $1 AND job_id = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(j.job_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(questions::not_found());
    }
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "validation.question_delete",
            "validation_question",
            id.to_string(),
        )
        .job(&j.job_key),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The `file` part of an import form (CSV only), capped at `MAX_IMPORT_BYTES`.
async fn read_csv_part(form: Result<Multipart, MultipartRejection>) -> ApiResult<Vec<u8>> {
    let mut form = form.map_err(|_| {
        ApiError::BadRequest(
            "invalid_body",
            "the body must be multipart/form-data".into(),
        )
    })?;
    let bad_form = |_| {
        ApiError::BadRequest(
            "invalid_body",
            "the multipart body could not be read".into(),
        )
    };
    while let Some(mut field) = form.next_field().await.map_err(bad_form)? {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or_default().trim().to_lowercase();
        let csv_type = field
            .content_type()
            .is_some_and(|t| t.starts_with("text/csv") || t.starts_with("application/csv"));
        if !name.ends_with(".csv") && !csv_type {
            return Err(ApiError::Unprocessable(
                "unsupported_file",
                "Import a .csv file with the columns question, expected, reference[, activity]."
                    .into(),
            ));
        }
        let mut data = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(bad_form)? {
            if data.len() + chunk.len() > questions::MAX_IMPORT_BYTES {
                return Err(ApiError::TooLarge(
                    "too_large",
                    "The file is larger than 4 MiB.".into(),
                ));
            }
            data.extend_from_slice(&chunk);
        }
        return Ok(data);
    }
    Err(ApiError::Unprocessable(
        "empty_file",
        "Attach the CSV as the form field file.".into(),
    ))
}

/// `importValidationQuestions` (F-50): all or nothing, one `import_batch`.
async fn import_questions(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    form: Result<Multipart, MultipartRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let bytes = read_csv_part(form).await?;
    let rows = questions::parse_csv(&bytes)?;
    let mut tx = j.begin(&st.db).await?;
    if !has_release(&mut tx, j.job_id).await? {
        return Err(questions::no_release());
    }
    lock_questions(&mut tx, j.job_id).await?;
    if live_count(&mut tx, j.job_id).await? + rows.len() > questions::MAX_LIVE {
        return Err(questions::too_many());
    }
    let batch = format!("imp-{}", &Uuid::new_v4().simple().to_string()[..16]);
    for f in &rows {
        sqlx::query(
            "INSERT INTO validation_question \
                 (job_id, question, expected, reference, activity, created_by, source, import_batch) \
             VALUES ($1, $2, $3, $4, $5, $6, 'csv', $7)",
        )
        .bind(j.job_id)
        .bind(&f.question)
        .bind(&f.expected)
        .bind(&f.reference)
        .bind(&f.activity)
        .bind(&j.t.caller.sub)
        .bind(&batch)
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "validation.question_import",
            "validation_question",
            batch.clone(),
        )
        .job(&j.job_key)
        .detail("count", rows.len()),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"imported": rows.len(), "import_batch": batch})),
    ))
}

/// `releaseValidation` (T12a): the cut's rows + the latest grid run. A read only.
async fn release_validation(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let rel = cuts::release(&mut tx, j.job_id, &id).await?;
    let mut v = cuts::cut_json(&mut tx, j.job_id, &rel).await?;
    let g = grid::latest_json(&mut tx, j.job_id, &rel.release_id).await?;
    tx.commit().await?;
    if let Some(o) = v.as_object_mut() {
        o.insert("grid".into(), g);
    }
    Ok(Json(v))
}
