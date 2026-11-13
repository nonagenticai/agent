//! B7's three design places on B2's draft ledger (spec B2 §2.5 rows 15, 20, 21; plan B7 "Draft vs live read model",
//! T11–T14): the Rust form of the fixture `TARGETS` entries `route`, `mapping_override` and `tenant_template`
//! (`web/src/lib/fixtures/ledger.ts`). Each copies the fixture's `key`, `proj`, `text` and `places`.
//!
//! | kind | scope | key | working rows |
//! |---|---|---|---|
//! | `route` | job | `route:<object.verb{variant}>:<env>` | `operation_route` |
//! | `mapping_override` | tenant | `mapping:<instance>:<op>:<field>` | `field_mapping_override` |
//! | `tenant_template` | tenant | `tenant-template:<key>@<version>` | `tenant_connector_template(_op)` |
//!
//! This module is the only writer of those four tables (tests/design_antidrift.rs): the targets' `set` (discard,
//! follow-live, restore) and the handlers' `mutate` closures passed to `design::track` / `design::track_tenant` call
//! the write functions here. The result columns `validation` / `contract` of a tenant template are not design and are
//! written by crate::connectors::tenant_templates::set_results.
//!
//! Production reads ([`live_rows`], [`live_connectors`]) go through B2's read API (`design::values_of_kind` with
//! `DesignOf::Live`) over the job's CURRENT release only: a job never released has no Production connectors (the
//! callers answer `no_release`), and a release whose snapshot does not cover a kind holds none of its places. Neither
//! ever falls back to the working (draft) rows.
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::model::{OpRef, TenantTemplateOp};
use super::opkey::op_key;
use super::resolve::{OperationRoute, RouteTarget};
use super::routes_store::{RouteDbRow, route_of};
use super::tenant_templates::{self as tt, TemplateRow};
use crate::design::{
    self, DesignOf, DesignRow, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap,
};
use crate::error::{ApiError, ApiResult};

/// Design area of the tenant-wide connection kinds (ledger `area`).
pub const AREA_CONNECTIONS: &str = "connections";
/// Design area of a route write.
pub const AREA_ROUTES: &str = "routes";
/// `updated_by` / `created_by` of a working row that `set` creates and whose value does not name its writer.
pub const SET_BY: &str = "design";

/// Register B7's three targets (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(RoutePlace));
    r.register(Arc::new(MappingOverridePlace));
    r.register(Arc::new(TenantTemplatePlace));
}

// ---- helpers --------------------------------------------------------------------------------------------------------

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn job_of(at: Place<'_>) -> ApiResult<Uuid> {
    at.job
        .map(|j| j.id)
        .ok_or_else(|| internal("a route is read and written at a job place".into()))
}

fn field<'r>(r: &'r DraftRef, f: &str) -> &'r str {
    r.get(f).and_then(Value::as_str).unwrap_or("")
}

/// A ref of string fields.
pub fn draft_ref(fields: &[(&str, &str)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in fields {
        m.insert((*k).to_string(), Value::String((*v).to_string()));
    }
    m
}

/// The variant of a ref: `*` when absent, null or empty.
fn variant_of(r: &DraftRef) -> &str {
    match field(r, "variant") {
        "" => "*",
        v => v,
    }
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

/// An RFC 3339 `updated_at` of a value, if it has one.
fn at_of(v: &Value) -> Option<DateTime<Utc>> {
    v.get("updated_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// A non-blank `updated_by` of a value, else [`SET_BY`].
fn by_of(v: &Value) -> String {
    v.get("updated_by")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(SET_BY)
        .to_string()
}

// ---- route (ledger.ts `route`) --------------------------------------------------------------------------------------

/// The ref of a route place.
pub fn route_ref(op: &OpRef, env: &str) -> DraftRef {
    let variant = if op.variant.is_empty() {
        "*"
    } else {
        op.variant.as_str()
    };
    draft_ref(&[
        ("object", op.object.as_str()),
        ("verb", op.verb.as_str()),
        ("variant", variant),
        ("env", env),
    ])
}

/// Insert or replace the working route of `op` on `env` of `job`, stamped `by` at `at` (now when `None`); the row as
/// stored.
pub async fn write_route(
    c: &mut PgConnection,
    job: Uuid,
    op: &OpRef,
    env: &str,
    target: &RouteTarget,
    by: &str,
    at: Option<DateTime<Utc>>,
) -> ApiResult<OperationRoute> {
    let (instance_id, detail, url) = match target {
        RouteTarget::Instance {
            instance_id,
            detail,
        } => (
            Some(instance_id.as_str()),
            detail.as_deref().unwrap_or(""),
            None,
        ),
        // `detail` is NOT NULL DEFAULT '' (0094): an explicit NULL would not take the default.
        RouteTarget::DeepLink { url } => (None, "", Some(url.as_str())),
        _ => (None, "", None),
    };
    let variant = if op.variant.is_empty() {
        "*"
    } else {
        op.variant.as_str()
    };
    let row: RouteDbRow = sqlx::query_as(
        "INSERT INTO operation_route \
                (job_id, object, verb, variant, env, target_kind, instance_id, detail, url, updated_by, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE($11, now())) \
         ON CONFLICT (tenant_id, job_id, object, verb, variant, env) DO UPDATE \
            SET target_kind = EXCLUDED.target_kind, instance_id = EXCLUDED.instance_id, \
                detail = EXCLUDED.detail, url = EXCLUDED.url, updated_by = EXCLUDED.updated_by, \
                updated_at = EXCLUDED.updated_at \
         RETURNING object, verb, variant, env, target_kind, instance_id, detail, url, updated_by, updated_at",
    )
    .bind(job)
    .bind(&op.object)
    .bind(&op.verb)
    .bind(variant)
    .bind(env)
    .bind(target.kind())
    .bind(instance_id)
    .bind(detail)
    .bind(url)
    .bind(by)
    .bind(at)
    .fetch_one(&mut *c)
    .await?;
    route_of(row)
}

/// Delete the working route of `op` on `env` of `job`; `false` when there was none.
pub async fn delete_route(
    c: &mut PgConnection,
    job: Uuid,
    op: &OpRef,
    env: &str,
) -> ApiResult<bool> {
    let variant = if op.variant.is_empty() {
        "*"
    } else {
        op.variant.as_str()
    };
    let done = sqlx::query(
        "DELETE FROM operation_route \
          WHERE job_id = $1 AND object = $2 AND verb = $3 AND variant = $4 AND env = $5",
    )
    .bind(job)
    .bind(&op.object)
    .bind(&op.verb)
    .bind(variant)
    .bind(env)
    .execute(&mut *c)
    .await?;
    Ok(done.rows_affected() > 0)
}

fn op_of_ref(r: &DraftRef) -> OpRef {
    OpRef {
        object: field(r, "object").to_string(),
        verb: field(r, "verb").to_string(),
        variant: variant_of(r).to_string(),
    }
}

/// One J6 route of a job (`operation_route`); the value is the fixture `OperationRoute` `{object, verb, variant, env,
/// target, updated_by, updated_at}`; what counts is the `target`.
pub struct RoutePlace;

impl DesignTarget for RoutePlace {
    fn kind(&self) -> &'static str {
        "route"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("route:{}:{}", op_key(&op_of_ref(r)), field(r, "env"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let op = op_of_ref(r);
            let row: Option<RouteDbRow> = sqlx::query_as(
                "SELECT object, verb, variant, env, target_kind, instance_id, detail, url, updated_by, updated_at \
                   FROM operation_route \
                  WHERE job_id = $1 AND object = $2 AND verb = $3 AND variant = $4 AND env = $5",
            )
            .bind(job)
            .bind(&op.object)
            .bind(&op.verb)
            .bind(&op.variant)
            .bind(field(r, "env"))
            .fetch_optional(&mut *c)
            .await?;
            match row {
                None => Ok(None),
                Some(row) => {
                    let route = route_of(row)?;
                    serde_json::to_value(&route)
                        .map(Some)
                        .map_err(|e| internal(format!("a route does not serialise: {e}")))
                }
            }
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let op = op_of_ref(r);
            let env = field(r, "env");
            let Some(v) = v.as_ref().filter(|x| !x.is_null()) else {
                delete_route(&mut *c, job, &op, env).await?;
                return Ok(());
            };
            let target: RouteTarget = v
                .get("target")
                .cloned()
                .ok_or_else(|| internal("a route value has no target".into()))
                .and_then(|t| {
                    serde_json::from_value(t)
                        .map_err(|e| internal(format!("a route target does not parse: {e}")))
                })?;
            write_route(&mut *c, job, &op, env, &target, &by_of(v), at_of(v)).await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let rows: Vec<(String, String, String, String)> = sqlx::query_as(
                "SELECT object, verb, variant, env FROM operation_route WHERE job_id = $1 \
                  ORDER BY object COLLATE \"C\", verb COLLATE \"C\", variant COLLATE \"C\", env COLLATE \"C\"",
            )
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                rows.iter()
                    .map(|(o, vb, va, e)| {
                        draft_ref(&[
                            ("object", o.as_str()),
                            ("verb", vb.as_str()),
                            ("variant", va.as_str()),
                            ("env", e.as_str()),
                        ])
                    })
                    .collect(),
            ))
        })
    }
    /// The target (fixture `proj`).
    fn proj(&self, v: &Snap) -> Value {
        v.as_ref()
            .and_then(|x| x.get("target"))
            .cloned()
            .unwrap_or(Value::Null)
    }
    /// Fixture `routeText`: `system <id>`, `link <url>`, else the target kind; nothing → `None`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let t = v.as_ref()?.get("target")?;
        let kind = t.get("kind").and_then(Value::as_str).unwrap_or("");
        let s = |f: &str| t.get(f).and_then(Value::as_str).unwrap_or("").to_string();
        Some(match kind {
            "instance" => format!("system {}", s("instance_id")),
            "deep_link" => format!("link {}", s("url")),
            other => other.to_string(),
        })
    }
}

// ---- mapping_override (ledger.ts `mapping_override`) ----------------------------------------------------------------

/// The ref of a mapping-override place.
pub fn override_ref(instance: &str, op: &str, field: &str) -> DraftRef {
    draft_ref(&[("instance", instance), ("op", op), ("field", field)])
}

/// Insert or replace the working override of `(instance, op, field)`, stamped `by` at `at` (now when `None`).
#[allow(clippy::too_many_arguments)]
pub async fn write_override(
    c: &mut PgConnection,
    instance: &str,
    op: &str,
    field: &str,
    default_path: &str,
    path: &str,
    by: &str,
    at: Option<DateTime<Utc>>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO field_mapping_override (instance_id, op, field, default_path, path, updated_by, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, now())) \
         ON CONFLICT (tenant_id, instance_id, op, field) DO UPDATE \
            SET default_path = EXCLUDED.default_path, path = EXCLUDED.path, \
                updated_by = EXCLUDED.updated_by, updated_at = EXCLUDED.updated_at",
    )
    .bind(instance)
    .bind(op)
    .bind(field)
    .bind(default_path)
    .bind(path)
    .bind(by)
    .bind(at)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Delete the working override of `(instance, op, field)`; whether one existed.
pub async fn delete_override(
    c: &mut PgConnection,
    instance: &str,
    op: &str,
    field: &str,
) -> ApiResult<bool> {
    let n = sqlx::query(
        "DELETE FROM field_mapping_override WHERE instance_id = $1 AND op = $2 AND field = $3",
    )
    .bind(instance)
    .bind(op)
    .bind(field)
    .execute(&mut *c)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// A tenant's JSONPath override of one field of one op on one system (`field_mapping_override`); the value is the
/// fixture `FieldMappingOverride` `{op, field, default_path, path, updated_by, updated_at}`; what counts is the path.
pub struct MappingOverridePlace;

impl DesignTarget for MappingOverridePlace {
    fn kind(&self) -> &'static str {
        "mapping_override"
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn key(&self, r: &DraftRef) -> String {
        format!(
            "mapping:{}:{}:{}",
            field(r, "instance"),
            field(r, "op"),
            field(r, "field")
        )
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        _at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row: Option<(String, String, String, String, String, DateTime<Utc>)> =
                sqlx::query_as(
                    "SELECT op, field, default_path, path, updated_by, updated_at FROM field_mapping_override \
                      WHERE instance_id = $1 AND op = $2 AND field = $3",
                )
                .bind(field(r, "instance"))
                .bind(field(r, "op"))
                .bind(field(r, "field"))
                .fetch_optional(&mut *c)
                .await?;
            Ok(row.map(|(op, f, default_path, path, by, at)| {
                json!({
                    "op": op,
                    "field": f,
                    "default_path": default_path,
                    "path": path,
                    "updated_by": by,
                    "updated_at": iso(at),
                })
            }))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        _at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let (instance, op, f) = (field(r, "instance"), field(r, "op"), field(r, "field"));
            let Some(v) = v.as_ref().filter(|x| !x.is_null()) else {
                delete_override(&mut *c, instance, op, f).await?;
                return Ok(());
            };
            let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
            let path = text("path")
                .ok_or_else(|| internal("a mapping override value has no path".into()))?;
            let default_path = text("default_path").unwrap_or_default();
            write_override(
                &mut *c,
                instance,
                op,
                f,
                &default_path,
                &path,
                &by_of(v),
                at_of(v),
            )
            .await
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT instance_id, op, field FROM field_mapping_override \
                  ORDER BY instance_id COLLATE \"C\", op COLLATE \"C\", field COLLATE \"C\"",
            )
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                rows.iter().map(|(i, o, f)| override_ref(i, o, f)).collect(),
            ))
        })
    }
    /// The path (fixture `proj`).
    fn proj(&self, v: &Snap) -> Value {
        v.as_ref()
            .and_then(|x| x.get("path"))
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn text(&self, v: &Snap, r: &DraftRef) -> Option<String> {
        let (op, f) = (field(r, "op"), field(r, "field"));
        Some(
            match v
                .as_ref()
                .and_then(|x| x.get("path"))
                .and_then(Value::as_str)
            {
                Some(path) => format!("{op} {f} \u{2190} {path}"),
                None => format!("{op} {f}: template default"),
            },
        )
    }
}

// ---- tenant_template (ledger.ts `tenant_template`) ------------------------------------------------------------------

/// The ref of a tenant-template place.
pub fn template_ref(key: &str, version: &str) -> DraftRef {
    draft_ref(&[("key", key), ("version", version)])
}

/// The `task_tools` of a design value (a non-array, or a non-string entry, is skipped).
fn task_tools_of(v: &Value) -> Vec<String> {
    v.get("task_tools")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The design value of a tenant template version: everything but the results (`validation`, `contract`) and the
/// bookkeeping (`tenant_id`, `created_by`, `updated_at`).
pub fn template_value(row: &TemplateRow) -> Value {
    let mut v = json!({
        "key": row.key,
        "version": row.version,
        "label": row.label,
        "kind": row.kind,
        "base": row.base,
        "transport": row.transport,
        "auth_kind": row.auth_kind,
        "auth_params": row.auth_params,
        "credential_schema": row.credential_schema,
        "status": row.status,
        "description": row.description,
        "ops": row.ops,
    });
    // An empty list is not part of the value: a template without task tools round-trips exactly as it did before
    // the field existed (the design contract compares values, not their storage), and 0250's default is empty.
    if !row.task_tools.is_empty() {
        v["task_tools"] = json!(row.task_tools);
    }
    v
}

/// A template version as Production runs it, from its snapshot value ([`template_value`]); `tenant` = the owner.
pub fn template_row_of(tenant: Uuid, v: &Value) -> ApiResult<TemplateRow> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(TemplateRow {
        tenant_id: tenant,
        key: text("key"),
        version: text("version"),
        label: text("label"),
        kind: text("kind"),
        base: text("base"),
        transport: text("transport"),
        auth_kind: text("auth_kind"),
        auth_params: v.get("auth_params").cloned().unwrap_or_else(|| json!({})),
        credential_schema: v
            .get("credential_schema")
            .cloned()
            .unwrap_or_else(|| json!([])),
        task_tools: task_tools_of(v),
        status: text("status"),
        description: text("description"),
        validation: None,
        contract: None,
        created_by: SET_BY.to_string(),
        updated_at: Utc::now(),
        ops: v.get("ops").cloned().unwrap_or_else(|| json!([])),
    })
}

/// A new draft version (no operations).
pub struct NewTemplate {
    pub tenant: Uuid,
    pub key: String,
    pub version: String,
    pub label: String,
    pub kind: String,
    /// `rest` | `mcp`.
    pub transport: String,
    pub auth_kind: String,
    pub auth_params: Value,
    pub credential_schema: Value,
    /// The operations this template runs as durable MCP tasks (`object.verb` keys).
    pub task_tools: Vec<String>,
    pub description: String,
    pub created_by: String,
}

/// Insert a draft version (no operations).
pub async fn insert_template(c: &mut PgConnection, r: &NewTemplate) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO tenant_connector_template \
                (tenant_id, key, version, label, kind, base, transport, auth_kind, auth_params, credential_schema, \
                 status, description, created_by, task_tools) \
         VALUES ($1, $2, $3, $4, $5, $6, $12, $7, $8, $9, 'draft', $10, $11, $13)",
    )
    .bind(r.tenant)
    .bind(&r.key)
    .bind(&r.version)
    .bind(&r.label)
    .bind(&r.kind)
    .bind(tt::BASE)
    .bind(&r.auth_kind)
    .bind(&r.auth_params)
    .bind(&r.credential_schema)
    .bind(&r.description)
    .bind(&r.created_by)
    .bind(&r.transport)
    .bind(&r.task_tools)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Save the design fields of a draft version; its validation and contract results are reset (the 0092 guard
/// refuses it for any other status).
#[allow(clippy::too_many_arguments)]
pub async fn update_template(
    c: &mut PgConnection,
    key: &str,
    version: &str,
    label: &str,
    description: &str,
    credential_schema: &Value,
    auth_params: &Value,
    task_tools: &[String],
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE tenant_connector_template \
            SET label = $3, description = $4, credential_schema = $5, auth_params = $6, task_tools = $7, \
                validation = NULL, contract = NULL, updated_at = now() \
          WHERE key = $1 AND version = $2",
    )
    .bind(key)
    .bind(version)
    .bind(label)
    .bind(description)
    .bind(credential_schema)
    .bind(auth_params)
    .bind(task_tools)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Move a version to `status` (the 0092 guard allows draft → any, active → retired).
pub async fn set_template_status(
    c: &mut PgConnection,
    key: &str,
    version: &str,
    status: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE tenant_connector_template SET status = $3, updated_at = now() \
          WHERE key = $1 AND version = $2",
    )
    .bind(key)
    .bind(version)
    .bind(status)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Replace the operations of a draft version with `ops` (the 0092 guard refuses it for any other status).
pub async fn replace_ops(
    c: &mut PgConnection,
    tenant: Uuid,
    key: &str,
    version: &str,
    ops: &[TenantTemplateOp],
) -> ApiResult<()> {
    let mut raw = Vec::with_capacity(ops.len());
    for o in ops {
        let to = |what: &str, v: Result<Value, serde_json::Error>| {
            v.map_err(|e| internal(format!("{what} of {}.{}: {e}", o.object, o.verb)))
        };
        raw.push(json!({
            "object": o.object,
            "verb": o.verb,
            "variant": o.variant,
            "method": o.method,
            "path": o.path,
            "request": o.request,
            "field_map": to("field_map", serde_json::to_value(&o.field_map))?,
            "status_map": to("status_map", serde_json::to_value(&o.status_map))?,
            "idempotency": to("idempotency", serde_json::to_value(&o.idempotency))?,
            "input_types": to("input_types", serde_json::to_value(&o.input_types))?,
            "def": to("def", serde_json::to_value(&o.def))?,
        }));
    }
    replace_ops_raw(c, tenant, key, version, &raw).await
}

/// Replace the operations of a draft version with `ops` as stored JSON (`{object, verb, variant, method, path,
/// request, field_map, status_map, idempotency, input_types, def}`, author order).
pub async fn replace_ops_raw(
    c: &mut PgConnection,
    tenant: Uuid,
    key: &str,
    version: &str,
    ops: &[Value],
) -> ApiResult<()> {
    sqlx::query("DELETE FROM tenant_connector_template_op WHERE key = $1 AND version = $2")
        .bind(key)
        .bind(version)
        .execute(&mut *c)
        .await?;
    for (i, o) in ops.iter().enumerate() {
        let text = |k: &str, d: &str| o.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
        let json_or = |k: &str, d: Value| o.get(k).filter(|x| !x.is_null()).cloned().unwrap_or(d);
        let ord = i32::try_from(i).map_err(|e| internal(format!("op position: {e}")))?;
        sqlx::query(
            "INSERT INTO tenant_connector_template_op \
                    (tenant_id, key, version, object, verb, variant, ord, method, path, request, field_map, \
                     status_map, idempotency, input_types, def) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(tenant)
        .bind(key)
        .bind(version)
        .bind(text("object", ""))
        .bind(text("verb", ""))
        .bind(text("variant", "*"))
        .bind(ord)
        .bind(text("method", "GET"))
        .bind(text("path", ""))
        .bind(text("request", ""))
        .bind(json_or("field_map", json!([])))
        .bind(json_or("status_map", json!([])))
        .bind(json_or("idempotency", json!({"kind": "none"})))
        .bind(json_or("input_types", json!({})))
        .bind(o.get("def").filter(|x| !x.is_null()).cloned())
        .execute(&mut *c)
        .await?;
    }
    Ok(())
}

/// A tenant-private connector template version (`tenant_connector_template` + its ops); the value is
/// [`template_value`]; what counts is the fixture `proj` `[label, description, credential_schema, ops, status]`.
///
/// `set` (discard, restore) writes the design fields of a DRAFT version, then its status; a published version only
/// changes status (0092: active → retired). Removing a version deletes a draft that no system pins (409
/// `template_in_use` otherwise); a published version is never deleted (the 0092 guard refuses it).
pub struct TenantTemplatePlace;

impl DesignTarget for TenantTemplatePlace {
    fn kind(&self) -> &'static str {
        "tenant_template"
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn key(&self, r: &DraftRef) -> String {
        format!(
            "tenant-template:{}@{}",
            field(r, "key"),
            field(r, "version")
        )
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        _at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            Ok(tt::one(&mut *c, field(r, "key"), field(r, "version"))
                .await?
                .map(|row| template_value(&row)))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let (key, version) = (field(r, "key"), field(r, "version"));
            let existing = tt::one(&mut *c, key, version).await?;
            let Some(v) = v.as_ref().filter(|x| !x.is_null()) else {
                let Some(row) = existing else {
                    return Ok(());
                };
                let users = tt::pinned_by(&mut *c, key, version).await?;
                if !users.is_empty() {
                    return Err(ApiError::Conflict(
                        "template_in_use",
                        format!(
                            "{key}@{version} is pinned by {}. Re-pin those systems first.",
                            users.join(", ")
                        ),
                    ));
                }
                if row.status == "draft" {
                    sqlx::query(
                        "DELETE FROM tenant_connector_template_op WHERE key = $1 AND version = $2",
                    )
                    .bind(key)
                    .bind(version)
                    .execute(&mut *c)
                    .await?;
                }
                // the 0092 guard refuses deleting a published version (template_immutable)
                sqlx::query(
                    "DELETE FROM tenant_connector_template WHERE key = $1 AND version = $2",
                )
                .bind(key)
                .bind(version)
                .execute(&mut *c)
                .await?;
                return Ok(());
            };
            let text = |k: &str, d: &str| v.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
            let json_or =
                |k: &str, d: Value| v.get(k).filter(|x| !x.is_null()).cloned().unwrap_or(d);
            let status = text("status", "draft");
            let ops: Vec<Value> = v
                .get("ops")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let current = match existing {
                None => {
                    insert_template(
                        &mut *c,
                        &NewTemplate {
                            tenant: at.tenant,
                            key: key.to_string(),
                            version: version.to_string(),
                            label: text("label", key),
                            kind: text("kind", "generic"),
                            transport: text("transport", "rest"),
                            auth_kind: text("auth_kind", "none"),
                            auth_params: json_or("auth_params", json!({})),
                            credential_schema: json_or("credential_schema", json!([])),
                            task_tools: task_tools_of(v),
                            description: text("description", ""),
                            created_by: SET_BY.to_string(),
                        },
                    )
                    .await?;
                    replace_ops_raw(&mut *c, at.tenant, key, version, &ops).await?;
                    "draft".to_string()
                }
                Some(row) if row.status == "draft" => {
                    update_template(
                        &mut *c,
                        key,
                        version,
                        &text("label", &row.label),
                        &text("description", ""),
                        &json_or("credential_schema", json!([])),
                        &json_or("auth_params", json!({})),
                        &task_tools_of(v),
                    )
                    .await?;
                    replace_ops_raw(&mut *c, at.tenant, key, version, &ops).await?;
                    row.status
                }
                Some(row) => row.status,
            };
            if current != status {
                set_template_status(&mut *c, key, version, &status).await?;
            }
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT key, version FROM tenant_connector_template \
                  ORDER BY key COLLATE \"C\", version COLLATE \"C\"",
            )
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(rows.iter().map(|(k, v)| template_ref(k, v)).collect()))
        })
    }
    fn proj(&self, v: &Snap) -> Value {
        match v {
            None => Value::Null,
            Some(x) => {
                let pick = |f: &str| x.get(f).cloned().unwrap_or(Value::Null);
                json!([
                    pick("label"),
                    pick("description"),
                    pick("credential_schema"),
                    pick("task_tools"),
                    pick("ops"),
                    pick("status")
                ])
            }
        }
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(match v {
            None => "no such template".to_string(),
            Some(x) => {
                let s = |f: &str| x.get(f).and_then(Value::as_str).unwrap_or("").to_string();
                let n = x.get("ops").and_then(Value::as_array).map_or(0, Vec::len);
                format!(
                    "{}@{} {} \u{b7} {n} operation{}",
                    s("key"),
                    s("version"),
                    s("status"),
                    if n == 1 { "" } else { "s" }
                )
            }
        })
    }
}

// ---- Production reads (DesignOf::Live) ------------------------------------------------------------------------------

/// The job's current release id, if it has one.
pub async fn current_release(c: &mut PgConnection, job: Uuid) -> ApiResult<Option<String>> {
    let id: Option<Option<String>> =
        sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_optional(&mut *c)
            .await?;
    Ok(id.flatten())
}

/// The places of `kind` in the job's CURRENT release snapshot (B2 `design::values_of_kind`, `DesignOf::Live`).
/// `None` = the job has no current release. A release whose snapshot does not cover `kind` holds none of its places
/// (never the working rows).
pub async fn live_rows(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    kind: &str,
) -> ApiResult<Option<Vec<DesignRow>>> {
    let head: Option<(Option<String>, Option<Vec<String>>)> = sqlx::query_as(
        "SELECT j.current_release_id, r.snapshot_kinds FROM jobs j \
           LEFT JOIN job_release r ON r.job_id = j.id AND r.release_id = j.current_release_id \
          WHERE j.id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?;
    let Some((Some(_), kinds)) = head else {
        return Ok(None);
    };
    if !kinds.unwrap_or_default().iter().any(|k| k == kind) {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(
        design::values_of_kind(c, reg, job, &DesignOf::Live, kind).await?,
    ))
}

/// The routes of `rows` (places of kind `route`), sorted as the working rows are (object, verb, variant, env;
/// bytewise).
pub fn routes_of(rows: &[DesignRow]) -> ApiResult<Vec<OperationRoute>> {
    let mut out: Vec<OperationRoute> = Vec::new();
    for r in rows {
        if let Some(v) = &r.snap {
            out.push(
                serde_json::from_value(v.clone())
                    .map_err(|e| internal(format!("a stored route does not parse: {e}")))?,
            );
        }
    }
    out.sort_by(|a, b| {
        (&a.object, &a.verb, &a.variant, &a.env).cmp(&(&b.object, &b.verb, &b.variant, &b.env))
    });
    Ok(out)
}

/// The routes of the job's design `of` (B2 read API; `Draft` = the working rows).
pub async fn design_routes(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<OperationRoute>> {
    let rows = design::values_of_kind(c, reg, job, of, "route").await?;
    routes_of(&rows)
}

/// One mapping override of a snapshot: `(instance, op, field, path)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveOverride {
    pub instance: String,
    pub op: String,
    pub field: String,
    pub path: String,
}

/// The connector design Production runs for a job: its current release's routes, mapping overrides and tenant
/// template versions (by `(key, version)`, the [`template_value`]).
#[derive(Debug, Clone, Default)]
pub struct LiveConnectors {
    pub routes: Vec<OperationRoute>,
    pub overrides: Vec<LiveOverride>,
    pub templates: BTreeMap<(String, String), Value>,
}

/// The connector places of the job's current release snapshot (D62). `None` = the job has no current release:
/// Production then has nothing to run and never reads the working (draft) rows.
pub async fn live_connectors(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Option<LiveConnectors>> {
    let Some(routes) = live_rows(&mut *c, reg, job, "route").await? else {
        return Ok(None);
    };
    let overrides = live_rows(&mut *c, reg, job, "mapping_override")
        .await?
        .unwrap_or_default();
    let templates = live_rows(&mut *c, reg, job, "tenant_template")
        .await?
        .unwrap_or_default();
    let mut out = LiveConnectors {
        routes: routes_of(&routes)?,
        ..LiveConnectors::default()
    };
    for r in &overrides {
        let Some(v) = &r.snap else { continue };
        let Some(path) = v.get("path").and_then(Value::as_str) else {
            continue;
        };
        out.overrides.push(LiveOverride {
            instance: field(&r.r#ref, "instance").to_string(),
            op: field(&r.r#ref, "op").to_string(),
            field: field(&r.r#ref, "field").to_string(),
            path: path.to_string(),
        });
    }
    for r in &templates {
        if let Some(v) = &r.snap {
            out.templates.insert(
                (
                    field(&r.r#ref, "key").to_string(),
                    field(&r.r#ref, "version").to_string(),
                ),
                v.clone(),
            );
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_the_fixture() {
        let r = RoutePlace;
        assert_eq!(
            r.key(&draft_ref(&[
                ("object", "customer"),
                ("verb", "lookup"),
                ("variant", "by_email"),
                ("env", "production")
            ])),
            "route:customer.lookup{by_email}:production"
        );
        assert_eq!(
            r.key(&draft_ref(&[
                ("object", "ticket"),
                ("verb", "create"),
                ("variant", "*"),
                ("env", "test")
            ])),
            "route:ticket.create:test"
        );
        let mut no_variant =
            draft_ref(&[("object", "invoice"), ("verb", "credit"), ("env", "prod")]);
        no_variant.insert("variant".into(), Value::Null);
        assert_eq!(r.key(&no_variant), "route:invoice.credit:prod");
        assert_eq!(
            MappingOverridePlace.key(&override_ref("crm-1", "customer.lookup", "email")),
            "mapping:crm-1:customer.lookup:email"
        );
        assert_eq!(
            TenantTemplatePlace.key(&template_ref("custom.erp", "12")),
            "tenant-template:custom.erp@12"
        );
    }

    #[test]
    fn texts_match_the_fixture() {
        let r = draft_ref(&[
            ("instance", "crm"),
            ("op", "case.read"),
            ("field", "status"),
        ]);
        assert_eq!(
            MappingOverridePlace.text(&None, &r).as_deref(),
            Some("case.read status: template default")
        );
        assert_eq!(
            MappingOverridePlace
                .text(&Some(json!({"path": "$.state"})), &r)
                .as_deref(),
            Some("case.read status \u{2190} $.state")
        );
        let none = DraftRef::new();
        assert_eq!(RoutePlace.text(&None, &none), None);
        assert_eq!(
            RoutePlace
                .text(
                    &Some(json!({"target": {"kind": "instance", "instance_id": "shop"}})),
                    &none
                )
                .as_deref(),
            Some("system shop")
        );
        assert_eq!(
            TenantTemplatePlace.text(&None, &none).as_deref(),
            Some("no such template")
        );
        assert_eq!(
            TenantTemplatePlace
                .text(
                    &Some(json!({"key": "erp", "version": "1", "status": "draft", "ops": [{}]})),
                    &none
                )
                .as_deref(),
            Some("erp@1 draft \u{b7} 1 operation")
        );
    }

    #[test]
    fn route_proj_is_the_target() {
        let a = Some(json!({"target": {"kind": "builtin"}, "updated_at": "2026-10-20T10:00:00Z"}));
        let b = Some(json!({"target": {"kind": "builtin"}, "updated_at": "2026-10-21T10:00:00Z"}));
        assert_eq!(RoutePlace.proj(&a), RoutePlace.proj(&b));
        assert_eq!(RoutePlace.proj(&None), Value::Null);
    }
}
