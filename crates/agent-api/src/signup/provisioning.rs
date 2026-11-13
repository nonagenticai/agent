//! What an approval does, step by step, journaled in `signup_provisioning_steps`, in commonpractice's order (CP main
//! 4b5bb275 `routes/onboarding.ts` `approveApprovalRow`; early-access guide): the signup is marked `approved` LAST,
//! only after its workspace, identity and seat landed. A failure before that leaves it `pending` and decides nothing;
//! the next approve resumes into the same workspace, and a reject offboards what the aborted approve founded.
//!
//! The approve takes a claim on the row first (`approving_until` / `approving_claim`, CP `claimApproval`): one
//! approver at a time across replicas. Every step re-confirms and extends the claim before it runs (CP `assertHeld`),
//! so a reject can never land between that check and the step; a lost claim stops the run with 409
//! `approval_claim_lost`, nothing further done. A reject is refused while a claim is live.
//!
//! Gating steps (they must land before the decision):
//! 1. `keycloak_user` — the portal identity for the requester's email: the existing one, else a new one created
//!    WITHOUT an email. An existing identity that already belongs to a workspace is refused (409
//!    `user_belongs_to_another_workspace`, CP `assertApprovalAdmitsUser`) before anything is founded.
//! 2. `tenant` — the `tenants` row. Its id is remembered before the insert (an attempt that died after it is found
//!    again, never founded twice). A taken name becomes `<name>-2` … `<name>-10` (CP `autoSuffixName`, signup
//!    approvals only), then 409 `tenant_name_taken`.
//! 3. `membership` — an identity THIS approval created: the `users` row, the workspace's first member (every role,
//!    D88). ⛔ An identity that existed before is NEVER seated at approval (CP: a pre-existing realm account, whose
//!    email may be unverified, is seated only when it accepts an invite with a verified matching email): a pending
//!    `member_invites` row is recorded instead and the step is `skipped` ([`EXISTING_INVITED`]).
//!
//! Then, attempted inline before the decision and finished by the tick after it (CP: the licence and email steps
//! never block the approval):
//! 4. `licences_tenant` — the tenant registered in licences, its own licence minted and stored (D77 L4).
//! 5. `member_licence` — the requester's licence (D77): `member_entitlement` written `pending`, the licence created
//!    and minted after that commits, stored sealed by a conditional UPDATE (`pending` → `active`). Skipped for an
//!    existing identity (not seated).
//! 6. `invite_email` — ONE email, after every earlier step is ok or skipped: `locale` + `signup_app` on the minted
//!    identity, then the set-password execute-actions-email (UPDATE_PASSWORD, VERIFY_EMAIL; client `agent`, redirect
//!    the app root). Skipped for an existing identity: nothing lets it in until it accepts its invite.
//!
//! Each step is claimed (`lease_until`) by one runner at a time — the approve request or the tick, possibly on
//! another replica — and every action is idempotent or remembers its progress in `data` before the next
//! upstream call, so a lease that lapses under a slow call costs a duplicate no-op, never a duplicate effect.
//! A transient failure is `pending` with bounded exponential backoff (1, 2, 4 … min, 6 h cap, 10 attempts); a
//! refusal that will fail identically again is `failed` and stops the chain. The tick only ever runs the steps of an
//! APPROVED signup (CP decision gate).
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Actor, Event, PlatformEvent};
use crate::auth::RoleSet;
use crate::error::{ApiError, ApiResult};
use crate::identity::{Access, AccessError, IssuedLicence, Stored, store_member_entitlement};
pub use crate::identity::{EXISTING_ACCESS, EXISTING_SET_PASSWORD};
use crate::members::{self, MintOutcome, RevokeResult};
use crate::platform_db::{begin_platform, enter};
use crate::service_token::UpstreamError;
use crate::signup::store::{self, Approval, StepRow};
use crate::signup::{DEFAULT_LOCALE, Signup, default_tenant_name};
use crate::state::AppState;
use crate::tenancy::begin_scoped;
use crate::tenants::app_licence;
use crate::tenants::lifecycle;

/// The steps, in run order (`ord` = index + 1). The first [`GATING`] decide whether the signup is approved.
///
/// `app_licences` (the tenant's own inference-router / strata-rag / strata-ingest licences, crate::tenants::app_licence)
/// runs LAST on purpose: a slow or unready licences bundle must not hold back the member licence and the one invite email
/// (a step that does not land stops the chain), and the tick finishes it on its own.
pub const STEPS: [&str; 7] = [
    "keycloak_user",
    "tenant",
    "membership",
    "licences_tenant",
    "member_licence",
    "invite_email",
    "app_licences",
];
/// How many of [`STEPS`] must land before the signup is marked approved.
pub const GATING: usize = 3;
/// The per-step claim; longer than any one step's upstream calls.
const LEASE_SECS: f64 = 120.0;
/// One approve-claim window (CP `claimMs`); every step extends it before it runs.
pub const CLAIM_SECS: f64 = 300.0;
/// `<name>`, then `<name>-2` … `<name>-10` (CP `SELF_SIGNUP_NAME_ATTEMPTS`).
pub const NAME_ATTEMPTS: u32 = 10;
const BACKOFF_BASE_SECS: f64 = 60.0;
const BACKOFF_CAP_SECS: f64 = 6.0 * 3600.0;
pub const MAX_ATTEMPTS: i32 = 10;
/// The audit actor of the provisioning steps and the licence reconciler (whoever runs them: the approve request or
/// the tick).
pub const TICK_ACTOR: &str = "service:agent-api-signup-tick";
/// Licence work items per reconciler pass and kind (D77 L7).
pub const RECONCILE_BATCH: i64 = 10;
/// Tenants whose app licences the backfill provisions per reconciler pass (each mints, smokes and stores up to three
/// upstream credentials, so it is far smaller than [`RECONCILE_BATCH`]); a failing tenant then waits out its own
/// [`reconcile_backoff_secs`] in `licence_work_backoff` (kind `app_licence`) and does not take a slot.
pub const APP_LICENCE_BATCH: i64 = 3;
/// Failed attempts after which a licence work item is `error` (review #2): audited once more, still retried at the
/// backoff cap.
pub const RECONCILE_MAX_ATTEMPTS: i32 = 8;
const RECONCILE_BACKOFF_BASE_SECS: f64 = 30.0;
const RECONCILE_BACKOFF_CAP_SECS: f64 = 3600.0;
/// Legacy `invite_email` code (rows journaled before the access email existed): nothing was sent.
pub const EXISTING_SKIPPED: &str = "existing_identity";
/// `membership`, `member_licence` and `invite_email` code for a requester whose portal identity existed before the
/// approval: not seated, no licence, no email; a pending `member_invites` row holds their tenant_admin seat.
pub const EXISTING_INVITED: &str = "existing_identity_invite_pending";
/// 409: the address (or its identity) already has a member row in another workspace.
pub const BELONGS_ELSEWHERE: &str = "user_belongs_to_another_workspace";

/// Backoff before attempt `attempts + 1`.
pub fn backoff_secs(attempts: i32) -> f64 {
    let exp = (attempts.max(1) - 1).min(30);
    (BACKOFF_BASE_SECS * 2f64.powi(exp)).min(BACKOFF_CAP_SECS)
}

/// What one step run produced.
enum Outcome {
    Ok(Value),
    /// `ok`, with a code and a note that say HOW (no personal data in either).
    Done(&'static str, String, Value),
    /// `skipped`, with its code and note (no personal data).
    Skip(&'static str, String),
    /// Transient: the tick retries it.
    Retry(&'static str, String),
    /// Will fail identically next time: stops the chain.
    Fail(&'static str, String),
    /// `ok`, already recorded by the step itself in the transaction that stored its result (the member licence,
    /// D77 L11).
    Recorded,
}

impl Outcome {
    fn landed(&self) -> bool {
        matches!(
            self,
            Outcome::Ok(_) | Outcome::Done(..) | Outcome::Skip(..) | Outcome::Recorded
        )
    }
}

fn tick_actor() -> Actor {
    Actor::system(TICK_ACTOR)
}

/// The platform-log label of a signup: the workspace name it suggests (never the address).
pub fn signup_label(a: &Approval) -> String {
    default_tenant_name(a.company.as_deref(), &a.email, Some(a.name.as_str()))
}

/// An upstream failure as a step outcome: transport, 5xx, 408 and 429 retry; any other status is a refusal.
fn upstream(code: &'static str, e: &UpstreamError) -> Outcome {
    let msg: String = e.message.chars().take(400).collect();
    match e.status {
        None => Outcome::Retry(code, msg),
        Some(s) if s >= 500 || s == 408 || s == 429 => Outcome::Retry(code, msg),
        Some(_) => Outcome::Fail(code, msg),
    }
}

/// 409 [`BELONGS_ELSEWHERE`] (CP `assertApprovalAdmitsUser`).
pub fn belongs_elsewhere() -> ApiError {
    ApiError::Conflict(
        BELONGS_ELSEWHERE,
        "this email already belongs to another workspace; approving would move that person out of it".into(),
    )
}

/// Does `email` (or the identity `sub`) already have a member row in a workspace other than `except`? Disabled and
/// invited members count: an approval never moves anybody.
pub async fn belongs_to_another(
    st: &AppState,
    email: &str,
    sub: Option<&str>,
    except: Option<Uuid>,
) -> ApiResult<bool> {
    let homes: Vec<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM agent_signup_homes($1, $2)")
        .bind(email)
        .bind(sub)
        .fetch_all(&st.db)
        .await?;
    Ok(homes.iter().any(|t| Some(*t) != except))
}

/// The result of a decision call.
pub enum Decision {
    Approved {
        approval: Box<Approval>,
        tenant: Uuid,
        tenant_name: String,
    },
    /// Rejected; with what was undone of an aborted approve.
    Rejected(Box<Approval>, Cleanup),
    /// Not pending any more; the row as it stands.
    AlreadyDecided(Box<Approval>),
    /// Another approve holds the claim right now (CP `approval_in_progress`).
    InProgress(Box<Approval>),
    NotFound,
}

/// What a reject undid of an aborted approve (CP reject repair).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// The workspace it founded, now offboarded.
    pub offboarded: Option<Uuid>,
    /// The portal identity it created: `deleted` | `absent` | `disabled` | `disabled_unmarked` | `kept_in_use` |
    /// `failed: …` (CP `retireRejectedIdentity`); `None` when it created none (a pre-existing identity is never
    /// touched).
    pub identity: Option<String>,
}

impl Cleanup {
    pub fn is_empty(&self) -> bool {
        self.offboarded.is_none() && self.identity.is_none()
    }
}

/// The attribute that marks an identity a rejected signup retired (`agent_rejected:<signup id>`; CP
/// `signup_app=cp_rejected:<id>`).
pub const REJECT_MARKER_ATTR: &str = "signup_app";
/// The value prefix of [`REJECT_MARKER_ATTR`] on an identity a reject retired.
pub const REJECT_MARKER_PREFIX: &str = "agent_rejected:";

/// What an approve founds when its `tenant` step runs.
pub struct TenantInput<'a> {
    pub name: &'a str,
    pub residency: &'a str,
}

/// The approve run's claim on its signup row (CP `HeldApprovalClaim`). No heartbeat: every step re-confirms and
/// extends it ([`Claim::hold`]) before it runs, and no single step outlasts a window.
pub struct Claim {
    id: Uuid,
    token: Uuid,
}

fn claim_lost(step: &str) -> ApiError {
    ApiError::Conflict(
        "approval_claim_lost",
        format!(
            "the approve stopped before '{step}': this run no longer holds the signup; nothing further was done; approve again to resume"
        ),
    )
}

impl Claim {
    /// Call BEFORE every step: 409 `approval_claim_lost` unless the claim is still ours, live and the row pending;
    /// afterwards it is live for a full window.
    async fn hold(&self, st: &AppState, step: &str) -> ApiResult<()> {
        let mut tx = begin_platform(&st.db).await?;
        let held = store::hold(&mut tx, self.id, self.token, CLAIM_SECS).await?;
        tx.commit().await?;
        if held { Ok(()) } else { Err(claim_lost(step)) }
    }

    /// Release the claim (best effort: a lapsed one releases itself).
    async fn release(&self, st: &AppState) {
        let res = async {
            let mut tx = begin_platform(&st.db).await?;
            store::release(&mut tx, self.id, self.token).await?;
            tx.commit().await?;
            Ok::<_, ApiError>(())
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(signup = %self.id, error = %e, "signup_approve_claim_release_failed");
        }
    }
}

/// Who runs a chain of steps: the approve (its founding input, its claim) or the tick / Retry (neither).
struct Ctx<'a> {
    input: Option<&'a TenantInput<'a>>,
    claim: Option<&'a Claim>,
    actor: Actor,
}

/// Approve the PENDING signup `id` the commonpractice way: claim it, refuse an address that belongs to another
/// workspace, run the gating steps (identity, workspace, seat), attempt the rest, and only then mark it approved
/// (conditional on the claim). A gating step that does not land is an error ([`not_landed`]): the row stays
/// pending, the claim is released, and the next approve resumes.
pub async fn approve(
    st: &AppState,
    sg: &Signup,
    id: Uuid,
    input: &TenantInput<'_>,
    by: &str,
    via: &str,
    actor: &Actor,
) -> ApiResult<Decision> {
    let token = Uuid::new_v4();
    let mut tx = begin_platform(&st.db).await?;
    let claimed = store::claim(&mut tx, id, token, CLAIM_SECS).await?;
    let current = if claimed.is_some() {
        None
    } else {
        store::get(&mut tx, id).await?
    };
    tx.commit().await?;
    let a = match (claimed, current) {
        (Some(a), _) => a,
        (None, None) => return Ok(Decision::NotFound),
        (None, Some(a)) if a.status != "pending" => {
            return Ok(Decision::AlreadyDecided(Box::new(a)));
        }
        (None, Some(a)) => return Ok(Decision::InProgress(Box::new(a))),
    };
    let claim = Claim { id, token };
    let out = approve_claimed(st, sg, &a, &claim, input, by, via, actor).await;
    claim.release(st).await;
    out
}

#[allow(clippy::too_many_arguments)]
async fn approve_claimed(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    claim: &Claim,
    input: &TenantInput<'_>,
    by: &str,
    via: &str,
    actor: &Actor,
) -> ApiResult<Decision> {
    let id = a.id;
    // The journal: every step, born pending; a resumed approve keeps what an earlier attempt recorded, and a gating
    // step it left waiting or failed runs again now.
    let mut tx = begin_platform(&st.db).await?;
    for (i, step) in STEPS.iter().enumerate() {
        sqlx::query(
            "INSERT INTO signup_provisioning_steps (signup_id, step, ord, status, attempts, data, next_attempt_at) \
             VALUES ($1, $2, $3, 'pending', 0, '{}', now()) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(*step)
        .bind(i16::try_from(i + 1).unwrap_or(i16::MAX))
        .execute(&mut *tx)
        .await?;
    }
    let gating: Vec<String> = STEPS[..GATING].iter().map(|s| s.to_string()).collect();
    sqlx::query(
        "UPDATE signup_provisioning_steps \
            SET status = 'pending', attempts = 0, next_attempt_at = now(), code = NULL, message = NULL, \
                lease_until = NULL, updated_at = now() \
          WHERE signup_id = $1 AND step = ANY($2) AND status IN ('pending', 'failed')",
    )
    .bind(id)
    .bind(&gating)
    .execute(&mut *tx)
    .await?;
    let rows = store::steps(&mut tx, id).await?;
    tx.commit().await?;

    // Refuse BEFORE creating anything (CP `assertApprovalAdmitsUser`); the workspace this approval founded earlier is
    // the resume case and passes.
    if belongs_to_another(st, &a.email, None, planned_tenant(&rows)).await? {
        return Err(belongs_elsewhere());
    }
    let ctx = Ctx {
        input: Some(input),
        claim: Some(claim),
        actor: actor.clone(),
    };
    let gated = run_chain(st, sg, a, &STEPS[..GATING], &ctx).await;
    if !matches!(gated, Ok(None)) {
        restore_reclaimed(st, id).await;
    }
    if let Some((code, message)) = gated? {
        let mut tx = begin_platform(&st.db).await?;
        let steps = store::steps(&mut tx, id).await?;
        tx.commit().await?;
        return Err(not_landed(code, message, &steps));
    }
    // The licence and email steps: attempted now; whatever does not land is the tick's once the signup is approved.
    run_chain(st, sg, a, &STEPS[GATING..], &ctx).await?;

    claim.hold(st, "decide").await?;
    let mut tx = begin_platform(&st.db).await?;
    let rows = store::steps(&mut tx, id).await?;
    let (Some(tenant), Some(tenant_name)) = (founded_tenant(&rows), founded_name(&rows)) else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "signup {id}: the gating steps landed without a workspace"
        )));
    };
    let Some(approval) = store::mark_approved(&mut tx, id, tenant, by, via, claim.token).await?
    else {
        return Err(claim_lost("decide"));
    };
    let seated = rows
        .iter()
        .any(|r| r.step == "membership" && r.status == "ok");
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new("signup.approve", tenant_name.clone())
            .tenant(tenant)
            .detail("signup", id.to_string())
            .detail("via", via.to_string())
            .detail("by", by.to_string())
            .detail("residency", input.residency.to_string())
            .detail("seated", seated),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(signup = %id, %tenant, via, seated, "signup_approved");
    Ok(Decision::Approved {
        approval: Box::new(approval),
        tenant,
        tenant_name,
    })
}

/// A gating step that did not land, as the approve's answer: 409 for a refusal the super_admin resolves
/// ([`BELONGS_ELSEWHERE`], `tenant_name_taken`, `user_in_other_tenant`), else 502 with the step's code. The error
/// object carries the journal (`steps`). Nothing was decided.
pub fn not_landed(code: &'static str, message: String, steps: &[StepRow]) -> ApiError {
    let status = match code {
        BELONGS_ELSEWHERE | "tenant_name_taken" | "user_in_other_tenant" | "step_in_progress" => {
            axum::http::StatusCode::CONFLICT
        }
        _ => axum::http::StatusCode::BAD_GATEWAY,
    };
    let steps: Vec<Value> = steps
        .iter()
        .map(|r| json!({"step": r.step, "status": r.status, "code": r.code}))
        .collect();
    ApiError::detailed(
        status,
        code,
        format!("the approval did not complete; nothing was decided ({message})"),
    )
    .with("steps", steps)
}

/// Reject the PENDING signup `id`. Refused while an approve holds its claim ([`Decision::InProgress`], CP 409
/// `approval_in_progress`). The workspace an aborted approve founded is offboarded after the decision commits (CP
/// reject repair: licences revoked, members disabled; the Keycloak identity is left as it is).
pub async fn reject(
    st: &AppState,
    id: Uuid,
    by: &str,
    via: &str,
    reason: Option<&str>,
    actor: &Actor,
) -> ApiResult<Decision> {
    let mut tx = begin_platform(&st.db).await?;
    let Some(a) = store::lock(&mut tx, id).await? else {
        return Ok(Decision::NotFound);
    };
    if a.status != "pending" {
        return Ok(Decision::AlreadyDecided(Box::new(a)));
    }
    if a.approving {
        return Ok(Decision::InProgress(Box::new(a)));
    }
    let Some(a) = store::mark_rejected(&mut tx, id, by, via, reason).await? else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "signup {id} stopped being pending under its row lock"
        )));
    };
    let rows = store::steps(&mut tx, id).await?;
    let planned = planned_tenant(&rows);
    // Only an identity THIS approval minted (`existing: false`); a pre-existing one is never touched.
    let minted = identity(&rows)
        .filter(|(_, existing)| !existing)
        .map(|(sub, _)| sub);
    // The reason is free text a person typed: never in the audit row.
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new("signup.reject", signup_label(&a))
            .detail("signup", id.to_string())
            .detail("via", via.to_string())
            .detail("aborted_workspace", planned.is_some()),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(signup = %id, via, "signup_rejected");
    let mut cleanup = Cleanup::default();
    if let Some(t) = planned {
        match offboard_aborted(st, actor, t).await {
            Ok(true) => cleanup.offboarded = Some(t),
            Ok(false) => {}
            Err(e) => {
                tracing::error!(signup = %id, tenant = %t, error = %e, "signup_reject_offboard_failed")
            }
        }
    }
    if let Some(sub) = minted {
        cleanup.identity = Some(retire_minted(st, &a, &sub, planned).await);
    }
    if !cleanup.is_empty() {
        let mut e = PlatformEvent::new("signup.reject_repair", signup_label(&a))
            .detail("signup", id.to_string())
            .detail("workspace_offboarded", cleanup.offboarded.is_some());
        if let Some(t) = cleanup.offboarded {
            e = e.tenant(t);
        }
        if let Some(i) = &cleanup.identity {
            e = e.detail("identity", i.clone());
        }
        if let Err(err) = audit::record_platform_alone(&st.db, actor, e).await {
            tracing::warn!(signup = %id, error = %err, "signup_reject_repair_audit_failed");
        }
    }
    Ok(Decision::Rejected(Box::new(a), cleanup))
}

/// Retire the portal identity an aborted approve minted for the rejected signup `a` (CP `retireRejectedIdentity`),
/// unless a member row outside `tenant` still references it or its address: `kept_in_use`. No credential and no IdP
/// link (Keycloak says so positively) → deleted, so a re-signup mints a fresh one; anything else → disabled and
/// marked `signup_app=agent_rejected:<id>` in one read-modify-write, read back. Never fails the reject: an error is
/// `failed: …`, logged for a hand disable.
async fn retire_minted(st: &AppState, a: &Approval, sub: &str, tenant: Option<Uuid>) -> String {
    match belongs_to_another(st, &a.email, Some(sub), tenant).await {
        Ok(true) => return "kept_in_use".into(),
        Ok(false) => {}
        Err(e) => {
            tracing::error!(signup = %a.id, error = %e, "signup_reject_identity_retire_failed");
            return "failed: could not check the identity's other workspaces".into();
        }
    }
    let kc = &st.identity.keycloak;
    let marker = format!("{REJECT_MARKER_PREFIX}{}", a.id);
    let res: Result<&'static str, UpstreamError> = async {
        if kc.get_user(sub).await?.is_none() {
            return Ok("absent");
        }
        if kc.has_credential_or_link(sub).await? == Some(false) {
            kc.delete_user(sub).await?;
            return Ok("deleted");
        }
        Ok(
            if kc
                .disable_and_mark(sub, REJECT_MARKER_ATTR, &marker)
                .await?
            {
                "disabled"
            } else {
                "disabled_unmarked"
            },
        )
    }
    .await;
    match res {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::error!(
                signup = %a.id,
                error = %e,
                remediation = "the portal identity minted for this rejected signup may still be ENABLED; disable it by hand",
                "signup_reject_identity_retire_failed"
            );
            format!(
                "failed: {}",
                e.message.chars().take(200).collect::<String>()
            )
        }
    }
}

/// Offboard the workspace an aborted approve founded (when it was founded at all): `true` when it is now archived.
async fn offboard_aborted(st: &AppState, actor: &Actor, tenant: Uuid) -> ApiResult<bool> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    let Some(name) = name else {
        return Ok(false);
    };
    lifecycle::offboard(st, actor, tenant, &name).await?;
    Ok(true)
}

/// Claim `step` of `id` if it is pending, due and unclaimed; returns its attempts and data.
async fn claim_step(st: &AppState, id: Uuid, step: &str) -> ApiResult<Option<(i32, Value)>> {
    let mut tx = begin_platform(&st.db).await?;
    let claimed = sqlx::query_as(
        "UPDATE signup_provisioning_steps \
            SET lease_until = now() + make_interval(secs => $3), updated_at = now() \
          WHERE signup_id = $1 AND step = $2 AND status = 'pending' \
            AND (next_attempt_at IS NULL OR next_attempt_at <= now()) \
            AND (lease_until IS NULL OR lease_until < now()) \
          RETURNING attempts, data",
    )
    .bind(id)
    .bind(step)
    .bind(LEASE_SECS)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(claimed)
}

/// Merge `patch` into the step's `data` (progress an upstream call must not repeat).
async fn remember(st: &AppState, id: Uuid, step: &str, patch: &Value) -> ApiResult<()> {
    let mut tx = begin_platform(&st.db).await?;
    sqlx::query(
        "UPDATE signup_provisioning_steps SET data = data || $3, updated_at = now() \
          WHERE signup_id = $1 AND step = $2",
    )
    .bind(id)
    .bind(step)
    .bind(patch)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Record an outcome and release the claim, in its own platform transaction.
async fn record(
    st: &AppState,
    id: Uuid,
    step: &str,
    attempts: i32,
    out: &Outcome,
) -> ApiResult<()> {
    let mut tx = begin_platform(&st.db).await?;
    record_in(&mut tx, id, step, attempts, out).await?;
    tx.commit().await?;
    Ok(())
}

/// Record an outcome and release the claim on `conn` (which must be `agent_platform` now).
async fn record_in(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    step: &str,
    attempts: i32,
    out: &Outcome,
) -> ApiResult<()> {
    let attempts = attempts + 1;
    let (status, code, message, patch, next): (
        &str,
        Option<&str>,
        Option<String>,
        Value,
        Option<f64>,
    ) = match out {
        Outcome::Ok(d) => ("ok", None, None, d.clone(), None),
        Outcome::Recorded => return Ok(()),
        Outcome::Done(c, m, d) => ("ok", Some(*c), Some(m.clone()), d.clone(), None),
        Outcome::Skip(c, m) => ("skipped", Some(*c), Some(m.clone()), json!({}), None),
        Outcome::Retry(c, m) if attempts < MAX_ATTEMPTS => (
            "pending",
            Some(*c),
            Some(m.clone()),
            json!({}),
            Some(backoff_secs(attempts)),
        ),
        Outcome::Retry(c, m) | Outcome::Fail(c, m) => {
            ("failed", Some(*c), Some(m.clone()), json!({}), None)
        }
    };
    sqlx::query(
        "UPDATE signup_provisioning_steps \
            SET status = $3, code = $4, message = $5, data = data || $6, attempts = $7, \
                last_attempt_at = now(), lease_until = NULL, updated_at = now(), \
                next_attempt_at = CASE WHEN $8::float8 IS NULL THEN NULL \
                                       ELSE now() + make_interval(secs => $8::float8) END \
          WHERE signup_id = $1 AND step = $2",
    )
    .bind(id)
    .bind(step)
    .bind(status)
    .bind(code)
    .bind(message)
    .bind(patch)
    .bind(attempts)
    .bind(next)
    .execute(conn)
    .await?;
    if out.landed() {
        tracing::info!(signup = %id, step, status, "signup_provisioning_step")
    } else {
        tracing::warn!(signup = %id, step, status, code, attempts, "signup_provisioning_step")
    }
    Ok(())
}

/// Run every outstanding step of the APPROVED signup `id` in order, as far as they go (the tick and Retry). A
/// pending signup's steps run only under its approve claim, never here (CP decision gate).
pub async fn run(st: &AppState, sg: &Signup, id: Uuid) -> ApiResult<()> {
    let mut tx = begin_platform(&st.db).await?;
    let Some(a) = store::get(&mut tx, id).await? else {
        return Ok(());
    };
    tx.commit().await?;
    if a.status != "approved" {
        return Ok(());
    }
    let ctx = Ctx {
        input: None,
        claim: None,
        actor: tick_actor(),
    };
    run_chain(st, sg, &a, &STEPS, &ctx).await?;
    Ok(())
}

/// Run `steps` of `a` in order. `None` when every one is ok or skipped; else the (code, note) of the first that is
/// not (pending for later, failed for good, or claimed by another runner). Under an approve claim, the claim is
/// re-confirmed before each step and a lost claim is the error 409 `approval_claim_lost`.
async fn run_chain(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    steps: &[&str],
    ctx: &Ctx<'_>,
) -> ApiResult<Option<(&'static str, String)>> {
    for step in steps {
        let mut tx = begin_platform(&st.db).await?;
        let rows = store::steps(&mut tx, a.id).await?;
        tx.commit().await?;
        let Some(row) = rows.iter().find(|r| r.step == *step) else {
            // A signup journaled before `app_licences` existed has no such row: the backfill tick serves its tenant.
            if *step == "app_licences" {
                continue;
            }
            return Ok(Some((
                "journal_missing",
                format!("the journal has no {step} step"),
            )));
        };
        match row.status.as_str() {
            "ok" | "skipped" => continue,
            "failed" => {
                return Ok(Some((
                    "step_failed",
                    format!("the {step} step failed earlier"),
                )));
            }
            _ => {}
        }
        if let Some(c) = ctx.claim {
            c.hold(st, step).await?;
        }
        let Some((attempts, data)) = claim_step(st, a.id, step).await? else {
            return Ok(Some((
                "step_in_progress",
                format!("the {step} step is waiting or being run by another runner"),
            )));
        };
        let out = match run_step(st, sg, a, step, attempts, &data, &rows, ctx).await {
            Ok(o) => o,
            Err(e) => Outcome::Retry("internal", e.to_string().chars().take(400).collect()),
        };
        record(st, a.id, step, attempts, &out).await?;
        if let Outcome::Retry(c, m) | Outcome::Fail(c, m) = out {
            return Ok(Some((c, m)));
        }
    }
    Ok(None)
}

fn data_of<'a>(rows: &'a [StepRow], step: &str) -> Option<&'a Value> {
    rows.iter().find(|r| r.step == step).map(|r| &r.data)
}

/// The Keycloak sub the `keycloak_user` step resolved, and whether that identity existed before.
fn identity(rows: &[StepRow]) -> Option<(String, bool)> {
    let d = data_of(rows, "keycloak_user")?;
    Some((
        d.get("sub")?.as_str()?.to_string(),
        d.get("existing").and_then(Value::as_bool).unwrap_or(true),
    ))
}

/// The workspace id the `tenant` step chose (remembered before its insert, so it may not exist yet).
fn planned_tenant(rows: &[StepRow]) -> Option<Uuid> {
    data_of(rows, "tenant")?
        .get("tenant_id")?
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// The workspace the `tenant` step founded (the step is ok).
fn founded_tenant(rows: &[StepRow]) -> Option<Uuid> {
    rows.iter()
        .find(|r| r.step == "tenant" && r.status == "ok")
        .and_then(|_| planned_tenant(rows))
}

/// The name the `tenant` step founded the workspace under (after any suffix).
fn founded_name(rows: &[StepRow]) -> Option<String> {
    rows.iter()
        .find(|r| r.step == "tenant" && r.status == "ok")?
        .data
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// `name`, then `name-2` … `name-NAME_ATTEMPTS`, each at most 120 characters (the base is cut to make room).
pub fn name_candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    for n in 2..=NAME_ATTEMPTS {
        let suffix = format!("-{n}");
        let room = 120usize.saturating_sub(suffix.chars().count());
        let base: String = name.chars().take(room).collect();
        out.push(format!("{}{suffix}", base.trim_end()));
    }
    out
}

/// The `tenant` step: found the workspace under the first free name of [`name_candidates`] (CP auto-suffix, signup
/// approvals only). The id is remembered first, so an attempt that died after its insert finds its workspace again.
async fn found_tenant(
    st: &AppState,
    a: &Approval,
    input: &TenantInput<'_>,
    data: &Value,
    actor: &Actor,
) -> ApiResult<Outcome> {
    let remembered = data
        .get("tenant_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok());
    let tenant = match remembered {
        Some(t) => t,
        None => {
            let t = Uuid::now_v7();
            remember(st, a.id, "tenant", &json!({"tenant_id": t})).await?;
            t
        }
    };
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let existing: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(name) = existing {
        tx.commit().await?;
        return Ok(Outcome::Ok(json!({"tenant_id": tenant, "name": name})));
    }
    for candidate in name_candidates(input.name) {
        // A taken name is a conflict on tenants_name_lower: DO NOTHING keeps the transaction usable for the next.
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO tenants (id, name, residency, updated_by) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(tenant)
        .bind(&candidate)
        .bind(input.residency)
        .bind(format!("signup:{}", a.id))
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_some() {
            audit::record(
                &mut tx,
                actor,
                Event::new("tenant.create", "tenant", tenant.to_string())
                    .detail("residency", input.residency.to_string())
                    .detail("signup", a.id.to_string())
                    .detail("name_suffixed", candidate != input.name),
            )
            .await?;
            tx.commit().await?;
            return Ok(Outcome::Ok(json!({"tenant_id": tenant, "name": candidate})));
        }
    }
    tx.commit().await?;
    Ok(Outcome::Fail(
        "tenant_name_taken",
        format!(
            "the workspace name and its -2 to -{NAME_ATTEMPTS} variants are all taken; approve with another name"
        ),
    ))
}

/// A pre-existing identity is never seated at approval (CP): its tenant_admin seat waits in a pending
/// `member_invites` row (accepted with a verified matching email, B1b). Idempotent (one pending invite per address).
async fn invite_existing(
    st: &AppState,
    a: &Approval,
    tenant: Uuid,
    sub: &str,
) -> ApiResult<Outcome> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let inserted = sqlx::query(
        "INSERT INTO member_invites (tenant_id, email, roles, invitee_sub, invited_by, signup_id) \
         VALUES ($1, lower($2), agent_role_keys(), $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(tenant)
    .bind(&a.email)
    .bind(sub)
    .bind(format!("signup:{}", a.id))
    .bind(a.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        audit::record(
            &mut tx,
            &tick_actor(),
            Event::new("user.invite", "user", sub.to_string())
                .detail("roles", RoleSet::ALL.keys())
                .detail(
                    "email_domain",
                    a.email.rsplit_once('@').map(|(_, d)| d.to_lowercase()),
                )
                .detail("existing_identity", true)
                .detail("signup", a.id.to_string()),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Outcome::Skip(
        EXISTING_INVITED,
        "requester already had a portal account: not seated at approval; a pending tenant_admin invite was recorded"
            .into(),
    ))
}

/// The ownership stamp an identity is CREATED with (`signup_app=agent:<signup id>`): an identity found by email is
/// "minted by this approval" only when it carries it (a create whose answer was lost), never because a create was
/// attempted.
pub fn minted_stamp(signup: Uuid) -> String {
    format!("agent:{signup}")
}

/// An identity found by email for signup `a`: the step's data. Minted by this approval (its stamp) → `existing: false`.
/// Retired by a reject of this service (`agent_rejected:…`) → re-enabled and unmarked (CP `reclaimDisabledIdentity`);
/// it is treated as minted ONLY when it has no credential and no IdP link (Keycloak says so positively), else it takes
/// the invite-pending path; `reclaimed_marker` lets a failed approve put the marker back. Anything else → `existing`.
async fn adopt_found(st: &AppState, a: &Approval, sub: &str) -> Result<Value, UpstreamError> {
    let kc = &st.identity.keycloak;
    let stamp = kc.attribute(sub, REJECT_MARKER_ATTR).await?;
    if stamp.as_deref() == Some(minted_stamp(a.id).as_str()) {
        return Ok(json!({"sub": sub, "existing": false}));
    }
    let Some(marker) = stamp.filter(|v| v.starts_with(REJECT_MARKER_PREFIX)) else {
        return Ok(json!({"sub": sub, "existing": true}));
    };
    // CP order: remember the marker BEFORE any mutation (so a failed approve can always put it back), read the
    // credentials and IdP links while the identity is still disabled and marked, and only then re-enable it.
    remember(
        st,
        a.id,
        "keycloak_user",
        &json!({"sub": sub, "reclaimed_marker": marker}),
    )
    .await
    .map_err(|e| UpstreamError::new(None, e.to_string()))?;
    let credentialless = kc.has_credential_or_link(sub).await? == Some(false);
    if kc
        .reclaim_marked(sub, REJECT_MARKER_ATTR, REJECT_MARKER_PREFIX)
        .await?
        .is_none()
    {
        return Ok(json!({"sub": sub, "existing": true}));
    }
    tracing::info!(signup = %a.id, credentialless, "signup_reject_identity_reenabled");
    Ok(json!({"sub": sub, "existing": !credentialless, "reclaimed_marker": marker}))
}

/// A failed approve after a reclaim (CP `restoreRejectedIdentity`): disable the identity again and put its marker
/// back, and send `keycloak_user` round again so the next approve reclaims it afresh. Never fails the caller.
async fn restore_reclaimed(st: &AppState, id: Uuid) {
    let res = async {
        let mut tx = begin_platform(&st.db).await?;
        let rows = store::steps(&mut tx, id).await?;
        tx.commit().await?;
        let Some(d) = data_of(&rows, "keycloak_user") else {
            return Ok::<_, ApiError>(());
        };
        let (Some(sub), Some(marker)) = (
            d.get("sub").and_then(Value::as_str),
            d.get("reclaimed_marker").and_then(Value::as_str),
        ) else {
            return Ok(());
        };
        st.identity
            .keycloak
            .disable_and_mark(sub, REJECT_MARKER_ATTR, marker)
            .await
            .map_err(|e| ApiError::BadGateway("keycloak_error", e.message))?;
        let mut tx = begin_platform(&st.db).await?;
        sqlx::query(
            "UPDATE signup_provisioning_steps SET status = 'pending', data = '{}', attempts = 0, code = NULL, \
                    message = NULL, next_attempt_at = now(), lease_until = NULL, updated_at = now() \
              WHERE signup_id = $1 AND step = 'keycloak_user'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(signup = %id, "signup_reclaimed_identity_restored");
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(
            signup = %id,
            error = %e,
            remediation = "a reclaimed identity of an aborted approve may still be ENABLED; disable it by hand",
            "signup_reclaimed_identity_restore_failed"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_step(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    step: &str,
    attempts: i32,
    data: &Value,
    rows: &[StepRow],
    ctx: &Ctx<'_>,
) -> ApiResult<Outcome> {
    let identity_missing = || {
        Outcome::Fail(
            "identity_missing",
            "the keycloak_user step recorded no sub".into(),
        )
    };
    let tenant = founded_tenant(rows);
    let tenant_missing = || {
        Outcome::Fail(
            "tenant_missing",
            "the tenant step founded no workspace".into(),
        )
    };
    Ok(match step {
        "keycloak_user" => {
            let kc = &st.identity.keycloak;
            match kc.find_by_email(&a.email).await {
                Err(e) => upstream("keycloak_unavailable", &e),
                Ok(Some(sub)) => {
                    if belongs_to_another(st, &a.email, Some(&sub), planned_tenant(rows)).await? {
                        Outcome::Fail(
                            BELONGS_ELSEWHERE,
                            "the requester's portal account already belongs to another workspace"
                                .into(),
                        )
                    } else {
                        match adopt_found(st, a, &sub).await {
                            Ok(v) => Outcome::Ok(v),
                            Err(e) => upstream("keycloak_unavailable", &e),
                        }
                    }
                }
                Ok(None) => {
                    remember(st, a.id, step, &json!({"create_attempted": true})).await?;
                    let (first, last) = split_name(&a.name);
                    let stamp = minted_stamp(a.id);
                    match kc
                        .create_user(
                            &a.email,
                            first,
                            last.as_deref(),
                            &[(REJECT_MARKER_ATTR, stamp.as_str())],
                        )
                        .await
                    {
                        Err(e) => upstream("keycloak_create_failed", &e),
                        Ok(Some(sub)) => Outcome::Ok(json!({"sub": sub, "existing": false})),
                        // 409: created concurrently or by another app; the next attempt finds it and its stamp decides.
                        Ok(None) => Outcome::Retry(
                            "keycloak_user_conflict",
                            "Keycloak answered 409 to the create; looking it up again".into(),
                        ),
                    }
                }
            }
        }
        "tenant" => match ctx.input {
            Some(input) => found_tenant(st, a, input, data, &ctx.actor).await?,
            None => Outcome::Fail(
                "tenant_input_missing",
                "only an approve founds the workspace".into(),
            ),
        },
        "licences_tenant" => {
            let Some(tenant) = tenant else {
                return Ok(tenant_missing());
            };
            // The tenant registered in licences AND its own licence minted and stored (D77 L4).
            match lifecycle::ensure_tenant_licence(st, tenant, &tick_actor()).await? {
                MintOutcome::Stored(l) => Outcome::Ok(json!({"license_id": l})),
                MintOutcome::NotPending | MintOutcome::LostRace => Outcome::Ok(json!({})),
                MintOutcome::Failed("licences_unavailable") => Outcome::Retry(
                    "licences_unavailable",
                    "the licence service did not answer; retrying".into(),
                ),
                MintOutcome::Failed(code) => Outcome::Fail(
                    code,
                    "the licence service refused the tenant licence".into(),
                ),
            }
        }
        "app_licences" => {
            let Some(tenant) = tenant else {
                return Ok(tenant_missing());
            };
            match app_licence::provision_tenant(st, tenant, &tick_actor()).await? {
                Ok(done) => {
                    let apps: serde_json::Map<String, Value> = done
                        .iter()
                        .map(|(a, p)| (a.key().to_string(), json!(p.label())))
                        .collect();
                    Outcome::Ok(json!({"apps": apps}))
                }
                Err(e) if e.retry => Outcome::Retry(e.code, e.message.chars().take(400).collect()),
                Err(e) => Outcome::Fail(e.code, e.message.chars().take(400).collect()),
            }
        }
        "membership" => {
            let (Some(tenant), Some((sub, existing))) = (tenant, identity(rows)) else {
                return Ok(if tenant.is_none() {
                    tenant_missing()
                } else {
                    identity_missing()
                });
            };
            if existing {
                invite_existing(st, a, tenant, &sub).await?
            } else {
                membership(st, a, tenant, &sub).await?
            }
        }
        "member_licence" => {
            let (Some(tenant), Some((sub, existing))) = (tenant, identity(rows)) else {
                return Ok(if tenant.is_none() {
                    tenant_missing()
                } else {
                    identity_missing()
                });
            };
            // A pre-existing identity is licensed once it is seated: when it accepts its invite
            // (crate::members::accept marks `membership` ok and re-runs this step).
            let seated = rows
                .iter()
                .any(|r| r.step == "membership" && r.status == "ok");
            if existing && !seated {
                Outcome::Skip(
                    EXISTING_INVITED,
                    "not seated at approval: the licence is minted when the invite is accepted"
                        .into(),
                )
            } else {
                member_licence(st, a, tenant, &sub, attempts, data).await?
            }
        }
        "invite_email" => {
            let Some((sub, existing)) = identity(rows) else {
                return Ok(identity_missing());
            };
            if existing {
                return Ok(Outcome::Skip(
                    EXISTING_INVITED,
                    "requester already had a portal account: no email sent at approval; not seated until the invite is accepted"
                        .into(),
                ));
            }
            // Defence in depth: the runner only reaches this step when every earlier one landed (the steps after it,
            // `app_licences`, never hold the email back).
            let earlier = &STEPS[..STEPS
                .iter()
                .position(|s| *s == "invite_email")
                .unwrap_or(STEPS.len())];
            let all_landed = rows
                .iter()
                .filter(|r| earlier.contains(&r.step.as_str()))
                .all(|r| r.status == "ok" || r.status == "skipped");
            if !all_landed {
                return Ok(Outcome::Retry(
                    "earlier_steps_pending",
                    "the email waits for every earlier step".into(),
                ));
            }
            let out = invite_email(st, sg, a, &sub, false).await;
            // B1b T11: the email's outcome in the new workspace's log (the journal keeps the step).
            if let (Some(t), Some(r)) = (tenant, out.1) {
                crate::members::access_email_outcome(st, t, &ctx.actor, &sub, "signup", &r).await;
            }
            out.0
        }
        other => Outcome::Fail("unknown_step", format!("no such step {other}")),
    })
}

/// The `users` row of an identity THIS approval created (the requester is the workspace's first member, every role,
/// origin `tenant_create`) and the audit `user.add`; idempotent. A requester who already belongs to ANOTHER workspace
/// is refused (a Keycloak subject belongs to exactly one tenant).
async fn membership(st: &AppState, a: &Approval, tenant: Uuid, sub: &str) -> ApiResult<Outcome> {
    let home: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM agent_auth_user($1)")
        .bind(sub)
        .fetch_optional(&st.db)
        .await?;
    match home {
        Some(t) if t == tenant => return Ok(Outcome::Ok(json!({}))),
        Some(_) => {
            return Ok(Outcome::Fail(
                BELONGS_ELSEWHERE,
                "the requester already belongs to another workspace".into(),
            ));
        }
        None => {}
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let inserted = sqlx::query(
        "INSERT INTO users (sub, tenant_id, roles, display_name, email, origin, added_by, updated_by) \
         VALUES ($1, $2, agent_role_keys(), $3, $4, 'tenant_create', $5, $5) ON CONFLICT (sub) DO NOTHING",
    )
    .bind(sub)
    .bind(tenant)
    .bind(&a.name)
    .bind(a.email.to_lowercase())
    .bind(format!("signup:{}", a.id))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        audit::record(
            &mut tx,
            &tick_actor(),
            Event::new("user.add", "user", sub.to_string())
                .detail("roles", RoleSet::ALL.keys())
                .detail("signup", a.id.to_string()),
        )
        .await?;
        tx.commit().await?;
        return Ok(Outcome::Ok(json!({})));
    }
    // The sub's users row exists (ON CONFLICT waited for any concurrent insert to commit, and this statement sees
    // it). Visible under this tenant's scope: a concurrent runner of this step won, done. Invisible: the row belongs
    // to ANOTHER tenant — a member disabled there (agent_auth_user hides disabled members). That fails identically
    // every time, so it is a permanent failure at once (review #6), not 10 retries; super_admin Retry is the path
    // once the member is moved.
    let here: Option<String> = sqlx::query_scalar("SELECT sub FROM users WHERE sub = $1")
        .bind(sub)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    if here.is_some() {
        return Ok(Outcome::Ok(json!({})));
    }
    Ok(Outcome::Fail(
        "user_in_other_tenant",
        "the requester belongs to another workspace (a disabled member there)".into(),
    ))
}

/// The member licence, in the D77 L10/L11 order: the requester's `member_entitlement` row is written `pending` and
/// committed first; the licence is created once (its id remembered before the mint), minted and sealed; then ONE
/// transaction stores it with the conditional UPDATE (L4, `pending` → `active`), audits `license.issue` AND marks this
/// step `ok`, so the step is `ok` exactly when the entitlement is `active`. A mint failure leaves both `pending`.
/// Losing the store race to another runner revokes the token this run minted, unless both runs hold the same licence
/// (licences' create is idempotent per tenant, app and subject).
async fn member_licence(
    st: &AppState,
    a: &Approval,
    tenant: Uuid,
    sub: &str,
    attempts: i32,
    data: &Value,
) -> ApiResult<Outcome> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    members::entitle_pending(&mut tx, tenant, sub).await?;
    let (status, stored): (String, Option<String>) = sqlx::query_as(
        "SELECT status, license_id FROM member_entitlement WHERE tenant_id = $1 AND user_sub = $2",
    )
    .bind(tenant)
    .bind(sub)
    .fetch_one(&mut *tx)
    .await?;
    // B1b T10: the licence is issued in the region of the tenant's residency.
    let residency: String = sqlx::query_scalar("SELECT residency FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    if status == "active"
        && let Some(license_id) = stored
    {
        return Ok(Outcome::Ok(json!({"license_id": license_id})));
    }
    if status != "pending" {
        return Ok(Outcome::Fail(
            "entitlement_not_pending",
            format!("the requester's entitlement is {status}"),
        ));
    }
    let remembered = data
        .get("license_id")
        .and_then(Value::as_str)
        .zip(data.get("subject_id").and_then(Value::as_str))
        .map(|(l, s)| (l.to_string(), s.to_string()));
    let (license_id, subject_id) = match remembered {
        Some(ids) => ids,
        None => match st.identity.create_licence(tenant, sub, &residency).await {
            Err(e) => return Ok(upstream("licence_create_failed", &e)),
            Ok(c) => {
                remember(
                    st,
                    a.id,
                    "member_licence",
                    &json!({"license_id": c.license_id, "subject_id": c.subject_id}),
                )
                .await?;
                (c.license_id, c.subject_id)
            }
        },
    };
    let token_sealed = match st.identity.mint_sealed(tenant, &license_id).await {
        Ok(t) => t,
        Err(e) => return Ok(upstream("licence_mint_failed", &e)),
    };
    let issued = IssuedLicence {
        license_id,
        subject_id,
        token_sealed,
    };
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let stored = store_member_entitlement(&mut tx, tenant, sub, &issued).await?;
    if stored == Stored::Stale {
        // Review #8: the remembered licence was revoked since; forget it so the retry creates a fresh one.
        tx.commit().await?;
        remember(
            st,
            a.id,
            "member_licence",
            &json!({"license_id": null, "subject_id": null}),
        )
        .await?;
        return Ok(Outcome::Retry(
            "licence_stale",
            "the licence was revoked while it was minted; a fresh one is created on retry".into(),
        ));
    }
    let winner: Option<String> = if stored == Stored::LostRace {
        sqlx::query_scalar(
            "SELECT license_id FROM member_entitlement \
              WHERE tenant_id = $1 AND user_sub = $2 AND status = 'active'",
        )
        .bind(tenant)
        .bind(sub)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
    } else {
        audit::record(
            &mut tx,
            &tick_actor(),
            Event::new("license.issue", "member_entitlement", sub.to_string())
                .detail("license_id", issued.license_id.clone())
                .detail("signup", a.id.to_string()),
        )
        .await?;
        Some(issued.license_id.clone())
    };
    let Some(winner) = winner else {
        // Nothing active and our store refused: the row moved on (revoking, …). Not ok; give the licence back.
        tx.commit().await?;
        let r = st.identity.revoke_licence(tenant, &issued.license_id).await;
        tracing::warn!(signup = %a.id, outcome = ?r, "signup_member_licence_row_moved");
        members::orphan_if_unrevoked(st, tenant, &issued.license_id, &r).await;
        return Ok(Outcome::Fail(
            "entitlement_not_pending",
            "the requester's entitlement changed while the licence was minted".into(),
        ));
    };
    enter(&mut tx).await?;
    record_in(
        &mut tx,
        a.id,
        "member_licence",
        attempts,
        &Outcome::Ok(json!({"license_id": winner})),
    )
    .await?;
    tx.commit().await?;
    if winner != issued.license_id {
        let r = st.identity.revoke_licence(tenant, &issued.license_id).await;
        tracing::warn!(signup = %a.id, outcome = ?r, "signup_member_licence_lost_race");
        members::orphan_if_unrevoked(st, tenant, &issued.license_id, &r).await;
    }
    Ok(Outcome::Recorded)
}

/// The one email (contract C5, crate::identity::Identity::send_access). An identity this approval created gets
/// its `locale` + `signup_app` attributes first (non-fatal), then the set-password email. The step only reaches
/// here for such an identity: a pre-existing one is skipped before it ([`EXISTING_INVITED`]) and is never written
/// to (fr `assertOwnIdentity`: never write to, or mail, a login this signup did not mint), because `signup_app`
/// is also another app's ownership / reject marker ([`REJECT_MARKER_ATTR`], CP `cp_rejected:<id>`) on the shared
/// realm. The `existing` branch below is kept for the shape of [`Access`] only.
/// Also answers what the email call itself returned (`None` when it was not made), for
/// [`crate::members::access_email_outcome`].
async fn invite_email(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    sub: &str,
    existing: bool,
) -> (Outcome, Option<Result<Access, AccessError>>) {
    let id = &st.identity;
    let locale = a.locale.as_deref().unwrap_or(DEFAULT_LOCALE);
    if !existing
        && let Err(e) = id
            .keycloak
            .update_attributes(
                sub,
                &[
                    ("locale", Some(locale)),
                    ("signup_app", Some(sg.cfg.app_key.as_str())),
                ],
            )
            .await
    {
        // Without them Keycloak sends its generic set-password email in the realm default language: still
        // the right email and the right link.
        tracing::warn!(signup = %a.id, error = %e, "signup_email_attributes_failed");
    }
    let sent = id.send_access(sub, existing).await;
    let out = match &sent {
        Ok(Access::SetPassword) => Outcome::Ok(json!({"locale": locale})),
        Ok(Access::ExistingSetPassword) => {
            tracing::info!(signup = %a.id, code = EXISTING_SET_PASSWORD, "signup_existing_identity_email");
            Outcome::Done(
                EXISTING_SET_PASSWORD,
                "requester already had a portal account without a password; set-password email sent".into(),
                json!({"email": EXISTING_SET_PASSWORD}),
            )
        }
        Ok(Access::ExistingAccess) => {
            tracing::info!(signup = %a.id, code = EXISTING_ACCESS, "signup_existing_identity_email");
            Outcome::Done(
                EXISTING_ACCESS,
                access_note(&format!("{}/", id.cfg.public_url)),
                json!({"email": EXISTING_ACCESS}),
            )
        }
        Err(e) => upstream(e.code, &e.error),
    };
    (out, Some(sent))
}

/// The note for [`EXISTING_ACCESS`] (journal, queue, Telegram).
pub fn access_note(app_url: &str) -> String {
    format!(
        "requester already has a portal account; no set-password email sent; access email sent; they sign in at {app_url}"
    )
}

/// How the requester was told, from the `invite_email` row (None while it is not done). The note never
/// carries personal data.
pub fn email_note(steps: &[StepRow], app_url: &str) -> Option<String> {
    let r = steps.iter().find(|r| r.step == "invite_email")?;
    match (r.status.as_str(), r.code.as_deref()) {
        ("ok", None) => Some("set-password email sent".into()),
        ("ok", Some(EXISTING_SET_PASSWORD)) => Some(
            "requester already had a portal account without a password; set-password email sent"
                .into(),
        ),
        ("ok", Some(EXISTING_ACCESS)) => Some(access_note(app_url)),
        ("skipped", Some(EXISTING_INVITED)) => Some(
            "requester already has a portal account: NOT seated and NO email sent at approval; a pending tenant_admin invite is recorded"
                .into(),
        ),
        ("skipped", Some(EXISTING_SKIPPED)) => Some(format!(
            "requester already has a portal account; NO email was sent; tell them to sign in at {app_url}"
        )),
        ("ok" | "skipped", _) => Some(
            r.message
                .clone()
                .unwrap_or_else(|| "email step done".into()),
        ),
        _ => None,
    }
}

/// Steps of `id` that are neither ok nor skipped: (step, status, code).
pub async fn not_ok(st: &AppState, id: Uuid) -> ApiResult<Vec<(String, String, Option<String>)>> {
    let mut tx = begin_platform(&st.db).await?;
    let steps = store::steps(&mut tx, id).await?;
    tx.commit().await?;
    Ok(steps
        .into_iter()
        .filter(|r| r.status != "ok" && r.status != "skipped")
        .map(|r| (r.step, r.status, r.code))
        .collect())
}

/// APPROVED signups whose FIRST outstanding step is pending, due and unclaimed (a pending signup's steps run only
/// under its approve claim).
async fn due(st: &AppState) -> ApiResult<Vec<Uuid>> {
    let mut tx = begin_platform(&st.db).await?;
    let ids = sqlx::query_scalar(
        "SELECT signup_id FROM ( \
             SELECT DISTINCT ON (s.signup_id) s.signup_id, s.status, s.next_attempt_at, s.lease_until \
               FROM signup_provisioning_steps s \
               JOIN signup_approvals a ON a.id = s.signup_id AND a.status = 'approved' \
              WHERE s.status IN ('pending', 'failed') \
              ORDER BY s.signup_id, s.ord) AS head \
          WHERE status = 'pending' AND next_attempt_at <= now() \
            AND (lease_until IS NULL OR lease_until < now()) \
          LIMIT 20",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ids)
}

/// One pass of the retry tick over the signup journals: the number of signups it ran.
pub async fn tick(st: &AppState, sg: &Signup) -> ApiResult<usize> {
    let ids = due(st).await?;
    for id in &ids {
        if let Err(e) = run(st, sg, *id).await {
            tracing::warn!(signup = %id, error = %e, "signup_provisioning_tick_failed");
        }
    }
    Ok(ids.len())
}

/// What a `retry` did.
pub enum Retried {
    NotFound,
    /// The signup is not approved (its status).
    NotApproved(String),
    /// The journal after the re-run; `requeued` = the failed steps that were set pending again (none = a no-op).
    Steps {
        requeued: Vec<String>,
        steps: Vec<StepRow>,
    },
}

/// The super_admin Retry: every `failed` step of an APPROVED signup is set `pending` (attempts 0, due now, code
/// cleared), platform-audited `signup.retry` (the step names and their failure codes), and the steps run inline. No
/// failed step: nothing changes, nothing is audited.
pub async fn retry(st: &AppState, sg: &Signup, id: Uuid, actor: &Actor) -> ApiResult<Retried> {
    let mut tx = begin_platform(&st.db).await?;
    let Some(a) = store::lock(&mut tx, id).await? else {
        return Ok(Retried::NotFound);
    };
    if a.status != "approved" {
        return Ok(Retried::NotApproved(a.status));
    }
    let requeued: Vec<(String, Option<String>)> = sqlx::query_as(
        "WITH f AS (SELECT step, code FROM signup_provisioning_steps \
                     WHERE signup_id = $1 AND status = 'failed' FOR UPDATE) \
         UPDATE signup_provisioning_steps s \
            SET status = 'pending', attempts = 0, next_attempt_at = now(), code = NULL, message = NULL, \
                lease_until = NULL, updated_at = now() \
           FROM f WHERE s.signup_id = $1 AND s.step = f.step \
         RETURNING s.step, f.code",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    if requeued.is_empty() {
        let steps = store::steps(&mut tx, id).await?;
        tx.commit().await?;
        return Ok(Retried::Steps {
            requeued: Vec::new(),
            steps,
        });
    }
    let names: Vec<String> = requeued.iter().map(|(s, _)| s.clone()).collect();
    let codes: Vec<String> = requeued.iter().filter_map(|(_, c)| c.clone()).collect();
    let mut e = PlatformEvent::new("signup.retry", signup_label(&a))
        .detail("signup", id.to_string())
        .detail("steps", names.clone())
        .detail("codes", codes);
    if let Some(t) = a.tenant_id {
        e = e.tenant(t);
    }
    audit::record_platform(&mut tx, actor, e).await?;
    tx.commit().await?;
    if let Err(e) = run(st, sg, id).await {
        tracing::warn!(signup = %id, error = %e, "signup_retry_run_failed");
    }
    let mut tx = begin_platform(&st.db).await?;
    let steps = store::steps(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Retried::Steps {
        requeued: names,
        steps,
    })
}

/// What one [`reconcile`] pass did (D77 L7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub minted: usize,
    pub revoked: usize,
    pub drained: usize,
    pub tenant_licences: usize,
    /// Tenants whose app licences the backfill completed this pass.
    pub app_licences: usize,
    pub failed: usize,
}

/// The licence reconciler (D77 L7), the backstop of every inline mint and revoke: (a) `pending` member entitlements
/// of active tenants are minted, (b) `revoking` ones revoked, (c) parked ids (`tenants.licence_revoke_pending`)
/// revoked and dropped only when licences really revoked them, (d) every `active` tenant without a tenant licence
/// gets one (heals tenants created before D77). Work is listed as `agent_platform` (`agent_licence_work`, one
/// cross-tenant read, at most [`RECONCILE_BATCH`] per kind); each item is written under its own tenant scope.
/// Concurrent passes are safe: every store is the conditional UPDATE (L4) and a lost race revokes its own token.
///
/// Review #2: a failed item gets a `licence_work_backoff` row (attempts, next attempt [`reconcile_backoff_secs`]
/// later, `error` from [`RECONCILE_MAX_ATTEMPTS`]); the work list skips items not yet due and takes the never-failed
/// first, so a stuck item cannot starve the batch. A failure is audited (`license.revoke_failed` in the tenant log,
/// `license.issue_failed` in the platform log) only when its backoff state changes: the first failure, a different
/// error code while retrying, and the move to `error`. Success clears the row; rows whose work is gone are pruned at
/// the start of every pass.
pub async fn reconcile(st: &AppState) -> ApiResult<Reconciled> {
    let mut tx = begin_platform(&st.db).await?;
    sqlx::query("SELECT agent_licence_backoff_prune()")
        .execute(&mut *tx)
        .await?;
    let work: Vec<(String, Uuid, Option<String>)> =
        sqlx::query_as("SELECT kind, tenant_id, item FROM agent_licence_work($1)")
            .bind(RECONCILE_BATCH)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let actor = tick_actor();
    let mut r = Reconciled::default();
    for (kind, tenant, reference) in work {
        let reference = reference.unwrap_or_default();
        // Some((code, licence id)) when the item failed.
        let failure: Option<(&'static str, Option<String>)> = match kind.as_str() {
            "mint" => match members::ensure_member_licence(st, tenant, &reference, &actor).await {
                Ok(MintOutcome::Stored(_)) => {
                    r.minted += 1;
                    None
                }
                Ok(MintOutcome::Failed(code)) => Some((code, None)),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(%tenant, error = %e, "reconcile_mint_failed");
                    Some(("internal_error", None))
                }
            },
            "revoke" => {
                match members::revoke_member_licence(st, tenant, &reference, &actor, false).await {
                    Ok(RevokeResult::Revoked(_)) => {
                        r.revoked += 1;
                        None
                    }
                    Ok(RevokeResult::NothingToRevoke) => None,
                    Ok(RevokeResult::Failed(code, id)) => Some((code, Some(id))),
                    Err(e) => {
                        tracing::warn!(%tenant, error = %e, "reconcile_revoke_failed");
                        Some(("internal_error", None))
                    }
                }
            }
            "parked" => {
                match lifecycle::drain_parked(st, &actor, tenant, &reference, false).await {
                    Ok(RevokeResult::Failed(code, id)) => Some((code, Some(id))),
                    Ok(_) => {
                        r.drained += 1;
                        None
                    }
                    Err(e) => {
                        tracing::warn!(%tenant, error = %e, "reconcile_drain_failed");
                        Some(("internal_error", Some(reference.clone())))
                    }
                }
            }
            "tenant" => match lifecycle::ensure_tenant_licence(st, tenant, &actor).await {
                Ok(MintOutcome::Stored(_)) => {
                    r.tenant_licences += 1;
                    None
                }
                Ok(MintOutcome::Failed(code)) => Some((code, None)),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(%tenant, error = %e, "reconcile_tenant_licence_failed");
                    Some(("internal_error", None))
                }
            },
            _ => None,
        };
        let Some((code, licence)) = failure else {
            if let Err(e) = backoff_clear(st, &kind, tenant, &reference).await {
                tracing::warn!(%tenant, error = %e, "reconcile_backoff_clear_failed");
            }
            continue;
        };
        r.failed += 1;
        match backoff_fail(st, &kind, tenant, &reference, code).await {
            Ok(Some((attempts, status))) => {
                let f = Failure {
                    kind: &kind,
                    tenant,
                    item: &reference,
                    code,
                    licence,
                    attempts,
                    status,
                };
                if let Err(e) = audit_failure(st, &actor, f).await {
                    tracing::warn!(%tenant, error = %e, "reconcile_failure_audit_failed");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(%tenant, error = %e, "reconcile_backoff_failed"),
        }
    }
    backfill_app_licences(st, &actor, &mut r).await;
    Ok(r)
}

/// The app-licence BACKFILL (tenants that predate `tenant_app_licence`, and any whose signup step did not land):
/// at most [`APP_LICENCE_BATCH`] tenants per pass from `agent_app_licence_work`, each with its own persisted backoff
/// (`licence_work_backoff`, kind `app_licence`, the reconciler's limits), failures audited like the other kinds when
/// their backoff state changes and counted in the `agent_licence_work_backoff` gauges. Never fails the pass.
async fn backfill_app_licences(st: &AppState, actor: &Actor, r: &mut Reconciled) {
    let work: ApiResult<Vec<Uuid>> = async {
        let mut tx = begin_platform(&st.db).await?;
        let ids = sqlx::query_scalar("SELECT tenant_id FROM agent_app_licence_work($1)")
            .bind(APP_LICENCE_BATCH)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ids)
    }
    .await;
    let tenants = match work {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "app_licence_backfill_list_failed");
            return;
        }
    };
    for tenant in tenants {
        let failure: Option<&'static str> =
            match app_licence::provision_tenant(st, tenant, actor).await {
                Ok(Ok(_)) => {
                    r.app_licences += 1;
                    None
                }
                Ok(Err(e)) => Some(e.code),
                Err(e) => {
                    tracing::warn!(%tenant, error = %e, "app_licence_backfill_failed");
                    Some("internal_error")
                }
            };
        let Some(code) = failure else {
            if let Err(e) = backoff_clear(st, "app_licence", tenant, "").await {
                tracing::warn!(%tenant, error = %e, "app_licence_backoff_clear_failed");
            }
            continue;
        };
        r.failed += 1;
        match backoff_fail(st, "app_licence", tenant, "", code).await {
            Ok(Some((attempts, status))) => {
                let f = Failure {
                    kind: "app_licence",
                    tenant,
                    item: "",
                    code,
                    licence: None,
                    attempts,
                    status,
                };
                if let Err(e) = audit_failure(st, actor, f).await {
                    tracing::warn!(%tenant, error = %e, "app_licence_failure_audit_failed");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(%tenant, error = %e, "app_licence_backoff_failed"),
        }
    }
}

/// Backoff before the next attempt of a licence work item that has failed `attempts` times (review #2): 30 s
/// doubling, capped at 1 h.
pub fn reconcile_backoff_secs(attempts: i32) -> f64 {
    let exp = (attempts.max(1) - 1).min(30);
    (RECONCILE_BACKOFF_BASE_SECS * 2f64.powi(exp)).min(RECONCILE_BACKOFF_CAP_SECS)
}

/// The item succeeded (or left the work list): forget its failures.
async fn backoff_clear(st: &AppState, kind: &str, tenant: Uuid, item: &str) -> ApiResult<()> {
    let mut tx = begin_platform(&st.db).await?;
    sqlx::query(
        "DELETE FROM licence_work_backoff WHERE kind = $1 AND tenant_id = $2 AND item = $3",
    )
    .bind(kind)
    .bind(tenant)
    .bind(item)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Record one more failure of an item; answers `(attempts, status)` when its state changed (first failure, a new code
/// while retrying, or the move to `error`), i.e. when it is to be audited.
async fn backoff_fail(
    st: &AppState,
    kind: &str,
    tenant: Uuid,
    item: &str,
    code: &'static str,
) -> ApiResult<Option<(i32, &'static str)>> {
    let mut tx = begin_platform(&st.db).await?;
    let prev: Option<(i32, String, String)> = sqlx::query_as(
        "SELECT attempts, status, last_error FROM licence_work_backoff \
          WHERE kind = $1 AND tenant_id = $2 AND item = $3 FOR UPDATE",
    )
    .bind(kind)
    .bind(tenant)
    .bind(item)
    .fetch_optional(&mut *tx)
    .await?;
    let attempts = prev.as_ref().map_or(0, |p| p.0).saturating_add(1);
    let status = if attempts >= RECONCILE_MAX_ATTEMPTS {
        "error"
    } else {
        "retrying"
    };
    sqlx::query(
        "INSERT INTO licence_work_backoff (kind, tenant_id, item, attempts, status, last_error, next_attempt_at) \
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7::float8)) \
         ON CONFLICT (kind, tenant_id, item) DO UPDATE \
            SET attempts = EXCLUDED.attempts, status = EXCLUDED.status, last_error = EXCLUDED.last_error, \
                next_attempt_at = EXCLUDED.next_attempt_at, updated_at = now()",
    )
    .bind(kind)
    .bind(tenant)
    .bind(item)
    .bind(attempts)
    .bind(status)
    .bind(code)
    .bind(reconcile_backoff_secs(attempts))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let changed = match &prev {
        None => true,
        Some((_, was, last)) => was != status || (status == "retrying" && last != code),
    };
    if !changed {
        tracing::debug!(%tenant, kind, code, attempts, "reconcile_failure_unchanged");
    }
    Ok(changed.then_some((attempts, status)))
}

/// One reconciler failure to audit.
struct Failure<'a> {
    kind: &'a str,
    tenant: Uuid,
    item: &'a str,
    code: &'static str,
    licence: Option<String>,
    attempts: i32,
    status: &'static str,
}

/// Audit a reconciler failure whose backoff state changed: a revoke in the tenant log (`license.revoke_failed`), a
/// mint in the platform log (`license.issue_failed`, labelled with the tenant's name as the create path does).
async fn audit_failure(st: &AppState, actor: &Actor, f: Failure<'_>) -> ApiResult<()> {
    match f.kind {
        "revoke" | "parked" => {
            let (target, id) = if f.kind == "revoke" {
                ("member_entitlement", f.item.to_string())
            } else {
                ("tenant", f.tenant.to_string())
            };
            let mut e = Event::new("license.revoke_failed", target, id)
                .failed(f.code)
                .detail("attempts", f.attempts)
                .detail("retry_status", f.status);
            if let Some(l) = f.licence {
                e = e.detail("license_id", l);
            }
            audit::record_alone(&st.db, f.tenant, actor, e).await?;
        }
        _ => {
            let mut tx = begin_scoped(&st.db, f.tenant).await?;
            let name: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
                .bind(f.tenant)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            let holder = if f.kind == "mint" { "member" } else { "tenant" };
            let e = PlatformEvent::new(
                "license.issue_failed",
                name.unwrap_or_else(|| f.tenant.to_string()),
            )
            .tenant(f.tenant)
            .failed(f.code)
            .detail("holder", holder)
            .detail("attempts", f.attempts)
            .detail("retry_status", f.status);
            audit::record_platform_alone(&st.db, actor, e).await?;
        }
    }
    Ok(())
}

/// The provisioning tick, every `SIGNUP_TICK_SECONDS` (60 s while signup is dark): the signup journals (when signup
/// is on) and the licence reconciler (always: D77 L7 heals tenants whatever the signup switch says). Never stops on
/// an error.
pub fn spawn_tick(st: AppState, sg: Option<Arc<Signup>>) -> tokio::task::JoinHandle<()> {
    let every = Duration::from_secs(sg.as_ref().map_or(60, |s| s.cfg.tick_seconds));
    tokio::spawn(async move {
        let mut t = tokio::time::interval(every);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            if let Some(sg) = &sg
                && let Err(e) = tick(&st, sg).await
            {
                tracing::warn!(error = %e, "signup_provisioning_tick_failed");
            }
            match reconcile(&st).await {
                Ok(r) if r != Reconciled::default() => {
                    tracing::info!(report = ?r, "licence_reconcile")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "licence_reconcile_failed"),
            }
        }
    })
}

/// A person's name as Keycloak's firstName / lastName, by fr's rule: trim and split on runs of whitespace;
/// several words → the first word, then the rest joined by single spaces; ONE word → that word as BOTH
/// firstName and lastName; empty → neither. The portal realm requires both, so a one-word name still gets a
/// lastName and the user lands straight on the set-password form instead of Keycloak's update-profile one.
/// Every caller gets this rule: signup provisioning here, and `identity::PersonNames::resolve` (member invites,
/// user creation) when no explicit first/last is given. Signup intake refuses an empty name
/// (`routes/signups.rs` `single_line_field("name", ..)`), so the empty case never reaches Keycloak from signup.
pub(crate) fn split_name(name: &str) -> (Option<&str>, Option<String>) {
    let mut parts = name.split_whitespace();
    let Some(first) = parts.next() else {
        return (None, None);
    };
    let rest = parts.collect::<Vec<_>>().join(" ");
    if rest.is_empty() {
        return (Some(first), Some(first.to_string()));
    }
    (Some(first), Some(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_backoff_doubles_from_30s_and_caps_at_an_hour() {
        assert_eq!(reconcile_backoff_secs(1), 30.0);
        assert_eq!(reconcile_backoff_secs(2), 60.0);
        assert_eq!(reconcile_backoff_secs(4), 240.0);
        assert_eq!(reconcile_backoff_secs(8), 3600.0);
        assert_eq!(reconcile_backoff_secs(i32::MAX), 3600.0);
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_six_hours() {
        assert_eq!(backoff_secs(0), 60.0);
        assert_eq!(backoff_secs(1), 60.0);
        assert_eq!(backoff_secs(2), 120.0);
        assert_eq!(backoff_secs(4), 480.0);
        assert_eq!(backoff_secs(20), 6.0 * 3600.0);
    }

    #[test]
    fn upstream_failures_classify_by_status() {
        let retry = |s: Option<u16>| {
            matches!(
                upstream("x", &UpstreamError::new(s, "m")),
                Outcome::Retry(..)
            )
        };
        assert!(retry(None));
        assert!(retry(Some(503)));
        assert!(retry(Some(429)));
        assert!(retry(Some(408)));
        assert!(!retry(Some(400)));
        assert!(!retry(Some(403)));
    }

    #[test]
    fn split_name_follows_the_fr_rule() {
        assert_eq!(
            split_name("Gregorius Dekker"),
            (Some("Gregorius"), Some("Dekker".to_string()))
        );
        assert_eq!(
            split_name("Anna van der Berg"),
            (Some("Anna"), Some("van der Berg".to_string()))
        );
        assert_eq!(
            split_name(" Anna \t van  der\nBerg "),
            (Some("Anna"), Some("van der Berg".to_string()))
        );
        assert_eq!(split_name("Cher"), (Some("Cher"), Some("Cher".to_string())));
        assert_eq!(
            split_name("  Cher  "),
            (Some("Cher"), Some("Cher".to_string()))
        );
        assert_eq!(split_name(""), (None, None));
        assert_eq!(split_name(" \t\n "), (None, None));
    }
}
