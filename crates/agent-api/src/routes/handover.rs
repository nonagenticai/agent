//! The handover routes B4 T16 adds next to B12's queue routes (crate::conversations::routes): the SLA policy, the
//! feedback verdict and the draft proposal (route table: `handoverPolicy`, `saveHandoverPolicy`, `handoverFeedback`,
//! `draftHandoverProposal`).
//!
//! - `handoverPolicy`: any member; `{job, live, draft, drafted}` (web `HandoverPolicyView`).
//! - `saveHandoverPolicy`: `support_manager`; `{targets, hours?}`. Validates (`J21Handover.ts` `policyError`, 422
//!   `invalid_policy`; a bad `hours` (T16a) is 422 `invalid_policy` with `details.field = "hours"`), then writes the
//!   draft through `design::track` (kind `handover_policy`, D54: never live on save; live items keep the due times
//!   they were stamped with). No `hours` keeps the stored hours; `hours: null` = always staffed.
//! - `handoverFeedback`: any of `support_agent`, `support_manager`, `operator`, `job_designer` (SW T4: the staff roles
//!   that work a conversation, crate::conversations::routes::STAFF_ROLES; the role is checked before the body is used,
//!   else 403 `missing_role: support_agent`); live data, one verdict per item (a second is 409 `feedback_exists`), audited
//!   `handover.feedback` (verdict only, never the note). Resolve may carry the verdict too
//!   (crate::conversations::ownership::resolve).
//! - `draftHandoverProposal`: `job_designer`; answers its refusals and writes a draftable proposal to the draft as a
//!   J4 `activity` write (B8 T7, crate::handover::feedback::draft); the answer is the item.
//!
//! An unknown item is 404 `case_not_found` (the UI branches on it).
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::{get, post};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::auth::Role;
use crate::conversations::routes::STAFF_ROLES;
use crate::conversations::{handover as queue, staff};
use crate::design::{self, DraftRef, DraftWrite, JobRef, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::handover::feedback::{self, Verdict};
use crate::handover::{hours, policy};
use crate::interaction::Scope;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, missing_role, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/handover/policy",
            get(handover_policy).put(save_handover_policy),
        )
        .route(
            "/api/jobs/{job}/handover/{id}/feedback",
            post(handover_feedback),
        )
        .route(
            "/api/jobs/{job}/handover/{id}/feedback/draft",
            post(draft_handover_proposal),
        )
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

async fn handover_policy(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let v = policy::view(&mut tx, &job_ref(&j)).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct PolicyBody {
    targets: Value,
    /// Absent = keep the stored hours; `null` = always staffed (T16a).
    #[serde(default, deserialize_with = "present")]
    hours: Option<Value>,
}

/// A field that is present (`null` included) is `Some`.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// 422 `invalid_policy` naming the field.
fn invalid_hours(msg: String) -> ApiError {
    ApiError::Unprocessable("invalid_policy", msg).with("details", json!({"field": "hours"}))
}

async fn save_handover_policy(
    JobRequires(j, _): JobRequires<role::SupportManager>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<PolicyBody>,
) -> ApiResult<Json<Value>> {
    let targets = policy::validate(&b.targets)
        .map_err(|msg| ApiError::Unprocessable("invalid_policy", msg))?;
    let hours = match &b.hours {
        None => None,
        Some(Value::Null) => Some(None),
        Some(h) => Some(Some(hours::validate(h).map_err(invalid_hours)?)),
    };
    let mut tx = j.begin(&st.db).await?;
    if let Some(Some(h)) = &hours
        && !hours::known_timezone(&mut tx, &h.timezone).await?
    {
        return Err(invalid_hours("hours: unknown timezone".into()));
    }
    let w = DraftWrite {
        area: "handover",
        page: "J21",
        kind: policy::KIND,
        r#ref: DraftRef::new(),
        label: Label::Now(format!(
            "handover SLA: {}",
            policy::policy_text(Some(&targets))
        )),
    };
    let (tenant, job_id, by) = (j.t.tenant, j.job_id, j.t.caller.sub.clone());
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            policy::write_targets(
                c,
                tenant,
                job_id,
                &targets,
                hours.as_ref().map(Option::as_ref),
                &by,
            )
            .await
        })
    })
    .await?;
    let v = policy::view(&mut tx, &job_ref(&j)).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct FeedbackBody {
    verdict: Verdict,
    #[serde(default)]
    note: String,
}

async fn handover_feedback(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    if !STAFF_ROLES.iter().any(|r| j.t.effective.has(*r)) {
        return Err(missing_role(Role::SupportAgent));
    }
    let b: FeedbackBody = serde_json::from_value(raw).map_err(|e| {
        ApiError::BadRequest(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })?;
    let mut tx = j.begin(&st.db).await?;
    // the feedback names the caller
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    let item = queue::lock(&mut tx, &id).await?;
    feedback::give(
        &mut tx,
        &j.t.actor(),
        j.job_id,
        &j.job_key,
        &id,
        b.verdict,
        &b.note,
        &j.t.caller.sub,
    )
    .await?;
    let v = queue::view(&mut tx, &id).await?;
    tx.commit().await?;
    staff::publish(
        &st,
        &Scope::from(&j),
        &item.conversation_id,
        "handover",
        v.clone(),
    )
    .await;
    Ok(Json(v))
}

async fn draft_handover_proposal(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    queue::lock(&mut tx, &id).await?;
    feedback::draft(&mut tx, &st.design, st.compiler.as_ref(), &j, &id).await?;
    let v = queue::view(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Json(v))
}
