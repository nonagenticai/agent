//! J12 context spaces and procedures (B5 T7, plan "Route table", R6/R12). A port of the fixture
//! `areas/ontology.ts` J12 methods (codes, messages, labels); versions are integers (R6).
//!
//! | Method | Route | Role | Ledger kind |
//! |---|---|---|---|
//! | contextSpaces | `GET /api/jobs/{job}/context-spaces` | member | — |
//! | saveContextSpaceRequired | B2's `PUT /api/jobs/{job}/context-spaces/{key}` `{required}` (`routes/release/refine.rs`) | job_designer | `space_required` |
//! | procedures | `GET /api/jobs/{job}/procedures` | member | — |
//! | procedureVersions | `GET /api/jobs/{job}/procedures/{name}` | member | — |
//! | createProcedureVersion | `POST /api/jobs/{job}/procedures` | job_designer | `procedure` |
//! | approveProcedure | `POST …/procedures/{name}/versions/{v}/approve` | release_manager | `procedure` |
//! | setProcedureStatus | `POST …/procedures/{name}/versions/{v}/status` `{to}` | job_designer; → PUBLISHED: release_manager | `procedure` |
//!
//! - Versions: the tenant's (`procedure_version`, the `procedure` kind) and the base's (the compiled `procedure` rows
//!   `name@N` at the design's pins: PUBLISHED, read-only, no step bodies, `origin: base`). A new tenant version is
//!   above every base and tenant version of its name (409 `version_exists`).
//! - Each row is the DRAFT version with the effective flags: `effective` per the LIVE design (what production runs),
//!   `effective_draft` per the draft (what the test chat runs), `live_status`, `shadowed` (a base PUBLISHED version a
//!   tenant PUBLISHED version of the name replaces), `activity_grant` (the operations of the space's activity, T4's
//!   folded activity operations) and `outside_grant` (step operations no grant entry covers).
//! - D88: approving is an optional review mark (one reviewer is enough, none is required); publishing is the
//!   `release_manager`'s (fixture `setProcedureStatus`: not the designer's), every other move the `job_designer`'s.
//!   There is no reviewer count.
//! - A refused transition (409 `base_version`, `invalid_transition`, `operation_outside_grant`) appends its
//!   `ok:false` history row in its own committed statement before the 409 (plan "Global constraints").
//! - `history` is `procedure_history` (append-only, not design); `runs_7d` / `success_rate` count the job's
//!   Production cases of the version over 7 days (`resolved` + `not_eligible` succeed), `test_runs` its latest Test
//!   cases (`agent_case`).
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{body, job_ref};
use crate::auth::Role;
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::job_design::base::Bases;
use crate::job_design::context_space::{self, EffSpace};
use crate::job_design::keys::is_snake_name;
use crate::job_design::types::{
    CatalogueOperation, Condition, ContextSpace, HistoryAction, Origin, Paged,
    ProcedureHistoryEntry, ProcedureStep, ProcedureTestRun, ProcedureVersion,
    ProcedureVersionInput, ProcedureView, TestRunOutcome, VersionStatus,
};
use crate::job_design::{catalogue_with_builtins, grant_allows, op_label};
use crate::runtime::conditions::{self, ConditionError};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/context-spaces", get(context_spaces))
        .route(
            "/api/jobs/{job}/procedures",
            get(procedures).post(create_procedure_version),
        )
        .route("/api/jobs/{job}/procedures/{name}", get(procedure_versions))
        .route(
            "/api/jobs/{job}/procedures/{name}/versions/{v}/approve",
            post(approve_procedure),
        )
        .route(
            "/api/jobs/{job}/procedures/{name}/versions/{v}/status",
            post(set_procedure_status),
        )
}

const AREA: &str = "procedures";
const PAGE: &str = "J12";
/// `procedures` returns at most this many rows (fixture `paged(rows, {limit: 200})`).
pub const LIST_LIMIT: usize = 200;
/// Test runs shown per version.
pub const TEST_RUNS_PER_VERSION: usize = 10;
/// Status order of the list (fixture `STATUSES`).
const STATUSES: [VersionStatus; 5] = [
    VersionStatus::Draft,
    VersionStatus::Canary,
    VersionStatus::Published,
    VersionStatus::Archived,
    VersionStatus::Discarded,
];

/// Allowed status moves (fixture `NEXT_STATUS`, plan 05 procedure_management `allowed`).
pub fn next_status(from: VersionStatus) -> &'static [VersionStatus] {
    use VersionStatus::*;
    match from {
        Draft => &[Canary, Discarded],
        Canary => &[Published, Discarded],
        Published => &[Archived],
        Archived => &[Published],
        Discarded => &[],
    }
}

/// The wire / column text of a status.
pub fn status_text(s: VersionStatus) -> &'static str {
    match s {
        VersionStatus::Draft => "DRAFT",
        VersionStatus::Canary => "CANARY",
        VersionStatus::Published => "PUBLISHED",
        VersionStatus::Archived => "ARCHIVED",
        VersionStatus::Discarded => "DISCARDED",
    }
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn status_of(s: &str) -> ApiResult<VersionStatus> {
    serde_json::from_value(Value::String(s.to_owned()))
        .map_err(|e| internal(format!("procedure status {s} is unknown: {e}")))
}

fn status_rank(s: VersionStatus) -> usize {
    STATUSES
        .iter()
        .position(|x| *x == s)
        .unwrap_or(STATUSES.len())
}

fn place(name: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("name".to_owned(), Value::String(name.to_owned()));
    m
}

fn not_found(name: &str, version: &str) -> ApiError {
    ApiError::Missing(
        "procedure_not_found",
        format!("No version {version} of {name}."),
    )
}

// ---- the versions of a design ---------------------------------------------------------------------------------------

/// One version (a tenant row or a base version).
#[derive(Clone, Debug)]
struct Ver {
    name: String,
    version: u32,
    status: VersionStatus,
    origin: Origin,
    context_space: String,
    description: String,
    reviewers: Vec<String>,
    steps: Vec<ProcedureStep>,
    released_in: Option<String>,
    updated_at: DateTime<Utc>,
    success_criteria: Vec<Condition>,
    tool_requirements: Vec<String>,
    pddl_domain: Option<String>,
}

#[derive(sqlx::FromRow)]
struct VersionRow {
    name: String,
    version: i32,
    status: String,
    context_space: String,
    description: String,
    reviewers: Vec<String>,
    steps: Value,
    released_in: Option<String>,
    updated_at: DateTime<Utc>,
    success_criteria: Value,
    tool_requirements: Vec<String>,
    pddl_domain: Option<String>,
}

impl VersionRow {
    fn ver(self) -> ApiResult<Ver> {
        let steps: Vec<ProcedureStep> = serde_json::from_value(self.steps).map_err(|e| {
            internal(format!(
                "procedure_version {}@{} steps are unreadable: {e}",
                self.name, self.version
            ))
        })?;
        let success_criteria: Vec<Condition> = serde_json::from_value(self.success_criteria)
            .map_err(|e| {
                internal(format!(
                    "procedure_version {}@{} success_criteria are unreadable: {e}",
                    self.name, self.version
                ))
            })?;
        Ok(Ver {
            status: status_of(&self.status)?,
            version: u32::try_from(self.version).unwrap_or(0),
            name: self.name,
            origin: Origin::Tenant,
            context_space: self.context_space,
            description: self.description,
            reviewers: self.reviewers,
            steps,
            released_in: self.released_in,
            updated_at: self.updated_at,
            success_criteria,
            tool_requirements: self.tool_requirements,
            pddl_domain: self.pddl_domain,
        })
    }
}

const VERSION_COLUMNS: &str = "name, version, status, context_space, description, reviewers, steps, released_in, \
     updated_at, success_criteria, tool_requirements, pddl_domain";

/// The job's tenant versions (the working rows = the draft), by name and version.
async fn tenant_versions(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<Ver>> {
    let rows = sqlx::query_as::<_, VersionRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {VERSION_COLUMNS} FROM procedure_version WHERE job_id = $1 ORDER BY name, version"
    )))
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    rows.into_iter().map(VersionRow::ver).collect()
}

/// One tenant version, if the job has it.
async fn tenant_version(
    c: &mut PgConnection,
    job: Uuid,
    name: &str,
    version: u32,
) -> ApiResult<Option<Ver>> {
    let row = sqlx::query_as::<_, VersionRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {VERSION_COLUMNS} FROM procedure_version WHERE job_id = $1 AND name = $2 AND version = $3"
    )))
    .bind(job)
    .bind(name)
    .bind(i32::try_from(version).unwrap_or(i32::MAX))
    .fetch_optional(&mut *c)
    .await?;
    row.map(VersionRow::ver).transpose()
}

/// The base versions of `bases` (`name@N` compiled rows: PUBLISHED; step bodies, criteria and requirements as
/// `TemplateProcedure::from_item` reads them, so what the design shows is what the orchestrator runs).
fn base_versions(bases: &Bases) -> Vec<Ver> {
    bases
        .procedures
        .iter()
        .map(|(key, f)| {
            let (name, version) = match key.rsplit_once('@') {
                Some((n, v)) => (n, v.parse().unwrap_or(1)),
                None => (key.as_str(), 1),
            };
            let text = |k: &str| {
                f.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Ver {
                name: name.to_owned(),
                version,
                status: VersionStatus::Published,
                origin: Origin::Base,
                context_space: text("context_space"),
                description: text("label"),
                reviewers: Vec::new(),
                steps: crate::runtime::spaces::base_steps(key, f),
                released_in: None,
                updated_at: DateTime::UNIX_EPOCH,
                success_criteria: crate::runtime::spaces::base_criteria(f),
                tool_requirements: crate::runtime::spaces::base_requirements(f),
                pddl_domain: None,
            }
        })
        .collect()
}

/// A version as the LIVE design has it (only what the flags read).
#[derive(Clone, Debug)]
struct LiveVer {
    name: String,
    version: u32,
    status: VersionStatus,
    origin: Origin,
}

#[derive(Deserialize)]
struct LiveVersion {
    version: u32,
    status: VersionStatus,
}

/// The LIVE versions: the live `procedure` places (tenant) + the base versions at the live pins.
async fn live_versions(
    c: &mut PgConnection,
    st: &AppState,
    jr: &JobRef,
) -> ApiResult<Vec<LiveVer>> {
    let mut out = Vec::new();
    for row in design::values_of_kind(&mut *c, &st.design, jr, &DesignOf::Live, "procedure").await?
    {
        let Some(name) = row.r#ref.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(v) = row.snap.filter(|v| !v.is_null()) else {
            continue;
        };
        let versions: Vec<LiveVersion> = serde_json::from_value(v)
            .map_err(|e| internal(format!("live procedure {name} is unreadable: {e}")))?;
        out.extend(versions.into_iter().map(|x| LiveVer {
            name: name.to_owned(),
            version: x.version,
            status: x.status,
            origin: Origin::Tenant,
        }));
    }
    let bases = crate::job_design::base::at(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        jr,
        &DesignOf::Live,
    )
    .await?;
    out.extend(base_versions(&bases).into_iter().map(|b| LiveVer {
        name: b.name,
        version: b.version,
        status: b.status,
        origin: Origin::Base,
    }));
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    name: String,
    version: i32,
    at: DateTime<Utc>,
    actor: String,
    action: String,
    from_status: Option<String>,
    to_status: Option<String>,
    ok: bool,
    reason: Option<String>,
}

type VersionKey = (String, u32);

async fn history(
    c: &mut PgConnection,
    job: Uuid,
) -> ApiResult<BTreeMap<VersionKey, Vec<ProcedureHistoryEntry>>> {
    let rows = sqlx::query_as::<_, HistoryRow>(
        "SELECT name, version, at, actor, action, from_status, to_status, ok, reason \
           FROM procedure_history WHERE job_id = $1 ORDER BY at, id",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    let mut out: BTreeMap<VersionKey, Vec<ProcedureHistoryEntry>> = BTreeMap::new();
    for r in rows {
        let action: HistoryAction = serde_json::from_value(Value::String(r.action.clone()))
            .map_err(|e| internal(format!("procedure_history action {}: {e}", r.action)))?;
        let entry = ProcedureHistoryEntry {
            at: r.at,
            actor: r.actor,
            action,
            from: r.from_status.as_deref().map(status_of).transpose()?,
            to: r.to_status.as_deref().map(status_of).transpose()?,
            ok: r.ok,
            reason: r.reason,
        };
        out.entry((r.name, u32::try_from(r.version).unwrap_or(0)))
            .or_default()
            .push(entry);
    }
    Ok(out)
}

/// Production runs over 7 days per version: `(runs, succeeded)`.
async fn prod_runs(c: &mut PgConnection, job: Uuid) -> ApiResult<BTreeMap<VersionKey, (u64, u64)>> {
    let rows: Vec<(String, i32, i64, i64)> = sqlx::query_as(
        "SELECT procedure_name, procedure_version, count(*), \
                count(*) FILTER (WHERE status IN ('resolved', 'not_eligible')) \
           FROM agent_case \
          WHERE job_id = $1 AND env = 'prod' AND started_at > now() - interval '7 days' \
            AND procedure_name IS NOT NULL AND procedure_version IS NOT NULL \
          GROUP BY procedure_name, procedure_version",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(n, v, runs, ok)| {
            (
                (n, u32::try_from(v).unwrap_or(0)),
                (
                    u64::try_from(runs).unwrap_or(0),
                    u64::try_from(ok).unwrap_or(0),
                ),
            )
        })
        .collect())
}

/// A finished Test case as a test run; `None` while it is still open.
fn test_outcome(status: &str) -> Option<TestRunOutcome> {
    match status {
        "resolved" => Some(TestRunOutcome::Resolved),
        "not_eligible" => Some(TestRunOutcome::NotEligible),
        "handover" => Some(TestRunOutcome::HandedOver),
        "failed" | "cancelled_offboard" => Some(TestRunOutcome::Refused),
        _ => None,
    }
}

#[derive(sqlx::FromRow)]
struct TestCaseRow {
    procedure_name: String,
    procedure_version: i32,
    started_at: DateTime<Utc>,
    initiator_sub: Option<String>,
    title: String,
    status: String,
    outcome: Option<String>,
}

/// The latest finished Test cases per version (newest first).
async fn test_runs(
    c: &mut PgConnection,
    job: Uuid,
) -> ApiResult<BTreeMap<VersionKey, Vec<ProcedureTestRun>>> {
    let rows = sqlx::query_as::<_, TestCaseRow>(
        "SELECT procedure_name, procedure_version, started_at, initiator_sub, title, status, outcome \
           FROM agent_case \
          WHERE job_id = $1 AND env = 'test' \
            AND procedure_name IS NOT NULL AND procedure_version IS NOT NULL \
          ORDER BY started_at DESC LIMIT 1000",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    let mut out: BTreeMap<VersionKey, Vec<ProcedureTestRun>> = BTreeMap::new();
    for r in rows {
        let Some(o) = test_outcome(&r.status) else {
            continue;
        };
        let runs = out
            .entry((
                r.procedure_name,
                u32::try_from(r.procedure_version).unwrap_or(0),
            ))
            .or_default();
        if runs.len() < TEST_RUNS_PER_VERSION {
            runs.push(ProcedureTestRun {
                at: r.started_at,
                actor: r.initiator_sub.unwrap_or_default(),
                utterance: r.title,
                outcome: o,
                reached_step: 0,
                note: r.outcome.unwrap_or_default(),
            });
        }
    }
    Ok(out)
}

/// Everything the J12 procedure views need.
struct Design {
    tenant: String,
    job: String,
    bases: Arc<Bases>,
    spaces: Vec<EffSpace>,
    /// The draft: tenant versions + the base versions at the draft's pins.
    all: Vec<Ver>,
    live: Vec<LiveVer>,
    history: BTreeMap<VersionKey, Vec<ProcedureHistoryEntry>>,
    runs: BTreeMap<VersionKey, (u64, u64)>,
    tests: BTreeMap<VersionKey, Vec<ProcedureTestRun>>,
}

/// The draft's bases, spaces and versions (what a write validates against).
async fn draft_parts(
    c: &mut PgConnection,
    st: &AppState,
    jr: &JobRef,
) -> ApiResult<(Arc<Bases>, Vec<EffSpace>, Vec<Ver>)> {
    let (bases, spaces) = context_space::of_design(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        jr,
        &DesignOf::Draft,
    )
    .await?;
    let mut all = tenant_versions(&mut *c, jr.id).await?;
    all.extend(base_versions(&bases));
    Ok((bases, spaces, all))
}

async fn load(c: &mut PgConnection, st: &AppState, j: &JobCtx) -> ApiResult<Design> {
    let jr = job_ref(j);
    let (bases, spaces, all) = draft_parts(&mut *c, st, &jr).await?;
    Ok(Design {
        tenant: j.t.tenant.to_string(),
        job: j.job_key.clone(),
        bases,
        spaces,
        all,
        live: live_versions(&mut *c, st, &jr).await?,
        history: history(&mut *c, jr.id).await?,
        runs: prod_runs(&mut *c, jr.id).await?,
        tests: test_runs(&mut *c, jr.id).await?,
    })
}

/// Fixture `shadowedIn`: a base PUBLISHED version of a name that also has a tenant PUBLISHED version.
fn shadowed<'a>(
    name: &str,
    origin: Origin,
    status: VersionStatus,
    mut all: impl Iterator<Item = (&'a str, Origin, VersionStatus)>,
) -> bool {
    origin == Origin::Base
        && status == VersionStatus::Published
        && all.any(|(n, o, s)| n == name && o == Origin::Tenant && s == VersionStatus::Published)
}

/// The grant of version `p`'s space and the step operations it does not cover (fixture `view`).
fn grant_of(bases: &Bases, spaces: &[EffSpace], p: &Ver) -> (Vec<String>, Vec<String>) {
    let space = spaces.iter().find(|s| s.key == p.context_space);
    let grant = context_space::activity_ops(bases, space);
    let mut outside: Vec<String> = Vec::new();
    for s in &p.steps {
        let label = op_label(&s.operation, s.variant.as_deref());
        if !outside.contains(&label) && !grant_allows(&grant, &label) {
            outside.push(label);
        }
    }
    (grant, outside)
}

impl Design {
    fn view(&self, p: &Ver) -> ProcedureView {
        let (activity_grant, outside_grant) = grant_of(&self.bases, &self.spaces, p);
        let is_shadowed = shadowed(
            &p.name,
            p.origin,
            p.status,
            self.all
                .iter()
                .map(|x| (x.name.as_str(), x.origin, x.status)),
        );
        let lp = self
            .live
            .iter()
            .find(|x| x.name == p.name && x.version == p.version);
        let effective = lp.is_some_and(|l| {
            l.status == VersionStatus::Published
                && !shadowed(
                    &l.name,
                    l.origin,
                    l.status,
                    self.live
                        .iter()
                        .map(|x| (x.name.as_str(), x.origin, x.status)),
                )
        });
        let key = (p.name.clone(), p.version);
        let (runs, ok) = self.runs.get(&key).copied().unwrap_or((0, 0));
        ProcedureView {
            version: ProcedureVersion {
                tenant_id: self.tenant.clone(),
                job: self.job.clone(),
                name: p.name.clone(),
                version: p.version,
                status: p.status,
                origin: p.origin,
                context_space: p.context_space.clone(),
                description: p.description.clone(),
                reviewers: p.reviewers.clone(),
                steps: p.steps.clone(),
                history: self.history.get(&key).cloned().unwrap_or_default(),
                updated_at: p.updated_at,
                runs_7d: runs,
                success_rate: if runs > 0 {
                    Some(ok as f64 / runs as f64)
                } else {
                    None
                },
                released_in: p.released_in.clone(),
                test_runs: self.tests.get(&key).cloned().unwrap_or_default(),
                success_criteria: p.success_criteria.clone(),
                tool_requirements: p.tool_requirements.clone(),
                pddl_domain: p.pddl_domain.clone(),
            },
            shadowed: is_shadowed,
            effective,
            effective_draft: p.status == VersionStatus::Published && !is_shadowed,
            live_status: lp.map(|l| l.status),
            outside_grant,
            activity_grant,
        }
    }

    fn find(&self, name: &str, version: u32) -> Option<&Ver> {
        self.all
            .iter()
            .find(|x| x.name == name && x.version == version)
    }

    /// Version `name@version` as the draft has it now, as a view.
    fn view_of(&self, name: &str, version: u32) -> ApiResult<ProcedureView> {
        self.find(name, version)
            .map(|p| self.view(p))
            .ok_or_else(|| internal(format!("{name}@{version} is gone after its own write")))
    }
}

// ---- reads ----------------------------------------------------------------------------------------------------------

async fn context_spaces(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Paged<ContextSpace>>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let items = context_space::list(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &jr,
        &j.t.tenant.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(Paged { items, next: None }))
}

async fn procedures(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Paged<ProcedureView>>> {
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &job_ref(&j)).await?;
    let d = load(&mut tx, &st, &j).await?;
    tx.commit().await?;
    let mut rows: Vec<ProcedureView> = d.all.iter().map(|p| d.view(p)).collect();
    rows.sort_by(|a, b| {
        a.version
            .name
            .cmp(&b.version.name)
            .then(status_rank(a.version.status).cmp(&status_rank(b.version.status)))
            .then(b.version.version.cmp(&a.version.version))
    });
    let next = (rows.len() > LIST_LIMIT).then(|| LIST_LIMIT.to_string());
    rows.truncate(LIST_LIMIT);
    Ok(Json(Paged { items: rows, next }))
}

async fn procedure_versions(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, name)): ApiPath<(String, String)>,
) -> ApiResult<Json<Vec<ProcedureView>>> {
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &job_ref(&j)).await?;
    let d = load(&mut tx, &st, &j).await?;
    tx.commit().await?;
    let mut rows: Vec<ProcedureView> = d
        .all
        .iter()
        .filter(|p| p.name == name)
        .map(|p| d.view(p))
        .collect();
    if rows.is_empty() {
        return Err(ApiError::Missing(
            "procedure_not_found",
            format!("This job has no procedure “{name}”."),
        ));
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.version.version));
    Ok(Json(rows))
}

// ---- createProcedureVersion ---------------------------------------------------------------------------------------

/// A version number on the wire: a JSON integer from 1 (fits `procedure_version.version integer`).
fn version_of(v: &Value) -> Option<u32> {
    v.as_u64()
        .filter(|n| (1..=i32::MAX as u64).contains(n))
        .map(|n| n as u32)
}

/// The catalogue row a step names: its exact variant, else the operation's own row, else any row of the operation
/// (fixture `catalogueOp(s.operation)`).
fn catalogue_op<'a>(
    ops: &'a [CatalogueOperation],
    op: &str,
    variant: Option<&str>,
) -> Option<&'a CatalogueOperation> {
    let variant = variant.filter(|v| !v.is_empty());
    ops.iter()
        .find(|o| o.op == op && o.variant.as_deref() == variant)
        .or_else(|| ops.iter().find(|o| o.op == op && o.variant.is_none()))
        .or_else(|| ops.iter().find(|o| o.op == op))
}

// ---- T7a: step conditions, per-case cap, success criteria (F-15, F-20) -----------------------------------------------

/// The longest `pddl_domain` (bytes), as the 0147 CHECK.
pub const MAX_PDDL_BYTES: usize = 64 * 1024;

/// `performed_by` names one of the job's agent roles (422 `unknown_role`); `max_per_case` is set on X/P steps only
/// and is 1..=20 (422 `invalid_cap`, B4 `runner::cap`). `steps` carry the catalogue's side effect already.
fn check_steps(steps: &[ProcedureStep], roles: &[&str]) -> ApiResult<()> {
    use agent_model::kinds::catalog::SideEffect;
    let max = u32::from(crate::runtime::runner::cap::MAX_CAP);
    for s in steps {
        if let Some(role) = s.performed_by.as_deref()
            && !roles.contains(&role)
        {
            return Err(ApiError::Unprocessable(
                "unknown_role",
                format!(
                    "Step {} names role {role}, which this job does not have.",
                    s.step_number
                ),
            )
            .with("step", s.step_number));
        }
        let Some(cap) = s.max_per_case else {
            continue;
        };
        if !matches!(s.side_effect, SideEffect::X | SideEffect::P) {
            return Err(ApiError::Unprocessable(
                "invalid_cap",
                format!(
                    "Step {} reads or writes ({}): only X and P steps take a per-case cap.",
                    s.step_number, s.operation
                ),
            )
            .with("step", s.step_number));
        }
        if !(1..=max).contains(&cap) {
            return Err(ApiError::Unprocessable(
                "invalid_cap",
                format!(
                    "Step {}: the per-case cap is a whole number from 1 to {max}.",
                    s.step_number
                ),
            )
            .with("step", s.step_number));
        }
    }
    Ok(())
}

/// The condition id a validation error names.
fn condition_of(e: &ConditionError) -> &str {
    match e {
        ConditionError::BadId(id)
        | ConditionError::DuplicateId(id)
        | ConditionError::UnknownPath { id, .. }
        | ConditionError::ForwardStepReference { id, .. }
        | ConditionError::BadValue { id } => id,
    }
}

/// 422 `invalid_condition` with `step` (the step number, null for a success criterion), `list` and `condition`.
fn invalid_condition(step: Option<u32>, list: &str, e: &ConditionError) -> ApiError {
    let at = match step {
        Some(n) => format!("Step {n} {list}"),
        None => "Success criteria".to_owned(),
    };
    ApiError::Unprocessable("invalid_condition", format!("{at}: {e}."))
        .with("step", step.map_or(Value::Null, Value::from))
        .with("list", list)
        .with("condition", condition_of(e))
}

/// B4's validator ([`conditions::validate`]) over every list: a precondition reads the steps of EARLIER phases (the
/// runner checks it before the call), a postcondition those plus its own step (after the answer), a success
/// criterion every step (after the last phase).
fn check_conditions(steps: &[ProcedureStep], success: &[Condition]) -> ApiResult<()> {
    for s in steps {
        let before: Vec<&str> = steps
            .iter()
            .filter(|o| o.phase < s.phase)
            .map(|o| o.name.as_str())
            .collect();
        conditions::validate(&s.preconditions, &before)
            .map_err(|e| invalid_condition(Some(s.step_number), "preconditions", &e))?;
        let mut after = before;
        after.push(s.name.as_str());
        conditions::validate(&s.postconditions, &after)
            .map_err(|e| invalid_condition(Some(s.step_number), "postconditions", &e))?;
    }
    let all: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    conditions::validate(success, &all).map_err(|e| invalid_condition(None, "success_criteria", &e))
}

/// `object.verb` / `object.verb{variant}` (the `toolset_narrowing.op` shape): `(op, variant)`.
fn parse_tool(key: &str) -> Option<(&str, Option<&str>)> {
    let (op, variant) = match key.split_once('{') {
        Some((op, rest)) => {
            let v = rest.strip_suffix('}')?;
            if v.is_empty() || v.contains(['{', '}', '\n']) {
                return None;
            }
            (op, Some(v))
        }
        None => (key, None),
    };
    let (object, verb) = op.split_once('.')?;
    let ident = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    (ident(object) && ident(verb)).then_some((op, variant))
}

/// Every tool requirement is a catalogue operation (422 `unknown_operation`); duplicates collapse, order kept.
fn check_tool_requirements(
    catalogue: &[CatalogueOperation],
    tools: Vec<String>,
) -> ApiResult<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(tools.len());
    for t in tools {
        let known = parse_tool(&t).filter(|(op, v)| catalogue_op(catalogue, op, *v).is_some());
        let Some((op, variant)) = known else {
            return Err(ApiError::Unprocessable(
                "unknown_operation",
                format!("Needed tool {t} is not an operation from the catalogue."),
            )
            .with("tool", t.as_str()));
        };
        let label = op_label(op, variant);
        if !out.contains(&label) {
            out.push(label);
        }
    }
    Ok(out)
}

/// The PDDL domain is stored verbatim, never parsed: only its size is checked (422 `invalid_pddl`).
fn check_pddl(pddl: Option<&str>) -> ApiResult<()> {
    match pddl {
        Some(p) if p.len() > MAX_PDDL_BYTES => Err(ApiError::Unprocessable(
            "invalid_pddl",
            format!("A PDDL domain is at most {} KiB.", MAX_PDDL_BYTES / 1024),
        )),
        _ => Ok(()),
    }
}

/// One `procedure_history` row.
struct Hist<'a> {
    name: &'a str,
    version: u32,
    actor: &'a str,
    action: &'a str,
    from: Option<VersionStatus>,
    to: Option<VersionStatus>,
    ok: bool,
    reason: Option<&'a str>,
}

async fn insert_history(c: &mut PgConnection, h: Hist<'_>) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO procedure_history (name, version, actor, action, from_status, to_status, ok, reason) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(h.name)
    .bind(i32::try_from(h.version).unwrap_or(i32::MAX))
    .bind(h.actor)
    .bind(h.action)
    .bind(h.from.map(status_text))
    .bind(h.to.map(status_text))
    .bind(h.ok)
    .bind(h.reason)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The new DRAFT version and its `create` history row (the `mutate` of the tracked write).
async fn insert_version(c: &mut PgConnection, job: Uuid, v: Ver, actor: String) -> ApiResult<()> {
    let steps = serde_json::to_value(&v.steps)
        .map_err(|e| internal(format!("procedure steps do not encode: {e}")))?;
    let success = serde_json::to_value(&v.success_criteria)
        .map_err(|e| internal(format!("procedure success criteria do not encode: {e}")))?;
    sqlx::query(
        "INSERT INTO procedure_version \
                (job_id, name, version, status, context_space, description, reviewers, steps, created_by, \
                 success_criteria, tool_requirements, pddl_domain) \
         VALUES ($1, $2, $3, 'DRAFT', $4, $5, '{}', $6, $7, $8, $9, $10)",
    )
    .bind(job)
    .bind(&v.name)
    .bind(i32::try_from(v.version).unwrap_or(i32::MAX))
    .bind(&v.context_space)
    .bind(&v.description)
    .bind(&steps)
    .bind(&actor)
    .bind(&success)
    .bind(&v.tool_requirements)
    .bind(&v.pddl_domain)
    .execute(&mut *c)
    .await?;
    insert_history(
        &mut *c,
        Hist {
            name: &v.name,
            version: v.version,
            actor: &actor,
            action: "create",
            from: None,
            to: Some(VersionStatus::Draft),
            ok: true,
            reason: None,
        },
    )
    .await
}

async fn create_procedure_version(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(mut raw): ApiJson<Value>,
) -> ApiResult<Json<ProcedureView>> {
    // R6: the version is an integer; anything else is 422 `invalid_version` (after the name check, as the fixture)
    let version = raw.get("version").and_then(version_of);
    if let Some(o) = raw.as_object_mut()
        && o.contains_key("version")
    {
        o.insert("version".to_owned(), json!(version.unwrap_or(1)));
    }
    let i: ProcedureVersionInput = body(raw)?;
    if !is_snake_name(&i.name) {
        return Err(ApiError::Unprocessable(
            "invalid_name",
            "A procedure name is snake_case, e.g. return.".into(),
        ));
    }
    let Some(version) = version else {
        return Err(ApiError::Unprocessable(
            "invalid_version",
            "A version is a whole number from 1, e.g. 3.".into(),
        ));
    };
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    lock_job(&mut tx, jr.id).await?;
    let (bases, spaces, all) = draft_parts(&mut tx, &st, &jr).await?;
    if !spaces.iter().any(|s| s.key == i.context_space) {
        return Err(ApiError::Unprocessable(
            "unknown_context_space",
            format!("No context space {}.", i.context_space),
        ));
    }
    let top = all
        .iter()
        .filter(|p| p.name == i.name)
        .map(|p| p.version)
        .max();
    if all.iter().any(|p| p.name == i.name && p.version == version) {
        return Err(ApiError::Conflict(
            "version_exists",
            format!("{}@{version} already exists.", i.name),
        ));
    }
    if let Some(top) = top.filter(|t| version <= *t) {
        return Err(ApiError::Conflict(
            "version_exists",
            format!(
                "{} is at version {top}; a new version is {} or higher.",
                i.name,
                top + 1
            ),
        ));
    }
    if i.steps.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_steps",
            "A procedure needs at least one step.".into(),
        ));
    }
    let catalogue = catalogue_with_builtins(&bases.operations);
    let mut steps = Vec::with_capacity(i.steps.len());
    for (n, s) in i.steps.into_iter().enumerate() {
        let Some(cat) = catalogue_op(&catalogue, &s.operation, s.variant.as_deref()) else {
            return Err(ApiError::Unprocessable(
                "unknown_operation",
                "Every step names an operation from the catalogue.".into(),
            ));
        };
        steps.push(ProcedureStep {
            step_number: u32::try_from(n + 1).unwrap_or(u32::MAX),
            modality: cat.modality,
            side_effect: cat.side_effect,
            identity_level: cat.identity_level.clone(),
            confirm_back: cat.side_effect != agent_model::kinds::catalog::SideEffect::R,
            ..s
        });
    }
    // T7a (F-15, F-20): roles, caps, conditions, success criteria, tool requirements, PDDL
    let roles: Vec<&str> = bases.agent_roles.iter().map(|r| r.key.as_str()).collect();
    check_steps(&steps, &roles)?;
    check_conditions(&steps, &i.success_criteria)?;
    let tool_requirements = check_tool_requirements(&catalogue, i.tool_requirements)?;
    check_pddl(i.pddl_domain.as_deref())?;
    let actor = j.t.caller.sub.clone();
    let v = Ver {
        name: i.name.clone(),
        version,
        status: VersionStatus::Draft,
        origin: Origin::Tenant,
        context_space: i.context_space,
        description: i.description.trim().to_owned(),
        reviewers: Vec::new(),
        steps,
        released_in: None,
        updated_at: Utc::now(),
        success_criteria: i.success_criteria,
        tool_requirements,
        pddl_domain: i.pddl_domain,
    };
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "procedure",
        r#ref: place(&i.name),
        label: Label::Now(format!("{}@{version} drafted", i.name)),
    };
    let job = jr.id;
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(insert_version(c, job, v, actor))
    })
    .await?;
    let d = load(&mut tx, &st, &j).await?;
    let out = d.view_of(&i.name, version)?;
    tx.commit().await?;
    Ok(Json(out))
}

/// Lock the job row (as `design::track` does) so the checks and the tracked write see one state.
async fn lock_job(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// Version `v` of `name` in the draft: the tenant's row, else a base version; 404 `procedure_not_found`.
async fn find_version(
    c: &mut PgConnection,
    st: &AppState,
    jr: &JobRef,
    name: &str,
    v: &str,
) -> ApiResult<(Ver, Arc<Bases>, Vec<EffSpace>)> {
    let Ok(version) = v.parse::<u32>() else {
        return Err(not_found(name, v));
    };
    let (bases, spaces, all) = draft_parts(&mut *c, st, jr).await?;
    let p = match tenant_version(&mut *c, jr.id, name, version).await? {
        Some(p) => p,
        None => all
            .iter()
            .find(|x| x.name == name && x.version == version && x.origin == Origin::Base)
            .cloned()
            .ok_or_else(|| not_found(name, v))?,
    };
    Ok((p, bases, spaces))
}

// ---- approveProcedure ---------------------------------------------------------------------------------------------

/// The caller as a person reads them (fixture `nameOf`): their display name, else their username.
async fn name_of(c: &mut PgConnection, j: &JobCtx) -> ApiResult<String> {
    let name: Option<Option<String>> =
        sqlx::query_scalar("SELECT display_name FROM users WHERE sub = $1")
            .bind(&j.t.caller.sub)
            .fetch_optional(&mut *c)
            .await?;
    Ok(name
        .flatten()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| j.t.caller.actor.clone()))
}

/// The caller joins the reviewers, with an `approve` history row (the `mutate` of the tracked write).
async fn add_reviewer(
    c: &mut PgConnection,
    job: Uuid,
    name: String,
    version: u32,
    actor: String,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE procedure_version \
            SET reviewers = array_append(reviewers, $4), updated_at = clock_timestamp() \
          WHERE job_id = $1 AND name = $2 AND version = $3",
    )
    .bind(job)
    .bind(&name)
    .bind(i32::try_from(version).unwrap_or(i32::MAX))
    .bind(&actor)
    .execute(&mut *c)
    .await?;
    insert_history(
        &mut *c,
        Hist {
            name: &name,
            version,
            actor: &actor,
            action: "approve",
            from: None,
            to: None,
            ok: true,
            reason: None,
        },
    )
    .await
}

async fn approve_procedure(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiPath((_job, name, v)): ApiPath<(String, String, String)>,
) -> ApiResult<Json<ProcedureView>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    lock_job(&mut tx, jr.id).await?;
    let (p, ..) = find_version(&mut tx, &st, &jr, &name, &v).await?;
    if p.origin == Origin::Base {
        return Err(ApiError::Conflict(
            "base_version",
            "Base versions are reviewed when the template is tagged.".into(),
        ));
    }
    if p.status != VersionStatus::Canary {
        return Err(ApiError::Conflict(
            "not_canary",
            "Only a CANARY version can be approved.".into(),
        ));
    }
    let actor = j.t.caller.sub.clone();
    if p.reviewers.contains(&actor) {
        return Err(ApiError::Conflict(
            "already_reviewed",
            "You already approved this version.".into(),
        ));
    }
    let who = name_of(&mut tx, &j).await?;
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "procedure",
        r#ref: place(&name),
        label: Label::Now(format!("{name}@{} approved by {who}", p.version)),
    };
    let (job, n, version) = (jr.id, name.clone(), p.version);
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(add_reviewer(c, job, n, version, actor))
    })
    .await?;
    let d = load(&mut tx, &st, &j).await?;
    let out = d.view_of(&name, p.version)?;
    tx.commit().await?;
    Ok(Json(out))
}

// ---- setProcedureStatus -------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusInput {
    to: VersionStatus,
}

/// Move `name@version` from `from` to `to`; → PUBLISHED archives the name's other tenant PUBLISHED version first
/// (`replaced by <version>`). Each move gets its `transition` history row (the `mutate` of the tracked write).
async fn transition(
    c: &mut PgConnection,
    job: Uuid,
    name: String,
    version: u32,
    from: VersionStatus,
    to: VersionStatus,
    actor: String,
) -> ApiResult<()> {
    let v = i32::try_from(version).unwrap_or(i32::MAX);
    if to == VersionStatus::Published {
        let replaced: Vec<i32> = sqlx::query_scalar(
            "UPDATE procedure_version SET status = 'ARCHIVED', updated_at = clock_timestamp() \
              WHERE job_id = $1 AND name = $2 AND status = 'PUBLISHED' AND version <> $3 \
              RETURNING version",
        )
        .bind(job)
        .bind(&name)
        .bind(v)
        .fetch_all(&mut *c)
        .await?;
        for other in replaced {
            let reason = format!("replaced by {version}");
            insert_history(
                &mut *c,
                Hist {
                    name: &name,
                    version: u32::try_from(other).unwrap_or(0),
                    actor: &actor,
                    action: "transition",
                    from: Some(VersionStatus::Published),
                    to: Some(VersionStatus::Archived),
                    ok: true,
                    reason: Some(&reason),
                },
            )
            .await?;
        }
    }
    sqlx::query(
        "UPDATE procedure_version SET status = $4, updated_at = clock_timestamp() \
          WHERE job_id = $1 AND name = $2 AND version = $3",
    )
    .bind(job)
    .bind(&name)
    .bind(v)
    .bind(status_text(to))
    .execute(&mut *c)
    .await?;
    insert_history(
        &mut *c,
        Hist {
            name: &name,
            version,
            actor: &actor,
            action: "transition",
            from: Some(from),
            to: Some(to),
            ok: true,
            reason: None,
        },
    )
    .await
}

/// A refused transition: its `ok:false` history row, committed on its own (the request's transaction is rolled
/// back), then the 409.
async fn refuse_transition(
    st: &AppState,
    j: &JobCtx,
    p: &Ver,
    to: VersionStatus,
    e: ApiError,
) -> ApiResult<Json<ProcedureView>> {
    let code = match &e {
        ApiError::Conflict(code, _) => *code,
        _ => "refused",
    };
    let mut tx = j.begin(&st.db).await?;
    insert_history(
        &mut tx,
        Hist {
            name: &p.name,
            version: p.version,
            actor: &j.t.caller.sub,
            action: "transition",
            from: Some(p.status),
            to: Some(to),
            ok: false,
            reason: Some(code),
        },
    )
    .await?;
    tx.commit().await?;
    Err(e)
}

/// The role a move needs (fixture `setProcedureStatus`, §R.1 / D88): → PUBLISHED is the Release manager's (not the
/// Job designer's), every other move the Job designer's. The route takes any member and checks here: first that the
/// caller holds one of the two (else 403 `missing_role: job_designer`, before the body is read), then the move's.
async fn set_procedure_status(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, name, v)): ApiPath<(String, String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<ProcedureView>> {
    if !j.t.effective.has(Role::ReleaseManager) {
        j.t.require(Role::JobDesigner)?;
    }
    let StatusInput { to } = body(raw)?;
    j.t.require(if to == VersionStatus::Published {
        Role::ReleaseManager
    } else {
        Role::JobDesigner
    })?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    lock_job(&mut tx, jr.id).await?;
    let (p, bases, spaces) = find_version(&mut tx, &st, &jr, &name, &v).await?;
    let refusal = if p.origin == Origin::Base {
        Some(ApiError::Conflict(
            "base_version",
            "Base versions change with a template release. Start a tenant version from it.".into(),
        ))
    } else if !next_status(p.status).contains(&to) {
        Some(ApiError::Conflict(
            "invalid_transition",
            format!(
                "{} → {} is not allowed.",
                status_text(p.status),
                status_text(to)
            ),
        ))
    } else if matches!(to, VersionStatus::Canary | VersionStatus::Published) {
        let (_, outside) = grant_of(&bases, &spaces, &p);
        (!outside.is_empty()).then(|| {
            ApiError::Conflict(
                "operation_outside_grant",
                format!(
                    "{} {} outside the activity grant of {}.",
                    outside.join(", "),
                    if outside.len() == 1 { "is" } else { "are" },
                    p.context_space
                ),
            )
        })
    } else {
        None
    };
    if let Some(e) = refusal {
        drop(tx);
        return refuse_transition(&st, &j, &p, to, e).await;
    }
    let from = p.status;
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "procedure",
        r#ref: place(&name),
        label: Label::Now(format!(
            "{name}@{} {} → {}",
            p.version,
            status_text(from).to_lowercase(),
            status_text(to).to_lowercase()
        )),
    };
    let (job, n, version, actor) = (jr.id, name.clone(), p.version, j.t.caller.sub.clone());
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(transition(c, job, n, version, from, to, actor))
    })
    .await?;
    let d = load(&mut tx, &st, &j).await?;
    let out = d.view_of(&name, p.version)?;
    tx.commit().await?;
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_follow_the_ui() {
        use VersionStatus::*;
        assert_eq!(next_status(Draft), [Canary, Discarded]);
        assert_eq!(next_status(Canary), [Published, Discarded]);
        assert_eq!(next_status(Published), [Archived]);
        assert_eq!(next_status(Archived), [Published]);
        assert!(next_status(Discarded).is_empty());
        for s in STATUSES {
            assert_eq!(status_of(status_text(s)).unwrap(), s);
        }
    }

    #[test]
    fn versions_are_positive_integers() {
        assert_eq!(version_of(&json!(3)), Some(3));
        for bad in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("3"),
            json!("1.4.0"),
            json!(null),
            json!(3_000_000_000u64),
        ] {
            assert_eq!(version_of(&bad), None, "{bad}");
        }
    }

    fn t7a_step(n: u32, phase: u32, name: &str, effect: &str) -> ProcedureStep {
        serde_json::from_value(json!({
            "step_number": n, "phase": phase, "name": name, "operation": "order.read", "variant": null,
            "modality": "observe", "side_effect": effect, "identity_level": "2", "confirm_back": false,
            "gate": false, "args": {}
        }))
        .unwrap()
    }

    fn cond(id: &str, fact: &str) -> Condition {
        serde_json::from_value(json!({"id": id, "fact": fact, "op": "exists"})).unwrap()
    }

    /// The 422 code of `e`.
    fn code(e: ApiError) -> (&'static str, ()) {
        let (status, code) = e.status_and_code();
        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        (code, ())
    }

    #[test]
    fn caps_only_on_x_and_p_steps_within_1_to_20() {
        let mut x = t7a_step(1, 0, "cancel", "X");
        x.max_per_case = Some(20);
        assert!(check_steps(std::slice::from_ref(&x), &[]).is_ok());
        for bad in [0, 21] {
            x.max_per_case = Some(bad);
            assert_eq!(
                code(check_steps(std::slice::from_ref(&x), &[]).unwrap_err()).0,
                "invalid_cap"
            );
        }
        for effect in ["R", "W"] {
            let mut s = t7a_step(1, 0, "s", effect);
            s.max_per_case = Some(1);
            assert_eq!(
                code(check_steps(&[s], &[]).unwrap_err()).0,
                "invalid_cap",
                "{effect}"
            );
        }
        let mut s = t7a_step(1, 0, "s", "R");
        s.performed_by = Some("pilot".into());
        assert_eq!(
            code(check_steps(std::slice::from_ref(&s), &["procedure"]).unwrap_err()).0,
            "unknown_role"
        );
        s.performed_by = Some("procedure".into());
        assert!(check_steps(&[s], &["procedure"]).is_ok());
    }

    #[test]
    fn conditions_read_only_earlier_steps() {
        let mut a = t7a_step(1, 0, "order", "R");
        let mut b = t7a_step(2, 1, "cancel", "X");
        b.preconditions = vec![cond("read", "steps.order.outcome")];
        b.postconditions = vec![cond("done", "steps.cancel.out.status")];
        assert!(
            check_conditions(
                &[a.clone(), b.clone()],
                &[cond("ok", "steps.cancel.outcome")]
            )
            .is_ok()
        );
        // a precondition may not read its own phase or a later one
        a.preconditions = vec![cond("early", "steps.cancel.outcome")];
        assert_eq!(
            code(check_conditions(&[a.clone(), b.clone()], &[]).unwrap_err()).0,
            "invalid_condition"
        );
        a.preconditions.clear();
        // unknown fact path
        assert_eq!(
            code(check_conditions(&[a, b], &[cond("x", "weather.today")]).unwrap_err()).0,
            "invalid_condition"
        );
    }

    #[test]
    fn tool_requirements_are_catalogue_operations() {
        use agent_model::kinds::catalog::{Modality, SideEffect};
        let cat = vec![CatalogueOperation {
            op: "order.cancel".into(),
            variant: None,
            modality: Modality::Do,
            side_effect: SideEffect::X,
            identity_level: "2".into(),
        }];
        assert_eq!(
            check_tool_requirements(&cat, vec!["order.cancel".into(), "order.cancel".into()])
                .unwrap(),
            ["order.cancel"]
        );
        for bad in ["order.read", "order", "Order.cancel", "order.cancel{", ""] {
            assert_eq!(
                code(check_tool_requirements(&cat, vec![bad.into()]).unwrap_err()).0,
                "unknown_operation",
                "{bad}"
            );
        }
        assert!(check_pddl(Some(&"x".repeat(MAX_PDDL_BYTES))).is_ok());
        assert_eq!(
            code(check_pddl(Some(&"x".repeat(MAX_PDDL_BYTES + 1))).unwrap_err()).0,
            "invalid_pddl"
        );
    }

    #[test]
    fn shadowing_needs_a_tenant_published_version() {
        use VersionStatus::*;
        let all = [
            ("return", Origin::Base, Published),
            ("return", Origin::Tenant, Published),
            ("track", Origin::Base, Published),
            ("track", Origin::Tenant, Canary),
        ];
        assert!(shadowed(
            "return",
            Origin::Base,
            Published,
            all.iter().copied()
        ));
        assert!(!shadowed(
            "return",
            Origin::Tenant,
            Published,
            all.iter().copied()
        ));
        assert!(!shadowed(
            "track",
            Origin::Base,
            Published,
            all.iter().copied()
        ));
    }

    #[test]
    fn base_version_lists_its_steps_criteria_and_requirements() {
        let step = |n: u32, name: &str, op: &str| {
            json!({"step_number": n, "phase": n - 1, "name": name, "operation": op, "variant": null,
                   "modality": "observe", "side_effect": "R", "identity_level": "2", "confirm_back": false,
                   "gate": false, "args": {}})
        };
        let mut b = Bases::default();
        let mut put = |key: &str, row: Value| {
            b.procedures
                .insert(key.to_owned(), row.as_object().cloned().unwrap());
        };
        put(
            "order_status@2",
            json!({"label": "Order status", "context_space": "retail:order-support:order_status",
                   "steps": [step(1, "order", "order.read"), step(2, "ship", "delivery.read")],
                   "success_criteria": ["order", "ship"],
                   "tool_requirements": ["order.read", "delivery.read"]}),
        );
        put("old@1", json!({"label": "Old", "steps": 3}));
        let got = base_versions(&b);
        let v = got.iter().find(|v| v.name == "order_status").unwrap();
        assert_eq!((v.version, v.steps.len()), (2, 2));
        assert_eq!(v.steps[1].operation, "delivery.read");
        assert_eq!(v.success_criteria.len(), 2);
        assert_eq!(v.success_criteria[0].fact, "steps.order.outcome");
        assert_eq!(v.tool_requirements, ["order.read", "delivery.read"]);
        let old = got.iter().find(|v| v.name == "old").unwrap();
        assert!(old.steps.is_empty() && old.success_criteria.is_empty());
    }
}
