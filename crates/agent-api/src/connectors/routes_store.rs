//! B7 T13: the database side of job operation routes (0094 `operation_route`, D54/D63): the working rows (the
//! draft; the test env runs them), the job's expected operations, and the J6 [`RouteData`] the cells and the
//! `/api/jobs/{job}/operations…` methods read. Every function takes a connection scoped to the tenant
//! (crate::tenancy::TenantCtx::begin); RLS confines it.
//!
//! Expected operations (B6a): the job's pinned stack compiled with crate::platform::templates::compile_pinned (the
//! selection = the job template's default selection), and the `operation_expectation` rows of the pinned tag
//! members (`required · {variant, …}` / `optional`), each `object.verb` plus its listed variants, joined with the
//! compiled operation catalogue (the variant's own row, else its `*` row). B5's `job_activity` refinement and B8's
//! later narrowing replace the expectation source; the join stays.
//!
//! Production (`DesignOf::Live`) reads the job's CURRENT release (D62): its `route` places through B2's read API
//! (crate::connectors::places::live_rows) and its pins (`job_release.pins`). A job never released has neither: Live
//! answers no routes and no expected operations (fail closed: Production never reads draft rows).
use std::collections::BTreeMap;

use agent_model::catalog::OperationRow;
use agent_model::compile::{CatalogueItem, Selection};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::instances::{self as inst_db, InstanceRow, ResultRow};
use super::model::{ConnectorTemplate, OpRef, TemplateStatus};
use super::opkey::{op_key, parse_op_key, same_op};
use super::places;
use super::resolve::{
    ContractLite, ExpectedOperation, OperationRoute, RouteData, RouteInstance, RouteTarget,
};
use crate::design::{JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::platform::db::tags as db_tags;
use crate::platform::templates::{self, TemplatePin};

/// Which design a read sees (B2 `design::DesignOf`): the draft working rows, or Production's current release
/// snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesignOf {
    Draft,
    Live,
}

/// The `jobs` columns this module reads.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub id: Uuid,
    pub key: String,
    pub pins: Value,
    pub template_layer: Option<String>,
    pub template_version: Option<i32>,
    pub draft_rev: i64,
}

/// One job of the tenant, else 404 `job_not_found`.
pub async fn job_row(conn: &mut PgConnection, job: Uuid) -> ApiResult<JobRow> {
    sqlx::query_as::<_, JobRow>(
        "SELECT id, key, pins, template_layer, template_version, draft_rev FROM jobs WHERE id = $1",
    )
    .bind(job)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::Missing("job_not_found", format!("no job {job}")))
}

/// The expected operations of a job with the compiled catalogue rows behind them.
#[derive(Debug, Clone, Default)]
pub struct ExpectedCatalogue {
    pub ops: Vec<ExpectedOperation>,
    /// `defs[i]` is the catalogue row of `ops[i]` (its own row, else its `*` row).
    pub defs: Vec<OperationRow>,
    /// The compiled `object` catalogue items (fields, states, transitions).
    pub objects: Vec<CatalogueItem>,
    /// Every operation row of the compiled catalogue (what the job's template offers, expected or not).
    pub catalogue: Vec<OperationRow>,
}

impl ExpectedCatalogue {
    /// The catalogue row of an expected op (`same_op`), when it is expected.
    pub fn find(&self, op: &OpRef) -> Option<(&ExpectedOperation, &OperationRow)> {
        self.ops
            .iter()
            .zip(self.defs.iter())
            .find(|(o, _)| same_op(&o.op_ref(), op))
    }

    /// Add `op` as a required expected operation because a procedure needs it, when the catalogue offers it (its own
    /// row, else its `*` row) and it is not expected yet. `false`: the catalogue does not offer it.
    pub fn add_needed(&mut self, op: &OpRef) -> bool {
        if self.ops.iter().any(|o| same_op(&o.op_ref(), op)) {
            return true;
        }
        let k = op_key(op);
        let star = format!("{}.{}", op.object, op.verb);
        let Some(def) = self
            .catalogue
            .iter()
            .find(|d| d.key == k)
            .or_else(|| self.catalogue.iter().find(|d| d.key == star))
            .cloned()
        else {
            return false;
        };
        self.ops.push(ExpectedOperation {
            object: op.object.clone(),
            verb: op.verb.clone(),
            variant: op.variant.clone(),
            modality: text_attr(&def.attrs, "modality", "observe"),
            is_derive: def
                .attrs
                .get("is_derive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            side_effect: text_attr(&def.attrs, "side_effect", "R"),
            identity_level: text_attr(&def.attrs, "identity_level", "0"),
            stages: def.stages.clone(),
            required: true,
            note: "Needed by a procedure".to_owned(),
        });
        self.defs.push(def);
        true
    }

    /// The fields of `object` as its catalogue item declares them (`ObjectFields.fields`).
    pub fn object(&self, object: &str) -> Option<&CatalogueItem> {
        self.objects.iter().find(|o| o.key == object)
    }
}

/// The variants an expectation summary lists (`required · {return, complaint}` → `return`, `complaint`).
fn variants_in(summary: &str) -> Vec<String> {
    let Some(open) = summary.find('{') else {
        return Vec::new();
    };
    let Some(close) = summary[open..].find('}') else {
        return Vec::new();
    };
    summary[open + 1..open + close]
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "*")
        .map(str::to_owned)
        .collect()
}

fn text_attr(attrs: &serde_json::Map<String, Value>, key: &str, default: &str) -> String {
    attrs
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

impl JobRow {
    /// The B2 job reference of this row.
    pub fn job_ref(&self) -> JobRef {
        JobRef {
            id: self.id,
            key: self.key.clone(),
        }
    }
}

/// The pins of the job's current release (`job_release.pins`); `None` = never released.
async fn live_pins(conn: &mut PgConnection, job: Uuid) -> ApiResult<Option<Value>> {
    let pins: Option<Value> = sqlx::query_scalar(
        "SELECT r.pins FROM jobs j \
           JOIN job_release r ON r.job_id = j.id AND r.release_id = j.current_release_id \
          WHERE j.id = $1",
    )
    .bind(job)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(pins)
}

/// The job's expected operations `of` the draft (its working pins) or Production (the pins of its current release;
/// never released → none), then the job's own operations (0246, design kind `job_operation`: a tenant's tools on its
/// job). An own op the compiled catalogue also offers is expected through the catalogue row (the template wins).
pub async fn expected(
    conn: &mut PgConnection,
    reg: &Registry,
    job: &JobRow,
    of: DesignOf,
) -> ApiResult<ExpectedCatalogue> {
    let mut out = expected_pinned(&mut *conn, job, of).await?;
    let design_of = match of {
        DesignOf::Draft => crate::design::DesignOf::Draft,
        DesignOf::Live => {
            if places::current_release(&mut *conn, job.id).await?.is_none() {
                return Ok(out);
            }
            crate::design::DesignOf::Live
        }
    };
    let own =
        crate::job_design::targets::job_ops::of_design(&mut *conn, reg, &job.job_ref(), &design_of)
            .await?;
    for (op, j) in own {
        let r = parse_op_key(&op);
        if r.object.is_empty() || out.ops.iter().any(|o| same_op(&o.op_ref(), &r)) {
            continue;
        }
        if out.add_needed(&r) {
            if let Some(e) = out.ops.last_mut() {
                e.note = format!("Added to the job from {}", j.template);
            }
            continue;
        }
        let row = j.row(&op);
        out.catalogue.push(row.clone());
        out.ops.push(j.expected(&op));
        out.defs.push(row);
    }
    Ok(out)
}

/// [`expected`] of the job's pinned template only.
async fn expected_pinned(
    conn: &mut PgConnection,
    job: &JobRow,
    of: DesignOf,
) -> ApiResult<ExpectedCatalogue> {
    let pins = match of {
        DesignOf::Draft => job.pins.clone(),
        DesignOf::Live => match live_pins(&mut *conn, job.id).await? {
            Some(p) => p,
            None => return Ok(ExpectedCatalogue::default()),
        },
    };
    let pins: Vec<TemplatePin> = serde_json::from_value(pins).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!("job {} has unreadable pins: {e}", job.key))
    })?;
    if pins.is_empty() {
        return Ok(ExpectedCatalogue::default());
    }
    // D132.2: the job's own chosen selection compiles its operations; the template default only while no
    // `job_target` selection exists (a blank job has no template at all).
    let stored: Option<Option<Value>> =
        sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
            .bind(job.id)
            .fetch_optional(&mut *conn)
            .await?;
    let selection = match stored.flatten() {
        Some(v) => serde_json::from_value::<Selection>(v).map_err(|e| {
            ApiError::Internal(anyhow::anyhow!(
                "job {} has an unreadable job_target.selection: {e}",
                job.key
            ))
        })?,
        None => match (&job.template_layer, job.template_version) {
            (Some(layer), Some(version)) => {
                match templates::resolve_template(&mut *conn, layer, version).await {
                    Ok(r) => r.default_selection,
                    Err(ApiError::Missing(..) | ApiError::Conflict(..)) => Selection::default(),
                    Err(e) => return Err(e),
                }
            }
            _ => Selection::default(),
        },
    };
    let job_key = job
        .template_layer
        .as_deref()
        .and_then(|l| l.strip_prefix("job/"))
        .map(|k| k.split('/').next().unwrap_or(k).to_owned());
    let compiled =
        match templates::compile_pinned(&mut *conn, &pins, &selection, job_key.as_deref()).await? {
            Ok(c) => c,
            Err(errors) => {
                let msg = errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                return Err(ApiError::Unprocessable(
                    "compile_failed",
                    format!("The job's pinned template does not compile: {msg}"),
                ));
            }
        };

    // operation_expectation rows of the pinned tags, folded by key over layers in key order (bytewise)
    let mut members = Vec::new();
    for pin in &pins {
        members.extend(
            db_tags::tag_members(&mut *conn, &pin.layer, pin.version)
                .await?
                .into_iter()
                .filter(|r| r.kind == "operation_expectation"),
        );
    }
    members.sort_by(|a, b| {
        (a.layer_key.as_str(), a.key.as_str()).cmp(&(b.layer_key.as_str(), b.key.as_str()))
    });
    let mut folded: Vec<(String, String)> = Vec::new();
    for r in &members {
        let at = folded.iter().position(|(k, _)| *k == r.key);
        if r.op == "tombstone" {
            if let Some(i) = at {
                folded.remove(i);
            }
            continue;
        }
        let summary = r
            .fields
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match at {
            Some(i) => folded[i].1 = summary,
            None => folded.push((r.key.clone(), summary)),
        }
    }

    let catalogue = &compiled.catalogue.operations;
    let mut out = ExpectedCatalogue {
        objects: compiled.catalogue.objects.clone(),
        catalogue: catalogue.clone(),
        ..ExpectedCatalogue::default()
    };
    for (key, summary) in &folded {
        let base = parse_op_key(key);
        if base.object.is_empty() {
            continue;
        }
        let required = summary.trim_start().starts_with("required");
        let mut variants = vec!["*".to_owned()];
        variants.extend(variants_in(summary));
        for v in variants {
            let op = OpRef {
                object: base.object.clone(),
                verb: base.verb.clone(),
                variant: v,
            };
            let k = op_key(&op);
            let star = format!("{}.{}", op.object, op.verb);
            let Some(def) = catalogue
                .iter()
                .find(|d| d.key == k)
                .or_else(|| catalogue.iter().find(|d| d.key == star))
            else {
                // not in the compiled catalogue: nothing can describe or route it
                continue;
            };
            if out.ops.iter().any(|o| same_op(&o.op_ref(), &op)) {
                continue;
            }
            out.ops.push(ExpectedOperation {
                object: op.object.clone(),
                verb: op.verb.clone(),
                variant: op.variant.clone(),
                modality: text_attr(&def.attrs, "modality", "observe"),
                is_derive: def
                    .attrs
                    .get("is_derive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                side_effect: text_attr(&def.attrs, "side_effect", "R"),
                identity_level: text_attr(&def.attrs, "identity_level", "0"),
                stages: def.stages.clone(),
                required,
                note: String::new(),
            });
            out.defs.push(def.clone());
        }
    }
    Ok(out)
}

/// `(object, verb, variant, env, target_kind, instance_id, detail, url, updated_by, updated_at)`.
pub type RouteDbRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    DateTime<Utc>,
);

fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// A stored route row as the J6 `OperationRoute`.
pub fn route_of(row: RouteDbRow) -> ApiResult<OperationRoute> {
    let (object, verb, variant, env, kind, instance_id, detail, url, updated_by, updated_at) = row;
    let target = match kind.as_str() {
        "instance" => RouteTarget::Instance {
            instance_id: instance_id.unwrap_or_default(),
            detail: detail.filter(|d| !d.is_empty()),
        },
        "builtin" => RouteTarget::Builtin,
        "agent_cases" => RouteTarget::AgentCases,
        "deep_link" => RouteTarget::DeepLink {
            url: url.unwrap_or_default(),
        },
        "not_offered" => RouteTarget::NotOffered,
        other => {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "operation_route has an unknown target_kind {other:?}"
            )));
        }
    };
    Ok(OperationRoute {
        object,
        verb,
        variant,
        env,
        target,
        updated_by,
        updated_at: iso(updated_at),
    })
}

/// The job's working route rows (the draft), by (object, verb, variant, env).
pub async fn working_routes(conn: &mut PgConnection, job: Uuid) -> ApiResult<Vec<OperationRoute>> {
    let rows: Vec<RouteDbRow> = sqlx::query_as(
        "SELECT object, verb, variant, env, target_kind, instance_id, detail, url, updated_by, updated_at \
           FROM operation_route WHERE job_id = $1 \
          ORDER BY object COLLATE \"C\", verb COLLATE \"C\", variant COLLATE \"C\", env COLLATE \"C\"",
    )
    .bind(job)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(route_of).collect()
}

/// The routes of `job` `of` the draft (working rows) or Production (its current release's `route` places; never
/// released → none).
pub async fn routes_of(
    conn: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: DesignOf,
) -> ApiResult<Vec<OperationRoute>> {
    match of {
        DesignOf::Draft => working_routes(conn, job.id).await,
        DesignOf::Live => Ok(places::live_rows(conn, reg, job, "route")
            .await?
            .map(|rows| places::routes_of(&rows))
            .transpose()?
            .unwrap_or_default()),
    }
}

/// The routes `job` runs on `env`, `of` the draft (working rows) or Production (the current release snapshot; never
/// released → none: Production never runs a draft row). B5's toolset resolver and B4's `RouteResolver` read routes
/// only through here.
pub async fn effective(
    conn: &mut PgConnection,
    reg: &Registry,
    job: Uuid,
    env: &str,
    of: DesignOf,
) -> ApiResult<Vec<OperationRoute>> {
    let jr = job_row(&mut *conn, job).await?.job_ref();
    Ok(routes_of(conn, reg, &jr, of)
        .await?
        .into_iter()
        .filter(|r| r.env == env)
        .collect())
}

/// One tenant mapping override (0094 `field_mapping_override`, T12 owns its writes).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OverrideRow {
    pub instance_id: String,
    pub op: String,
    pub field: String,
    pub path: String,
}

/// The tenant's working mapping overrides (the draft).
pub async fn overrides(conn: &mut PgConnection) -> ApiResult<Vec<OverrideRow>> {
    Ok(sqlx::query_as::<_, OverrideRow>(
        "SELECT instance_id, op, field, path FROM field_mapping_override \
          ORDER BY instance_id COLLATE \"C\", op COLLATE \"C\", field COLLATE \"C\"",
    )
    .fetch_all(&mut *conn)
    .await?)
}

/// The mapping overrides of `job` `of` the draft (the tenant's working rows) or Production (its current release's
/// `mapping_override` places; never released → none).
pub async fn overrides_of(
    conn: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: DesignOf,
) -> ApiResult<Vec<OverrideRow>> {
    match of {
        DesignOf::Draft => overrides(conn).await,
        DesignOf::Live => {
            let Some(live) = places::live_connectors(conn, reg, job).await? else {
                return Ok(Vec::new());
            };
            let mut out: Vec<OverrideRow> = live
                .overrides
                .into_iter()
                .map(|o| OverrideRow {
                    instance_id: o.instance,
                    op: o.op,
                    field: o.field,
                    path: o.path,
                })
                .collect();
            out.sort_by(|a, b| {
                (&a.instance_id, &a.op, &a.field).cmp(&(&b.instance_id, &b.op, &b.field))
            });
            Ok(out)
        }
    }
}

/// Everything J6 shows for one job, with the rows behind it.
#[derive(Debug, Clone)]
pub struct JobRouteView {
    pub job: JobRow,
    pub expected: ExpectedCatalogue,
    pub data: RouteData,
    pub instances: Vec<InstanceRow>,
    /// The pinned template of each instance id (absent when it no longer exists).
    pub templates: BTreeMap<String, ConnectorTemplate>,
    pub results: Vec<ResultRow>,
}

impl JobRouteView {
    /// The template ops `instance` implements (`object.verb{variant}`).
    pub fn implements(&self, instance: &str) -> Vec<String> {
        self.data
            .implements
            .get(instance)
            .cloned()
            .unwrap_or_default()
    }
}

/// The J6 [`RouteData`] of `job` `of` the draft or Production (its current release: expected ops of its pins, its
/// routes; never released → neither). Instances, their templates and contract results are live in both (D69).
pub async fn route_data(
    conn: &mut PgConnection,
    reg: &Registry,
    job: Uuid,
    of: DesignOf,
) -> ApiResult<JobRouteView> {
    let job = job_row(&mut *conn, job).await?;
    let expected = expected(&mut *conn, reg, &job, of).await?;
    let routes = routes_of(&mut *conn, reg, &job.job_ref(), of).await?;
    view_of(conn, job, expected, routes).await
}

/// The J6 view of `job` with `expected` ops and `routes`, over the tenant's systems as they are now.
pub async fn view_of(
    conn: &mut PgConnection,
    job: JobRow,
    expected: ExpectedCatalogue,
    routes: Vec<OperationRoute>,
) -> ApiResult<JobRouteView> {
    let instances = inst_db::all(&mut *conn).await?;
    let results = inst_db::results(&mut *conn, None).await?;
    let mut templates = BTreeMap::new();
    let mut data = RouteData {
        ops: expected.ops.clone(),
        routes,
        ..RouteData::default()
    };
    let mut contract = BTreeMap::new();
    for i in &instances {
        let tpl = inst_db::template_in_scope(
            &mut *conn,
            &i.template_scope,
            &i.template_key,
            &i.template_version,
        )
        .await?;
        let implements: Vec<String> = tpl
            .as_ref()
            .map(|t| t.ops.iter().map(|o| op_key(&o.op_ref())).collect())
            .unwrap_or_default();
        if tpl
            .as_ref()
            .is_some_and(|t| t.status == TemplateStatus::Draft)
        {
            data.draft_instances.push(i.id.clone());
        }
        data.implements.insert(i.id.clone(), implements);
        let mine: Vec<&ResultRow> = results.iter().filter(|r| r.instance_id == i.id).collect();
        data.tested.insert(
            i.id.clone(),
            mine.iter()
                .filter(|r| r.outcome == "green")
                .map(|r| r.op.clone())
                .collect(),
        );
        contract.insert(
            i.id.clone(),
            mine.iter()
                .map(|r| ContractLite {
                    op: r.op.clone(),
                    outcome: r.outcome.clone(),
                    code: r.code.clone(),
                    note: r.note.clone(),
                    at: r.at.map(iso),
                })
                .collect(),
        );
        data.instances.push(RouteInstance {
            id: i.id.clone(),
            label: i.label.clone(),
            env: i.env.clone(),
            stub: i.stub,
            platform: i.platform,
            template_key: i.template_key.clone(),
            template_version: i.template_version.clone(),
            status: i.status.clone(),
        });
        if let Some(t) = tpl {
            templates.insert(i.id.clone(), t);
        }
    }
    data.contract = Some(contract);
    Ok(JobRouteView {
        job,
        expected,
        data,
        instances,
        templates,
        results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_of_an_expectation_summary() {
        assert_eq!(
            variants_in("required \u{b7} {return, warranty_claim}"),
            vec!["return".to_owned(), "warranty_claim".to_owned()]
        );
        assert!(variants_in("optional").is_empty());
        assert!(variants_in("required \u{b7} {*}").is_empty());
    }
}
