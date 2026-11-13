//! Job operations and routes (B7 T13, J6/J25, D54/D63/D67) and the integration field-spec export (T13a, F-36).
//! Shapes = `web/src/lib/api/types/connectors.ts` (`JobOperations`, `JobTool[]`, `RouteWriteResult`); refusals = the
//! plan's Route table and the fixture (`web/src/lib/api/areas/connectors.ts`).
//!
//! - `GET /api/jobs/{job}/operations` (member) → `JobOperations`: expected ops (crate::connectors::routes_store), the
//!   working routes (the draft), instances, implements, draft instances, tested, contract, names and the D67
//!   `cells`. 404 `job_not_found`. `stats` is `[]` (B4 seam: `operation_call` 7-day `calls_7d`, `error_rate`,
//!   `p95_ms`); `live_routes` / `live_cells` = the routes of the job's current release snapshot and the cells on
//!   them, present only when they differ from the draft (fixture `jobOperations`).
//! - `GET /api/jobs/{job}/tools` (member) → `JobTool[]`: per expected op its catalogue description, inputs,
//!   outputs, confirm-back, idempotency (null for reads), the status changes of its object's state machine and its
//!   draft `policy` (A2 declaration, `tool_policy`; null when none).
//! - `PUT /api/jobs/{job}/operations/{op}/routes/{env}` `{target}` (integrations_manager) → `RouteWriteResult`.
//!   404 `operation_not_expected`, `instance_not_found`; 422 `required_operation`, `env_mismatch`,
//!   `not_implemented`, `stub_not_allowed_in_prod`, `invalid_url`, `invalid_input` (env); 409 `template_not_active`.
//! - `DELETE /api/jobs/{job}/operations/{op}/routes/{env}` (integrations_manager) → `RouteWriteResult` with
//!   `route: null`. 404 `route_not_found`.
//! - `GET /api/jobs/{job}/operations/spec?format=csv|md&of=draft|live` (member, read-only, no audit row) → the spec
//!   as an attachment (`text/csv` / `text/markdown`, `Content-Disposition`). 422 `invalid_format`, `invalid_of`;
//!   409 `no_release` for `of=live` when the job has no current release.
//!
//! `{op}` is the op key `object.verb{variant}` URL-encoded (`case.create%7Breturn%7D`). A route write is a job
//! design write (D54): the working row changes inside B2's `design::track` (area `routes`, page `J6`, kind `route`,
//! ref `{object, verb, variant, env}`: the job's ledger row and `draft_rev` bump), plus one audit row, in one
//! transaction. `draft_rev` is the job's draft revision after the write, `previous_rev` the one before it.
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::connectors::instances as inst_db;
use crate::connectors::model::{OpRef, TemplateStatus};
use crate::connectors::opkey::{op_key, parse_op_key};
use crate::connectors::places;
use crate::connectors::readiness::{self, cells_of};
use crate::connectors::resolve::{
    BUILTIN_OPS_TEXT, OperationRoute, RouteData, RouteTarget, is_builtin_op,
};
use crate::connectors::routes_store::{self, DesignOf};
use crate::connectors::spec_export;
use crate::design::{self, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/operations", get(job_operations))
        .route("/api/jobs/{job}/operations/spec", get(export_spec))
        .route("/api/jobs/{job}/tools", get(job_tools))
        .route(
            "/api/jobs/{job}/operations/{op}/routes/{env}",
            put(set_route).delete(clear_route),
        )
}

/// The D69 ledger kind of a route write.
const KIND: &str = "route";
/// The audit resource of every write here.
const RESOURCE: &str = "operation_route";

#[derive(Deserialize)]
struct OpEnvPath {
    op: String,
    env: String,
}

#[derive(Deserialize)]
struct RouteBody {
    target: RouteTarget,
}

#[derive(Deserialize)]
struct SpecQuery {
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    of: Option<String>,
}

fn to_value<T: serde::Serialize>(v: &T) -> ApiResult<Value> {
    serde_json::to_value(v).map_err(|e| ApiError::Internal(anyhow::anyhow!("to JSON: {e}")))
}

/// `prod` / `test`, else 422 `invalid_input`.
fn env_of(env: &str) -> ApiResult<&'static str> {
    match env {
        "prod" => Ok("prod"),
        "test" => Ok("test"),
        _ => Err(ApiError::Unprocessable(
            "invalid_input",
            "The env is prod or test.".into(),
        )),
    }
}

fn op_ref_of(raw: &str) -> Option<OpRef> {
    let op = parse_op_key(raw);
    (!op.object.is_empty()).then_some(op)
}

/// Lock the job row (the lock `design::track` takes again) and answer its draft revision before the write.
async fn lock_job(c: &mut PgConnection, job: Uuid) -> ApiResult<i64> {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", format!("no job {job}")))
}

/// The B2 draft write of the route of `op` on `env` (D54, per job).
fn route_write(op: &OpRef, env: &str, label: String) -> DraftWrite {
    // the ledger keeps labels to 500 characters (0044); paths and keys come from requests
    let label: String = label.chars().take(400).collect();
    DraftWrite {
        area: places::AREA_ROUTES,
        page: "J6",
        kind: KIND,
        r#ref: places::route_ref(op, env),
        label: Label::Now(label),
    }
}

async fn job_operations(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let mut view = routes_store::route_data(&mut tx, &st.design, j.job_id, DesignOf::Draft).await?;
    // the operations the job's procedures need are listed (and routable) too
    readiness::add_procedure_ops(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &view.job.job_ref(),
        &crate::design::DesignOf::Draft,
        &mut view.expected,
    )
    .await?;
    view.data.ops = view.expected.ops.clone();
    // D54: what Production runs, only when it differs from the draft (fixture `jobOperations`)
    let live: Option<Vec<OperationRoute>> = match places::current_release(&mut tx, j.job_id).await?
    {
        None => None,
        Some(_) => {
            let jr = view.job.job_ref();
            let routes = routes_store::routes_of(&mut tx, &st.design, &jr, DesignOf::Live).await?;
            (routes != view.data.routes).then_some(routes)
        }
    };
    let mut subs: Vec<String> = view
        .data
        .routes
        .iter()
        .chain(live.iter().flatten())
        .map(|r| r.updated_by.clone())
        .collect();
    subs.sort();
    subs.dedup();
    let names = inst_db::names(&mut tx, &subs).await?;
    tx.commit().await?;
    let cells = cells_of(&view.data);
    let mut out = json!({
        "job": view.job.key,
        "draft_rev": view.job.draft_rev,
        "ops": to_value(&view.data.ops)?,
        "routes": to_value(&view.data.routes)?,
        // B4 seam: operation_call over 7 days (calls_7d, error_rate, p95_ms) per op
        "stats": [],
        "instances": to_value(&view.data.instances)?,
        "implements": to_value(&view.data.implements)?,
        "draft_instances": view.data.draft_instances,
        "tested": to_value(&view.data.tested)?,
        "names": names,
        "contract": to_value(&view.data.contract)?,
        "cells": to_value(&cells)?,
    });
    if let Some(routes) = live {
        let data = RouteData {
            routes,
            ..view.data.clone()
        };
        out["live_cells"] = to_value(&cells_of(&data))?;
        out["live_routes"] = to_value(&data.routes)?;
    }
    Ok(Json(out))
}

async fn job_tools(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let job = routes_store::job_row(&mut tx, j.job_id).await?;
    let expected = routes_store::expected(&mut tx, &st.design, &job, DesignOf::Draft).await?;
    // A2: the draft declaration of each op (kind `tool_policy`), by canonical op key
    let policies: std::collections::BTreeMap<String, Value> = sqlx::query_as::<_, (String, Value)>(
        "SELECT op, policy FROM job_tool_policy WHERE job_id = $1",
    )
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    tx.commit().await?;
    let mut tools = Vec::new();
    for (o, def) in expected.ops.iter().zip(expected.defs.iter()) {
        let key = op_key(&o.op_ref());
        let attrs = &def.attrs;
        let strings = |k: &str| -> Vec<String> {
            attrs
                .get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let read = o.side_effect == "R";
        let confirm_back = attrs
            .get("confirm_back")
            .and_then(Value::as_bool)
            .unwrap_or(!read);
        let idempotency = if read {
            Value::Null
        } else {
            attrs
                .get("idempotency")
                .filter(|v| v.is_string())
                .cloned()
                .unwrap_or(Value::Null)
        };
        let verb = format!("{}.{}", o.object, o.verb);
        let transitions: Vec<Value> = expected
            .object(&o.object)
            .and_then(|item| item.fields.get("transitions"))
            .and_then(Value::as_array)
            .map(|ts| {
                ts.iter()
                    .filter(|t| t["verb"].as_str() == Some(verb.as_str()))
                    .filter(|t| t["from_status"] != t["to_status"])
                    .map(|t| {
                        json!({
                            "object": o.object,
                            "from": t["from_status"],
                            "to": t["to_status"],
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let description = if def.description.is_empty() {
            o.note.clone()
        } else {
            def.description.clone()
        };
        tools.push(json!({
            "object": o.object,
            "verb": o.verb,
            "variant": o.variant,
            "key": key,
            "description": description,
            "input_fields": strings("input_fields"),
            "output_fields": strings("output_fields"),
            "confirm_back": confirm_back,
            "idempotency": idempotency,
            "transitions": transitions,
            "policy": policies.get(&key).cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(Json(Value::Array(tools)))
}

/// A deep link is one `https://` URL without whitespace.
fn valid_deep_link(url: &str) -> bool {
    url.strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.chars().any(char::is_whitespace))
}

async fn set_route(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OpEnvPath>,
    ApiJson(body): ApiJson<RouteBody>,
) -> ApiResult<Json<Value>> {
    let env = env_of(&p.env)?;
    let not_expected = |key: &str| {
        ApiError::Missing(
            "operation_not_expected",
            format!("{key} is not an expected operation of this job."),
        )
    };
    let op = op_ref_of(&p.op).ok_or_else(|| not_expected(p.op.trim()))?;
    let key = op_key(&op);
    let star = op_key(&OpRef {
        object: op.object.clone(),
        verb: op.verb.clone(),
        variant: "*".to_owned(),
    });
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let job = routes_store::job_row(&mut tx, j.job_id).await?;
    let mut expected = routes_store::expected(&mut tx, &st.design, &job, DesignOf::Draft).await?;
    // an operation the catalogue offers and a procedure of the job needs is routable too
    readiness::add_procedure_ops(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &job.job_ref(),
        &crate::design::DesignOf::Draft,
        &mut expected,
    )
    .await?;
    let (exp, _) = expected.find(&op).ok_or_else(|| not_expected(&key))?;
    let mut target = body.target;
    match &mut target {
        RouteTarget::NotOffered if exp.required => {
            return Err(ApiError::Unprocessable(
                "required_operation",
                format!(
                    "{key} is required (R): it needs a route or a fallback; only optional (O) operations can be \
                     not offered."
                ),
            ));
        }
        RouteTarget::Instance { .. } if is_builtin_op(&op) => {
            // the agent serves these in-process; a connector instance must never stand in for them (B4 audit, agent#87)
            return Err(ApiError::Unprocessable(
                "not_implemented",
                format!(
                    "{key} is a built-in operation the agent runs itself; it cannot be routed to a connector \
                     instance ({BUILTIN_OPS_TEXT} are built-in)."
                ),
            ));
        }
        RouteTarget::Instance { instance_id, .. } => {
            let inst = inst_db::require(&mut tx, instance_id).await?;
            if inst.env != "all" && inst.env != env {
                return Err(ApiError::Unprocessable(
                    "env_mismatch",
                    format!(
                        "{} is a {} system; it cannot serve the {env} route.",
                        inst.id, inst.env
                    ),
                ));
            }
            let tpl = inst_db::template_in_scope(
                &mut tx,
                &inst.template_scope,
                &inst.template_key,
                &inst.template_version,
            )
            .await?;
            let implements = tpl.as_ref().is_some_and(|t| {
                t.ops.iter().any(|o| {
                    let k = op_key(&o.op_ref());
                    k == key || k == star
                })
            });
            if !implements {
                return Err(ApiError::Unprocessable(
                    "not_implemented",
                    format!(
                        "{}@{} does not implement {key}.",
                        inst.template_key, inst.template_version
                    ),
                ));
            }
            if inst.stub && env == "prod" {
                return Err(ApiError::Unprocessable(
                    "stub_not_allowed_in_prod",
                    "Stub instances may serve test routes only.".into(),
                ));
            }
            if env == "prod" && tpl.is_some_and(|t| t.status == TemplateStatus::Draft) {
                return Err(ApiError::Conflict(
                    "template_not_active",
                    format!(
                        "{} runs on the draft template {}@{}; it cannot serve prod routes until the template is \
                         published.",
                        inst.id, inst.template_key, inst.template_version
                    ),
                ));
            }
        }
        RouteTarget::Builtin if !is_builtin_op(&op) => {
            return Err(ApiError::Unprocessable(
                "not_implemented",
                format!("Only {BUILTIN_OPS_TEXT} are built-in operations."),
            ));
        }
        RouteTarget::AgentCases if op.object != "case" && op.object != "handover" => {
            return Err(ApiError::Unprocessable(
                "not_implemented",
                "agent_cases serves case.* and handover operations only.".into(),
            ));
        }
        RouteTarget::DeepLink { url } => {
            let trimmed = url.trim().to_owned();
            if !valid_deep_link(&trimmed) {
                return Err(ApiError::Unprocessable(
                    "invalid_url",
                    "A deep link is an https:// URL.".into(),
                ));
            }
            *url = trimmed;
        }
        _ => {}
    }
    let w = route_write(
        &op,
        env,
        format!("{key} on {env} \u{2192} {}", target.kind()),
    );
    let (job_id, by, row_op, row_target) =
        (j.job_id, j.t.caller.sub.clone(), op.clone(), target.clone());
    let out = design::track::<OperationRoute, _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            places::write_route(c, job_id, &row_op, env, &row_target, &by, None).await
        })
    })
    .await?;
    let mut e = Event::new("connector.route.set", RESOURCE, key.clone())
        .job(&j.job_key)
        .detail("env", env)
        .detail("target", target.kind())
        .detail("draft", KIND);
    if let RouteTarget::Instance { instance_id, .. } = &target {
        e = e.detail("instance", instance_id.clone());
    }
    audit::record(&mut tx, &j.t.actor(), e).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
        "route": to_value(&out.value)?,
    })))
}

async fn clear_route(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OpEnvPath>,
) -> ApiResult<Json<Value>> {
    let env = env_of(&p.env)?;
    let not_found =
        |key: &str| ApiError::Missing("route_not_found", format!("{key} has no {env} route."));
    let op = op_ref_of(&p.op).ok_or_else(|| not_found(p.op.trim()))?;
    let key = op_key(&op);
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let w = route_write(&op, env, format!("{key} on {env} cleared"));
    let (job_id, row_op, missing) = (j.job_id, op.clone(), not_found(&key));
    let out = design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            if places::delete_route(c, job_id, &row_op, env).await? {
                Ok(())
            } else {
                Err(missing)
            }
        })
    })
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("connector.route.clear", RESOURCE, key)
            .job(&j.job_key)
            .detail("env", env)
            .detail("draft", KIND),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
        "route": null,
    })))
}

async fn export_spec(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<SpecQuery>,
) -> ApiResult<Response> {
    let (format, content_type) = match q.format.as_deref() {
        Some("csv") => ("csv", "text/csv; charset=utf-8"),
        Some("md") => ("md", "text/markdown; charset=utf-8"),
        _ => {
            return Err(ApiError::Unprocessable(
                "invalid_format",
                "format is csv or md.".into(),
            ));
        }
    };
    let of = match q.of.as_deref() {
        None | Some("draft") => DesignOf::Draft,
        Some("live") => DesignOf::Live,
        Some(_) => {
            return Err(ApiError::Unprocessable(
                "invalid_of",
                "of is draft or live.".into(),
            ));
        }
    };
    let mut tx = j.begin(&st.db).await?;
    if of == DesignOf::Live && places::current_release(&mut tx, j.job_id).await?.is_none() {
        // Production runs the current release snapshot; with none there is nothing live to export (never the
        // draft's rows).
        return Err(ApiError::Conflict(
            "no_release",
            "This job has no active release yet; export the draft instead.".into(),
        ));
    }
    let input = spec_export::load(&mut tx, &st.design, j.job_id, of).await?;
    tx.commit().await?;
    let body = if format == "csv" {
        spec_export::spec_csv(&input)
    } else {
        spec_export::spec_md(&input)
    };
    let filename = spec_export::spec_filename(&j.job_key, &input.of, format);
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, content_type.to_owned()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response())
}
