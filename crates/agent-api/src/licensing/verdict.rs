//! Tenant licence verdicts (B7 T2; addendum 2026-10-21 R3, R4): what licences last said about each tenant's own
//! licence, kept in `tenant_licence_verdict` (0090) for the platform licensing overview (WS-C), B4's `LicenceGate` and
//! the edge's core seam.
//!
//! **Writer** ([`write`]): reads the tenant's stored licence (`tenants.licence_id` + the sealed token), asks licences
//! ([`super::introspect`]) and upserts one row. The mapping (plan T2):
//! - tenant `suspended` → `inactive`, reason `tenant_suspended`, licences is NOT called;
//! - `licence_id IS NULL` → `none` (reason `no_licence`): the B1 tick mints the first one;
//! - no stored token, or one that does not open → `inactive`, reason `license_unreadable`;
//! - licences unreachable / non-2xx → `inactive`, reason `license_unavailable` (an old `active` is never kept);
//! - inactive answer → `revoked` for `token_invalid` (a licence revoke revokes its tokens; agent's tokens are minted
//!   perpetual, so an invalid one is a revoked one), else `inactive` with licences' reason (`license_inactive`,
//!   `token_not_found`, `license_not_found`);
//! - active answer for another app (`LICENSE_APP_KEY`), another tenant or another licence id → `inactive` with
//!   `app_mismatch` / `tenant_mismatch` / `licence_mismatch`; an active answer whose licence status is not `active`
//!   → that status (`expired`, `revoked`, else `inactive`).
//!
//! The row is written only while the tenant still holds the licence id it checked (a re-issue that stored a new one
//! meanwhile wins; its own [`changed`] rewrites the row). [`read`] gives `age_seconds`; a verdict
//! [`Verdict::fails_closed`] unless it is `active` and at most [`MAX_AGE_SECONDS`] old.
//!
//! **Refresher** ([`spawn_refresher`]): every [`REFRESH_EVERY`] and on every nudge. One pass writes every non-archived
//! tenant's verdict, runs the R3 reconcile sweep and refreshes the R4 gauge. While `LICENSE_INTROSPECT_URL` is unset
//! the verdict and reconcile halves idle (logged once at boot); the gauge still runs.
//!
//! **Nudge** ([`nudge`]): no Redis (agent-api has no Redis client; `REDIS_URL` is accepted and unused, crate::config).
//! A nudge is a Postgres `NOTIFY` on [`CHANNEL`] with the tenant id (or `*` for every tenant); every replica's
//! refresher `LISTEN`s on its own connection and rewrites. Should `LISTEN` be unavailable (a transaction-mode pooler),
//! that is logged and the 60 s timer still converges.
//!
//! **R3 reconcile** ([`reconcile`]): the T3a re-issue (crate::licensing::reissue) revokes the old licence at licences
//! while its transaction holds the holder row; a crash before COMMIT leaves licences revoked and agent still storing
//! the dead id and token. Such a re-issue left a platform `license.reissue_request` row with no closing
//! `license.reissue` row. For each such request older than [`RECONCILE_GRACE`] (and younger than 7 days) the sweep
//! looks at what the holder stores: still the old id AND licences answers its token `token_invalid` → the drift is
//! repaired exactly as the re-issue's `Revoked` branch would have (holder cleared, id unparked, tenant audit
//! `license.revoke` with `via=reconcile` = the tombstone) and the B1 tick mints the replacement; otherwise nothing
//! drifted. Either way the request is closed with a platform `license.reissue` row (failed, `crash_repaired` /
//! `crash_no_drift`, `via=reconcile`), so the sweep is idempotent. Licences unreachable or a token that does not open:
//! left open for the next pass. A licence revoked out-of-band (no open re-issue) is NOT auto-repaired: its verdict says
//! `revoked` and an operator re-issues.
//!
//! **R4 gauge**: `agent_licence_pending_seconds{tenant}`, the age of each active tenant's oldest pending licence
//! (0038's `first_failed_at` counts, so a retrying mint does not reset it); 0 when nothing is pending.
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use sqlx::postgres::PgListener;
use sqlx::{PgConnection, PgPool};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::introspect::{Introspection, Introspector};
use crate::audit::{self, Actor, Event, PlatformEvent};
use crate::error::ApiResult;
use crate::identity::lock_licence;
use crate::members;
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::tenancy::{begin_scoped, lock_tenant_row};

/// How often the refresher runs a full pass.
pub const REFRESH_EVERY: Duration = Duration::from_secs(60);
/// A verdict older than this fails closed even when `active`.
pub const MAX_AGE_SECONDS: i64 = 300;
/// The Postgres NOTIFY channel of a nudge (payload: a tenant uuid, or `*`).
pub const CHANNEL: &str = "agent_licence_changed";
/// An open re-issue request younger than this may still be running: the sweep leaves it alone.
pub const RECONCILE_GRACE: Duration = Duration::from_secs(10 * 60);
/// At most this many tenants' verdicts are written at once in one pass. Each write may wait up to
/// [`super::introspect::INTROSPECT_TIMEOUT`] (3 s) on licences; a sequential pass over ~48 tenants on a slow licences
/// service would take 48 × 3 s = 144 s plus the database round trips, and at ~100 tenants it passes
/// [`MAX_AGE_SECONDS`] (300 s), so every verdict reads stale (fail closed) before the pass ends. 8 at a time bounds a
/// pass at ⌈n / 8⌉ × 3 s (48 tenants: 18 s; 100 tenants: 39 s). A write holds a pooled connection only for its two
/// short transactions, never across the licences call, so 8 leaves the 10-connection runtime pool room for requests.
const REFRESH_CONCURRENCY: usize = 8;
/// At most this many open re-issue requests per pass.
const RECONCILE_BATCH: i64 = 20;
/// The audit actor of the refresher and the reconcile sweep.
const ACTOR: &str = "service:agent-api-licence-refresher";
/// Back-off before the nudge listener reconnects.
const LISTEN_RETRY: Duration = Duration::from_secs(30);

/// One tenant's verdict as stored, with its age.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Verdict {
    pub tenant_id: Uuid,
    /// `active|inactive|revoked|expired|none`.
    pub status: String,
    pub reason: Option<String>,
    pub licence_id: Option<String>,
    pub plan: Option<String>,
    pub kid: Option<String>,
    pub platform_owned: bool,
    pub checked_at: DateTime<Utc>,
    /// Seconds since `checked_at` when it was read.
    pub age_seconds: i64,
}

impl Verdict {
    /// True unless the verdict is `active` and at most [`MAX_AGE_SECONDS`] old.
    pub fn fails_closed(&self) -> bool {
        self.status != "active" || self.age_seconds > MAX_AGE_SECONDS
    }
}

/// What one refresher pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Refreshed {
    /// Tenants the pass looked at (not archived).
    pub tenants: u32,
    /// Verdict rows written.
    pub written: u32,
    /// Of those, written `inactive` / `license_unavailable` (licences not reachable).
    pub unavailable: u32,
    /// Tenants whose verdict could not be written (database error; logged).
    pub failed: u32,
    /// R3: re-issue crash windows repaired.
    pub repaired: u32,
}

/// Is the introspect client configured (`LICENSE_INTROSPECT_URL`)? Without it no verdict is written.
pub fn configured(st: &AppState) -> bool {
    st.cfg.license_introspect_url.is_some()
}

const COLUMNS: &str = "tenant_id, status, reason, licence_id, plan, kid, platform_owned, checked_at, \
     EXTRACT(EPOCH FROM now() - checked_at)::bigint AS age_seconds";

/// The verdict of `tenant`, read in `conn` (scoped to it: `begin_scoped`). `None` = never written.
pub async fn read(conn: &mut PgConnection, tenant: Uuid) -> ApiResult<Option<Verdict>> {
    let sql = format!("SELECT {COLUMNS} FROM tenant_licence_verdict WHERE tenant_id = $1");
    Ok(sqlx::query_as::<_, Verdict>(sqlx::AssertSqlSafe(sql))
        .bind(tenant)
        .fetch_optional(conn)
        .await?)
}

/// Every tenant's verdict (the licensing overview, WS-C). `conn` must be `agent_platform` (`begin_platform`, or
/// between `platform_db::enter` and `leave`): `agent_licence_verdicts()` is executable by that role only.
pub async fn overview(conn: &mut PgConnection) -> ApiResult<Vec<Verdict>> {
    let sql = format!("SELECT {COLUMNS} FROM agent_licence_verdicts()");
    Ok(sqlx::query_as::<_, Verdict>(sqlx::AssertSqlSafe(sql))
        .fetch_all(conn)
        .await?)
}

/// Ask every replica's refresher to rewrite `tenant`'s verdict (`None`: every tenant's) now. Fire-and-forget: the
/// rewrite happens on the refreshers' own connections (`NOTIFY` is delivered at COMMIT; this runs in its own
/// statement).
pub async fn nudge(db: &PgPool, tenant: Option<Uuid>) -> ApiResult<()> {
    let payload = tenant.map_or_else(|| "*".to_string(), |t| t.to_string());
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload)
        .execute(db)
        .await?;
    Ok(())
}

/// Rewrite `tenant`'s verdict now (in this process) and nudge the other replicas. Call it after anything that changes
/// a tenant's licence (reseal, re-issue, re-mint). `None` while `LICENSE_INTROSPECT_URL` is unset.
pub async fn changed(st: &AppState, tenant: Uuid) -> ApiResult<Option<Verdict>> {
    let v = write(st, tenant).await?;
    if let Err(e) = nudge(&st.db, Some(tenant)).await {
        tracing::warn!(%tenant, error = %e, "licence_verdict_nudge_failed");
    }
    Ok(v)
}

/// Check `tenant`'s licence with licences and store the verdict (see the module doc). `None` while
/// `LICENSE_INTROSPECT_URL` is unset, or when the tenant does not exist / the licence changed while it was checked.
pub async fn write(st: &AppState, tenant: Uuid) -> ApiResult<Option<Verdict>> {
    let Some(intro) = Introspector::for_state(st) else {
        return Ok(None);
    };
    write_with(st, &intro, tenant).await
}

/// The tenant row the writer reads.
#[derive(sqlx::FromRow)]
struct Held {
    status: String,
    licence_id: Option<String>,
    licence_token_encrypted: Option<String>,
}

/// What the writer decided, before it is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Decided {
    status: &'static str,
    reason: Option<String>,
    plan: Option<String>,
}

impl Decided {
    fn new(status: &'static str, reason: &str) -> Self {
        Self {
            status,
            reason: Some(reason.to_string()),
            plan: None,
        }
    }
}

async fn write_with(
    st: &AppState,
    intro: &Introspector,
    tenant: Uuid,
) -> ApiResult<Option<Verdict>> {
    // Never hold a transaction across the licences call: read, commit, ask, then write in a second transaction.
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let held: Option<Held> = sqlx::query_as(
        "SELECT status, licence_id, licence_token_encrypted FROM tenants WHERE id = $1",
    )
    .bind(tenant)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some(held) = held else {
        return Ok(None);
    };
    let decided = decide(st, intro, tenant, &held).await;
    if decided.reason.as_deref() == Some("license_unavailable") {
        metrics::counter!("agent_licence_verdict_unavailable_total").increment(1);
    }
    let kid = held.licence_token_encrypted.as_deref().and_then(kid_of);
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let sql = format!(
        "INSERT INTO tenant_licence_verdict \
                (tenant_id, status, reason, licence_id, plan, kid, platform_owned, checked_at) \
         SELECT t.id, $2::text, $3::text, $4::text, $5::text, $6::text, t.platform_owned, now() \
           FROM tenants t \
          WHERE t.id = $1 AND t.licence_id IS NOT DISTINCT FROM $4::text \
             ON CONFLICT (tenant_id) DO UPDATE \
            SET status = EXCLUDED.status, reason = EXCLUDED.reason, licence_id = EXCLUDED.licence_id, \
                plan = EXCLUDED.plan, kid = EXCLUDED.kid, platform_owned = EXCLUDED.platform_owned, \
                checked_at = EXCLUDED.checked_at \
         RETURNING {COLUMNS}"
    );
    let v: Option<Verdict> = sqlx::query_as::<_, Verdict>(sqlx::AssertSqlSafe(sql))
        .bind(tenant)
        .bind(decided.status)
        .bind(decided.reason.as_deref().map(|r| clip(r, 80)))
        .bind(held.licence_id.as_deref())
        .bind(decided.plan.as_deref().map(|p| clip(p, 80)))
        .bind(kid)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    if v.is_none() {
        tracing::info!(%tenant, "licence_verdict_skipped_licence_changed");
    }
    Ok(v)
}

async fn decide(st: &AppState, intro: &Introspector, tenant: Uuid, held: &Held) -> Decided {
    if held.status == "suspended" {
        return Decided::new("inactive", "tenant_suspended");
    }
    let Some(licence_id) = held.licence_id.as_deref() else {
        return Decided::new("none", "no_licence");
    };
    let Some(token) = held
        .licence_token_encrypted
        .as_deref()
        .and_then(|sealed| open_token(st, tenant, sealed))
    else {
        return Decided::new("inactive", "license_unreadable");
    };
    match intro.check(&token).await {
        Ok(i) => map_answer(
            &i,
            tenant,
            licence_id,
            &st.cfg.identity.licenses_admin.app_key,
        ),
        Err(e) => {
            tracing::warn!(%tenant, error = %e, "licence_introspect_unavailable");
            Decided::new("inactive", "license_unavailable")
        }
    }
}

/// Licences' answer for `tenant`'s stored licence `licence_id` (app `app_key`), as a verdict.
fn map_answer(i: &Introspection, tenant: Uuid, licence_id: &str, app_key: &str) -> Decided {
    if !i.active {
        let reason = i.reason.as_deref().unwrap_or("license_inactive");
        let status = if reason == "token_invalid" {
            "revoked"
        } else {
            "inactive"
        };
        return Decided::new(status, reason);
    }
    if i.app.as_deref() != Some(app_key) {
        return Decided::new("inactive", "app_mismatch");
    }
    if i.tenant_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()) != Some(tenant) {
        return Decided::new("inactive", "tenant_mismatch");
    }
    if i.license_id.as_deref() != Some(licence_id) {
        return Decided::new("inactive", "licence_mismatch");
    }
    let status = match i.status.as_deref() {
        None | Some("active") => "active",
        Some("expired") => "expired",
        Some("revoked") => "revoked",
        Some(_) => "inactive",
    };
    Decided {
        status,
        reason: (status != "active").then(|| format!("license_{status}")),
        plan: i.plan.clone(),
    }
}

/// The plaintext of a stored sealed licence token, `None` when it does not open. The ONE place this module opens a
/// token, sealed by the `Purpose::LicenceToken` keyring (`identity.cipher`, B7 T1).
fn open_token(st: &AppState, tenant: Uuid, sealed: &str) -> Option<String> {
    st.identity.cipher.open(tenant, sealed).ok()
}

/// The keyring kid of a `v2:<kid>:<blob>` value; `None` for `v1:` or anything else.
fn kid_of(sealed: &str) -> Option<String> {
    let mut parts = sealed.splitn(3, ':');
    if parts.next() != Some("v2") {
        return None;
    }
    let kid = parts.next()?;
    let ok = (1..=32).contains(&kid.len())
        && kid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    (ok && parts.next().is_some()).then(|| kid.to_string())
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// One refresher pass: every non-archived tenant's verdict, the R3 reconcile sweep, the R4 gauge. Database errors of
/// one tenant are logged and counted, never stop the pass.
pub async fn refresh_all(st: &AppState) -> ApiResult<Refreshed> {
    let mut r = Refreshed::default();
    if let Some(intro) = Introspector::for_state(st) {
        let mut tx = begin_platform(&st.db).await?;
        let tenants: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT tenant_id, status FROM agent_licence_verdict_tenants()")
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let intro = &intro;
        // The futures are built before streaming them: a `.map(closure)` held across the await trips rustc's
        // higher-ranked lifetime check (rust-lang/rust#102211) where the caller must be `Send` (the spawned refresher).
        let writes: Vec<_> = tenants
            .into_iter()
            .map(|(tenant, _)| async move { (tenant, write_with(st, intro, tenant).await) })
            .collect();
        let results: Vec<(Uuid, ApiResult<Option<Verdict>>)> = stream::iter(writes)
            .buffer_unordered(REFRESH_CONCURRENCY)
            .collect()
            .await;
        for (tenant, written) in results {
            r.tenants += 1;
            match written {
                Ok(Some(v)) => {
                    r.written += 1;
                    if v.reason.as_deref() == Some("license_unavailable") {
                        r.unavailable += 1;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    r.failed += 1;
                    tracing::warn!(%tenant, error = %e, "licence_verdict_write_failed");
                }
            }
        }
        match reconcile(st, intro).await {
            Ok(n) => r.repaired = n,
            Err(e) => tracing::warn!(error = %e, "licence_reconcile_sweep_failed"),
        }
    }
    if let Err(e) = refresh_pending_gauge(&st.db).await {
        tracing::warn!(error = %e, "licence_pending_gauge_failed");
    }
    Ok(r)
}

/// Tenants that carry an `agent_licence_pending_seconds` series, so one that stops being active is set to 0.
fn gauged() -> &'static Mutex<HashSet<Uuid>> {
    static SEEN: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();
    SEEN.get_or_init(Mutex::default)
}

/// R4: `agent_licence_pending_seconds{tenant}` from `agent_licence_pending()`.
pub async fn refresh_pending_gauge(db: &PgPool) -> ApiResult<()> {
    let mut tx = begin_platform(db).await?;
    let rows: Vec<(Uuid, f64)> =
        sqlx::query_as("SELECT tenant_id, pending_seconds FROM agent_licence_pending()")
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let mut seen = gauged().lock().unwrap_or_else(|p| p.into_inner());
    let now: HashSet<Uuid> = rows.iter().map(|(t, _)| *t).collect();
    for gone in seen.difference(&now) {
        metrics::gauge!("agent_licence_pending_seconds", "tenant" => gone.to_string()).set(0.0);
    }
    for (t, s) in &rows {
        metrics::gauge!("agent_licence_pending_seconds", "tenant" => t.to_string()).set(*s);
    }
    seen.extend(now);
    Ok(())
}

/// An open re-issue request (platform `license.reissue_request` with no closing `license.reissue`).
#[derive(sqlx::FromRow)]
struct OpenRequest {
    tenant: Uuid,
    label: String,
    holder: String,
    sub: Option<String>,
    old: String,
}

/// R3 (see the module doc). Returns how many holders were repaired.
pub async fn reconcile(st: &AppState, intro: &Introspector) -> ApiResult<u32> {
    let mut tx = begin_platform(&st.db).await?;
    let open: Vec<OpenRequest> = sqlx::query_as(
        "SELECT r.subject_tenant AS tenant, r.subject_label AS label, r.details ->> 'holder' AS holder, \
                r.details ->> 'sub' AS sub, r.details ->> 'old_licence_id' AS old \
           FROM platform_audit r \
          WHERE r.action = 'license.reissue_request' AND r.status = 'ok' AND r.subject_tenant IS NOT NULL \
            AND r.details ->> 'old_licence_id' IS NOT NULL \
            AND r.details ->> 'holder' IN ('tenant', 'member') \
            AND r.at < now() - make_interval(secs => $1) AND r.at > now() - interval '7 days' \
            AND NOT EXISTS ( \
                SELECT 1 FROM platform_audit d \
                 WHERE d.action = 'license.reissue' AND d.id > r.id AND d.subject_tenant = r.subject_tenant \
                   AND d.details ->> 'holder' = r.details ->> 'holder' \
                   AND d.details ->> 'old_licence_id' = r.details ->> 'old_licence_id' \
                   AND (d.details ->> 'sub') IS NOT DISTINCT FROM (r.details ->> 'sub')) \
          ORDER BY r.id LIMIT $2",
    )
    .bind(RECONCILE_GRACE.as_secs_f64())
    .bind(RECONCILE_BATCH)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut repaired = 0;
    for req in open {
        let member = (req.holder == "member").then(|| req.sub.clone()).flatten();
        if req.holder == "member" && member.is_none() {
            continue;
        }
        match reconcile_one(st, intro, &req, member.as_deref()).await {
            Ok(Some(true)) => repaired += 1,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(tenant = %req.tenant, holder = %req.holder, error = %e, "licence_reconcile_failed")
            }
        }
    }
    Ok(repaired)
}

/// One open request: `Some(true)` repaired, `Some(false)` closed without drift, `None` left open (licences not
/// reachable, or the stored token does not open).
async fn reconcile_one(
    st: &AppState,
    intro: &Introspector,
    req: &OpenRequest,
    member: Option<&str>,
) -> ApiResult<Option<bool>> {
    let tenant = req.tenant;
    let old = req.old.as_str();
    // What the holder stores now (still the old id → its sealed token).
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let sealed: Option<Option<String>> = match member {
        None => {
            sqlx::query_scalar(
                "SELECT licence_token_encrypted FROM tenants WHERE id = $1 AND licence_id = $2",
            )
            .bind(tenant)
            .bind(old)
            .fetch_optional(&mut *tx)
            .await?
        }
        Some(sub) => {
            sqlx::query_scalar(
                "SELECT token_encrypted FROM member_entitlement \
              WHERE user_sub = $1 AND license_id = $2 AND status = 'active'",
            )
            .bind(sub)
            .bind(old)
            .fetch_optional(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;
    let Some(sealed) = sealed else {
        // The holder moved on (the re-issue committed, or another flow cleared it): nothing drifted.
        close(st, req, false).await?;
        return Ok(Some(false));
    };
    let Some(token) = sealed.as_deref().and_then(|s| open_token(st, tenant, s)) else {
        tracing::warn!(%tenant, holder = %req.holder, "licence_reconcile_unreadable_token");
        return Ok(None);
    };
    let revoked = match intro.check(&token).await {
        Ok(i) => !i.active && i.reason.as_deref() == Some("token_invalid"),
        Err(e) => {
            tracing::warn!(%tenant, error = %e, "licence_reconcile_licences_unavailable");
            return Ok(None);
        }
    };
    if !revoked {
        // Licences never revoked it: the re-issue died before its revoke; the holder's licence is intact.
        close(st, req, false).await?;
        return Ok(Some(false));
    }
    // The crash window: repair as the re-issue's `Revoked` branch (crate::licensing::reissue `swap`) would have.
    let actor = Actor::system(ACTOR);
    let mut tx = begin_scoped(&st.db, tenant).await?;
    lock_licence(&mut tx, tenant, old).await?;
    lock_tenant_row(&mut tx, tenant).await?;
    let (n, resource, resource_id) = match member {
        None => {
            let n = sqlx::query(
                "UPDATE tenants SET licence_id = NULL, licence_token_encrypted = NULL, updated_at = now() \
                  WHERE id = $1 AND licence_id = $2",
            )
            .bind(tenant)
            .bind(old)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            members::unpark(&mut tx, tenant, old).await?;
            (n, "tenant", tenant.to_string())
        }
        Some(sub) => {
            members::unpark(&mut tx, tenant, old).await?;
            let n = sqlx::query(
                "UPDATE member_entitlement \
                    SET status = 'pending', license_id = NULL, license_subject_id = NULL, token_encrypted = NULL, \
                        error_code = NULL, updated_at = now() \
                  WHERE user_sub = $1 AND license_id = $2 AND status = 'active'",
            )
            .bind(sub)
            .bind(old)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            (n, "member_entitlement", sub.to_string())
        }
    };
    if n == 0 {
        // Moved on between the read and the lock: nothing to repair.
        tx.rollback().await?;
        close(st, req, false).await?;
        return Ok(Some(false));
    }
    audit::record(
        &mut tx,
        &actor,
        Event::new("license.revoke", resource, resource_id)
            .detail("license_id", old.to_string())
            .detail("via", "reconcile"),
    )
    .await?;
    tx.commit().await?;
    let holder: &'static str = if member.is_some() { "member" } else { "tenant" };
    metrics::counter!("agent_licence_reconcile_repaired_total", "holder" => holder).increment(1);
    tracing::warn!(%tenant, holder = %req.holder, "licence_reissue_crash_repaired");
    close(st, req, true).await?;
    if member.is_none()
        && let Err(e) = write_with(st, intro, tenant).await
    {
        tracing::warn!(%tenant, error = %e, "licence_verdict_write_failed");
    }
    Ok(Some(true))
}

/// Close an open re-issue request with a platform `license.reissue` row (failed, `crash_repaired` /
/// `crash_no_drift`), so the sweep never picks it again.
async fn close(st: &AppState, req: &OpenRequest, repaired: bool) -> ApiResult<()> {
    let mut e = PlatformEvent::new("license.reissue", req.label.clone())
        .tenant(req.tenant)
        .failed(if repaired {
            "crash_repaired"
        } else {
            "crash_no_drift"
        })
        .detail("holder", req.holder.clone())
        .detail("old_licence_id", req.old.clone())
        .detail("via", "reconcile");
    if let Some(s) = &req.sub {
        e = e.detail("sub", s.clone());
    }
    audit::record_platform_alone(&st.db, &Actor::system(ACTOR), e).await?;
    Ok(())
}

/// Start the refresher (once, at boot). The verdict + reconcile halves idle while `LICENSE_INTROSPECT_URL` is unset
/// (logged here); the R4 gauge always runs.
pub fn spawn_refresher(st: AppState) -> tokio::task::JoinHandle<()> {
    spawn_refresher_every(st, REFRESH_EVERY)
}

/// [`spawn_refresher`] with another period (tests).
#[doc(hidden)]
pub fn spawn_refresher_every(st: AppState, every: Duration) -> tokio::task::JoinHandle<()> {
    if configured(&st) {
        tracing::info!(
            every_s = every.as_secs(),
            "licence_verdict_refresher_started"
        );
    } else {
        tracing::warn!(
            "licence_verdicts_disabled: LICENSE_INTROSPECT_URL is unset; no licence verdict is refreshed \
             (only agent_licence_pending_seconds)"
        );
    }
    let (tx, rx) = mpsc::unbounded_channel();
    if configured(&st) {
        tokio::spawn(listen(st.cfg.database_url.clone(), tx));
    }
    tokio::spawn(run(st, every, rx))
}

async fn run(st: AppState, every: Duration, mut nudges: mpsc::UnboundedReceiver<Option<Uuid>>) {
    let mut t = tokio::time::interval(every);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = t.tick() => pass(&st).await,
            Some(n) = nudges.recv() => match n {
                Some(tenant) => {
                    if let Err(e) = write(&st, tenant).await {
                        tracing::warn!(%tenant, error = %e, "licence_verdict_write_failed");
                    }
                }
                None => pass(&st).await,
            },
        }
    }
}

/// Unix time of the last refresh pass that completed (whatever the verdicts it wrote). The licence gate fails closed on
/// a verdict older than [`MAX_AGE_SECONDS`], so a stalled refresher refuses every paying tenant while the pod stays
/// Ready: `AgentLicenceVerdictStale` alerts on this gauge's age.
pub const REFRESHED_AT_GAUGE: &str = "agent_licence_verdict_refreshed_timestamp_seconds";

async fn pass(st: &AppState) {
    match refresh_all(st).await {
        Ok(r) => {
            if configured(st) {
                metrics::gauge!(REFRESHED_AT_GAUGE).set(Utc::now().timestamp() as f64);
            }
            if r.failed > 0 || r.unavailable > 0 || r.repaired > 0 {
                tracing::warn!(report = ?r, "licence_verdict_refresh")
            }
        }
        Err(e) => tracing::warn!(error = %e, "licence_verdict_refresh_failed"),
    }
}

/// `LISTEN` on [`CHANNEL`] on its own connection (`DATABASE_URL`, outside the pool) and forward each nudge (`None` = every tenant). Reconnects after
/// [`LISTEN_RETRY`]; returns once the refresher is gone.
async fn listen(url: String, tx: mpsc::UnboundedSender<Option<Uuid>>) {
    loop {
        match PgListener::connect(&url).await {
            Ok(mut l) => match l.listen(CHANNEL).await {
                Ok(()) => {
                    tracing::info!(channel = CHANNEL, "licence_verdict_listening");
                    loop {
                        match l.recv().await {
                            Ok(n) => {
                                if tx.send(Uuid::parse_str(n.payload()).ok()).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "licence_verdict_listen_lost");
                                break;
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "licence_verdict_listen_unavailable"),
            },
            Err(e) => tracing::warn!(error = %e, "licence_verdict_listen_unavailable"),
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(LISTEN_RETRY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(tenant: Uuid) -> Introspection {
        Introspection {
            active: true,
            status: Some("active".into()),
            app: Some("agent".into()),
            tenant_id: Some(tenant.to_string()),
            license_id: Some("lic-1".into()),
            plan: Some("byo".into()),
            ..Introspection::default()
        }
    }

    #[test]
    fn answers_map_to_verdicts() {
        let t = Uuid::new_v4();
        let d = map_answer(&active(t), t, "lic-1", "agent");
        assert_eq!(d.status, "active");
        assert_eq!(d.reason, None);
        assert_eq!(d.plan.as_deref(), Some("byo"));
        let revoked = Introspection {
            active: false,
            reason: Some("token_invalid".into()),
            ..Introspection::default()
        };
        assert_eq!(map_answer(&revoked, t, "lic-1", "agent").status, "revoked");
        let inactive = Introspection {
            active: false,
            reason: Some("license_inactive".into()),
            ..Introspection::default()
        };
        let d = map_answer(&inactive, t, "lic-1", "agent");
        assert_eq!(
            (d.status, d.reason.as_deref()),
            ("inactive", Some("license_inactive"))
        );
        let d = map_answer(&active(t), t, "lic-1", "other");
        assert_eq!(d.reason.as_deref(), Some("app_mismatch"));
        let d = map_answer(&active(t), Uuid::new_v4(), "lic-1", "agent");
        assert_eq!(d.reason.as_deref(), Some("tenant_mismatch"));
        let d = map_answer(&active(t), t, "lic-2", "agent");
        assert_eq!(d.reason.as_deref(), Some("licence_mismatch"));
        let expired = Introspection {
            status: Some("expired".into()),
            ..active(t)
        };
        assert_eq!(map_answer(&expired, t, "lic-1", "agent").status, "expired");
    }

    #[test]
    fn kid_is_read_from_v2_only() {
        assert_eq!(kid_of("v2:k1:abc").as_deref(), Some("k1"));
        assert_eq!(kid_of("v1:abc"), None);
        assert_eq!(kid_of("v2:bad kid:abc"), None);
        assert_eq!(kid_of("v2:k1"), None);
    }

    #[test]
    fn stale_or_inactive_verdicts_fail_closed() {
        let v = Verdict {
            tenant_id: Uuid::nil(),
            status: "active".into(),
            reason: None,
            licence_id: Some("l".into()),
            plan: None,
            kid: None,
            platform_owned: false,
            checked_at: Utc::now(),
            age_seconds: 10,
        };
        assert!(!v.fails_closed());
        assert!(
            Verdict {
                age_seconds: MAX_AGE_SECONDS + 1,
                ..v.clone()
            }
            .fails_closed()
        );
        assert!(
            Verdict {
                status: "revoked".into(),
                ..v
            }
            .fails_closed()
        );
    }
}
