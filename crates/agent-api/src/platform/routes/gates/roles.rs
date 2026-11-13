//! P15 agent roles (plan B6 §5, T12): `agentRoleJobs`, `agentRoles`, `addAgentRoleGrant`, `removeAgentRoleGrant`,
//! `setAgentRoleMaxSideEffect`. A job's roles are the `base` roles with the job head's own `agent_role` rows on top
//! (a job row replaces the base row; a tombstone removes the role). Every write is one revision of the role in
//! `job/<job>` (a `patch` over a base role, an `add` for a role only the job has) through `write_authored`, with
//! the agent-model role rules (`editable`, `check_add_grant`, `check_max_side_effect`) against the head operation
//! catalogue: every current `operation` row, more specific layers last, resolved variant ⊕ `*`.
use std::collections::BTreeMap;

use agent_model::catalog::{OperationDefRow, op_key, resolve_operations};
use agent_model::kinds::catalog::{Modality, SideEffect};
use agent_model::kinds::role::{self, AgentRoleFields};
use agent_model::kinds::{self, Kind, Op};
use axum::extract::State;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::super::{Who, missing, read_tx, unprocessable, write_tx};
use super::library::{field_errors, iso, refused};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::paged;
use crate::platform::db::authored::{Expect, write_authored};
use crate::platform::db::layers;
use crate::platform::db::lock_family;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/platform/agent-roles/jobs", get(role_jobs))
        .route("/api/platform/agent-roles", get(list_roles))
        .route("/api/platform/agent-roles/{job}/{role}", patch(set_max))
        .route(
            "/api/platform/agent-roles/{job}/{role}/enabled",
            put(set_enabled),
        )
        .route(
            "/api/platform/agent-roles/{job}/{role}/grants",
            post(add_grant),
        )
        .route(
            "/api/platform/agent-roles/{job}/{role}/grants/{op}",
            delete(remove_grant),
        )
}

/// The UI's role order (`AgentRoleKey`), then any other key alphabetically.
const ROLE_ORDER: [&str; 4] = ["qualifier", "procedure", "post_gate", "assistant"];

fn role_rank(key: &str) -> (usize, String) {
    let i = ROLE_ORDER
        .iter()
        .position(|k| *k == key)
        .unwrap_or(ROLE_ORDER.len());
    (i, key.to_owned())
}

fn job_layer(job: &str) -> String {
    format!("job/{job}")
}

type RoleRow = (String, String, String, Value, i32, String, DateTime<Utc>);

/// One effective role of a job.
#[derive(Debug, Clone)]
pub struct EffRole {
    pub fields: AgentRoleFields,
    /// The current revision of this role in the job layer itself (any op), if any.
    pub job_rev: Option<i32>,
    /// The base layer defines the role (the job row is then a `patch`).
    pub in_base: bool,
    pub author: String,
    pub at: DateTime<Utc>,
}

/// The current `agent_role` rows of `base` and `job_layer`: (layer, key, op, fields, rev, author, created_at).
async fn role_rows(conn: &mut PgConnection, job_layer: &str) -> ApiResult<Vec<RoleRow>> {
    Ok(sqlx::query_as(
        "SELECT layer_key, key, op, fields, rev, author, created_at FROM layer_head \
          WHERE kind = 'agent_role' AND layer_key IN ('base', $1) \
          ORDER BY (layer_key = 'base') DESC, key",
    )
    .bind(job_layer)
    .fetch_all(conn)
    .await?)
}

/// Base roles with the job's rows on top. A row that does not parse is left out (logged).
pub fn effective(rows: Vec<RoleRow>, job_layer: &str) -> BTreeMap<String, EffRole> {
    let mut out: BTreeMap<String, EffRole> = BTreeMap::new();
    let mut job_revs: BTreeMap<String, i32> = BTreeMap::new();
    for (layer, key, op, fields, rev, author, at) in rows {
        let is_job = layer == job_layer;
        if is_job {
            job_revs.insert(key.clone(), rev);
        }
        if op == Op::Tombstone.as_str() {
            out.remove(&key);
            continue;
        }
        let parsed: AgentRoleFields = match serde_json::from_value(fields) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(layer = %layer, role = %key, error = %e, "agent_role_unreadable");
                continue;
            }
        };
        let in_base = !is_job || out.get(&key).is_some_and(|r| r.in_base);
        out.insert(
            key,
            EffRole {
                fields: parsed,
                job_rev: None,
                in_base,
                author,
                at,
            },
        );
    }
    for (key, r) in out.iter_mut() {
        r.job_rev = job_revs.get(key).copied();
    }
    out
}

/// The procedure role's derived grants: the job head's `activity` rows that carry `ops` (`[]` until B8 gives
/// activities their operations).
async fn derived(conn: &mut PgConnection, job_layer: &str) -> ApiResult<Vec<Value>> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT key, fields FROM layer_head \
          WHERE layer_key = $1 AND kind = 'activity' AND op <> 'tombstone' ORDER BY key",
    )
    .bind(job_layer)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(key, f)| {
            let ops = f.get("ops")?.as_array()?;
            let ops: Vec<String> = ops
                .iter()
                .filter_map(|o| {
                    let op = o.get("op")?.as_str()?;
                    Some(role::op_label(op, o.get("variant").and_then(Value::as_str)))
                })
                .collect();
            let label = f.get("label").and_then(Value::as_str).unwrap_or(&key);
            Some(json!({"activity": key, "label": label, "ops": ops}))
        })
        .collect())
}

/// The web `AgentRoleView`.
fn view(job: &str, key: &str, r: &EffRole, derived: &[Value]) -> Value {
    let mut v = serde_json::to_value(&r.fields).unwrap_or_else(|_| json!({}));
    if let Some(m) = v.as_object_mut() {
        m.remove("summary");
        m.insert("job".into(), json!(job));
        m.insert("role".into(), json!(key));
        m.insert("updated_by".into(), json!(r.author));
        m.insert("updated_at".into(), json!(iso(r.at)));
        let d = if r.fields.derives_grants() {
            derived.to_vec()
        } else {
            Vec::new()
        };
        m.insert("derived".into(), Value::Array(d));
    }
    v
}

fn ordered(roles: &BTreeMap<String, EffRole>) -> Vec<(&String, &EffRole)> {
    let mut v: Vec<(&String, &EffRole)> = roles.iter().collect();
    v.sort_by_key(|(k, _)| role_rank(k));
    v
}

// ---- reads --------------------------------------------------------------------------------------------------------

/// `GET /api/platform/agent-roles/jobs`: every job template head, with its number of effective roles. Since KC D119
/// (seed v6) a job's roles are base's and its qualifier/post-gate grants derive from its expectations, so a job needs
/// no `agent_role` row of its own to be listed.
async fn role_jobs(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let jobs: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT l.job, l.key FROM layer l \
          WHERE l.scope = 'job' AND l.dimension_key IS NULL AND l.job IS NOT NULL \
          ORDER BY l.job",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::with_capacity(jobs.len());
    for (job, layer) in jobs {
        let roles = effective(role_rows(&mut tx, &layer).await?, &layer);
        out.push(json!({"job": job, "roles": roles.len()}));
    }
    Ok(Json(Value::Array(out)))
}

#[derive(Debug, Deserialize)]
struct RolesQuery {
    job: String,
}

/// `GET /api/platform/agent-roles?job=`: 404 `layer_not_found` for a job without a template head.
async fn list_roles(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<RolesQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let layer = job_layer(&q.job);
    layers::layer_by_key(&mut tx, &layer).await?;
    let roles = effective(role_rows(&mut tx, &layer).await?, &layer);
    let derived = derived(&mut tx, &layer).await?;
    let items = ordered(&roles)
        .into_iter()
        .map(|(k, r)| view(&q.job, k, r, &derived))
        .collect();
    Ok(Json(paged(items, None)))
}

// ---- writes -------------------------------------------------------------------------------------------------------

/// The resolved head operation catalogue: operation key (`order.read`, `case.create{return}`) → side effect.
pub struct Catalogue(BTreeMap<String, SideEffect>);

impl Catalogue {
    pub fn side_effect(&self, key: &str) -> Option<SideEffect> {
        self.0.get(key).copied()
    }
}

/// Scope order of the fold: base, industry, base_dept (D116), industry_dept, job (more specific wins).
const SCOPE_ORDER: &str = "CASE scope WHEN 'base' THEN 0 WHEN 'industry' THEN 1 WHEN 'base_dept' THEN 2 \
                           WHEN 'industry_dept' THEN 3 WHEN 'job' THEN 4 ELSE 5 END";

async fn catalogue(conn: &mut PgConnection) -> ApiResult<Catalogue> {
    let rows: Vec<(String, String, Value)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT key, op, fields FROM layer_head WHERE kind = 'operation' \
          ORDER BY {SCOPE_ORDER}, layer_key, key"
    )))
    .fetch_all(conn)
    .await?;
    let mut folded: BTreeMap<String, Option<Value>> = BTreeMap::new();
    for (key, op, fields) in rows {
        let f = (op != Op::Tombstone.as_str()).then_some(fields);
        folded.insert(key, f);
    }
    let defs: Vec<OperationDefRow> = folded
        .iter()
        .filter_map(|(key, f)| {
            let f = f.as_ref()?;
            match OperationDefRow::from_row(key, f) {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!(op = %key, error = %e, "operation_unreadable");
                    None
                }
            }
        })
        .collect();
    let mut out = BTreeMap::new();
    for op in resolve_operations(&defs, &[]) {
        let side = op
            .typed_attrs()
            .ok()
            .and_then(|a| a.side_effect)
            .unwrap_or(SideEffect::R);
        out.insert(op.key, side);
    }
    Ok(Catalogue(out))
}

/// 404 `role_not_found`.
fn role_not_found(job: &str, role: &str) -> ApiError {
    missing(
        "role_not_found",
        format!("The job template {job} has no role {role}."),
    )
}

/// One role edit: family lock, effective role (404 `role_not_found`), `role::editable` (409 `derived_grants`,
/// `role_disabled`), `f` against the catalogue, whole-row validation, one revision in `job/<job>` (unchanged
/// fields write nothing), and the role's view.
async fn edit_role(
    st: &AppState,
    sa: &SuperAdmin,
    job: &str,
    role: &str,
    switch: bool,
    f: impl FnOnce(&mut AgentRoleFields, &Catalogue) -> ApiResult<()>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(sa);
    let layer_key = job_layer(job);
    let mut tx = write_tx(st).await?;
    let Some(layer) = layers::find_layer_by_key(&mut tx, &layer_key).await? else {
        return Err(role_not_found(job, role));
    };
    lock_family(&mut tx, &layer.family).await?;
    let roles = effective(role_rows(&mut tx, &layer_key).await?, &layer_key);
    let Some(current) = roles.get(role) else {
        return Err(role_not_found(job, role));
    };
    // the on/off switch is the one edit a disabled role takes (its grants stay locked until it is on)
    if !switch {
        role::editable(&current.fields).map_err(refused)?;
    }
    let cat = catalogue(&mut tx).await?;
    let mut next = current.fields.clone();
    f(&mut next, &cat)?;
    let derived = derived(&mut tx, &layer_key).await?;
    if next == current.fields {
        return Ok(Json(view(job, role, current, &derived)));
    }
    let now = Utc::now();
    next.updated_by = Some(who.by.clone());
    next.updated_at = Some(iso(now));
    next.summary = None;
    let fields = serde_json::to_value(&next).map_err(|e| ApiError::Internal(e.into()))?;
    kinds::validate_fields(Kind::AgentRole, &fields).map_err(field_errors)?;
    let expect = match current.job_rev {
        Some(rev) => Expect::Rev(rev),
        None => Expect::New,
    };
    let op = if current.in_base { Op::Patch } else { Op::Add };
    let row = write_authored(
        &mut tx,
        &who.actor,
        &layer_key,
        Kind::AgentRole,
        role,
        op,
        fields,
        &who.by,
        expect,
    )
    .await?;
    tx.commit().await?;
    let written = EffRole {
        fields: next,
        job_rev: Some(row.rev),
        in_base: current.in_base,
        author: row.author,
        at: row.created_at,
    };
    Ok(Json(view(job, role, &written, &derived)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantIn {
    op: String,
    #[serde(default)]
    variant: Option<String>,
    modalities: Vec<Modality>,
}

fn variant_of(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|v| !v.is_empty())
}

/// `POST /api/platform/agent-roles/{job}/{role}/grants`: 422 `unknown_operation`, `exceeds_max_side_effect`,
/// `invalid_modalities`; 409 `grant_exists`.
async fn add_grant(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((job, role)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<GrantIn>,
) -> ApiResult<Json<Value>> {
    edit_role(&st, &sa, &job, &role, false, |r, cat| {
        let variant = variant_of(b.variant.as_deref());
        let unknown = || {
            unprocessable(
                "unknown_operation",
                format!(
                    "{} is not in the operation catalogue.",
                    role::op_label(&b.op, variant)
                ),
            )
        };
        let (object, verb) = b.op.split_once('.').ok_or_else(unknown)?;
        let side = cat
            .side_effect(&op_key(object, verb, variant.unwrap_or("*")))
            .ok_or_else(unknown)?;
        let grant =
            role::check_add_grant(r, &b.op, side, variant, &b.modalities).map_err(refused)?;
        r.grants.push(grant);
        Ok(())
    })
    .await
}

#[derive(Debug, Deserialize)]
struct VariantQuery {
    #[serde(default)]
    variant: Option<String>,
}

/// `DELETE /api/platform/agent-roles/{job}/{role}/grants/{object.verb}?variant=`: 404 `grant_not_found`.
async fn remove_grant(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((job, role, op)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<VariantQuery>,
) -> ApiResult<Json<Value>> {
    edit_role(&st, &sa, &job, &role, false, |r, _| {
        let variant = variant_of(q.variant.as_deref());
        let Some(i) = r
            .grants
            .iter()
            .position(|g| g.op() == op && g.variant.as_deref() == variant)
        else {
            return Err(missing(
                "grant_not_found",
                format!("{} has no grant {}.", r.label, role::op_label(&op, variant)),
            ));
        };
        r.grants.remove(i);
        Ok(())
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaxIn {
    max_side_effect: SideEffect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledIn {
    enabled: bool,
}

/// `PUT /api/platform/agent-roles/{job}/{role}/enabled` `{enabled}` (caller identity plan A4/A5): switch a platform
/// role on or off for the job template; a job then turns it on for itself (`setAgentRoleEnabled`). The procedure
/// role (derived grants) is not switchable: 409 `derived_grants`.
async fn set_enabled(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((job, role)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<EnabledIn>,
) -> ApiResult<Json<Value>> {
    edit_role(&st, &sa, &job, &role, true, |r, _| {
        if r.derives_grants() {
            return Err(refused(role::editable(r).unwrap_err()));
        }
        r.enabled = b.enabled;
        Ok(())
    })
    .await
}

/// `PATCH /api/platform/agent-roles/{job}/{role}` `{max_side_effect}`: 422 `grants_exceed_max` naming them.
async fn set_max(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((job, role)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<MaxIn>,
) -> ApiResult<Json<Value>> {
    edit_role(&st, &sa, &job, &role, false, |r, cat| {
        role::check_max_side_effect(r, b.max_side_effect, |op| cat.side_effect(op))
            .map_err(refused)?;
        r.max_side_effect = b.max_side_effect;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(layer: &str, key: &str, op: &str, fields: Value, rev: i32) -> RoleRow {
        (
            layer.into(),
            key.into(),
            op.into(),
            fields,
            rev,
            "owner".into(),
            Utc::now(),
        )
    }

    fn role_fields(label: &str, enabled: bool) -> Value {
        json!({"label": label, "used_by": "edge", "max_side_effect": "R", "enabled": enabled, "grants": [], "note": ""})
    }

    #[test]
    fn job_rows_override_base() {
        let rows = vec![
            row(
                "base",
                "assistant",
                "add",
                role_fields("Assistant", false),
                1,
            ),
            row("base", "composer", "tombstone", json!({}), 2),
            row(
                "base",
                "qualifier",
                "add",
                role_fields("Qualifier", true),
                1,
            ),
            row(
                "job/care",
                "qualifier",
                "patch",
                role_fields("Care qualifier", true),
                3,
            ),
            row(
                "job/care",
                "only_here",
                "add",
                role_fields("Only here", true),
                1,
            ),
        ];
        let eff = effective(rows, "job/care");
        assert_eq!(
            ordered(&eff)
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["qualifier", "assistant", "only_here"]
        );
        let q = &eff["qualifier"];
        assert_eq!(q.fields.label, "Care qualifier");
        assert_eq!(q.job_rev, Some(3));
        assert!(q.in_base);
        assert_eq!(eff["assistant"].job_rev, None);
        assert!(!eff["only_here"].in_base);
        assert!(!eff["assistant"].fields.enabled);
        let v = view("care", "assistant", &eff["assistant"], &[]);
        assert_eq!(v["job"], "care");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["enabled"], false);
        assert_eq!(v["derived"], json!([]));
        assert_eq!(v["updated_by"], "owner");
    }

    #[test]
    fn job_tombstone_removes_a_base_role() {
        let rows = vec![
            row(
                "base",
                "qualifier",
                "add",
                role_fields("Qualifier", true),
                1,
            ),
            row("job/care", "qualifier", "tombstone", json!({}), 2),
        ];
        assert!(effective(rows, "job/care").is_empty());
    }
}
