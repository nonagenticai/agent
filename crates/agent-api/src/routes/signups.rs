//! Early-access signup routes. Ported from strata_configurator `routes/signups.rs` with the commonpractice v2
//! deltas (locale, Telegram decision door C3, journaled approve). Mounted only while signup is on.
//!
//! - `POST /api/signup-approvals/intake` — the intake handler's client-credentials token (azp
//!   `agent-signup-intake`). 202 `{"status":"queued"}` for a new pending row, 202 `{"status":"already_pending"}`
//!   when the address already waits; 503 `signup_queue_full` past `SIGNUP_QUEUE_MAX_PENDING`.
//! - `GET /api/signup-approvals?status=&limit=&before=` — super_admin: the queue, with each row's suggested
//!   workspace name, provisioning journal and intake `signals` (B1b F-03, crate::signup::signals).
//! - `POST /api/signup-approvals/{id}/approve` `{tenantName?, residency}` — super_admin: the commonpractice approve
//!   (crate::signup::provisioning): the signup is approved only once its identity, workspace and seat landed. 404
//!   `signup_not_found`, 409 `signup_not_pending` | `approval_in_progress` | `approval_claim_lost` |
//!   `user_belongs_to_another_workspace` | `tenant_name_taken`, 422 `invalid_residency` (D66: required, immutable
//!   after), 502 `<step code>` with `error.steps` (nothing decided; approve again to resume).
//! - `POST /api/signup-approvals/invite` `{email, name, company?, tenantName?, locale?, residency}` — super_admin: the
//!   pre-approved invite (CP `inviteEarlyAccess`): a `source = 'invite'` signup, then the same approve.
//! - `POST /api/signup-approvals/{id}/retry` — super_admin: the provisioning Retry; every failed step of an approved
//!   signup is run again (409 `signup_not_approved`; no failed step = 200, nothing changes). `{steps}`.
//! - `POST /api/signup-approvals/{id}/reject` `{reason?}` — super_admin; 409 `approval_in_progress` while an approve
//!   runs; the workspace an aborted approve founded is offboarded (`offboarded_tenant_id`).
//! - `POST /api/signup-approvals/{id}/telegram-decision` `{action, telegramUserId}` — chat-bridge's decider
//!   token (C3, CP reply shape): 200 `{status: approved|rejected|already_decided, tenantId?, outcome?, cleanup?,
//!   summary}` (`outcome` = the decided status or `in_progress`; `tenantId` only when approved), 502
//!   `{status:"failed", code, summary}` when the decision could not be carried out (the row stays pending). A tap
//!   approves into `SIGNUP_TELEGRAM_RESIDENCY`.
//!
//! Decisions are platform-audited (`signup.intake` by `system:<intake azp>`, `signup.invite|approve|reject|retry` by the
//! super_admin's sub or `system:telegram`); the approved tenant's steps audit `tenant.create`, `user.add`,
//! `license.issue` in its own log.
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Actor, ActorType, PlatformEvent};
use crate::auth::ServiceClaims;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::platform_db::begin_platform;
use crate::signup::provisioning::{self, Decision, TenantInput};
use crate::signup::store::{self, Approval, NewSignup, StepRow};
use crate::signup::telegram::{self, Decided};
use crate::signup::{
    SIGNUP_LOCALES, Signup, default_tenant_name, email_address, multi_line_field, normalise_locale,
    require_decider, require_intake, single_line_field, tenant_name,
};
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/signup-approvals", get(list))
        .route("/api/signup-approvals/intake", post(intake))
        .route("/api/signup-approvals/invite", post(invite))
        .route("/api/signup-approvals/{id}/approve", post(approve))
        .route("/api/signup-approvals/{id}/reject", post(reject))
        .route("/api/signup-approvals/{id}/retry", post(retry))
        .route(
            "/api/signup-approvals/{id}/telegram-decision",
            post(telegram_decision),
        )
}

/// Requester-typed limits (commonpractice `intakeSchema`).
const MAX_TEXT: usize = 160;
const MAX_MESSAGE: usize = 2000;
const MAX_TURNSTILE_TOKEN: usize = 4096;
const MAX_REASON: usize = 500;
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

fn signup(st: &AppState) -> ApiResult<Arc<Signup>> {
    st.signup
        .clone()
        .ok_or_else(|| ApiError::Missing("not_found", "route not found".into()))
}

/// A service actor named by its client: `system:<azp>` (`system:telegram` for a Telegram tap).
fn system_actor(client: &str) -> Actor {
    Actor {
        id: format!("system:{client}"),
        kind: ActorType::System,
        platform: false,
        view_as: None,
    }
}

fn invalid(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.into())
}

/// What the intake handler sends (early-access guide: `{email,name,company?,message?,source,locale?}`, plus
/// an optional relayed Turnstile token). Strict, camelCase on the wire.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Intake {
    email: String,
    name: String,
    company: Option<String>,
    message: Option<String>,
    source: String,
    locale: Option<String>,
    turnstile_token: Option<String>,
}

async fn intake(
    State(st): State<AppState>,
    caller: ServiceClaims,
    ApiJson(b): ApiJson<Intake>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let sg = signup(&st)?;
    let client = require_intake(&sg.cfg, &caller)?;
    let email = email_address("email", &b.email)?;
    let name = single_line_field("name", &b.name, MAX_TEXT)?;
    let company = b
        .company
        .as_deref()
        .map(|c| single_line_field("company", c, MAX_TEXT))
        .transpose()?;
    let message = match b.message.as_deref() {
        Some(m) => multi_line_field("message", m, MAX_MESSAGE)?,
        None => None,
    };
    let source = match b.source.as_str() {
        "web" | "chat" => b.source.clone(),
        _ => return Err(invalid("source must be web or chat")),
    };
    // Lenient (C6): an unknown language is "not told", never a refused signup.
    let locale = normalise_locale(b.locale.as_deref());
    if let Some(t) = &b.turnstile_token
        && (t.is_empty() || t.chars().count() > MAX_TURNSTILE_TOKEN)
    {
        return Err(invalid(&format!(
            "turnstileToken must be 1 to {MAX_TURNSTILE_TOKEN} characters"
        )));
    }
    let turnstile = sg.verify_turnstile(b.turnstile_token.as_deref()).await?;

    let mut tx = begin_platform(&st.db).await?;
    // Serialise intakes so count-then-insert cannot overshoot the ceiling.
    store::lock_queue(&mut tx).await?;
    let pending = store::pending_count(&mut tx).await?;
    if pending >= sg.cfg.max_pending {
        tracing::warn!(
            pending,
            cap = sg.cfg.max_pending,
            intake_client = %client,
            "signup_queue_full: refusing new signups until the queue is worked down"
        );
        return Err(ApiError::Unavailable(
            "signup_queue_full",
            "sign-up requests are temporarily closed; try again later".into(),
            None,
        ));
    }
    let new = NewSignup {
        email: &email,
        name: &name,
        company: company.as_deref(),
        message: message.as_deref(),
        source: &source,
        locale,
        intake_client: &client,
    };
    let Some(id) = store::insert(&mut tx, &new).await? else {
        tx.commit().await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({"status": "already_pending"})),
        ));
    };
    audit::record_platform(
        &mut tx,
        &system_actor(&client),
        PlatformEvent::new(
            "signup.intake",
            default_tenant_name(company.as_deref(), &email, Some(name.as_str())),
        )
        .detail("signup", id.to_string())
        .detail("source", source.clone())
        .detail("turnstile", turnstile.as_str()),
    )
    .await?;
    tx.commit().await?;
    // Ids only in the log: the requester's address and name never reach it.
    tracing::info!(signup = %id, %source, intake_client = %client, "signup_intake_queued");
    let notice = telegram::NewSignup {
        id,
        name: &name,
        email: &email,
        company: company.as_deref(),
        message: message.as_deref(),
        source: &source,
        locale,
        pending: pending + 1,
    };
    telegram::send(
        &sg.cfg,
        &sg.http,
        telegram::new_signup_payload(&sg.cfg, &notice),
        "new_signup",
    );
    Ok((StatusCode::ACCEPTED, Json(json!({"status": "queued"}))))
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    limit: Option<String>,
    before: Option<String>,
}

fn invalid_query(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_query", msg.into())
}

/// `{us}.{uuid}`, the position of a row in the newest-first list.
fn cursor(raw: Option<&str>) -> ApiResult<Option<(i64, Uuid)>> {
    let Some(s) = raw.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let bad = || invalid_query("before must be a cursor returned as next by this list");
    let (us, id) = s.split_once('.').ok_or_else(bad)?;
    Ok(Some((
        us.parse().map_err(|_| bad())?,
        Uuid::parse_str(id).map_err(|_| bad())?,
    )))
}

fn step_json(r: &StepRow) -> Value {
    json!({
        "step": r.step,
        "status": r.status,
        "code": r.code,
        "message": r.message,
        "attempts": r.attempts,
        "at": r.last_attempt_at,
        "next_attempt_at": r.next_attempt_at,
    })
}

/// The intake signals of one row (a decision's answer), read in its own platform transaction. Best effort: the
/// decision already committed, so a failed read answers `[]` (logged), never an error.
async fn signals_of(st: &AppState, a: &Approval) -> Value {
    let read = async {
        let mut tx = begin_platform(&st.db).await?;
        let s = crate::signup::signals::for_rows(&mut tx, std::slice::from_ref(a)).await?;
        tx.commit().await?;
        Ok::<_, ApiError>(s.json(a.id))
    };
    match read.await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(signup = %a.id, error = %e, "signup_signals_read_failed");
            Value::Array(Vec::new())
        }
    }
}

fn item(a: &Approval, steps: &[&StepRow], signals: Value) -> Value {
    let suggested = default_tenant_name(a.company.as_deref(), &a.email, Some(a.name.as_str()));
    let mut v = json!(a);
    v["suggested_tenant_name"] = json!(suggested);
    v["steps"] = Value::Array(steps.iter().map(|s| step_json(s)).collect());
    v["signals"] = signals;
    v
}

/// The queue, newest first, paged; `?status=pending|approved|rejected|decided` narrows it.
async fn list(
    State(st): State<AppState>,
    SuperAdmin(_): SuperAdmin,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let status = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all");
    if let Some(s) = status
        && !matches!(s, "pending" | "approved" | "rejected" | "decided")
    {
        return Err(invalid_query(
            "status must be pending, approved, rejected, decided or all",
        ));
    }
    let limit = match q.limit.as_deref() {
        None | Some("") => DEFAULT_LIMIT,
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| invalid_query("limit must be an integer from 1 to 200"))?,
    };
    let before = cursor(q.before.as_deref())?;
    let mut tx = begin_platform(&st.db).await?;
    let mut rows = store::list(&mut tx, status, before, limit + 1).await?;
    let next = if rows.len() as i64 > limit {
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        rows.last().map(|a| format!("{}.{}", a.ts_us, a.id))
    } else {
        None
    };
    let ids: Vec<Uuid> = rows.iter().map(|a| a.id).collect();
    let journal = store::steps_many(&mut tx, &ids).await?;
    let mut by_id: HashMap<Uuid, Vec<&StepRow>> = HashMap::new();
    for (id, s) in &journal {
        by_id.entry(*id).or_default().push(s);
    }
    // B1b T3 (F-03): what intake noticed, computed for the page at read time.
    let signals = crate::signup::signals::for_rows(&mut tx, &rows).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|a| {
            item(
                a,
                by_id.get(&a.id).map(Vec::as_slice).unwrap_or_default(),
                signals.json(a.id),
            )
        })
        .collect();
    let (received, pending, approved, rejected) = store::counts(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "items": items,
        "next": next,
        "queue_cap": sg.cfg.max_pending,
        "pending": pending,
        "counts": {"received": received, "approved": approved, "rejected": rejected},
        "as_of": chrono::Utc::now(),
    })))
}

fn signup_not_found(id: &str) -> ApiError {
    ApiError::Missing("signup_not_found", format!("no signup {id:?}"))
}

fn signup_not_pending(a: &Approval) -> ApiError {
    ApiError::Conflict(
        "signup_not_pending",
        format!("this signup was {} already", a.status),
    )
}

#[derive(Deserialize)]
struct IdPath {
    id: String,
}

/// The path's signup id: a uuid, else 404 `signup_not_found` (it cannot name a signup).
fn signup_id(raw: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(raw).map_err(|_| signup_not_found(raw))
}

/// An optional JSON body: none, `null` and `{}` all mean the defaults.
fn optional_body<T: Default + serde::de::DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    Ok(serde_json::from_slice::<Option<T>>(body)
        .map_err(|e| invalid(&format!("The request body is not valid: {e}")))?
        .unwrap_or_default())
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ApproveBody {
    #[serde(alias = "tenant_name")]
    tenant_name: Option<String>,
    residency: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RejectBody {
    reason: Option<String>,
}

/// The app root, where the requester signs in.
fn app_url(sg: &Signup) -> String {
    format!("{}/", sg.cfg.public_url)
}

/// After an approval (its steps already ran under the claim): announce the decision. Returns the journal.
async fn after_approval(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    tenant: Uuid,
    name: &str,
    via: &str,
) -> ApiResult<Vec<StepRow>> {
    let mut tx = begin_platform(&st.db).await?;
    let steps = store::steps(&mut tx, a.id).await?;
    tx.commit().await?;
    let not_ok: Vec<(String, String, Option<String>)> = steps
        .iter()
        .filter(|r| r.status != "ok" && r.status != "skipped")
        .map(|r| (r.step.clone(), r.status.clone(), r.code.clone()))
        .collect();
    let note = provisioning::email_note(&steps, &app_url(sg));
    let d = Decided {
        id: a.id,
        approved: true,
        name: &a.name,
        email: &a.email,
        via,
        tenant: Some((tenant, name)),
        not_ok: &not_ok,
        email_note: note.as_deref(),
    };
    telegram::send(
        &sg.cfg,
        &sg.http,
        telegram::decision_payload(&sg.cfg, &d),
        "decision",
    );
    Ok(steps)
}

fn announce_rejection(sg: &Signup, a: &Approval, via: &str) {
    let d = Decided {
        id: a.id,
        approved: false,
        name: &a.name,
        email: &a.email,
        via,
        tenant: None,
        not_ok: &[],
        email_note: None,
    };
    telegram::send(
        &sg.cfg,
        &sg.http,
        telegram::decision_payload(&sg.cfg, &d),
        "decision",
    );
}

/// The approval's workspace name: the given one, else the suggestion.
async fn chosen_name(
    st: &AppState,
    id: Uuid,
    given: Option<String>,
) -> ApiResult<(Approval, String)> {
    let mut tx = begin_platform(&st.db).await?;
    let a = store::get(&mut tx, id)
        .await?
        .ok_or_else(|| signup_not_found(&id.to_string()))?;
    tx.commit().await?;
    let name = tenant_name(&match given {
        Some(n) => n,
        None => default_tenant_name(a.company.as_deref(), &a.email, Some(a.name.as_str())),
    })?;
    Ok((a, name))
}

/// 409 `approval_in_progress` (CP): another approve holds the signup's claim right now.
fn approval_in_progress() -> ApiError {
    ApiError::Conflict(
        "approval_in_progress",
        "this signup is being approved right now; reload in a moment".into(),
    )
}

/// Was the requester seated at approval (an identity the approval created), from the journal?
fn seated(steps: &[StepRow]) -> bool {
    steps
        .iter()
        .any(|r| r.step == "membership" && r.status == "ok")
}

/// The approve answer of the queue and the invite: `{signup, tenant, seated, steps}`.
async fn approved(st: &AppState, sg: &Signup, d: Decision, raw_id: &str) -> ApiResult<Value> {
    match d {
        Decision::NotFound => Err(signup_not_found(raw_id)),
        Decision::AlreadyDecided(a) | Decision::Rejected(a, _) => Err(signup_not_pending(&a)),
        Decision::InProgress(_) => Err(approval_in_progress()),
        Decision::Approved {
            approval,
            tenant,
            tenant_name,
        } => {
            let steps = after_approval(
                st,
                sg,
                &approval,
                tenant,
                &tenant_name,
                "Agent signups page",
            )
            .await?;
            let refs: Vec<&StepRow> = steps.iter().collect();
            Ok(json!({
                "signup": item(&approval, &refs, signals_of(st, &approval).await),
                "tenant": {"id": tenant, "name": tenant_name, "status": "active"},
                "seated": seated(&steps),
                "steps": steps.iter().map(step_json).collect::<Vec<_>>(),
            }))
        }
    }
}

async fn approve(
    State(st): State<AppState>,
    SuperAdmin(caller): SuperAdmin,
    ApiPath(p): ApiPath<IdPath>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    let b: ApproveBody = optional_body(&body)?;
    let (a, name) = chosen_name(&st, id, b.tenant_name).await?;
    if a.status != "pending" {
        return Err(signup_not_pending(&a));
    }
    let residency = crate::tenants::lifecycle::residency(b.residency.as_deref())?;
    let actor = Actor::person(&caller);
    let input = TenantInput {
        name: &name,
        residency,
    };
    let d = provisioning::approve(&st, &sg, id, &input, &caller.sub, "ui", &actor).await?;
    Ok(Json(approved(&st, &sg, d, &p.id).await?))
}

/// `POST /api/signup-approvals/invite` (super_admin): the pre-approved early-access invite (CP
/// `inviteEarlyAccess`). Strict body `{email, name, company?, tenantName?, locale?, residency}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InviteBody {
    email: String,
    name: String,
    company: Option<String>,
    #[serde(alias = "tenant_name")]
    tenant_name: Option<String>,
    locale: Option<String>,
    residency: Option<String>,
}

/// Invite a person straight into early access (CP `inviteEarlyAccess`): refuse an address that belongs to another
/// workspace BEFORE writing anything, record the signup (`source = 'invite'`, no trust, no intake client; a pending
/// row for the address is reused and the chosen language wins), platform-audit `signup.invite`, then run the SAME
/// approve as the queue. No new-signup notice, no queue ceiling.
async fn invite(
    State(st): State<AppState>,
    SuperAdmin(caller): SuperAdmin,
    ApiJson(b): ApiJson<InviteBody>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let email = email_address("email", &b.email)?;
    let name = single_line_field("name", &b.name, MAX_TEXT)?;
    let company = b
        .company
        .as_deref()
        .map(|c| single_line_field("company", c, MAX_TEXT))
        .transpose()?;
    // Strict (CP: the super_admin picks from the list; anything else is a client bug).
    let locale = match b.locale.as_deref() {
        None => None,
        Some(l) => Some(
            SIGNUP_LOCALES
                .iter()
                .copied()
                .find(|x| *x == l)
                .ok_or_else(|| {
                    ApiError::Unprocessable(
                        "invalid_locale",
                        format!("locale must be one of {}", SIGNUP_LOCALES.join(", ")),
                    )
                })?,
        ),
    };
    let residency = crate::tenants::lifecycle::residency(b.residency.as_deref())?;
    let requested = b.tenant_name.as_deref().map(tenant_name).transpose()?;
    if provisioning::belongs_to_another(&st, &email, None, None).await? {
        return Err(provisioning::belongs_elsewhere());
    }
    let actor = Actor::person(&caller);
    let mut tx = begin_platform(&st.db).await?;
    let inserted = store::insert_invite(
        &mut tx,
        &email,
        &name,
        company.as_deref(),
        locale,
        &caller.sub,
    )
    .await?;
    let (id, reused) = match inserted {
        Some(id) => (id, false),
        None => {
            let id = store::pending_of(&mut tx, &email).await?.ok_or_else(|| {
                ApiError::Conflict(
                    "signup_not_pending",
                    "the pending signup for this address was just decided; send the invite again"
                        .into(),
                )
            })?;
            if let Some(l) = locale {
                store::set_locale(&mut tx, id, l).await?;
            }
            (id, true)
        }
    };
    audit::record_platform(
        &mut tx,
        &actor,
        PlatformEvent::new(
            "signup.invite",
            default_tenant_name(company.as_deref(), &email, Some(name.as_str())),
        )
        .detail("signup", id.to_string())
        .detail("reused_pending", reused),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(signup = %id, reused, "signup_invite_recorded");
    let (a, wname) = chosen_name(&st, id, requested).await?;
    if a.status != "pending" {
        return Err(signup_not_pending(&a));
    }
    let input = TenantInput {
        name: &wname,
        residency,
    };
    let d = provisioning::approve(&st, &sg, id, &input, &caller.sub, "ui", &actor).await?;
    let mut out = approved(&st, &sg, d, &id.to_string()).await?;
    out["reused_pending"] = json!(reused);
    Ok(Json(out))
}

async fn reject(
    State(st): State<AppState>,
    SuperAdmin(caller): SuperAdmin,
    ApiPath(p): ApiPath<IdPath>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    let b: RejectBody = optional_body(&body)?;
    let reason = match b.reason.as_deref() {
        Some(r) => multi_line_field("reason", r, MAX_REASON)?,
        None => None,
    };
    let actor = Actor::person(&caller);
    match provisioning::reject(&st, id, &caller.sub, "ui", reason.as_deref(), &actor).await? {
        Decision::NotFound => Err(signup_not_found(&p.id)),
        Decision::AlreadyDecided(a) | Decision::Approved { approval: a, .. } => {
            Err(signup_not_pending(&a))
        }
        Decision::InProgress(_) => Err(approval_in_progress()),
        Decision::Rejected(a, cleanup) => {
            announce_rejection(&sg, &a, "Agent signups page");
            let mut v = item(&a, &[], signals_of(&st, &a).await);
            v["offboarded_tenant_id"] = json!(cleanup.offboarded);
            v["identity_retired"] = json!(cleanup.identity);
            Ok(Json(v))
        }
    }
}

/// C3 body: strict.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TelegramDecision {
    action: String,
    telegram_user_id: i64,
}

/// At most 200 characters on one line (shown in Telegram).
fn summary(s: String) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 200 {
        flat
    } else {
        let mut t: String = flat.chars().take(199).collect();
        t.push('…');
        t
    }
}

fn c3(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// C3 `already_decided` for a decided row (CP): `outcome` is its status; `tenantId` only when approved.
fn already_decided(a: &Approval) -> Response {
    let tenant = a.tenant_id.filter(|_| a.status == "approved");
    let mut v = json!({
        "status": "already_decided",
        "outcome": a.status,
        "summary": summary(format!(
            "Already {}: {}{}.",
            a.status,
            a.email,
            tenant.map(|t| format!(" (tenant {t})")).unwrap_or_default()
        )),
    });
    if let Some(t) = tenant {
        v["tenantId"] = json!(t);
    }
    c3(StatusCode::OK, v)
}

/// C3 for a signup another decider is approving right now (CP: `already_decided`, outcome `in_progress`).
fn in_progress(a: &Approval) -> Response {
    c3(
        StatusCode::OK,
        json!({
            "status": "already_decided",
            "outcome": "in_progress",
            "summary": summary(format!("A decision for {} is already being carried out.", a.email)),
        }),
    )
}

/// How the requester was let in, for the C3 approve summary (CP: set-password email sent / follows the licence /
/// FAILED).
fn mail_note(steps: &[StepRow], app_url: &str) -> String {
    let status = |s: &str| {
        steps
            .iter()
            .find(|r| r.step == s)
            .map(|r| r.status.as_str())
            .unwrap_or("pending")
    };
    match status("invite_email") {
        "ok" | "skipped" => {
            provisioning::email_note(steps, app_url).unwrap_or_else(|| "email step done".into())
        }
        "failed" => "email FAILED - Retry it on the Agent signups page".into(),
        _ if status("member_licence") != "ok" || status("licences_tenant") != "ok" => {
            "email follows once the licence is ready".into()
        }
        _ => "email pending (retried automatically)".into(),
    }
}

/// The Telegram decision door (C3, CP reply shape). Approve uses the defaults: a new workspace with the suggested
/// name in `SIGNUP_TELEGRAM_RESIDENCY`. 200 `{status: approved|rejected|already_decided, tenantId?, outcome?,
/// cleanup?, summary}`; 502 `{status: "failed", code, summary}` when the decision could not be carried out (the row
/// stays pending and can be decided again).
async fn telegram_decision(
    State(st): State<AppState>,
    caller: ServiceClaims,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(b): ApiJson<TelegramDecision>,
) -> ApiResult<Response> {
    let sg = signup(&st)?;
    require_decider(&sg.cfg, &caller)?;
    let id = signup_id(&p.id)?;
    let approve = match b.action.as_str() {
        "approve" => true,
        "reject" => false,
        _ => return Err(invalid("action must be approve or reject")),
    };
    if b.telegram_user_id <= 0 {
        return Err(invalid("telegramUserId must be a Telegram user id"));
    }
    let read = async {
        let mut tx = begin_platform(&st.db).await?;
        let a = store::get(&mut tx, id).await?;
        tx.commit().await?;
        Ok::<_, ApiError>(a)
    }
    .await;
    let a = match read {
        Ok(Some(a)) => a,
        Ok(None) => return Err(signup_not_found(&p.id)),
        Err(e) => return Ok(failed(id, b.action.as_str(), &e)),
    };
    if a.status != "pending" {
        return Ok(already_decided(&a));
    }
    let by = format!("telegram:{}", b.telegram_user_id);
    let via = format!("Telegram ({})", b.telegram_user_id);
    let actor = system_actor("telegram");
    let out = if approve {
        telegram_approve(&st, &sg, &a, &by, &via, &actor).await
    } else {
        telegram_reject(&st, &sg, &a, &by, &via, &actor).await
    };
    Ok(out.unwrap_or_else(|e| failed(id, b.action.as_str(), &e)))
}

/// C3 502: `{status: "failed", code, summary}`; nothing was decided.
fn failed(id: Uuid, action: &str, e: &ApiError) -> Response {
    let code = e.code();
    tracing::error!(signup = %id, action, code, error = %e, "signup_telegram_decision_failed");
    // An internal error's text never reaches a reply.
    let message = match e {
        ApiError::Internal(_) => "internal error".to_string(),
        other => other.to_string(),
    };
    c3(
        StatusCode::BAD_GATEWAY,
        json!({
            "status": "failed",
            "code": code,
            "summary": summary(format!("Could not {action}: {code} - {message}")),
        }),
    )
}

async fn telegram_approve(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    by: &str,
    via: &str,
    actor: &Actor,
) -> ApiResult<Response> {
    let Some(residency) = sg.cfg.telegram_residency else {
        return Err(ApiError::BadGateway(
            "telegram_residency_not_configured",
            "SIGNUP_TELEGRAM_RESIDENCY is not set; approve on the Agent signups page. Nothing was decided."
                .into(),
        ));
    };
    let name = tenant_name(&default_tenant_name(
        a.company.as_deref(),
        &a.email,
        Some(a.name.as_str()),
    ))?;
    let input = TenantInput {
        name: &name,
        residency,
    };
    Ok(
        match provisioning::approve(st, sg, a.id, &input, by, "telegram", actor).await? {
            Decision::NotFound => return Err(signup_not_found(&a.id.to_string())),
            Decision::AlreadyDecided(now) | Decision::Rejected(now, _) => already_decided(&now),
            Decision::InProgress(now) => in_progress(&now),
            Decision::Approved {
                approval,
                tenant,
                tenant_name,
            } => {
                let steps = after_approval(st, sg, &approval, tenant, &tenant_name, via).await?;
                let mail = mail_note(&steps, &app_url(sg));
                c3(
                    StatusCode::OK,
                    json!({
                        "status": "approved",
                        "tenantId": tenant,
                        "summary": summary(format!(
                            "Approved: {} -> workspace \"{tenant_name}\"; {mail}.",
                            approval.email
                        )),
                    }),
                )
            }
        },
    )
}

async fn telegram_reject(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    by: &str,
    via: &str,
    actor: &Actor,
) -> ApiResult<Response> {
    Ok(
        match provisioning::reject(st, a.id, by, "telegram", None, actor).await? {
            Decision::NotFound => return Err(signup_not_found(&a.id.to_string())),
            Decision::AlreadyDecided(now) | Decision::Approved { approval: now, .. } => {
                already_decided(&now)
            }
            Decision::InProgress(now) => in_progress(&now),
            Decision::Rejected(now, cleanup) => {
                announce_rejection(sg, &now, via);
                let mut note = String::new();
                if cleanup.offboarded.is_some() {
                    note.push_str(
                        " Cleaned up: the workspace its aborted approve founded is offboarded.",
                    );
                }
                if let Some(i) = &cleanup.identity {
                    note.push_str(&format!(" The account its aborted approve created: {i}."));
                }
                let mut v = json!({
                    "status": "rejected",
                    "summary": summary(format!("Rejected: {}.{note}", now.email)),
                });
                if !cleanup.is_empty() {
                    let failed = cleanup
                        .identity
                        .as_deref()
                        .is_some_and(|i| i.starts_with("failed"));
                    v["cleanup"] = json!(if failed { "failed" } else { "done" });
                }
                c3(StatusCode::OK, v)
            }
        },
    )
}

/// `POST /api/signup-approvals/{id}/retry` (super_admin): the provisioning Retry.
async fn retry(
    State(st): State<AppState>,
    SuperAdmin(caller): SuperAdmin,
    ApiPath(p): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    match provisioning::retry(&st, &sg, id, &Actor::person(&caller)).await? {
        provisioning::Retried::NotFound => Err(signup_not_found(&p.id)),
        provisioning::Retried::NotApproved(status) => Err(ApiError::Conflict(
            "signup_not_approved",
            format!("only an approved signup is provisioned; this one is {status}"),
        )),
        provisioning::Retried::Steps { requeued, steps } => Ok(Json(json!({
            "requeued": requeued,
            "steps": steps.iter().map(step_json).collect::<Vec<_>>(),
        }))),
    }
}
