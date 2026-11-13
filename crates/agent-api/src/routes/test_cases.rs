//! Saved test cases (B4 T18, D54 TEST step; web `areas/live.ts` `testCases` / `saveTestCase`, `types/live.ts`
//! `TestCase`) on B12's `test_case` table (0061; never edited here).
//!
//! - `GET /api/jobs/{job}/test-cases` (any member): newest first; `stale` = the row's `draft_fp` is not the job's
//!   current draft fingerprint (B2 `design::fingerprint` of the ledger's change keys), computed on read.
//! - `POST /api/jobs/{job}/test-cases` `{conversation_id, title}` (`job_designer`) → 201 `TestCase`: a staff
//!   TEST-env conversation only (409 `not_a_test_conversation`) with at least one agent reply (422
//!   `nothing_to_save`). It snapshots the turns (each with the summaries of the reply trace's `differs`), the
//!   exercised change keys, `draft_rev`, `draft_fp` and `draft_keys` (the ledger keys of every unpublished change).
//!   Audited (`test_case.save`, ids and counts only).
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};

use crate::audit::{self, Event};
use crate::conversations::{iso, store};
use crate::design::{self, JobRef, ledger};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, role};

/// The longest test case title (characters; 0061's CHECK).
pub const MAX_TITLE: usize = 120;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/jobs/{job}/test-cases",
        get(test_cases).post(save_test_case),
    )
}

/// The job's draft as a saved test sees it: `(draft_fp, draft_keys)`.
async fn current_draft(c: &mut PgConnection, job: &JobRef) -> ApiResult<(String, Vec<String>)> {
    let rows = ledger::of_job(&mut *c, job).await?;
    let changes: Vec<String> = rows
        .iter()
        .map(|r| ledger::change_key(&r.key, &r.after))
        .collect();
    let keys: Vec<String> = rows.iter().map(|r| r.key.clone()).collect();
    Ok((design::fingerprint(&changes), keys))
}

/// The `TestCase` of one `test_case` row (selected with [`COLUMNS`]).
fn view(r: &sqlx::postgres::PgRow, current_fp: &str) -> Value {
    let draft_fp: String = r.get("draft_fp");
    json!({
        "id": r.get::<uuid::Uuid, _>("id").to_string(),
        "title": r.get::<String, _>("title"),
        "conversation_id": r.get::<String, _>("conversation_id"),
        "customer_id": r.get::<Option<String>, _>("customer_id").unwrap_or_default(),
        "env": "test",
        "design": "draft",
        "release_id": r.get::<Option<String>, _>("release_id"),
        "draft_rev": r.get::<i64, _>("draft_rev"),
        "stale": draft_fp != current_fp,
        "draft_fp": draft_fp,
        "draft_keys": r.get::<Vec<String>, _>("draft_keys"),
        "exercised": r.get::<Vec<String>, _>("exercised"),
        "differs": r.get::<i32, _>("differs"),
        "turns": r.get::<Value, _>("turns"),
        "saved_by": r.get::<String, _>("saved_by"),
        "saved_by_name": r.get::<String, _>("saved_by_name"),
        "saved_at": iso(r.get("saved_at")),
    })
}

/// The `test_case` columns [`view`] reads, `saved_by_name` from the member's display name (else the sub).
const COLUMNS: &str = "t.id, t.title, t.conversation_id, t.customer_id, t.release_id, t.draft_rev, t.draft_fp, \
     t.draft_keys, t.exercised, t.differs, t.turns, t.saved_by, t.saved_at, \
     COALESCE((SELECT NULLIF(btrim(u.display_name), '') FROM users u WHERE u.sub = t.saved_by), t.saved_by) \
       AS saved_by_name";

/// `testCases`.
async fn test_cases(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    let (fp, _) = current_draft(&mut tx, &job).await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM test_case t WHERE t.job_id = $1 ORDER BY t.saved_at DESC, t.id DESC"
    )))
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let items = rows.iter().map(|r| view(r, &fp)).collect();
    Ok(Json(crate::page::paged(items, None)))
}

#[derive(Deserialize)]
struct SaveBody {
    #[serde(default)]
    conversation_id: String,
    #[serde(default)]
    title: String,
}

/// One snapshotted turn: an agent reply and the customer line before it.
#[derive(Default)]
struct Turns {
    turns: Vec<Value>,
    exercised: Vec<String>,
    differs: i32,
}

/// One stored message, as a test case snapshots it.
#[derive(sqlx::FromRow)]
struct Line {
    role: String,
    text: String,
    case_id: Option<String>,
    refused: bool,
    trace: Option<Value>,
}

/// The turns of a message log (oldest first).
fn turns_of(log: &[Line]) -> Turns {
    let mut out = Turns::default();
    let mut customer = String::new();
    for Line {
        role,
        text,
        case_id,
        refused,
        trace,
    } in log
    {
        match role.as_str() {
            "customer" => customer = text.clone(),
            "agent" => {
                let differs: Vec<Value> = trace
                    .as_ref()
                    .and_then(|t| t["differs"].as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| d["summary"].as_str())
                            .map(|s| json!(s))
                            .collect()
                    })
                    .unwrap_or_default();
                for k in trace
                    .as_ref()
                    .and_then(|t| t["exercised"].as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if !out.exercised.iter().any(|e| e == k) {
                        out.exercised.push(k.to_string());
                    }
                }
                if !differs.is_empty() {
                    out.differs += 1;
                }
                let outcome = if *refused {
                    "refused"
                } else if case_id.is_some() {
                    "case"
                } else {
                    "answered"
                };
                out.turns.push(json!({
                    "customer": customer,
                    "agent": text,
                    "outcome": outcome,
                    "case_id": case_id,
                    "differs": differs,
                }));
            }
            _ => {}
        }
    }
    out
}

/// `saveTestCase`.
async fn save_test_case(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<SaveBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    let conv = store::conversation_or_404(&mut tx, b.conversation_id.trim()).await?;
    let title = b.title.trim();
    if title.is_empty() {
        return Err(ApiError::Unprocessable(
            "title_required",
            "Give the test case a name.".into(),
        ));
    }
    if title.chars().count() > MAX_TITLE {
        return Err(ApiError::Unprocessable(
            "title_too_long",
            "Names are at most 120 characters.".into(),
        ));
    }
    if conv.env != "test" {
        return Err(ApiError::Conflict(
            "not_a_test_conversation",
            "Only Test-env (draft) conversations can be saved as test cases. Switch to Draft and run it again."
                .into(),
        ));
    }
    let log: Vec<Line> = sqlx::query_as(
        "SELECT role, text, case_id, refused, trace FROM conversation_message \
          WHERE conversation_id = $1 ORDER BY at_ms, message_id",
    )
    .bind(&conv.id)
    .fetch_all(&mut *tx)
    .await?;
    let t = turns_of(&log);
    if t.turns.is_empty() {
        return Err(ApiError::Unprocessable(
            "nothing_to_save",
            "Send a message and wait for the reply first.".into(),
        ));
    }
    let (fp, keys) = current_draft(&mut tx, &job).await?;
    let draft_rev = design::draft_rev(&mut tx, &job).await?;
    let release_id: Option<String> =
        sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
            .bind(j.job_id)
            .fetch_one(&mut *tx)
            .await?;
    let n_turns = t.turns.len() as i64;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO test_case \
            (tenant_id, job_id, title, conversation_id, customer_id, release_id, draft_rev, draft_fp, draft_keys, \
             exercised, differs, turns, saved_by) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         RETURNING id",
    )
    .bind(j.job_id)
    .bind(title)
    .bind(&conv.id)
    .bind(&conv.customer_id)
    .bind(&release_id)
    .bind(draft_rev)
    .bind(&fp)
    .bind(&keys)
    .bind(&t.exercised)
    .bind(t.differs)
    .bind(Value::Array(t.turns))
    .bind(&j.t.caller.sub)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("test_case.save", "test_case", id.to_string())
            .job(&j.job_key)
            .detail("conversation_id", conv.id.clone())
            .detail("turns", n_turns)
            .detail("draft_rev", draft_rev),
    )
    .await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM test_case t WHERE t.id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(view(&row, &fp))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(role: &str, text: &str, case: Option<&str>, trace: Option<Value>) -> Line {
        Line {
            role: role.into(),
            text: text.into(),
            case_id: case.map(str::to_owned),
            refused: false,
            trace,
        }
    }

    #[test]
    fn turns_pair_each_reply_with_the_customer_line_before_it() {
        let log = vec![
            line("customer", "Where is RET-02?", None, None),
            line(
                "agent",
                "On its way.",
                None,
                Some(
                    json!({"differs": [{"summary": "live would say: delivered"}], "exercised": ["k1", "k2"]}),
                ),
            ),
            line("system", "notice", None, None),
            line("customer", "Return it", None, None),
            line(
                "agent",
                "Opened CS-1.",
                Some("CS-1"),
                Some(json!({"differs": [], "exercised": ["k2"]})),
            ),
        ];
        let t = turns_of(&log);
        assert_eq!(t.turns.len(), 2);
        assert_eq!(t.turns[0]["customer"], "Where is RET-02?");
        assert_eq!(t.turns[0]["outcome"], "answered");
        assert_eq!(t.turns[0]["differs"], json!(["live would say: delivered"]));
        assert_eq!(t.turns[1]["customer"], "Return it");
        assert_eq!(t.turns[1]["outcome"], "case");
        assert_eq!(t.exercised, vec!["k1".to_string(), "k2".to_string()]);
        assert_eq!(t.differs, 1);
        assert!(
            turns_of(&[line("customer", "hi", None, None)])
                .turns
                .is_empty()
        );
    }
}
