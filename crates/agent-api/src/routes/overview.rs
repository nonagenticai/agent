//! J1 job dashboard (B4 T15): `jobOverview` (`GET /api/jobs/{job}/overview`, `JobOverview` in web
//! `types/runtime.ts`). Any member reads.
//!
//! - The current release, the release status (B2 `release::summary::status`, in the J1 `ReleaseStatus` shape) and
//!   the template pins with their newer tags.
//! - `kpis`: the last 30 UTC days against the 30 before them (crate::routes::analytics definitions; Production only).
//! - `health`: postgres, redis (the bus), the bus agents, strata_rag, the job's LLM provider (its `qualify` binding,
//!   resolved, never called) and every system a Production route sends calls to (its 30-day error rate). Every
//!   upstream probe is bounded by [`STEP`]; a probe that fails is a `down` row, never a failed page.
//! - `attention`: `AttentionItem`s built by [`crate::readmodel::job_attention`] (the same list the inbox and the agent
//!   status count): down dependencies, the bus, unchecked unknown outcomes (never retried automatically), dead
//!   letters, handovers past their SLA, unclaimed handovers, operations failing at least 10 %, and the release items
//!   (selection error, approved cut waiting, newer tags, readiness), each with its owner role.
//! - `pulse`: open work right now, the agents, dead letters and the failing operations.
//!
//! Titles carry ids and codes only (K50): no customer text, name or case title.
use std::collections::BTreeMap;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::analytics::{self, names_containment, ratio};
use crate::bus_admin;
use crate::conversations::iso;
use crate::design::{self, DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::members::names::Names;
use crate::platform::templates::TemplatePin;
use crate::providers::llm::{self, Use};
use crate::readmodel::job_attention::{self, DbFacts};
use crate::release::summary;
use crate::runtime::scope::Env;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/jobs/{job}/overview", get(overview))
}

/// Every upstream probe of the overview is bounded so one hung dependency cannot hold the page.
pub const STEP: Duration = Duration::from_secs(2);
/// An operation (or a routed system) is failing from this error rate on.
pub const FAILING_RATE: f64 = 0.10;
/// The KPI window (days) and the window before it.
const KPI_DAYS: i64 = 30;
/// The case statuses that are open work.
const OPEN: &str = "status IN ('open', 'identity_required', 'confirmation_required', 'handover')";

fn health_row(name: &str, status: &str, detail: Option<String>) -> Value {
    json!({"name": name, "status": status, "detail": detail})
}

/// 30-day Production calls and errors of one `(operation, instance)`.
#[derive(sqlx::FromRow)]
pub struct OpRow {
    pub operation: String,
    pub instance_id: Option<String>,
    pub calls: i64,
    pub errors: i64,
}

/// The operations failing at least [`FAILING_RATE`], worst first (`JobPulse.failing_ops`).
pub fn failing_ops(ops: &[OpRow]) -> Vec<Value> {
    let mut f: Vec<(&OpRow, f64)> = ops
        .iter()
        .filter_map(|o| ratio(o.errors, o.calls).map(|r| (o, r)))
        .filter(|(_, r)| *r >= FAILING_RATE)
        .collect();
    f.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| b.0.calls.cmp(&a.0.calls))
            .then_with(|| a.0.operation.cmp(&b.0.operation))
    });
    f.into_iter()
        .map(|(o, r)| {
            json!({
                "op": o.operation,
                "instance": o.instance_id,
                "calls": o.calls,
                "errors": o.errors,
                "rate": (r * 1000.0).round() / 1000.0,
            })
        })
        .collect()
}

/// The health row of a routed system from its 30-day calls.
fn system_row(instance: &str, ops: &[OpRow]) -> Value {
    let (calls, errors) = ops
        .iter()
        .filter(|o| o.instance_id.as_deref() == Some(instance))
        .fold((0, 0), |(c, e), o| (c + o.calls, e + o.errors));
    match ratio(errors, calls) {
        None => health_row(instance, "ok", Some("no calls in 30 days".into())),
        Some(r) if r >= FAILING_RATE => health_row(
            instance,
            "degraded",
            Some(format!("{errors} of {calls} calls failed in 30 days")),
        ),
        Some(_) => health_row(instance, "ok", None),
    }
}

/// What the database part of the overview read, before the upstream probes.
struct Db {
    label: String,
    north_star: String,
    current_release: Value,
    release_status: Value,
    pins: Vec<Value>,
    kpis: Value,
    open_by_status: BTreeMap<String, i64>,
    open_prod: i64,
    open_test: i64,
    alerts: i64,
    /// What the attention list reads (shared with the inbox and the agent status).
    facts: DbFacts,
    systems: Vec<String>,
}

/// `GET /api/jobs/{job}/overview` — `JobOverview`.
async fn overview(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &job).await?;
    let db = read_db(&mut tx, &st, &job).await?;
    tx.commit().await?;

    // ---- upstream probes (no transaction held) ----
    let mut health = vec![health_row("postgres", "ok", None)];
    let (redis_ok, agents, dead) = match st.redis.as_ref() {
        None => {
            health.push(health_row(
                "redis (bus)",
                "unconfigured",
                Some("REDIS_URL is not set".into()),
            ));
            (false, None, None)
        }
        Some(root) => {
            let ping = tokio::time::timeout(
                STEP,
                redis::cmd("PING").query_async::<String>(&mut root.conn()),
            )
            .await;
            match ping {
                Ok(Ok(_)) => {
                    health.push(health_row("redis (bus)", "ok", None));
                    let agents = match tokio::time::timeout(STEP, bus_admin::agents(&st, &j)).await
                    {
                        Ok(Ok(v)) => Some(v),
                        _ => None,
                    };
                    let dead = dead_letters(root, &j).await;
                    (true, agents, dead)
                }
                Ok(Err(e)) => {
                    tracing::warn!(kind = ?e.kind(), "overview_redis_unreachable");
                    health.push(health_row(
                        "redis (bus)",
                        "down",
                        Some("unreachable".into()),
                    ));
                    (false, None, None)
                }
                Err(_) => {
                    health.push(health_row("redis (bus)", "down", Some("timed out".into())));
                    (false, None, None)
                }
            }
        }
    };
    let mut agent_counts = json!({"total": 0, "ok": 0, "behind": 0, "idle": 0});
    if redis_ok {
        match &agents {
            Some(list) => {
                let count = |s: &str| list.iter().filter(|a| a["status"] == s).count();
                let (ok, behind, idle) = (count("ok"), count("warn"), count("idle"));
                agent_counts =
                    json!({"total": list.len(), "ok": ok, "behind": behind, "idle": idle});
                health.push(if behind > 0 {
                    health_row(
                        "agents",
                        "degraded",
                        Some(format!(
                            "{behind} of {} behind on their streams",
                            list.len()
                        )),
                    )
                } else if ok == 0 {
                    health_row(
                        "agents",
                        "degraded",
                        Some("no agent consumes the job's streams yet".into()),
                    )
                } else {
                    health_row("agents", "ok", None)
                });
            }
            None => health.push(health_row(
                "agents",
                "down",
                Some("the agents could not be read".into()),
            )),
        }
    }
    health.push(probe_strata(&st).await);
    health.push(llm_health(&st, &j, &job).await?);
    for s in &db.systems {
        health.push(system_row(s, &db.facts.ops));
    }

    // ---- attention (job_attention: the list the inbox and the agent status count too) ----
    let down: Vec<String> = health
        .iter()
        .filter(|h| h["status"] == "down" && h["name"] != "redis (bus)")
        .map(|h| h["name"].as_str().unwrap_or("a dependency").to_string())
        .collect();
    let bus_down_reported = health
        .iter()
        .any(|h| h["name"] == "redis (bus)" && h["status"] == "down");
    let attention = job_attention::build(&job_attention::Facts {
        job: &j.job_key,
        db: &db.facts,
        bus_up: redis_ok,
        bus_down_reported,
        down: &down,
        agents: agents.as_deref(),
        dead,
    });
    let failing = failing_ops(&db.facts.ops);

    let (op_calls, op_errors) = db
        .facts
        .ops
        .iter()
        .fold((0, 0), |(c, e), o| (c + o.calls, e + o.errors));
    Ok(Json(json!({
        "job": j.job_key,
        "label": db.label,
        "north_star": db.north_star,
        "on_bus": redis_ok,
        "current_release": db.current_release,
        "release_status": db.release_status,
        "kpis": db.kpis,
        "health": health,
        "attention": attention,
        "pulse": {
            "open_by_status": db.open_by_status,
            "open_prod": db.open_prod,
            "open_test": db.open_test,
            "unclaimed_handovers": db.facts.unclaimed,
            "alerts": db.alerts,
            "agents": agent_counts,
            "dead_letters": dead.unwrap_or(0),
            "failing_ops": failing,
            "op_calls_30d": op_calls,
            "op_errors_30d": op_errors,
        },
        "pins": db.pins,
    })))
}

/// The database part of the overview, in the job-scoped transaction.
async fn read_db(c: &mut PgConnection, st: &AppState, job: &JobRef) -> ApiResult<Db> {
    let (label, north_star, pins): (String, String, Value) =
        sqlx::query_as("SELECT label, north_star, pins FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_one(&mut *c)
            .await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "jobs.pins of {} is unreadable: {e}",
            job.key
        ))
    })?;

    // ---- release ----
    let facts = job_attention::db_facts(&mut *c, st, job).await?;
    let status = &facts.status;
    let list = summary::releases(&mut *c, job).await?;
    let mut names = Names::default();
    let current_release = match list.iter().find(|r| r.state == "current") {
        Some(r) => {
            let by = match &r.released_by {
                Some(sub) => Some(names.of(&mut *c, sub).await?),
                None => None,
            };
            json!({
                "id": r.release_id,
                "hash": r.hash,
                "released_by": by,
                "released_at": r.released_at.map(iso),
            })
        }
        None => Value::Null,
    };
    let approved_waiting = match list.first().filter(|r| r.state == "approved") {
        Some(w) => {
            let by = names.of(&mut *c, &w.approved_by).await?;
            json!({
                "id": w.release_id,
                "hash": w.hash,
                "approved_by": by,
                "approved_at": iso(w.approved_at),
            })
        }
        None => Value::Null,
    };
    // newer tags of the pinned layers (`newer_pins` of the B2 status) with when they were tagged
    let mut newer: BTreeMap<String, (i32, Option<DateTime<Utc>>)> = BTreeMap::new();
    for n in status["newer_pins"].as_array().into_iter().flatten() {
        let (Some(layer), Some(latest)) = (n["layer"].as_str(), n["latest"].as_i64()) else {
            continue;
        };
        let latest = latest as i32;
        let at: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT tagged_at FROM layer_tag WHERE family = $1 AND version = $2",
        )
        .bind(layer)
        .bind(latest)
        .fetch_optional(&mut *c)
        .await?;
        newer.insert(layer.to_string(), (latest, at));
    }
    let base_changed: Vec<Value> = pins
        .iter()
        .filter_map(|p| {
            let (latest, at) = newer.get(&p.layer)?;
            Some(json!({
                "layer": p.layer,
                "pinned": p.version,
                "latest": latest,
                "tagged_at": at.map(iso),
            }))
        })
        .collect();
    let pin_rows: Vec<Value> = pins
        .iter()
        .map(|p| {
            let n = newer.get(&p.layer);
            json!({
                "layer": p.layer,
                "pinned": p.version,
                "latest": n.map(|(v, _)| *v),
                "tagged_at": n.and_then(|(_, at)| at.map(iso)),
            })
        })
        .collect();
    let release_status = json!({
        "base_changed": base_changed,
        "approved_waiting": approved_waiting,
        "selection_error": status["selection_error"],
        "missing_slots": status["missing_slots"],
    });

    // ---- kpis ----
    let w = analytics::window(&mut *c, KPI_DAYS).await?;
    let cur = analytics::outcomes(&mut *c, job.id, w.from, w.to).await?;
    let prev = analytics::outcomes(&mut *c, job.id, w.prev_from, w.prev_to).await?;
    let turns = analytics::turns(&mut *c, job.id, w.from, w.to).await?;
    let turns_prev = analytics::turns(&mut *c, job.id, w.prev_from, w.prev_to).await?;

    // ---- open work ----
    let open: Vec<(String, String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT status, env, count(*) FROM agent_case WHERE job_id = $1 AND {OPEN} GROUP BY status, env"
    )))
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let mut open_by_status: BTreeMap<String, i64> = BTreeMap::new();
    let (mut open_prod, mut open_test) = (0i64, 0i64);
    for (status, env, n) in &open {
        *open_by_status.entry(status.clone()).or_default() += n;
        if env == "prod" {
            open_prod += n;
        } else {
            open_test += n;
        }
    }
    let containment = ratio(cur.contained, cur.cases);
    let north = if names_containment(&north_star) {
        json!({"label": north_star, "value": containment, "unit": "pct"})
    } else {
        let value = (cur.cases > 0).then_some(cur.cases);
        json!({"label": north_star, "value": value, "unit": "n"})
    };
    let kpis = json!({
        "turns_30d": turns,
        "turns_prev": turns_prev,
        "containment": containment,
        "containment_prev": ratio(prev.contained, prev.cases),
        "escalation": ratio(cur.escalated, cur.cases),
        "escalation_prev": ratio(prev.escalated, prev.cases),
        "open_cases": open_prod,
        "north_star": north,
    });

    let alerts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_case WHERE job_id = $1 AND alert IS NOT NULL",
    )
    .bind(job.id)
    .fetch_one(&mut *c)
    .await?;
    let systems: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT instance_id FROM operation_route \
          WHERE job_id = $1 AND env = 'prod' AND target_kind = 'instance' AND instance_id IS NOT NULL \
          ORDER BY instance_id",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    Ok(Db {
        label,
        north_star,
        current_release,
        release_status,
        pins: pin_rows,
        kpis,
        open_by_status,
        open_prod,
        open_test,
        alerts,
        facts,
        systems,
    })
}

/// Dead letters of both environments of the job (`XLEN bus:dead` per scope); `None` when Redis does not answer.
pub async fn dead_letters(root: &crate::runtime::bus::Bus, j: &JobCtx) -> Option<i64> {
    let mut n = 0i64;
    for env in Env::ALL {
        let bus = root.scoped(&bus_admin::scope_of(j, env).ok()?);
        let full = bus.streams.key(bus_admin::DEAD);
        let len = tokio::time::timeout(
            STEP,
            redis::cmd("XLEN")
                .arg(&full)
                .query_async::<i64>(&mut bus.conn()),
        )
        .await;
        match len {
            Ok(Ok(x)) => n += x,
            _ => return None,
        }
    }
    Some(n)
}

/// strata_rag's `/health` (the URL is never echoed: `detail` carries only the failure reason).
async fn probe_strata(st: &AppState) -> Value {
    const NAME: &str = "strata_rag";
    let Some(base) = st.cfg.strata.rag_url.as_deref() else {
        return health_row(
            NAME,
            "unconfigured",
            Some("STRATA_RAG_URL is not set".into()),
        );
    };
    let url = format!("{}/health", base.trim_end_matches('/'));
    match st.http.get(url).timeout(STEP).send().await {
        Ok(r) if r.status().is_success() => health_row(NAME, "ok", None),
        Ok(r) => health_row(NAME, "down", Some(format!("HTTP {}", r.status().as_u16()))),
        Err(e) => {
            let why = if e.is_timeout() {
                "timed out"
            } else {
                "unreachable"
            };
            health_row(NAME, "down", Some(why.into()))
        }
    }
}

/// The job's LLM provider: its Production `qualify` binding resolved (no call is made). Read in its own
/// transaction, dropped (rolled back): a refused binding's failed statement cannot fail the page.
async fn llm_health(st: &AppState, j: &JobCtx, job: &JobRef) -> ApiResult<Value> {
    const NAME: &str = "LLM provider";
    let tenant: Uuid = j.t.tenant;
    let mut tx = j.begin(&st.db).await?;
    let r = llm::resolve(&mut tx, st, tenant, job, Use::Qualify, &DesignOf::Live).await;
    drop(tx);
    Ok(match r {
        Ok(Some(x)) => health_row(NAME, "ok", Some(x.model)),
        Ok(None) => health_row(
            NAME,
            "unconfigured",
            Some("no qualify provider is bound".into()),
        ),
        Err(e) => health_row(NAME, "down", Some(e.code().to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(operation: &str, instance: Option<&str>, calls: i64, errors: i64) -> OpRow {
        OpRow {
            operation: operation.into(),
            instance_id: instance.map(Into::into),
            calls,
            errors,
        }
    }

    #[test]
    fn failing_ops_start_at_ten_percent_worst_first() {
        let ops = [
            op("order.read", Some("shop"), 100, 9),
            op("refund.create", Some("shop"), 10, 1),
            op("label.create", Some("carrier"), 4, 2),
        ];
        let f = failing_ops(&ops);
        let names: Vec<&str> = f.iter().map(|v| v["op"].as_str().unwrap()).collect();
        assert_eq!(names, ["label.create", "refund.create"]);
        assert_eq!(f[0]["rate"], json!(0.5));
    }

    #[test]
    fn a_routed_system_degrades_at_the_failing_rate() {
        let ops = [
            op("order.read", Some("shop"), 8, 1),
            op("refund.create", Some("shop"), 2, 1),
            op("label.create", Some("carrier"), 10, 0),
        ];
        assert_eq!(system_row("shop", &ops)["status"], "degraded");
        assert_eq!(system_row("carrier", &ops)["status"], "ok");
        assert_eq!(system_row("crm", &ops)["detail"], "no calls in 30 days");
    }
}
