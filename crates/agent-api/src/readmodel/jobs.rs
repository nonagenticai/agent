//! Job read models (B1 T10/T11, X7): `JobSummary` and the job-board extras of `web/src/lib/api/types/{core,tenant}.ts`.
//! [`summary`] and [`board_extras`] are the X7 extension points. `on_bus` is the bus fact of
//! [`super::agent_status::bus_up`] (probed by the caller before its transaction opens), the readiness, release and case
//! numbers are read from the job's own tables. [`on_job_created`] is the creation hook (B2 job_target + baseline,
//! B3 store_ensure).
use agent_model::compile::Selection;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::conversations::iso;
use crate::design::{self, JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::TemplatePin;
use crate::routes::analytics::{self, ratio};
use crate::state::AppState;

/// The `jobs` columns a [`JobRow`] reads.
pub const JOB_COLUMNS: &str = "id, tenant_id, key, label, north_star, settings, pins, template_layer, \
     template_version, draft_rev, languages, audience, created_at, created_by, updated_at";

/// One `jobs` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct JobRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub key: String,
    pub label: String,
    pub north_star: String,
    pub settings: Value,
    pub pins: Value,
    pub template_layer: Option<String>,
    pub template_version: Option<i32>,
    pub draft_rev: i64,
    pub languages: Vec<String>,
    /// D129: who the job serves, `customers` or `employees`.
    pub audience: String,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub updated_at: DateTime<Utc>,
}

/// Every job of the scoped tenant, by key.
pub async fn list(conn: &mut PgConnection) -> ApiResult<Vec<JobRow>> {
    Ok(sqlx::query_as::<_, JobRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS} FROM jobs ORDER BY key"
    )))
    .fetch_all(conn)
    .await?)
}

/// The job `key` of the scoped tenant.
pub async fn by_key(conn: &mut PgConnection, key: &str) -> ApiResult<Option<JobRow>> {
    Ok(sqlx::query_as::<_, JobRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS} FROM jobs WHERE key = $1"
    )))
    .bind(key)
    .fetch_optional(conn)
    .await?)
}

/// X7 `jobs::summary`: the `JobSummary` of a row. `on_bus` is whether the message bus answers
/// ([`super::agent_status::bus_up`], probed by the caller before its transaction opens); `current_release` is the
/// applied release (`jobs.current_release_id`, written by B2 apply) as a `ReleaseRef`; `draft_rev` and `languages` are
/// the columns.
pub async fn summary(conn: &mut PgConnection, j: &JobRow, on_bus: bool) -> ApiResult<Value> {
    let job = JobRef {
        id: j.id,
        key: j.key.clone(),
    };
    let current = crate::release::summary::current_ref(conn, &job).await?;
    // the job's region: `job_target.selection.region` (absent for a job without a target yet, or without a region)
    let region: Option<String> =
        sqlx::query_scalar("SELECT selection->>'region' FROM job_target WHERE job_id = $1")
            .bind(j.id)
            .fetch_optional(&mut *conn)
            .await?
            .flatten();
    Ok(json!({
        "key": j.key,
        "tenant_id": j.tenant_id,
        "label": j.label,
        "on_bus": on_bus,
        "settings": j.settings,
        "north_star": j.north_star,
        "pins": j.pins,
        "current_release": current,
        "draft_rev": j.draft_rev,
        "languages": j.languages,
        "audience": j.audience,
        "region": region,
    }))
}

/// The summaries of every job of the scoped tenant.
pub async fn summaries(conn: &mut PgConnection, on_bus: bool) -> ApiResult<Vec<Value>> {
    let rows = list(&mut *conn).await?;
    let mut out = Vec::with_capacity(rows.len());
    for j in &rows {
        out.push(summary(&mut *conn, j, on_bus).await?);
    }
    Ok(out)
}

/// The release lane of a job: `(current, waiting)` as the tenant home shows it. `current` = the applied release
/// (`{id, hash, released_at}`), `waiting` = the newest release when it is an approved cut not applied yet
/// (`{id, hash, approved_by, approved_at}`); `null` for each that does not exist.
pub async fn release_lane(conn: &mut PgConnection, job_id: Uuid) -> ApiResult<(Value, Value)> {
    let current: Option<(String, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT r.release_id, r.hash, r.released_at FROM jobs j \
           JOIN job_release r ON r.tenant_id = j.tenant_id AND r.job_id = j.id \
                             AND r.release_id = j.current_release_id \
          WHERE j.id = $1",
    )
    .bind(job_id)
    .fetch_optional(&mut *conn)
    .await?;
    let newest: Option<(String, String, String, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT release_id, hash, approved_by, approved_at, state FROM job_release \
          WHERE job_id = $1 ORDER BY approved_at DESC, release_id DESC LIMIT 1",
    )
    .bind(job_id)
    .fetch_optional(&mut *conn)
    .await?;
    let current = match current {
        Some((id, hash, at)) => json!({"id": id, "hash": hash, "released_at": at.map(iso)}),
        None => Value::Null,
    };
    let waiting = match newest {
        Some((id, hash, by, at, state)) if state == "approved" => {
            json!({"id": id, "hash": hash, "approved_by": by, "approved_at": iso(at)})
        }
        _ => Value::Null,
    };
    Ok((current, waiting))
}

/// X7 `jobs::board_extras`: readiness, the waiting release, the last release and the last 7 days of cases.
/// `readiness` is the draft's blocking readiness gaps (`missing` = their subjects); `live` stays `null`: the board
/// lists every job, and the per-job agent heartbeat read belongs to the job dashboard (`/api/jobs/{job}/status`).
pub async fn board_extras(conn: &mut PgConnection, st: &AppState, j: &JobRow) -> ApiResult<Value> {
    let job = JobRef {
        id: j.id,
        key: j.key.clone(),
    };
    let blocking = st
        .readiness
        .draft_blocking(&mut *conn, &st.design, st.compiler.as_ref(), &job)
        .await?;
    let missing: Vec<String> = blocking.iter().map(|i| i.subject.clone()).collect();
    let (current, waiting) = release_lane(&mut *conn, j.id).await?;
    let approved_waiting = if waiting.is_null() {
        Value::Null
    } else {
        json!({"id": waiting["id"], "hash": waiting["hash"]})
    };
    let w = analytics::window(&mut *conn, 7).await?;
    let o = analytics::outcomes(&mut *conn, j.id, w.from, w.to).await?;
    Ok(json!({
        "readiness": {"ready": missing.is_empty(), "missing": missing},
        "selection": null,
        "approved_waiting": approved_waiting,
        "last_release_at": current["released_at"],
        "cases_7d": o.cases,
        "containment_7d": ratio(o.contained, o.cases),
        "live": null,
    }))
}

/// X7 `jobs::on_job_created`: runs in the creating transaction. B1 audits `job.create`; B2
/// ([`design::on_job_created`]) inserts the job's `job_target` row with the template's default selection (or `selection_override`, the
/// tenant-admin create's company-profile selection) and takes the baseline snapshot; B3 adds the `store_ensure` rows.
pub async fn on_job_created(
    conn: &mut PgConnection,
    j: &JobRow,
    actor: &Actor,
    reg: &Registry,
    selection_override: Option<Selection>,
) -> ApiResult<()> {
    let template = match (&j.template_layer, j.template_version) {
        (Some(l), Some(v)) => Some(format!("{l}@v{v}")),
        _ => None,
    };
    let selection = match (selection_override, &j.template_layer, j.template_version) {
        (Some(s), _, _) => Some(s),
        (None, Some(layer), Some(version)) => {
            design::default_selection(&mut *conn, layer, version).await?
        }
        _ => None,
    };
    audit::record(
        &mut *conn,
        actor,
        Event::new("job.create", "job", j.key.clone())
            .job(&j.key)
            .detail("template", template)
            .detail("key", j.key.clone())
            .detail("label", j.label.clone())
            .detail("audience", j.audience.clone())
            .detail(
                "departments",
                selection.as_ref().map(|s| s.departments.clone()),
            )
            .detail(
                "by_role",
                if actor.platform {
                    "super_admin"
                } else {
                    "tenant_admin"
                },
            ),
    )
    .await?;
    // B3 T5: the tenant's stores not yet in strata_rag are queued (`store_ensure`, parents first); none on a new tenant
    crate::knowledge::stores::ensure_unbound(&mut *conn, j.tenant_id, actor).await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(j.pins.clone()).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!("jobs.pins of {} is unreadable: {e}", j.key))
    })?;
    let job = JobRef {
        id: j.id,
        key: j.key.clone(),
    };
    // B3 T10: the template's knowledge kinds become working rows before the baseline is taken
    crate::knowledge::kinds::materialise_from_template(&mut *conn, &job, &pins, selection.as_ref())
        .await?;
    design::on_job_created(conn, reg, &job, selection, &pins).await
}

/// A pin (`{layer, version}`) as stored in `pins` arrays.
fn pin_key(p: &Value) -> (Option<&str>, Option<i64>) {
    (p["layer"].as_str(), p["version"].as_i64())
}

/// The newest TAGGED template of the job's template layer whose version is above the job's: `(version, pins)`.
/// `None` for a job not created from a template, or when nothing newer is tagged.
pub async fn newest_tag(conn: &mut PgConnection, j: &JobRow) -> ApiResult<Option<(i32, Value)>> {
    let (Some(layer), Some(version)) = (&j.template_layer, j.template_version) else {
        return Ok(None);
    };
    Ok(sqlx::query_as(
        "SELECT version, pins FROM job_template_tag \
          WHERE layer = $1 AND version > $2 AND status = 'tagged' AND retired_at IS NULL \
          ORDER BY version DESC LIMIT 1",
    )
    .bind(layer)
    .bind(version)
    .fetch_optional(conn)
    .await?)
}

/// `JobBoardRow.newer`: the pins of the newest tagged template that the job does not pin yet.
pub async fn newer(conn: &mut PgConnection, j: &JobRow) -> ApiResult<Vec<Value>> {
    let Some((_, pins)) = newest_tag(conn, j).await? else {
        return Ok(Vec::new());
    };
    let current: Vec<(Option<&str>, Option<i64>)> = j
        .pins
        .as_array()
        .map(|a| a.iter().map(pin_key).collect())
        .unwrap_or_default();
    Ok(pins
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !current.iter().any(|c| *c == pin_key(p)))
        .collect())
}

/// `JobBoardRow` of a job of the scoped tenant: the summary, the X7 board extras, `newer` and the open upgrade request.
pub async fn board_row(
    conn: &mut PgConnection,
    st: &AppState,
    j: &JobRow,
    on_bus: bool,
) -> ApiResult<Value> {
    let mut row = board_extras(&mut *conn, st, j).await?;
    row["job"] = summary(&mut *conn, j, on_bus).await?;
    row["newer"] = Value::Array(newer(&mut *conn, j).await?);
    let requested: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT requested_at FROM template_upgrade_request WHERE job_id = $1 AND status = 'open'",
    )
    .bind(j.id)
    .fetch_optional(&mut *conn)
    .await?;
    row["upgrade_requested_at"] = json!(requested);
    Ok(row)
}
