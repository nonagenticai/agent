//! Round-robin auto-assignment of handovers (Staff Workspace, support routing; migration 0204).
//!
//! - **Platform flag** (`tenants.support_auto_assign`, migration 0207, OFF by default, written only by a super_admin
//!   through `PATCH /api/tenants/{id}`): with it off NOTHING is auto-assigned, whatever `mode` says. [`route_item`]
//!   checks it under the tenant lock and `agent_support_routing_due` filters on it, so the sweeper and the NOTIFY path
//!   both skip the tenant. The mode setting stays editable while it is off (it takes effect once the flag is on).
//! - **Setting** (`support_routing`, one row per tenant; no row = the defaults: `round_robin`, 300 s):
//!   `GET /api/support/routing` (any staff role) and `PUT /api/support/routing` (`tenant_admin` or `support_manager`;
//!   audited `support_routing.update`) over `Routing = {mode, reassign_after_secs, updated_at, updated_by, enabled}` (`enabled` = the platform flag).
//! - **Eligible**: an enabled member holding one of `auth::CONVERSATION_WORKERS` (the one role rule) who is ONLINE: a
//!   `member_session` whose `support_seen_at` (written only by support-surface requests, so another surface cannot
//!   overwrite it) is within [`ONLINE_WITHIN_SECS`] (the `/api/support/events` stream and every request the
//!   web makes from `/support` mark it, crate::members::seen).
//! - **Pick** ([`pick`]): the least recently assigned eligible member (`support_routing_member.last_assigned_at`, NULL
//!   first), then the fewest open items assigned to them (all jobs), then the sub.
//! - **Assign**: [`route_item`] runs one routing decision in one transaction that holds the tenant's `support_routing`
//!   row `FOR UPDATE` (so two replicas never pick for the same tenant at once), picks, and assigns through
//!   `ownership::assign_scoped_in` as the system actor with `expected = Queue` (a new item; a person's Take always
//!   wins the race) or `expected = User(current)` (a reassignment). A 409 `owner_changed` is a normal skip.
//! - **Triggers**: [`auto_assign_item`], from `handover::notify_workers` after every path that queues an item (an item
//!   a Take / Assign created is already assigned and is skipped by `expected = Queue`); and the per-replica sweeper
//!   ([`tick`], every [`SWEEP_EVERY`]) which assigns still-queued items when someone is online and moves an AUTO
//!   assigned item whose assignee did nothing (no staff message or note since the assignment) for
//!   `reassign_after_secs` and is no longer online. An item a person took or assigned is never moved. The sweeper
//!   reads across tenants through `agent_support_routing_due` and takes each tenant's routing row `SKIP LOCKED`.
//!   Nobody online: the item stays queued.
//! - **Runtime processes** (the `orchestrator` and `runner` binaries, `state::build_runtime` / `optional_runtime`)
//!   are separate processes from agent-api and connect as the restricted `agent_runtime` role (0065), which holds no
//!   grant on the routing tables (least privilege: not widened). Their `notify_workers` reaches [`auto_assign_item`]
//!   on that pool, where the first routing write is refused (SQLSTATE 42501, [`is_denied`]): that is a normal
//!   deferral, logged at debug, not an error. The deferring process then runs `pg_notify('support_routing',
//!   '<tenant>:<job>:<item>')` (NOTIFY needs no table grant) and agent-api's sweeper, which LISTENs on that channel
//!   ([`CHANNEL`]), routes the item at once. Every replica gets the notification; the tenant's routing row `FOR
//!   UPDATE` plus `expected = Queue` make that safe (the second replica's assign is an `owner_changed` skip). The
//!   interval tick stays the safety net (a lost notification, a dropped listener connection): the item waits at most
//!   one [`SWEEP_EVERY`] tick (30 s).
//! - **Worker-bound jobs**: a job with an enabled worker-side staff binding (`connector_bindings.side = 'worker'`:
//!   helpdesk) is never auto-assigned, by the trigger or the sweeper: the external tool routes
//!   and the item stays queued for the worker flow ([`worker_bound`]).
use std::time::Duration;

use axum::extract::State;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::auth::Role;
use crate::conversations::ownership::{self, By, Owner};
use crate::conversations::{iso, store};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::interaction::Scope;
use crate::state::AppState;
use crate::tenancy::{Member, missing_role};

/// A member is online when seen on the support surface within this many seconds.
pub const ONLINE_WITHIN_SECS: i32 = 600;
/// The default `reassign_after_secs`.
pub const DEFAULT_REASSIGN_SECS: i32 = 300;
/// The bounds of `reassign_after_secs` (1 minute to 1 day).
pub const REASSIGN_MIN_SECS: i32 = 60;
pub const REASSIGN_MAX_SECS: i32 = 86_400;
/// How often the sweeper runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);
/// Items one [`tick`] handles at most.
pub const SWEEP_CAP: i32 = 100;
/// The audit actor, the handover event actor and the `by.sub` of every routing assignment.
pub const ACTOR: &str = "service:agent-api-support-routing";

/// `support_routing.mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Off,
    RoundRobin,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::RoundRobin => "round_robin",
        }
    }
}

/// The setting as the API shows it (the web's `SupportRouting`). `updated_at` / `updated_by` are `null` while the
/// tenant has no row, or for a row routing itself wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Routing {
    pub mode: Mode,
    pub reassign_after_secs: i32,
    pub updated_at: Option<String>,
    pub updated_by: Option<String>,
    /// The platform flag `tenants.support_auto_assign`: false = no auto-assignment at all.
    pub enabled: bool,
}

/// The tenant's setting (the defaults without a row), in the transaction's tenant.
pub async fn read(conn: &mut PgConnection) -> ApiResult<Routing> {
    let row = sqlx::query(
        "SELECT mode, reassign_after_secs, updated_at, updated_by FROM support_routing \
          WHERE tenant_id = app_current_tenant()",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let enabled = flag_enabled(&mut *conn).await?;
    Ok(match row {
        Some(r) => of_row(&r, enabled)?,
        None => Routing {
            mode: Mode::RoundRobin,
            reassign_after_secs: DEFAULT_REASSIGN_SECS,
            updated_at: None,
            updated_by: None,
            enabled,
        },
    })
}

/// The platform flag `tenants.support_auto_assign` of the transaction's tenant (false when the row is not visible).
async fn flag_enabled(conn: &mut PgConnection) -> ApiResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT support_auto_assign FROM tenants WHERE id = app_current_tenant()",
    )
    .fetch_optional(conn)
    .await?
    .unwrap_or(false))
}

fn mode_of(s: &str) -> ApiResult<Mode> {
    match s {
        "off" => Ok(Mode::Off),
        "round_robin" => Ok(Mode::RoundRobin),
        _ => Err(ApiError::Internal(anyhow::anyhow!(
            "unknown support routing mode"
        ))),
    }
}

fn of_row(r: &sqlx::postgres::PgRow, enabled: bool) -> ApiResult<Routing> {
    Ok(Routing {
        mode: mode_of(&r.get::<String, _>("mode"))?,
        reassign_after_secs: r.get("reassign_after_secs"),
        updated_at: r
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("updated_at")
            .map(iso),
        updated_by: r.get("updated_by"),
        enabled,
    })
}

/// `GET /api/support/routing`: any staff role.
pub async fn get_routing(
    Member(t): Member,
    State(st): State<AppState>,
) -> ApiResult<Json<Routing>> {
    super::require_worker(&t)?;
    let mut tx = t.begin(&st.db).await?;
    let r = read(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(r))
}

#[derive(Deserialize)]
pub struct RoutingBody {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    reassign_after_secs: Option<i64>,
}

fn invalid(msg: &str) -> ApiError {
    ApiError::Unprocessable("invalid_routing", msg.to_string())
}

/// The checked `(mode, reassign_after_secs)` of a PUT body; a field left out keeps its value. 422 `invalid_routing`.
pub fn checked(b: &RoutingBody) -> ApiResult<(Option<Mode>, Option<i32>)> {
    let mode = match b.mode.as_deref().map(str::trim) {
        None => None,
        Some("off") => Some(Mode::Off),
        Some("round_robin") => Some(Mode::RoundRobin),
        Some(_) => return Err(invalid("mode is off or round_robin.")),
    };
    let secs = match b.reassign_after_secs {
        None => None,
        Some(n) if (i64::from(REASSIGN_MIN_SECS)..=i64::from(REASSIGN_MAX_SECS)).contains(&n) => {
            Some(i32::try_from(n).map_err(|_| invalid("reassign_after_secs is out of range."))?)
        }
        Some(_) => {
            return Err(invalid(
                "reassign_after_secs is between 60 seconds and 1 day.",
            ));
        }
    };
    if mode.is_none() && secs.is_none() {
        return Err(invalid("Send mode and/or reassign_after_secs."));
    }
    Ok((mode, secs))
}

/// `PUT /api/support/routing` `{mode?, reassign_after_secs?}`: `tenant_admin` or `support_manager`, checked before the
/// body is read (tests/roles.rs ANY_OF; an observer is refused with `support_manager`). Audited.
pub async fn put_routing(
    Member(t): Member,
    State(st): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Routing>> {
    if !(t.effective.has(Role::TenantAdmin) || t.effective.has(Role::SupportManager)) {
        return Err(missing_role(Role::SupportManager));
    }
    let b: RoutingBody = serde_json::from_value(raw).map_err(|e| {
        ApiError::BadRequest(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })?;
    let (mode, secs) = checked(&b)?;
    let mut tx = t.begin(&st.db).await?;
    let r = sqlx::query(
        "INSERT INTO support_routing (mode, reassign_after_secs, updated_by) \
         VALUES (COALESCE($1, 'round_robin'), COALESCE($2, 300), $3) \
         ON CONFLICT (tenant_id) DO UPDATE \
            SET mode = COALESCE($1, support_routing.mode), \
                reassign_after_secs = COALESCE($2, support_routing.reassign_after_secs), \
                updated_at = now(), updated_by = $3 \
         RETURNING mode, reassign_after_secs, updated_at, updated_by",
    )
    .bind(mode.map(Mode::as_str))
    .bind(secs)
    .bind(&t.caller.sub)
    .fetch_one(&mut *tx)
    .await?;
    let enabled = flag_enabled(&mut tx).await?;
    let now = of_row(&r, enabled)?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "support_routing.update",
            "support_routing",
            t.tenant.to_string(),
        )
        .detail("mode", now.mode.as_str())
        .detail("reassign_after_secs", now.reassign_after_secs),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(now))
}

// ---- picking and assigning ---------------------------------------------------------------------------------------

/// The member to hand the next item to (never `exclude`): see the module doc. D125: when the job has a staff team
/// (`job_staff`) only its members are picked; no team = every eligible member. The open-item counts are tenant-wide,
/// so the job GUC is emptied for the read and restored (`job_id`).
pub async fn pick(
    conn: &mut PgConnection,
    job_id: Uuid,
    exclude: Option<&str>,
) -> ApiResult<Option<String>> {
    sqlx::query("SELECT set_config('app.current_job', '', true)")
        .execute(&mut *conn)
        .await?;
    let sub: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT u.sub FROM users u \
           LEFT JOIN support_routing_member m ON m.tenant_id = u.tenant_id AND m.sub = u.sub \
          WHERE u.disabled_at IS NULL \
            AND u.{workers} \
            AND ($1::text IS NULL OR u.sub <> $1) \
            AND {team} \
            AND EXISTS (SELECT 1 FROM member_session s \
                         WHERE s.tenant_id = u.tenant_id AND s.sub = u.sub \
                           AND s.support_seen_at > now() - make_interval(secs => $2)) \
          ORDER BY m.last_assigned_at ASC NULLS FIRST, \
                   (SELECT count(*) FROM agent_handover_items_owned o \
                     WHERE o.assignee = u.sub AND o.state NOT IN ('resolved', 'returned_to_agent')) ASC, \
                   u.sub \
          LIMIT 1",
        workers = crate::auth::CONVERSATION_WORKERS_SQL,
        team = super::job_staff::team_open_sql("u.tenant_id", "$3", "u.sub")
    )))
    .bind(exclude)
    .bind(f64::from(ONLINE_WITHIN_SECS))
    .bind(job_id)
    .fetch_optional(&mut *conn)
    .await?;
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(job_id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(sub)
}

/// Does job `job_id` have an enabled worker-side staff binding? Then an external tool routes its items.
pub async fn worker_bound(conn: &mut PgConnection, job_id: Uuid) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM connector_bindings WHERE job_id = $1 AND side = 'worker' AND enabled)",
    )
    .bind(job_id)
    .fetch_one(conn)
    .await?)
}

/// What one routing decision did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Assigned to this member.
    Assigned(String),
    /// Nothing to do: routing is off, nobody is online, the item is gone or someone else owns it now, or another
    /// replica holds the tenant.
    Skipped,
}

/// The system's [`By`] in `scope`.
fn system(scope: Scope) -> By {
    By {
        scope,
        actor: Actor::system(ACTOR),
        sub: ACTOR.to_string(),
    }
}

/// One routing decision for conversation `conv` of job `job_id` (see the module doc). `from`: `None` for a queued
/// item (`expected = Queue`), `Some(sub)` to move an auto-assigned item off `sub`. `wait`: block on the tenant's
/// routing row (the trigger) or skip a tenant another replica is routing (the sweeper).
pub async fn route_item(
    st: &AppState,
    tenant: Uuid,
    job_id: Uuid,
    conv: &str,
    from: Option<&str>,
    wait: bool,
) -> ApiResult<Outcome> {
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    // the row the assigners of this tenant serialise on (written with the defaults when the tenant has none)
    sqlx::query(
        "INSERT INTO support_routing (tenant_id) VALUES (app_current_tenant()) ON CONFLICT (tenant_id) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;
    let lock = if wait {
        "SELECT mode FROM support_routing WHERE tenant_id = app_current_tenant() FOR UPDATE"
    } else {
        "SELECT mode FROM support_routing WHERE tenant_id = app_current_tenant() FOR UPDATE SKIP LOCKED"
    };
    let mode: Option<String> = sqlx::query_scalar(lock).fetch_optional(&mut *tx).await?;
    // the platform flag first: off = no auto-assignment whatever the mode says (read after the lock is taken, so a
    // flag turned off is seen by the next decision)
    // D125: the job's own mode wins over the tenant's unless it is `inherit` (no row = inherit)
    let job_mode: Option<String> =
        sqlx::query_scalar("SELECT mode FROM job_routing WHERE job_id = $1")
            .bind(job_id)
            .fetch_optional(&mut *tx)
            .await?;
    let effective = match job_mode.as_deref() {
        None | Some("inherit") => mode.clone(),
        Some(m) => mode.as_ref().map(|_| m.to_string()),
    };
    if !flag_enabled(&mut tx).await? || effective.as_deref() != Some(Mode::RoundRobin.as_str()) {
        tx.commit().await?;
        return Ok(Outcome::Skipped);
    }
    if worker_bound(&mut tx, job_id).await? {
        tx.commit().await?;
        return Ok(Outcome::Skipped);
    }
    let Some(to) = pick(&mut tx, job_id, from).await? else {
        tx.commit().await?;
        return Ok(Outcome::Skipped);
    };
    let job_key = store::job_key(&mut tx, job_id).await?;
    let by = system(Scope {
        tenant,
        job_id,
        job_key,
    });
    let expected = match from {
        Some(sub) => Owner::User(sub.to_string()),
        None => Owner::Queue,
    };
    let out = match ownership::assign_scoped_in(&mut tx, &by, conv, &to, &expected).await {
        Ok(out) => out,
        // a person took it (or it changed otherwise) first: a normal skip
        Err(e) if e.code() == "owner_changed" => return Ok(Outcome::Skipped),
        Err(e) => return Err(e),
    };
    sqlx::query(
        "INSERT INTO support_routing_member (tenant_id, sub, last_assigned_at) \
         VALUES (app_current_tenant(), $1, now()) \
         ON CONFLICT (tenant_id, sub) DO UPDATE SET last_assigned_at = now()",
    )
    .bind(&to)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    ownership::after(st, &by, conv, &out).await;
    Ok(Outcome::Assigned(to))
}

/// Was `e` the database refusing a write for lack of a grant (SQLSTATE 42501)? The `agent_runtime` pool of the runtime
/// processes has no grant on the routing tables, so for it routing is the sweeper's job.
pub fn is_denied(e: &ApiError) -> bool {
    match e {
        ApiError::Internal(a) => matches!(
            a.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::Database(d)) if d.code().as_deref() == Some("42501")
        ),
        _ => false,
    }
}

/// The NOTIFY channel a runtime process wakes agent-api's sweeper on.
pub const CHANNEL: &str = "support_routing";

/// The notification payload for item `item_id` of job `job_id`: `<tenant>:<job>:<item>`.
pub fn notify_payload(tenant: Uuid, job_id: Uuid, item_id: &str) -> String {
    format!("{tenant}:{job_id}:{item_id}")
}

/// Wake agent-api's sweeper for one item (best effort: a failure is logged; the interval tick still gets it).
async fn notify_sweeper(st: &AppState, tenant: Uuid, job_id: Uuid, item_id: &str) {
    if let Err(e) = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(notify_payload(tenant, job_id, item_id))
        .execute(&st.db)
        .await
    {
        tracing::warn!(item = item_id, error = %e, "support_routing_notify_failed");
    }
}

/// Route one queued item. `true` = the database refused for lack of a grant (a runtime pool: not an error).
async fn assign_queued(st: &AppState, tenant: Uuid, job_id: Uuid, item_id: &str) -> bool {
    let conv: ApiResult<Option<String>> = async {
        let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
        let c = sqlx::query_scalar(
            "SELECT conversation_id FROM agent_handover_items WHERE id = $1 AND state = 'queued'",
        )
        .bind(item_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(c)
    }
    .await;
    let conv = match conv {
        Ok(Some(c)) => c,
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!(
                item = item_id,
                code = e.code(),
                "support_routing_item_unread"
            );
            return false;
        }
    };
    match route_item(st, tenant, job_id, &conv, None, true).await {
        Ok(Outcome::Assigned(_)) => tracing::info!(item = item_id, "support_routing_assigned"),
        Ok(Outcome::Skipped) => {}
        Err(e) if is_denied(&e) => return true,
        Err(e) => tracing::warn!(item = item_id, code = e.code(), "support_routing_failed"),
    }
    false
}

/// After item `item_id` of job `job_id` was queued (and committed): assign it now when routing is on and someone is
/// online. Best effort: a failure is logged (ids and code only); the item stays queued and the sweeper retries. On a
/// runtime pool (no grant on the routing tables) it wakes agent-api's sweeper with a NOTIFY instead.
pub async fn auto_assign_item(st: &AppState, tenant: Uuid, job_id: Uuid, item_id: &str) {
    if assign_queued(st, tenant, job_id, item_id).await {
        tracing::debug!(item = item_id, "support_routing_deferred_to_sweeper");
        notify_sweeper(st, tenant, job_id, item_id).await;
    }
}

/// The sweeper's handling of one `support_routing` notification (`<tenant>:<job>:<item>`): route that item now. A
/// malformed payload is ignored; an item already owned is a skip. Never notifies again (no loop).
pub async fn handle_notification(st: &AppState, payload: &str) {
    let mut parts = payload.splitn(3, ':');
    let (Some(t), Some(j), Some(item)) = (parts.next(), parts.next(), parts.next()) else {
        tracing::warn!("support_routing_notify_malformed");
        return;
    };
    let (Ok(tenant), Ok(job_id)) = (Uuid::parse_str(t), Uuid::parse_str(j)) else {
        tracing::warn!("support_routing_notify_malformed");
        return;
    };
    assign_queued(st, tenant, job_id, item).await;
}

// ---- the sweeper ------------------------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct Due {
    tenant_id: Uuid,
    job_id: Uuid,
    conversation_id: String,
    item_id: String,
    kind: String,
    assignee: Option<String>,
}

/// One pass: the items routing should act on (`agent_support_routing_due`, across tenants, at most [`SWEEP_CAP`]),
/// each through [`route_item`] without waiting on a tenant another replica holds. Returns how many were assigned. One
/// item's failure is logged and the rest go on. Tests call this directly.
pub async fn tick(st: &AppState) -> ApiResult<usize> {
    let due: Vec<Due> = sqlx::query_as(
        "SELECT tenant_id, job_id, conversation_id, item_id, kind, assignee FROM agent_support_routing_due($1)",
    )
    .bind(SWEEP_CAP)
    .fetch_all(&st.db)
    .await?;
    let mut assigned = 0;
    for d in &due {
        let from = if d.kind == "stale" {
            d.assignee.as_deref()
        } else {
            None
        };
        match route_item(st, d.tenant_id, d.job_id, &d.conversation_id, from, false).await {
            Ok(Outcome::Assigned(_)) => assigned += 1,
            Ok(Outcome::Skipped) => {}
            Err(e) => tracing::warn!(
                tenant = %d.tenant_id,
                item = %d.item_id,
                code = e.code(),
                "support_routing_sweep_item_failed"
            ),
        }
    }
    Ok(assigned)
}

/// Reconnect backoff of the listener: starts at 1 s, doubles, capped.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

async fn run_tick(st: &AppState) {
    match tick(st).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(assigned = n, "support_routing_assigned"),
        Err(e) => tracing::warn!(code = e.code(), "support_routing_sweep_failed"),
    }
}

async fn listen(st: &AppState) -> Result<sqlx::postgres::PgListener, sqlx::Error> {
    let mut l = sqlx::postgres::PgListener::connect_with(&st.db).await?;
    l.listen(CHANNEL).await?;
    Ok(l)
}

/// Start the sweeper: [`tick`] every [`SWEEP_EVERY`], on every replica, plus a LISTEN on [`CHANNEL`] that routes an
/// item the moment a runtime process queues one.
pub fn spawn(st: AppState) -> tokio::task::JoinHandle<()> {
    spawn_every(st, SWEEP_EVERY)
}

/// [`spawn`] with the tick interval `period` (tests set it very long to prove the listener alone assigns).
pub fn spawn_every(st: AppState, period: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(period);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut backoff = Duration::from_secs(1);
        loop {
            let mut l = match listen(&st).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "support_routing_listen_failed");
                    // keep ticking while waiting to reconnect
                    let wait = tokio::time::sleep(backoff);
                    tokio::pin!(wait);
                    loop {
                        tokio::select! {
                            () = &mut wait => break,
                            _ = every.tick() => run_tick(&st).await,
                        }
                    }
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue;
                }
            };
            backoff = Duration::from_secs(1);
            // notifications sent while there was no listener are gone: one pass now covers them
            run_tick(&st).await;
            loop {
                tokio::select! {
                    _ = every.tick() => run_tick(&st).await,
                    n = l.recv() => match n {
                        Ok(n) => {
                            let (st, payload) = (st.clone(), n.payload().to_string());
                            // off the loop: a routing decision may block on the tenant's row
                            tokio::spawn(async move { handle_notification(&st, &payload).await });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "support_routing_listener_lost");
                            break;
                        }
                    },
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(mode: Option<&str>, secs: Option<i64>) -> RoutingBody {
        RoutingBody {
            mode: mode.map(str::to_string),
            reassign_after_secs: secs,
        }
    }

    #[test]
    fn put_bodies_are_checked() {
        assert_eq!(
            checked(&body(Some("off"), None)).unwrap(),
            (Some(Mode::Off), None)
        );
        assert_eq!(
            checked(&body(Some(" round_robin "), Some(600))).unwrap(),
            (Some(Mode::RoundRobin), Some(600))
        );
        assert_eq!(checked(&body(None, Some(60))).unwrap(), (None, Some(60)));
        for bad in [
            body(None, None),
            body(Some("random"), None),
            body(None, Some(59)),
            body(None, Some(86_401)),
        ] {
            assert_eq!(checked(&bad).unwrap_err().code(), "invalid_routing");
        }
    }
}
