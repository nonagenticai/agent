//! The handover feedback loop (D64, B4 T16; fixture `handoverFeedback` / `draftHandoverProposal`).
//!
//! - [`record`]: the person's verdict on a handed-over item, live data (one row per item in
//!   `agent_handover_feedback`). `agent_should_handle` attaches the item's `proposal_hint`, else the `knowledge_gap`
//!   default; `right_to_hand_over` attaches nothing. One verdict per item (SW T4); a second is
//!   409 `feedback_exists`. [`give`] records its audit row. [`draft`] saves a draftable proposal through the
//!   activity design target (B8 T7) and marks the feedback drafted.
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::design::Registry;
use crate::error::{ApiError, ApiResult};
use crate::release::compile::TemplateCompiler;
use crate::tenancy::JobCtx;

/// The two verdicts (web `HandoverFeedback.verdict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    AgentShouldHandle,
    RightToHandOver,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::AgentShouldHandle => "agent_should_handle",
            Verdict::RightToHandOver => "right_to_hand_over",
        }
    }
}

/// The longest feedback note (0063 CHECK); a longer one is cut.
pub const NOTE_MAX: usize = 2000;

/// The proposal kinds the queue saves to the draft itself (`J21Handover.ts` `draftable`).
pub const DRAFTABLE: [&str; 2] = ["tenant_extension", "enable_activity"];

/// The `knowledge_gap` default of an item without a `proposal_hint` (fixture `handoverFeedback`).
pub fn default_proposal(job_key: &str, case_ref: Option<&str>) -> Value {
    let href = format!("/admin/jobs/{job_key}/cases/{}", case_ref.unwrap_or(""));
    json!({
        "kind": "knowledge_gap",
        "activity": null,
        "stage": null,
        "label": "Review what the agent was missing",
        "detail": "No specific change was derived; look at the case timeline for the gap.",
        "href": href.trim_end_matches('/'),
    })
}

/// An item's feedback row, as the checks read it.
#[derive(Clone, Debug)]
pub struct Feedback {
    pub verdict: String,
    pub proposal: Option<Value>,
    pub drafted: bool,
}

/// The item's feedback row, locked for the rest of the transaction.
pub async fn of_item(c: &mut PgConnection, item_id: &str) -> ApiResult<Option<Feedback>> {
    let row = sqlx::query(
        "SELECT verdict, proposal, drafted_at IS NOT NULL AS drafted \
           FROM agent_handover_feedback WHERE item_id = $1 FOR UPDATE",
    )
    .bind(item_id)
    .fetch_optional(&mut *c)
    .await?;
    Ok(row.map(|r| Feedback {
        verdict: r.get("verdict"),
        proposal: r.get("proposal"),
        drafted: r.get("drafted"),
    }))
}

/// 409 `feedback_exists`: an item takes one verdict.
pub fn feedback_exists(item_id: &str) -> ApiError {
    ApiError::Conflict(
        "feedback_exists",
        format!("{item_id} already has a verdict."),
    )
}

/// Record `verdict` on item `item_id` of job `job_id` (the caller holds the item's lock): one verdict per item, a
/// second is 409 `feedback_exists`. Returns the proposal attached.
pub async fn record(
    c: &mut PgConnection,
    job_id: Uuid,
    job_key: &str,
    item_id: &str,
    verdict: Verdict,
    note: &str,
    by: &str,
) -> ApiResult<Option<Value>> {
    if of_item(&mut *c, item_id).await?.is_some() {
        return Err(feedback_exists(item_id));
    }
    let note: String = note.trim().chars().take(NOTE_MAX).collect();
    let proposal = match verdict {
        Verdict::RightToHandOver => None,
        Verdict::AgentShouldHandle => {
            let (hint, case_ref): (Option<Value>, Option<String>) = sqlx::query_as(
                "SELECT proposal_hint, case_id FROM agent_handover_items WHERE id = $1",
            )
            .bind(item_id)
            .fetch_one(&mut *c)
            .await?;
            Some(
                hint.filter(|h| h.is_object())
                    .unwrap_or_else(|| default_proposal(job_key, case_ref.as_deref())),
            )
        }
    };
    let stored: Option<String> = sqlx::query_scalar(
        "INSERT INTO agent_handover_feedback (tenant_id, job_id, item_id, verdict, note, by_sub, at, proposal) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, $5, now(), $6) \
         ON CONFLICT (tenant_id, job_id, item_id) DO NOTHING RETURNING item_id",
    )
    .bind(job_id)
    .bind(item_id)
    .bind(verdict.as_str())
    .bind(note)
    .bind(by)
    .bind(proposal.clone())
    .fetch_optional(&mut *c)
    .await?;
    if stored.is_none() {
        return Err(feedback_exists(item_id));
    }
    Ok(proposal)
}

/// [`record`] and its `handover.feedback` audit row (the verdict and the proposal kind, never the note).
#[allow(clippy::too_many_arguments)]
pub async fn give(
    c: &mut PgConnection,
    actor: &Actor,
    job_id: Uuid,
    job_key: &str,
    item_id: &str,
    verdict: Verdict,
    note: &str,
    by: &str,
) -> ApiResult<()> {
    let proposal = record(&mut *c, job_id, job_key, item_id, verdict, note, by).await?;
    let kind = proposal
        .as_ref()
        .and_then(|p| p.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    audit::record(
        &mut *c,
        actor,
        Event::new("handover.feedback", "handover_item", item_id.to_string())
            .job(job_key)
            .detail("verdict", verdict.as_str())
            .detail("proposal", kind),
    )
    .await?;
    Ok(())
}

/// `draftHandoverProposal` for item `item_id` (B8 T7 registered the `activity` kind): in the fixture's order 409
/// `no_proposal`, 409 `already_drafted`, 422 `not_draftable_here`; then the proposal is written to the draft as the
/// J4 write it stands for ([`crate::journey::refine::draft_from_handover`]: `enable_activity` undoes the hide,
/// `tenant_extension` adds the extension; 409 `already_enabled` / `already_extended`) and the item's feedback is
/// marked drafted (frozen: its verdict can no longer change). The caller holds the item's lock and the job-scoped
/// transaction, and has run `ensure_job`. Returns the job's `draft_rev` after the write.
pub async fn draft(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    item_id: &str,
) -> ApiResult<i64> {
    let fb = of_item(&mut *c, item_id).await?;
    let Some(f) = fb.filter(|f| {
        f.verdict.as_str() == Verdict::AgentShouldHandle.as_str()
            && f.proposal.as_ref().is_some_and(Value::is_object)
    }) else {
        return Err(ApiError::Conflict(
            "no_proposal",
            format!("Mark {item_id} as \u{201c}agent should have handled\u{201d} first."),
        ));
    };
    let p = f.proposal.unwrap_or_default();
    if f.drafted {
        return Err(ApiError::Conflict(
            "already_drafted",
            format!("The proposal from {item_id} is already in the draft."),
        ));
    }
    let kind = p.get("kind").and_then(Value::as_str).unwrap_or("");
    let activity = p
        .get("activity")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty());
    if !DRAFTABLE.contains(&kind) || activity.is_none() {
        let href = p.get("href").and_then(Value::as_str).unwrap_or("");
        return Err(ApiError::Unprocessable(
            "not_draftable_here",
            format!("Make this change on its own page: {href}."),
        ));
    }
    let subject: String =
        sqlx::query_scalar("SELECT subject FROM agent_handover_items WHERE id = $1")
            .bind(item_id)
            .fetch_one(&mut *c)
            .await?;
    let rev = crate::journey::refine::draft_from_handover(
        &mut *c, reg, compiler, j, &p, &subject, item_id,
    )
    .await?;
    sqlx::query(
        "UPDATE agent_handover_feedback SET drafted_at = now() WHERE job_id = $1 AND item_id = $2",
    )
    .bind(j.job_id)
    .bind(item_id)
    .execute(&mut *c)
    .await?;
    Ok(rev)
}
