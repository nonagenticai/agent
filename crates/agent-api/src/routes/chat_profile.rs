//! The staff test chat's profile read (B4 T9): `chatProfile` (web `areas/live.ts`, `types/live.ts` `ChatProfile`).
//! Any member.
//!
//! No content is compiled in: the audience is the job's `jobs.audience` (`customers`, or `employees` shown as
//! `staff`) and the quick prompts are empty. A staff test chat
//! with a customer takes the test customer's email from the staff member (routes/chat.rs).
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::providers::llm::{self, Use};
use crate::runtime::design::RuntimeDesign;
use crate::state::AppState;
use crate::tenancy::JobMember;
use crate::turn::REFUSAL_TEXT;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/jobs/{job}/chat/profile", get(chat_profile))
}

/// The chat audience of the job, from `jobs.audience`: `staff` for an `employees` job, else `customers`
/// (404 `job_not_found` when the job does not exist).
pub async fn audience_of(conn: &mut PgConnection, job_id: Uuid) -> ApiResult<&'static str> {
    let audience: String = sqlx::query_scalar("SELECT audience FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "No such job.".into()))?;
    Ok(if audience == "employees" {
        "staff"
    } else {
        "customers"
    })
}

/// Who a staff test chat talks as: the typed test customer, or the signed-in employee of a staff-audience job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestParty {
    pub customer_id: String,
    pub name: String,
}

/// The connector instances an env reaches (`all` serves both).
async fn systems(conn: &mut PgConnection, env: &'static str) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM connector_instance WHERE env = $1 OR env = 'all' ORDER BY id",
    )
    .bind(env)
    .fetch_all(conn)
    .await?)
}

/// `chatProfile`.
async fn chat_profile(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let audience = audience_of(&mut tx, j.job_id).await?;
    let row = sqlx::query(
        "SELECT label, current_release_id IS NOT NULL AS has_release FROM jobs WHERE id = $1",
    )
    .bind(j.job_id)
    .fetch_one(&mut *tx)
    .await?;
    let has_release: bool = row.get("has_release");
    let test = systems(&mut tx, "test").await?;
    let live = systems(&mut tx, "prod").await?;
    let readiness = test_readiness(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "audience": audience,
        "agent_name": row.get::<String, _>("label"),
        "prompts": Vec::<String>::new(),
        "test_systems": test.join(", "),
        "live_systems": if has_release { Some(live.join(", ")) } else { None },
        "has_release": has_release,
        "refusal_text": REFUSAL_TEXT,
        "test_readiness": readiness,
    })))
}

/// What the DRAFT needs before a test chat can answer: `{qualify_bound, template_errors, items}`. `qualify_bound` =
/// the draft's `qualify` use resolves to a model ([`llm::resolve`]: a BYO binding, or the inference-router under the
/// tenant's own licence). Items: `llm_inference_not_provisioned` when it does not, `template_errors` when the draft template does not compile (the runtime then runs the built-ins).
async fn test_readiness(
    conn: &mut PgConnection,
    st: &AppState,
    j: &crate::tenancy::JobCtx,
) -> ApiResult<Value> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let bound = matches!(
        llm::resolve(
            &mut *conn,
            st,
            j.t.tenant,
            &job,
            Use::Qualify,
            &DesignOf::Draft
        )
        .await,
        Ok(Some(_))
    );
    let errors = RuntimeDesign::load(&mut *conn, &st.design, &job, &DesignOf::Draft)
        .await?
        .template_errors;
    Ok(test_readiness_json(bound, errors, &j.job_key))
}

/// The `test_readiness` JSON from its two facts.
pub fn test_readiness_json(
    qualify_bound: bool,
    template_errors: Vec<String>,
    job_key: &str,
) -> Value {
    let mut items = Vec::new();
    if !qualify_bound {
        items.push(json!({
            "code": llm::INFERENCE_NOT_PROVISIONED_CODE,
            "title": "Language model access is still being set up",
            "fix_label": "View setup status", "href": "/admin",
        }));
    }
    if !template_errors.is_empty() {
        items.push(json!({
            "code": "template_errors", "title": "The job\u{2019}s template does not compile",
            "fix_label": "Open the job", "href": format!("/admin/jobs/{job_key}"),
        }));
    }
    json!({"qualify_bound": qualify_bound, "template_errors": template_errors, "items": items})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_readiness_lists_what_is_missing() {
        let ok = test_readiness_json(true, Vec::new(), "care");
        assert_eq!(ok["qualify_bound"], true);
        assert_eq!(ok["items"], json!([]));
        let bad = test_readiness_json(false, vec!["x:y".into()], "care");
        assert_eq!(bad["qualify_bound"], false);
        assert_eq!(bad["template_errors"], json!(["x:y"]));
        assert_eq!(bad["items"][0]["code"], "llm_inference_not_provisioned");
        assert_eq!(
            bad["items"][0]["title"],
            "Language model access is still being set up"
        );
        assert_eq!(bad["items"][1]["code"], "template_errors");
        assert_eq!(bad["items"][1]["href"], "/admin/jobs/care");
    }
}
