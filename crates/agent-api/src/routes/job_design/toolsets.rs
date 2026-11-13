//! J8 toolsets (B5 T11, T11a; plan "Route table"): a port of the fixture `areas/ontology.ts` J8 methods (codes,
//! messages, labels). Reads are the DRAFT design unless the route says otherwise; every write is a D54 draft through
//! `design::track`, page J8, area `toolsets`.
//!
//! | Method | Route | Role | Ledger kind |
//! |---|---|---|---|
//! | toolsetMeta | `GET /api/jobs/{job}/toolsets` | member | — |
//! | narrowToolsetGrant | `POST /api/jobs/{job}/toolsets/{role}/narrowings` `{activity, op}` | job_designer | `narrowing` |
//! | restoreToolsetGrant | `DELETE /api/jobs/{job}/toolsets/{role}/narrowings` (JSON body) | job_designer | `narrowing` |
//! | toolsetOverview | `GET /api/jobs/{job}/toolsets/overview?env=prod\|test` | member | — |
//! | setAgentRoleEnabled | `PUT /api/jobs/{job}/toolsets/{role}/enabled` `{enabled, max_tool_calls?, turn_budget_s?, system_prompt?, case_per_thread?}` | job_designer | `role_setting` |
//!
//! - The toolset rows are `job_design::toolset` (N2 ∩ N3 − narrowings, the role switches), the same functions the
//!   runtime reads; this module adds N4, the route of each row in the env, through B7's effective routes and B4's
//!   defaults exactly as the runner resolves them (`runtime::runner::ops::EffectiveRoutes`). An operation with no
//!   route row whose default is `not_offered` is shown `unrouted` ("No route on <env> (N4)"); a route row that says
//!   `not_offered` is "Not offered by this tenant (N4)".
//! - The narrowing writes answer the role's toolset for the activity over the draft with the `prod` routes (fixture).
//! - `overview`: `prod` reads the LIVE design (its roles, activities, narrowings, role settings and effective
//!   procedures), `test` the DRAFT (D54).
//! - F-40 (T11a): only `assistant` is configurable (422 `role_not_configurable`); switching it on needs the platform
//!   row enabled at the draft's pins (409 `role_disabled_on_platform`); `max_tool_calls` is 1..=6 (422
//!   `invalid_max_tool_calls`; absent = keep, null = the runtime default). The setting row goes when it is back to
//!   the default (off, no budget). A5: `turn_budget_s` 5..=600 (422 `invalid_turn_budget_s`), `system_prompt` at most 4000
//!   characters (422 `invalid_system_prompt`; blank = the audience's default), `case_per_thread`; each absent = keep, null = default.
use std::collections::BTreeMap;

use axum::extract::State;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{body, job_ref};
use crate::connectors::resolve::{OperationRoute, RouteTarget};
use crate::connectors::routes_store;
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::job_design::base::Bases;
use crate::job_design::context_space;
use crate::job_design::toolset::{
    self, MAX_TOOL_CALLS_RANGE, Missing, Role, RoleSetting, Row, SYSTEM_PROMPT_MAX,
    TURN_BUDGET_S_RANGE, Toolsets, configurable,
};
use crate::job_design::types::{
    ActivityRef, AgentRole, AgentRoleGrant, AgentRoleKey, Origin, Toolset,
    ToolsetActivityProcedure, ToolsetMeta, ToolsetNarrowing, ToolsetProcedureStep, ToolsetRole,
    ToolsetRoute, ToolsetRow,
};
use crate::runtime::Env;
use crate::runtime::routes::{DefaultRoutes, ResolvedTarget, RouteQuery};
use crate::runtime::runner::ops::{route_for, target_of};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/toolsets", get(toolset_meta))
        .route("/api/jobs/{job}/toolsets/overview", get(toolset_overview))
        .route(
            "/api/jobs/{job}/toolsets/{role}/narrowings",
            post(narrow_toolset_grant).delete(restore_toolset_grant),
        )
        .route(
            "/api/jobs/{job}/toolsets/{role}/enabled",
            put(set_agent_role_enabled),
        )
}

const AREA: &str = "toolsets";
const PAGE: &str = "J8";

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn role_not_found(role: &str, job: &str) -> ApiError {
    ApiError::Missing("role_not_found", format!("No {role} role for {job}."))
}

fn activity_not_found(activity: &str) -> ApiError {
    ApiError::Missing(
        "activity_not_found",
        format!("This job has no activity {activity}."),
    )
}

fn missing(m: Missing, role: &str, activity: &str, job: &str) -> ApiError {
    match m {
        Missing::Role => role_not_found(role, job),
        Missing::Activity => activity_not_found(activity),
    }
}

fn of_env(env: Env) -> DesignOf {
    match env {
        Env::Prod => DesignOf::Live,
        Env::Test => DesignOf::Draft,
    }
}

fn place(pairs: &[(&str, &str)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), Value::String((*v).to_owned()));
    }
    m
}

/// The wire role key; a role the web does not know is 404 `role_not_found` (its toolset cannot be shown).
fn role_key(role: &str) -> Option<AgentRoleKey> {
    serde_json::from_value(Value::String(role.to_owned())).ok()
}

// ---- N4 -------------------------------------------------------------------------------------------------------------

/// The routes of one env, resolved as the runner does (B7's effective routes, then B4's defaults).
struct Routes {
    env: Env,
    rows: Vec<OperationRoute>,
    defaults: DefaultRoutes,
    query: RouteQuery,
}

impl Routes {
    async fn load(c: &mut PgConnection, st: &AppState, j: &JobCtx, env: Env) -> ApiResult<Routes> {
        let of = match env {
            Env::Prod => routes_store::DesignOf::Live,
            Env::Test => routes_store::DesignOf::Draft,
        };
        let rows = routes_store::effective(&mut *c, &st.design, j.job_id, env.as_str(), of).await?;
        Ok(Routes {
            env,
            rows,
            defaults: DefaultRoutes::new(),
            query: RouteQuery {
                tenant: j.t.tenant,
                job_id: j.job_id,
                job_key: j.job_key.clone(),
                env,
                op: String::new(),
            },
        })
    }

    /// The route of `label` (`object.verb{variant}`).
    fn of(&self, label: &str) -> ToolsetRoute {
        let explicit = route_for(&self.rows, label).map(|r| target_of(&r.target, label));
        let explicit_not_offered = route_for(&self.rows, label)
            .is_some_and(|r| matches!(r.target, RouteTarget::NotOffered));
        let target = match explicit {
            Some(t) => t,
            None => {
                let mut q = self.query.clone();
                q.op = label.to_owned();
                self.defaults.default_target(&q)
            }
        };
        match target {
            ResolvedTarget::Instance { instance_id } => ToolsetRoute::Instance {
                instance: instance_id,
            },
            ResolvedTarget::Builtin { .. } => ToolsetRoute::Builtin,
            ResolvedTarget::AgentCases => ToolsetRoute::Fallback {
                instance: "agent_cases".into(),
            },
            ResolvedTarget::DeepLink { url } => ToolsetRoute::DeepLink { url },
            ResolvedTarget::NotOffered if explicit_not_offered => ToolsetRoute::NotOffered,
            ResolvedTarget::NotOffered => ToolsetRoute::Unrouted,
        }
    }
}

/// A row with its route (fixture `computeToolset`: N4 after the toolset's own reasons).
fn wire_row(row: Row, routes: &Routes) -> ToolsetRow {
    let route = routes.of(&row.label);
    let reason = row.reason.or_else(|| match &route {
        ToolsetRoute::NotOffered => Some("Not offered by this tenant (N4)".to_owned()),
        ToolsetRoute::Unrouted => Some(format!("No route on {} (N4)", routes.env.as_str())),
        _ => None,
    });
    ToolsetRow {
        op: row.op,
        variant: row.variant,
        side_effect: row.side_effect,
        modality: row.modality,
        expected: row.expected,
        granted: row.granted,
        narrowed: row.narrowed,
        route,
        effective: reason.is_none(),
        reason,
    }
}

/// The toolset of `role` for `activity` with `routes` (404 `role_not_found` / `activity_not_found`).
fn toolset_of(
    t: &Toolsets,
    role: &str,
    activity: &str,
    routes: &Routes,
    job: &str,
) -> ApiResult<Toolset> {
    let key = role_key(role).ok_or_else(|| role_not_found(role, job))?;
    let rows = t
        .rows(role, activity)
        .map_err(|m| missing(m, role, activity, job))?;
    Ok(Toolset {
        role: key,
        role_enabled: t.settings.enabled(role),
        activity: activity.to_owned(),
        env: routes.env,
        rows: rows.into_iter().map(|r| wire_row(r, routes)).collect(),
    })
}

// ---- meta -----------------------------------------------------------------------------------------------------------

fn wire_role(r: &Role, t: &Toolsets, job: &str) -> Option<ToolsetRole> {
    let key = role_key(&r.key)?;
    let s = &t.settings;
    Some(ToolsetRole {
        role: AgentRole {
            job: job.to_owned(),
            role: key,
            label: r.fields.label.clone(),
            used_by: r.fields.used_by.clone(),
            max_side_effect: r.fields.max_side_effect,
            enabled: s.enabled(&r.key),
            grants: r
                .fields
                .grants
                .iter()
                .map(|g| AgentRoleGrant {
                    object: g.object.clone(),
                    verb: g.verb.clone(),
                    variant: g.variant.clone(),
                    modalities: g.modalities.clone(),
                    derive_from_activity: g.derive_from_activity,
                    derived: g.derived,
                })
                .collect(),
            note: r.fields.note.clone(),
            updated_by: r.fields.updated_by.clone(),
            updated_at: r
                .fields
                .updated_at
                .as_deref()
                .and_then(|x| DateTime::parse_from_rfc3339(x).ok())
                .map_or(DateTime::<Utc>::UNIX_EPOCH, |x| x.with_timezone(&Utc)),
        },
        enabled_job: Some(s.enabled_job(&r.key)),
        enabled_platform: Some(s.enabled_platform(&r.key)),
        max_tool_calls: s.setting(&r.key).and_then(|x| x.max_tool_calls),
        turn_budget_s: s.setting(&r.key).and_then(|x| x.turn_budget_s),
        system_prompt: s.setting(&r.key).and_then(|x| x.system_prompt.clone()),
        case_per_thread: s.setting(&r.key).is_some_and(|x| x.case_per_thread),
        configurable: Some(configurable(&r.key)),
    })
}

fn activity_refs(bases: &Bases) -> Vec<ActivityRef> {
    bases
        .activities
        .iter()
        .map(|a| {
            let text = |f: &str| a.item.get(f).and_then(Value::as_str).map(str::to_owned);
            ActivityRef {
                key: a.key.clone(),
                stage: text("stage").unwrap_or_default(),
                label: text("label").unwrap_or_else(|| a.key.clone()),
                hidden: false,
                origin: Origin::Base,
            }
        })
        .collect()
}

fn meta(bases: &Bases, t: &Toolsets, job: &str) -> ToolsetMeta {
    ToolsetMeta {
        roles: t
            .roles()
            .iter()
            .filter_map(|r| wire_role(r, t, job))
            .collect(),
        activities: activity_refs(bases),
        narrowings: t
            .narrowings()
            .map(|(r, a, o)| format!("{r}|{a}|{o}"))
            .collect(),
    }
}

async fn draft(
    c: &mut PgConnection,
    st: &AppState,
    jr: &JobRef,
) -> ApiResult<(std::sync::Arc<Bases>, Toolsets)> {
    toolset::of_design(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        jr,
        &DesignOf::Draft,
    )
    .await
}

async fn toolset_meta(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<ToolsetMeta>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, t) = draft(&mut tx, &st, &jr).await?;
    tx.commit().await?;
    Ok(Json(meta(&bases, &t, &j.job_key)))
}

// ---- overview -------------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct OverviewQuery {
    env: Option<String>,
}

/// A step of a stored version value, read leniently (only what the overview shows).
fn overview_step(s: &Value) -> Option<ToolsetProcedureStep> {
    Some(ToolsetProcedureStep {
        step_number: s
            .get("step_number")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())?,
        name: s.get("name")?.as_str()?.to_owned(),
        operation: s.get("operation")?.as_str()?.to_owned(),
        variant: s.get("variant").and_then(Value::as_str).map(str::to_owned),
        side_effect: serde_json::from_value(s.get("side_effect")?.clone()).ok()?,
        identity_level: match s.get("identity_level")? {
            Value::String(x) => x.clone(),
            other => other.to_string(),
        },
        confirm_back: s
            .get("confirm_back")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// The overview steps of a base procedure row: its step bodies, none when any step does not decode.
fn base_overview_steps(key: &str, f: &serde_json::Map<String, Value>) -> Vec<ToolsetProcedureStep> {
    crate::runtime::spaces::base_steps(key, f)
        .into_iter()
        .map(|s| ToolsetProcedureStep {
            step_number: s.step_number,
            name: s.name,
            operation: s.operation,
            variant: s.variant,
            side_effect: s.side_effect,
            identity_level: s.identity_level,
            confirm_back: s.confirm_back,
        })
        .collect()
}

/// The effective procedures of design `of` per activity (fixture `toolsetOverview`: PUBLISHED and not shadowed; a
/// tenant PUBLISHED version shadows the base versions of its name), each mapped to its space's activity (a version
/// whose space names no activity is left out). A base version's steps are its row's step bodies (none when any step does not decode).
async fn effective_procedures(
    c: &mut PgConnection,
    st: &AppState,
    jr: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<ToolsetActivityProcedure>> {
    let (bases, spaces) =
        context_space::of_design(&mut *c, &st.design, st.compiler.as_ref(), jr, of).await?;
    let activity_of = |space: &str| {
        spaces
            .iter()
            .find(|s| s.key == space)
            .map(context_space::EffSpace::activity)
            .filter(|a| !a.is_empty())
    };
    let mut tenant: Vec<ToolsetActivityProcedure> = Vec::new();
    let mut tenant_names: Vec<String> = Vec::new();
    for row in design::values_of_kind(&mut *c, &st.design, jr, of, "procedure").await? {
        let Some(name) = row.r#ref.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(Value::Array(versions)) = row.snap else {
            continue;
        };
        for v in versions {
            if v.get("status").and_then(Value::as_str) != Some("PUBLISHED") {
                continue;
            }
            tenant_names.push(name.to_owned());
            let space = v
                .get("context_space")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some(activity) = activity_of(&space) else {
                continue;
            };
            let version = v
                .get("version")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    internal(format!(
                        "procedure {name} has a version that is not an integer"
                    ))
                })?;
            tenant.push(ToolsetActivityProcedure {
                activity,
                context_space: space,
                name: name.to_owned(),
                version,
                origin: Origin::Tenant,
                steps: v
                    .get("steps")
                    .and_then(Value::as_array)
                    .map(|xs| xs.iter().filter_map(overview_step).collect())
                    .unwrap_or_default(),
            });
        }
    }
    let mut out: Vec<ToolsetActivityProcedure> = Vec::new();
    for (key, f) in &bases.procedures {
        let (name, version) = match key.rsplit_once('@') {
            Some((n, v)) => (n, v.parse().unwrap_or(1)),
            None => (key.as_str(), 1),
        };
        if tenant_names.iter().any(|n| n == name) {
            continue;
        }
        let space = f
            .get("context_space")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(activity) = activity_of(&space) else {
            continue;
        };
        out.push(ToolsetActivityProcedure {
            activity,
            context_space: space,
            name: name.to_owned(),
            version,
            origin: Origin::Base,
            steps: base_overview_steps(key, f),
        });
    }
    out.extend(tenant);
    Ok(out)
}

async fn toolset_overview(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<OverviewQuery>,
) -> ApiResult<Json<crate::job_design::types::ToolsetOverview>> {
    let env = match q.env.as_deref() {
        Some("prod") => Env::Prod,
        Some("test") => Env::Test,
        other => {
            return Err(ApiError::Unprocessable(
                "invalid_env",
                format!(
                    "env must be prod or test, not {}.",
                    other.unwrap_or("absent")
                ),
            ));
        }
    };
    let jr = job_ref(&j);
    let of = of_env(env);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (_, t) = toolset::of_design(&mut tx, &st.design, st.compiler.as_ref(), &jr, &of).await?;
    let routes = Routes::load(&mut tx, &st, &j, env).await?;
    let procedures = effective_procedures(&mut tx, &st, &jr, &of).await?;
    // provenance of the narrowings: the working rows' who/when (a narrowing only live keeps the job's stamp)
    let stamps: BTreeMap<(String, String, String), (String, DateTime<Utc>)> = sqlx::query_as::<
        _,
        (String, String, String, String, DateTime<Utc>),
    >(
        "SELECT role, activity, op, updated_by, updated_at FROM toolset_narrowing WHERE job_id = $1",
    )
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(r, a, o, by, at)| ((r, a, o), (by, at)))
    .collect();
    tx.commit().await?;
    let mut toolsets = Vec::new();
    for activity in t.activities() {
        for r in t.roles() {
            if role_key(&r.key).is_none() {
                continue;
            }
            toolsets.push(toolset_of(&t, &r.key, activity, &routes, &j.job_key)?);
        }
    }
    let narrowings = t
        .narrowings()
        .filter_map(|n| {
            let role = role_key(&n.0)?;
            let (by, at) = stamps
                .get(n)
                .cloned()
                .unwrap_or_else(|| (String::new(), DateTime::<Utc>::UNIX_EPOCH));
            Some(ToolsetNarrowing {
                role,
                activity: n.1.clone(),
                op: n.2.clone(),
                by,
                at,
            })
        })
        .collect();
    Ok(Json(crate::job_design::types::ToolsetOverview {
        env,
        toolsets,
        procedures,
        narrowings,
    }))
}

// ---- narrowings -----------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NarrowInput {
    activity: String,
    op: String,
}

/// The role's toolset for the activity over the draft, with the `prod` routes (fixture).
async fn answer(
    c: &mut PgConnection,
    st: &AppState,
    j: &JobCtx,
    role: &str,
    activity: &str,
) -> ApiResult<Toolset> {
    let (_, t) = draft(&mut *c, st, &job_ref(j)).await?;
    let routes = Routes::load(&mut *c, st, j, Env::Prod).await?;
    toolset_of(&t, role, activity, &routes, &j.job_key)
}

async fn narrow_toolset_grant(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, role)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<Toolset>> {
    let NarrowInput { activity, op } = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let current = answer(&mut tx, &st, &j, &role, &activity).await?;
    let row = current
        .rows
        .iter()
        .find(|r| crate::job_design::op_label(&r.op, r.variant.as_deref()) == op);
    let Some(row) = row.filter(|r| r.granted && r.expected.is_some()) else {
        return Err(ApiError::Conflict(
            "not_granted",
            format!("{op} is not granted here; there is nothing to narrow."),
        ));
    };
    if row.narrowed {
        return Err(ApiError::Conflict(
            "already_narrowed",
            format!("{op} is already narrowed."),
        ));
    }
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let (r, a, o) = (role.clone(), activity.clone(), op.clone());
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "narrowing",
        r#ref: place(&[
            ("role", role.as_str()),
            ("activity", activity.as_str()),
            ("op", op.as_str()),
        ]),
        label: Label::Now(format!("{op} removed from {role} for {activity}")),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO toolset_narrowing (job_id, role, activity, op, updated_by) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(job)
            .bind(&r)
            .bind(&a)
            .bind(&o)
            .bind(&by)
            .execute(&mut *c)
            .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let out = answer(&mut tx, &st, &j, &role, &activity).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn restore_toolset_grant(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, role)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<Toolset>> {
    let NarrowInput { activity, op } = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    // 404 role_not_found / activity_not_found first
    answer(&mut tx, &st, &j, &role, &activity).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM toolset_narrowing \
                         WHERE job_id = $1 AND role = $2 AND activity = $3 AND op = $4)",
    )
    .bind(j.job_id)
    .bind(&role)
    .bind(&activity)
    .bind(&op)
    .fetch_one(&mut *tx)
    .await?;
    if !exists {
        return Err(ApiError::Unprocessable(
            "widening_rejected",
            format!(
                "{op} is not in the {role} grant for this activity. Tenants may only narrow a grant; widening it is a template change."
            ),
        ));
    }
    let job = j.job_id;
    let (r, a, o) = (role.clone(), activity.clone(), op.clone());
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "narrowing",
        r#ref: place(&[
            ("role", role.as_str()),
            ("activity", activity.as_str()),
            ("op", op.as_str()),
        ]),
        label: Label::Now(format!("{op} restored to {role} for {activity}")),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query(
                "DELETE FROM toolset_narrowing \
                  WHERE job_id = $1 AND role = $2 AND activity = $3 AND op = $4",
            )
            .bind(job)
            .bind(&r)
            .bind(&a)
            .bind(&o)
            .execute(&mut *c)
            .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let out = answer(&mut tx, &st, &j, &role, &activity).await?;
    tx.commit().await?;
    Ok(Json(out))
}

// ---- F-40: the job's role switch ------------------------------------------------------------------------------------

/// `Some(None)` = null, `None` = absent.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleEnabledInput {
    enabled: bool,
    #[serde(default, deserialize_with = "present")]
    max_tool_calls: Option<Option<i64>>,
    /// A5: seconds per assistant turn (5..=600); absent = keep, null = the runtime default.
    #[serde(default, deserialize_with = "present")]
    turn_budget_s: Option<Option<i64>>,
    /// A5: the assistant's task prompt (at most 4000 characters); absent = keep, null or blank = the default.
    #[serde(default, deserialize_with = "present")]
    system_prompt: Option<Option<String>>,
    /// A5: a turn on a conversation with an open case continues it; absent = keep.
    #[serde(default)]
    case_per_thread: Option<bool>,
}

async fn set_agent_role_enabled(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, role)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<ToolsetMeta>> {
    let i: RoleEnabledInput = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (_, t) = draft(&mut tx, &st, &jr).await?;
    let Some(r) = t.role(&role).filter(|r| role_key(&r.key).is_some()) else {
        return Err(role_not_found(&role, &j.job_key));
    };
    if !configurable(&r.key) {
        return Err(ApiError::Unprocessable(
            "role_not_configurable",
            format!(
                "{} cannot be switched per job; only the assistant can.",
                r.fields.label
            ),
        ));
    }
    let current = t.settings.setting(&role).cloned();
    let max = match i.max_tool_calls {
        None => current.as_ref().and_then(|s| s.max_tool_calls),
        Some(None) => None,
        Some(Some(n)) => {
            let ok = u32::try_from(n)
                .ok()
                .filter(|n| MAX_TOOL_CALLS_RANGE.contains(n));
            Some(ok.ok_or_else(|| {
                ApiError::Unprocessable(
                    "invalid_max_tool_calls",
                    format!(
                        "Max tool calls per turn must be between {} and {}.",
                        MAX_TOOL_CALLS_RANGE.start(),
                        MAX_TOOL_CALLS_RANGE.end()
                    ),
                )
            })?)
        }
    };
    let turn_budget_s = match i.turn_budget_s {
        None => current.as_ref().and_then(|s| s.turn_budget_s),
        Some(None) => None,
        Some(Some(n)) => {
            let ok = u32::try_from(n)
                .ok()
                .filter(|n| TURN_BUDGET_S_RANGE.contains(n));
            Some(ok.ok_or_else(|| {
                ApiError::Unprocessable(
                    "invalid_turn_budget_s",
                    format!(
                        "The turn budget must be between {} and {} seconds.",
                        TURN_BUDGET_S_RANGE.start(),
                        TURN_BUDGET_S_RANGE.end()
                    ),
                )
            })?)
        }
    };
    let system_prompt = match i.system_prompt {
        None => current.as_ref().and_then(|s| s.system_prompt.clone()),
        Some(None) => None,
        Some(Some(p)) if p.trim().is_empty() => None,
        Some(Some(p)) if p.chars().count() > SYSTEM_PROMPT_MAX => {
            return Err(ApiError::Unprocessable(
                "invalid_system_prompt",
                format!("The system prompt is at most {SYSTEM_PROMPT_MAX} characters."),
            ));
        }
        Some(Some(p)) => Some(p),
    };
    let case_per_thread = i
        .case_per_thread
        .unwrap_or_else(|| current.as_ref().is_some_and(|s| s.case_per_thread));
    if i.enabled && !t.settings.enabled_platform(&role) {
        return Err(ApiError::Conflict(
            "role_disabled_on_platform",
            format!(
                "{} is disabled on the platform; it must be enabled there first.",
                r.fields.label
            ),
        ));
    }
    let next = RoleSetting {
        enabled: i.enabled,
        max_tool_calls: max,
        turn_budget_s,
        system_prompt,
        case_per_thread,
    };
    let label = format!(
        "{}: {}{}",
        r.fields.label,
        if next.enabled {
            "on for this job"
        } else {
            "off for this job"
        },
        next.max_tool_calls
            .map(|n| format!(" · up to {n} tool calls"))
            .unwrap_or_default(),
    );
    let label = format!(
        "{label}{}{}{}",
        next.turn_budget_s
            .map(|n| format!(" · {n}s per turn"))
            .unwrap_or_default(),
        if next.system_prompt.is_some() {
            " · own prompt"
        } else {
            ""
        },
        if next.case_per_thread {
            " · one case per thread"
        } else {
            ""
        }
    );
    let job: Uuid = j.job_id;
    let by = j.t.caller.sub.clone();
    let turn = next.turn_budget_s.and_then(|n| i32::try_from(n).ok());
    let row_role = role.clone();
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "role_setting",
        r#ref: place(&[("role", role.as_str())]),
        label: Label::Now(label),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            if next.is_default() {
                sqlx::query("DELETE FROM job_agent_role_setting WHERE job_id = $1 AND role = $2")
                    .bind(job)
                    .bind(&row_role)
                    .execute(&mut *c)
                    .await?;
            } else {
                let max = next.max_tool_calls.and_then(|n| i16::try_from(n).ok());
                sqlx::query(
                    "INSERT INTO job_agent_role_setting \
                            (job_id, role, enabled, max_tool_calls, turn_budget_s, system_prompt, \
                             case_per_thread, updated_by) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                     ON CONFLICT (tenant_id, job_id, role) DO UPDATE \
                        SET enabled = EXCLUDED.enabled, max_tool_calls = EXCLUDED.max_tool_calls, \
                            turn_budget_s = EXCLUDED.turn_budget_s, system_prompt = EXCLUDED.system_prompt, \
                            case_per_thread = EXCLUDED.case_per_thread, \
                            updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
                )
                .bind(job)
                .bind(&row_role)
                .bind(next.enabled)
                .bind(max)
                .bind(turn)
                .bind(next.system_prompt.as_deref())
                .bind(next.case_per_thread)
                .bind(&by)
                .execute(&mut *c)
                .await?;
            }
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let (bases, t) = draft(&mut tx, &st, &jr).await?;
    tx.commit().await?;
    Ok(Json(meta(&bases, &t, &j.job_key)))
}

#[cfg(test)]
mod base_overview_tests {
    use super::*;
    use serde_json::json;

    fn row(steps: Value) -> serde_json::Map<String, Value> {
        json!({"label": "Cancel", "steps": steps})
            .as_object()
            .cloned()
            .unwrap()
    }

    fn step(n: u32, op: &str, se: &str) -> Value {
        json!({"step_number": n, "phase": n - 1, "name": format!("s{n}"), "operation": op, "variant": null,
               "modality": "observe", "side_effect": se, "identity_level": "2", "confirm_back": true,
               "gate": false, "args": {}})
    }

    #[test]
    fn base_procedure_overview_lists_its_steps() {
        let f = row(json!([
            step(1, "order.read", "R"),
            step(2, "order.cancel", "X")
        ]));
        let got = base_overview_steps("cancel_order@1", &f);
        assert_eq!(got.len(), 2);
        assert_eq!(
            (got[1].name.as_str(), got[1].operation.as_str()),
            ("s2", "order.cancel")
        );
        assert_eq!(got[1].identity_level, "2");
        assert!(got[1].confirm_back);
    }

    #[test]
    fn base_procedure_overview_is_empty_for_a_count_or_any_undecodable_step() {
        assert!(base_overview_steps("a@1", &row(json!(4))).is_empty());
        let mut bad = step(2, "rule.evaluate", "R");
        bad["note"] = json!("builtin");
        let f = row(json!([step(1, "order.read", "R"), bad]));
        assert!(base_overview_steps("a@1", &f).is_empty());
    }
}
