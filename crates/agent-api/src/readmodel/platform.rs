//! Platform read models (B1 T10, X7): the tenant registry (`PlatformTenantRow`), the portfolio (`TenantPortfolio`)
//! and the footprint (`TenantFootprint`) of `web/src/lib/api/types/platform.ts`.
//!
//! The registry is ONE read across tenants, `agent_platform_tenant_rollup()` (migration 0036) inside
//! `begin_platform`. Portfolio and footprint add per-tenant reads (jobs, owners, audit history) inside
//! `begin_scoped(tenant)`. [`portfolio_extras`] and [`footprint_extras`] are the X7 extension points: B2/B4/B8 fill the
//! release, behind-pins, generator and usage fields, B3/B7 the slots, stores, providers and bundle.
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::ApiResult;
use crate::platform_db::begin_platform;
use crate::readmodel::tenant::{TenantRow, tenant_json};
use crate::tenancy::begin_scoped;

/// The provisioning targets, in panel order.
pub const TARGETS: [&str; 3] = ["licences", "strata_rag", "strata_ingest"];

/// One registry row (the rollup).
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RollupRow {
    #[sqlx(flatten)]
    pub t: TenantRow,
    pub jobs: i64,
    /// Enabled members that accepted their invite.
    pub users_active: i64,
    pub admins: i64,
    pub disabled: i64,
    pub invited: i64,
    /// Outbox rows pending or in flight.
    pub prov_open: i64,
    pub prov_failed: i64,
    pub prov_dead: i64,
    /// The latest outbox row per target: `[{target, status, last_error_code}]`.
    pub provision: Value,
}

impl RollupRow {
    /// The registry badge: the worst outbox state (dead > failed > pending) and how many rows are in it.
    pub fn badge(&self) -> (&'static str, i64) {
        if self.prov_dead > 0 {
            ("dead", self.prov_dead)
        } else if self.prov_failed > 0 {
            ("failed", self.prov_failed)
        } else if self.prov_open > 0 {
            ("pending", self.prov_open)
        } else {
            ("ok", 0)
        }
    }

    /// The lifecycle stage: `provisioning` while any outbox row is open, failed or dead, else the tenant status.
    pub fn stage(&self) -> &str {
        if self.prov_open + self.prov_failed + self.prov_dead > 0 {
            "provisioning"
        } else {
            self.t.status.as_str()
        }
    }

    /// `PlatformTenantRow`: the `Tenant` plus counts and the badge.
    pub fn registry_json(&self, plan: &str) -> Value {
        let mut v = tenant_json(&self.t, plan);
        let (badge, count) = self.badge();
        v["jobs"] = json!(self.jobs);
        v["users"] = json!(self.users_active + self.invited);
        v["provisioning"] = json!(badge);
        v["provisioning_count"] = json!(count);
        v
    }

    /// `TenantPortfolio.provision`: per target the latest row's status and error code (null = never queued).
    pub fn provision_json(&self) -> Value {
        let latest = self.provision.as_array().cloned().unwrap_or_default();
        Value::Array(
            TARGETS
                .iter()
                .map(|target| {
                    let row = latest.iter().find(|r| r["target"] == *target);
                    json!({
                        "target": target,
                        "status": row.map_or(Value::Null, |r| r["status"].clone()),
                        "error": row.map_or(Value::Null, |r| r["last_error_code"].clone()),
                    })
                })
                .collect(),
        )
    }

    /// `TenantPortfolio.members`.
    pub fn members_json(&self) -> Value {
        json!({
            "active": self.users_active,
            "admins": self.admins,
            "disabled": self.disabled,
            "invited": self.invited,
        })
    }
}

/// Every tenant's registry row, by name (one platform read).
pub async fn rollup(db: &PgPool) -> ApiResult<Vec<RollupRow>> {
    let mut tx = begin_platform(db).await?;
    let rows = sqlx::query_as::<_, RollupRow>("SELECT * FROM agent_platform_tenant_rollup()")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

/// The registry row of `id`, if it exists.
pub async fn rollup_one(db: &PgPool, id: Uuid) -> ApiResult<Option<RollupRow>> {
    Ok(rollup(db).await?.into_iter().find(|r| r.t.id == id))
}

/// X7 `platform::portfolio_extras`: what B2 (release, behind), B4 (cases) and B8 (generator) add to a portfolio job,
/// and the tenant's 30-day usage (B4). B1 answers the honest defaults, except `on_bus`: the request's one bus PING
/// (`agent_status::bus_up`), the same fact `GET /api/jobs/{job}/status` reports as `on_bus`.
pub fn portfolio_extras(job: &mut Map<String, Value>, bus_up: bool) {
    job.insert("on_bus".into(), json!(bus_up));
    job.insert("release".into(), Value::Null);
    job.insert("behind".into(), json!([]));
    job.insert("generator".into(), Value::Null);
    job.insert("cases_7d".into(), Value::Null);
    job.insert("containment_7d".into(), Value::Null);
}

/// The tenant's 30-day usage (X7, B4): zeros until the usage ledger exists.
pub fn usage_30d() -> Value {
    json!({"turns": 0, "tokens": 0, "last_day": null})
}

/// What needs a super_admin on this tenant (`TenantAttention[]`).
fn attention(r: &RollupRow) -> Vec<Value> {
    let href = format!("/platform/tenants/{}", r.t.id);
    let mut out = Vec::new();
    match r.badge() {
        ("dead", n) | ("failed", n) => out.push(json!({
            "kind": "provisioning",
            "severity": "err",
            "text": format!("{n} provisioning step(s) failed; retry them on the tenant"),
            "href": href,
        })),
        ("pending", n) => out.push(json!({
            "kind": "provisioning",
            "severity": "info",
            "text": format!("{n} provisioning step(s) still running"),
            "href": href,
        })),
        _ => {}
    }
    if r.t.status == "active" && r.t.licence_id.is_none() {
        out.push(json!({
            "kind": "licence",
            "severity": "warn",
            "text": "The tenant licence is not issued yet; the provisioning tick retries it",
            "href": href,
        }));
    }
    out
}

/// `TenantPortfolio` of one registry row: its jobs and owners read under its own tenant scope. `bus_up` is the
/// request's one bus PING.
pub async fn portfolio(db: &PgPool, r: &RollupRow, bus_up: bool) -> ApiResult<Value> {
    let mut tx = begin_scoped(db, r.t.id).await?;
    let jobs = portfolio_jobs(&mut tx, bus_up).await?;
    let owners: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT sub, coalesce(display_name, sub), coalesce(email, '') FROM users \
          WHERE 'tenant_admin' = ANY (roles) AND disabled_at IS NULL ORDER BY sub",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({
        "tenant_id": r.t.id,
        "stage": r.stage(),
        "provision": r.provision_json(),
        "owners": owners
            .into_iter()
            .map(|(sub, display_name, email)| json!({"sub": sub, "display_name": display_name, "email": email}))
            .collect::<Vec<_>>(),
        "members": r.members_json(),
        "jobs": jobs,
        "usage_30d": usage_30d(),
        "attention": attention(r),
    }))
}

async fn portfolio_jobs(conn: &mut PgConnection, bus_up: bool) -> ApiResult<Vec<Value>> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, label FROM jobs ORDER BY key")
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(key, label)| {
            let mut m = Map::new();
            m.insert("key".into(), json!(key));
            m.insert("label".into(), json!(label));
            portfolio_extras(&mut m, bus_up);
            Value::Object(m)
        })
        .collect())
}

/// X7 `platform::footprint_extras`: rag/ingest slots, knowledge stores and LLM providers (B3), the licence bundle
/// (B7). Reads the tenant's own tables through `conn` (scoped to that tenant); B1 answered empty.
pub async fn footprint_extras(
    conn: &mut PgConnection,
    f: &mut Map<String, Value>,
) -> ApiResult<()> {
    f.insert("slots".into(), json!([]));
    f.insert(
        "stores".into(),
        crate::knowledge::stores::footprint(conn).await?,
    );
    f.insert("llm_providers".into(), json!([]));
    f.insert("bundle".into(), Value::Null);
    Ok(())
}

/// The lifecycle actions the footprint history shows.
pub const HISTORY_ACTIONS: &str = r"^(tenant|provision|licence|license|user|job|release)\.";

/// `TenantFootprint` of one registry row: the portfolio, the X7 extras and the newest 50 lifecycle audit rows.
pub async fn footprint(db: &PgPool, r: &RollupRow, bus_up: bool) -> ApiResult<Value> {
    let portfolio = portfolio(db, r, bus_up).await?;
    let mut tx = begin_scoped(db, r.t.id).await?;
    type Event = (
        chrono::DateTime<chrono::Utc>,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
    );
    let rows: Vec<Event> = sqlx::query_as(
        "SELECT at, actor_id, actor_type, action, resource_id, status, job FROM audit_event \
          WHERE action ~ $1 ORDER BY id DESC LIMIT 50",
    )
    .bind(HISTORY_ACTIONS)
    .fetch_all(&mut *tx)
    .await?;
    let mut f = Map::new();
    footprint_extras(&mut tx, &mut f).await?;
    tx.commit().await?;
    let history: Vec<Value> = rows
        .into_iter()
        .map(
            |(at, actor_id, actor_type, action, resource_id, status, job)| {
                json!({
                    "at": at,
                    "actor_id": actor_id,
                    "actor_type": actor_type,
                    "action": action,
                    "resource_id": resource_id,
                    "ok": status == "success",
                    "job": job,
                })
            },
        )
        .collect();
    f.insert("tenant_id".into(), json!(r.t.id));
    f.insert("portfolio".into(), portfolio);
    f.insert("history".into(), Value::Array(history));
    Ok(Value::Object(f))
}
