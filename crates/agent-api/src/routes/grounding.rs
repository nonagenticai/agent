//! Answer grounding (B3 T11; plan §5.7, §0 R10): staff ratings of agent answers and the job's knowledge analytics.
//! Shapes = `web/src/lib/api/types/grounding.ts` (`AnswerFeedback`, `KnowledgeAnalytics`); refusals = the fixture
//! (`web/src/lib/api/areas/grounding.ts`). Feedback is operational data, not design (D54): no ledger, no draft.
//!
//! An answer is an agent reply in B12's message log: `answer_id` is its `conversation_message.message_id`. A rating
//! stores only the rating (0122 `answer_feedback`, FK to the message); the conversation, case, answer text and
//! question are read from the log, the grounding verdict from the reply's `answer_grounding` row (`no_knowledge`
//! without one). The input's `conversation_id` / `case_id` / `question` / `answer` / `grounding` are not stored: the
//! message is the source of truth.
//!
//! - `POST /api/jobs/{job}/answer-feedback` (any member) → `AnswerFeedback`: one rating per person per answer, rating
//!   again replaces it (upsert on `(tenant, message_id, by)`). A reason is kept on a down vote only; the note is
//!   trimmed (empty = none). 422 `invalid_feedback`: vote not up/down, unknown surface, an `answer_id` that is not an
//!   agent reply of this job, an unknown reason, a note over 500 characters. Audited `answer_feedback.submit`
//!   (vote, surface, reason; never the answer text or ids, which may be secret- or email-shaped).
//! - `GET /api/jobs/{job}/answer-feedback?conversation_id=&case_id=` (any member) → the job's ratings, newest first,
//!   with `by_name` as `crate::members::names` (display name, else email, else the remembered staff name, else the
//!   sub). At most [`LIST_CAP`] rows.
//! - `GET /api/jobs/{job}/analytics/knowledge?days=` (any member) → `KnowledgeAnalytics` over `[now - days, now]`
//!   (crate::knowledge::analytics): production answers (grounded agent replies of `prod` conversations; zeros
//!   before the runtime records any),
//!   ratings from every surface, and the documents of the bound tenant leaves of the LIVE design from strata_rag
//!   (never-cited). 404 `job_not_found`; 422 `invalid_window` (`days` missing, not an integer, or outside 1–90).
//!   strata_rag unreadable → the analytics without documents, header `X-Agent-Stale: strata_rag`.
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::design::{self, DesignOf, JobRef};
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::knowledge::analytics::{
    self, DocRow, MAX_DAYS, QUESTION_JOIN, REASONS, SURFACES, StoreNode,
};
use crate::knowledge::stores;
use crate::routes::knowledge_kinds::STALE_HEADER;
use crate::state::AppState;
use crate::strata::rag::{self, DocumentsQuery};
use crate::strata::{self, Plane, Service, StrataStoreId};
use crate::tenancy::{self, JobCtx, JobMember};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/answer-feedback",
            post(submit_feedback).get(list_feedback),
        )
        .route(
            "/api/jobs/{job}/analytics/knowledge",
            get(knowledge_analytics),
        )
}

/// The most ratings one list answers.
pub const LIST_CAP: i64 = 1000;
/// The longest note (characters).
pub const NOTE_MAX: usize = 500;
/// Documents read per leaf page, and pages per leaf.
const DOC_PAGE: i64 = 500;
const DOC_PAGES: usize = 10;

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

fn invalid_feedback(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_feedback", msg.into())
}

// ---------------------------------------------------------------------------------------------------------------
// submitAnswerFeedback / answerFeedback

/// `AnswerFeedbackInput`; every field optional so a missing one is a 422, not a 400. Its other fields
/// (`conversation_id`, `case_id`, `question`, `answer`, `grounding`) are read from the message, not the body.
#[derive(Deserialize)]
struct FeedbackInput {
    surface: Option<String>,
    answer_id: Option<String>,
    vote: Option<String>,
    reason: Option<String>,
    note: Option<String>,
}

/// A validated rating.
#[derive(Debug, PartialEq, Eq)]
struct Rating {
    surface: String,
    /// The rated agent reply (`conversation_message.message_id`).
    message_id: Uuid,
    vote: String,
    reason: Option<String>,
    note: Option<String>,
}

fn non_blank(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

fn which_answer() -> ApiError {
    invalid_feedback("Which answer is rated?")
}

/// The fixture's checks, in its order.
fn validate(i: FeedbackInput) -> ApiResult<Rating> {
    let vote = i.vote.unwrap_or_default();
    if vote != "up" && vote != "down" {
        return Err(invalid_feedback("Vote up or down."));
    }
    let surface = i.surface.unwrap_or_default();
    if !SURFACES.contains(&surface.as_str()) {
        return Err(invalid_feedback("Unknown surface."));
    }
    let message_id = non_blank(i.answer_id)
        .and_then(|a| Uuid::parse_str(a.trim()).ok())
        .ok_or_else(which_answer)?;
    let reason = if vote == "down" { i.reason } else { None };
    if let Some(r) = &reason
        && !REASONS.contains(&r.as_str())
    {
        return Err(invalid_feedback(format!("Unknown reason “{r}”.")));
    }
    let note = non_blank(i.note).map(|n| n.trim().to_string());
    if note.as_ref().is_some_and(|n| n.chars().count() > NOTE_MAX) {
        return Err(invalid_feedback("Notes are at most 500 characters."));
    }
    Ok(Rating {
        surface,
        message_id,
        vote,
        reason,
        note,
    })
}

#[derive(sqlx::FromRow)]
struct FeedbackRow {
    id: Uuid,
    surface: String,
    conversation_id: String,
    message_id: Uuid,
    case_id: Option<String>,
    question: String,
    answer: String,
    vote: String,
    reason: Option<String>,
    note: Option<String>,
    grounding: String,
    by_sub: String,
    by_name: String,
    at: DateTime<Utc>,
}

/// Columns of [`FeedbackRow`] over [`ROW_FROM`] (`f` = answer_feedback, `m` = the rated message, `g` = its
/// grounding, `q` = its question, `u` = users).
const ROW_COLUMNS: &str = "f.id, f.surface, m.conversation_id, f.message_id, m.case_id, \
     coalesce(q.text, '') AS question, m.text AS answer, f.vote, f.reason, f.note, \
     coalesce(g.grounding, 'no_knowledge') AS grounding, f.by_sub, \
     coalesce(nullif(btrim(u.display_name), ''), nullif(btrim(u.email), ''), \
         agent_actor_name(f.by_sub), f.by_sub) AS by_name, f.at";

/// The joins of [`ROW_COLUMNS`] except `q` ([`QUESTION_JOIN`], which follows).
const ROW_FROM: &str = "answer_feedback f \
     JOIN conversation_message m \
       ON m.tenant_id = f.tenant_id AND m.job_id = f.job_id AND m.message_id = f.message_id \
     LEFT JOIN answer_grounding g ON g.message_id = f.message_id \
     LEFT JOIN users u ON u.sub = f.by_sub";

/// `AnswerFeedback`.
fn row_json(tenant: Uuid, job: &str, r: &FeedbackRow) -> Value {
    json!({
        "id": r.id,
        "tenant_id": tenant,
        "job": job,
        "surface": r.surface,
        "conversation_id": r.conversation_id,
        "answer_id": r.message_id,
        "case_id": r.case_id,
        "question": r.question,
        "answer": r.answer,
        "vote": r.vote,
        "reason": r.reason,
        "note": r.note,
        "grounding": r.grounding,
        "by": r.by_sub,
        "by_name": r.by_name,
        "at": analytics::iso(r.at),
    })
}

/// `submitAnswerFeedback`.
async fn submit_feedback(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<FeedbackInput>,
) -> ApiResult<Json<Value>> {
    let r = validate(input)?;
    let by = j.t.caller.sub.clone();
    let mut tx = j.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, j.t.tenant).await?;
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    let is_reply: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_message \
                         WHERE message_id = $1 AND job_id = $2 AND role = 'agent' \
                           AND author IS DISTINCT FROM $3)",
    )
    .bind(r.message_id)
    .bind(j.job_id)
    // the stored handover notice is the edge's line, not an answer to rate
    .bind(crate::conversations::handover::NOTICE_AUTHOR)
    .fetch_one(&mut *tx)
    .await?;
    if !is_reply {
        return Err(which_answer());
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO answer_feedback (job_id, id, message_id, surface, vote, reason, note, by_sub) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (tenant_id, message_id, by_sub) DO UPDATE \
            SET surface = EXCLUDED.surface, vote = EXCLUDED.vote, reason = EXCLUDED.reason, \
                note = EXCLUDED.note, at = clock_timestamp() \
         RETURNING id",
    )
    .bind(j.job_id)
    .bind(Uuid::new_v4())
    .bind(r.message_id)
    .bind(&r.surface)
    .bind(&r.vote)
    .bind(&r.reason)
    .bind(&r.note)
    .bind(&by)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("answer_feedback.submit", "answer_feedback", id.to_string())
            .job(&j.job_key)
            .detail("surface", r.surface.clone())
            .detail("vote", r.vote.clone())
            .detail("reason", r.reason.clone()),
    )
    .await?;
    let row: FeedbackRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM {ROW_FROM} {QUESTION_JOIN} WHERE f.id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(row_json(j.t.tenant, &j.job_key, &row)))
}

#[derive(Deserialize)]
struct ListQuery {
    conversation_id: Option<String>,
    case_id: Option<String>,
}

/// `answerFeedback`.
async fn list_feedback(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let rows: Vec<FeedbackRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM {ROW_FROM} {QUESTION_JOIN} \
          WHERE f.job_id = $1 AND ($2::text IS NULL OR m.conversation_id = $2) \
            AND ($3::text IS NULL OR m.case_id = $3) \
          ORDER BY f.at DESC, f.id LIMIT $4"
    )))
    .bind(j.job_id)
    .bind(q.conversation_id.filter(|s| !s.is_empty()))
    .bind(q.case_id.filter(|s| !s.is_empty()))
    .bind(LIST_CAP)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| row_json(j.t.tenant, &j.job_key, r))
        .collect();
    Ok(Json(Value::Array(items)))
}

// ---------------------------------------------------------------------------------------------------------------
// knowledgeAnalytics

#[derive(Deserialize)]
struct WindowQuery {
    days: Option<String>,
}

/// `days` as an integer in 1–90 (422 `invalid_window` otherwise, missing included).
fn window(days: Option<&str>) -> ApiResult<i64> {
    days.and_then(|d| d.trim().parse::<i64>().ok())
        .filter(|d| (1..=MAX_DAYS).contains(d))
        .ok_or_else(|| ApiError::Unprocessable("invalid_window", "days must be 1–90.".into()))
}

/// Every document of strata leaf `sid` (paged, at most `DOC_PAGES` pages).
async fn leaf_docs(
    st: &AppState,
    p: &Plane<'_>,
    sid: StrataStoreId,
) -> ApiResult<Vec<rag::Document>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..DOC_PAGES {
        let q = DocumentsQuery {
            limit: Some(DOC_PAGE),
            cursor: cursor.take(),
            source_id: None,
        };
        let page = rag::documents(&st.http, p, sid, &q).await?;
        out.extend(page.items);
        match page.next {
            Some(n) => cursor = Some(n),
            None => break,
        }
    }
    Ok(out)
}

/// `knowledgeAnalytics`.
async fn knowledge_analytics(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<WindowQuery>,
) -> ApiResult<Response> {
    let days = window(q.days.as_deref())?;
    let jr = job_ref(&j);
    let as_of = DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap_or_else(Utc::now);
    let since = analytics::since_of(as_of, days);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let answers = analytics::prod_answers(&mut tx, jr.id, since).await?;
    let feedback = analytics::feedback_since(&mut tx, jr.id, since).await?;
    let leaves = analytics::bound_tenant_leaves(&mut tx, &st.design, &jr, &DesignOf::Live).await?;
    let all = stores::all(&mut tx).await?;
    tx.commit().await?;
    // never-cited: the documents of the bound tenant leaves (no lock is held across strata)
    let mut documents: Vec<DocRow> = Vec::new();
    let mut is_stale = false;
    if !leaves.is_empty() {
        match strata::plane(&st, j.t.tenant, Service::Rag).await {
            Ok(p) => {
                for leaf in &leaves {
                    // labelled by the agent store: after a model change the strata id is a generation's
                    let store_id = leaf.store.to_string();
                    match leaf_docs(&st, &p, leaf.strata).await {
                        Ok(docs) => documents.extend(docs.into_iter().map(|d| DocRow {
                            id: d.id.to_string(),
                            store_id: store_id.clone(),
                            title: d.title.unwrap_or(d.source),
                        })),
                        Err(_) => is_stale = true,
                    }
                }
            }
            Err(_) => is_stale = true,
        }
    }
    let stores_in: Vec<StoreNode> = all
        .iter()
        .map(|s| StoreNode {
            id: s.id.to_string(),
            key: s.key.clone(),
            parent_id: s.parent_id.map(|p| p.to_string()),
        })
        .collect();
    let k = analytics::analytics_of(&analytics::Input {
        job: jr.key.clone(),
        days,
        as_of,
        answers,
        feedback,
        documents,
        stores: stores_in,
    });
    if is_stale {
        Ok(([(STALE_HEADER, "strata_rag")], Json(k)).into_response())
    } else {
        Ok(Json(k).into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = "0199f0a0-0000-7000-8000-000000000001";

    fn input(vote: &str) -> FeedbackInput {
        FeedbackInput {
            surface: Some("test_chat".into()),
            answer_id: Some(format!(" {REPLY} ")),
            vote: Some(vote.into()),
            reason: Some("tone".into()),
            note: Some("  ".into()),
        }
    }

    #[test]
    fn up_vote_drops_reason_and_blank_note() {
        let r = validate(input("up")).unwrap();
        assert_eq!(
            (r.reason, r.note, r.message_id.to_string()),
            (None, None, REPLY.to_string())
        );
        let d = validate(input("down")).unwrap();
        assert_eq!(d.reason.as_deref(), Some("tone"));
    }

    #[test]
    fn answer_id_must_be_a_message_id() {
        for bad in [None, Some(""), Some(" "), Some("m-1-r")] {
            let mut i = input("up");
            i.answer_id = bad.map(str::to_string);
            assert!(validate(i).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn window_bounds() {
        for ok in ["1", "90", " 30 "] {
            assert!(window(Some(ok)).is_ok(), "{ok}");
        }
        for bad in [
            None,
            Some("0"),
            Some("91"),
            Some("7.5"),
            Some("x"),
            Some(""),
        ] {
            assert!(window(bad).is_err(), "{bad:?}");
        }
    }
}
