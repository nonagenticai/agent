//! Tenant-private connector templates (B7 T11, D31/D69): a tenant admin's own standard REST connector on the
//! declarative base `generic_rest@2` — their operations (method, relative path, request template, field map, status
//! map, idempotency), auth kind and credential schema. Database access for `tenant_connector_template(_op)` (0092;
//! every function takes a connection scoped to the tenant, RLS confines it), the request checks of the write routes,
//! the tenant **T1** contract run, and [`as_connector_template`], which presents a tenant template in the platform
//! template shape so instances pinned to it use the same mapping and transport path (`instances::pinned_template`).
//!
//! **Operation catalogue** ([`op_defs`]): the embedded `contracts/opdefs.json`, exported from the web fixture
//! `opDefs` (`web/scripts/export-connector-seed.ts`), which derives every entry from the B6 operation catalogue
//! (`seed/platform/v1/rows.json` plus the later deltas `seed/platform/vN/delta.json`, a variant inheriting what it
//! leaves out): B6 is the authority for side effects and input/output field names, and
//! `op_defs_equal_the_b6_catalogue` fails when the two drift. Validation, `connectorOpDefs` and the side effect of a
//! tenant op all read this one list.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::instances::cmp_version;
use super::jsonpath::json_path;
use super::mapping::{self, RenderCtx};
use super::model::{
    ConnectorTemplate, ConnectorTemplateOp, CredentialField, OpDef, OpKind, OpRef, OwnOpDef,
    SideEffect, TenantConnectorTemplate, TenantTemplateOp,
};
use super::opkey::{op_key, parse_op_key, same_op};
use super::transport::auth::{reserved_credential_header, reserved_static_header};
use super::validate::placeholders;
use crate::error::{ApiError, ApiResult};

/// The embedded operation catalogue (TS `OpDefLite[]`).
pub const OPDEFS_JSON: &str = include_str!("../../contracts/opdefs.json");

static OP_DEFS: LazyLock<Vec<OpDef>> =
    LazyLock::new(|| serde_json::from_str(OPDEFS_JSON).unwrap_or_default());

/// The operation catalogue tenant templates are validated against (`connectorOpDefs`).
pub fn op_defs() -> &'static [OpDef] {
    OP_DEFS.as_slice()
}

/// The catalogue entry of `op`: the exact variant, else the unqualified `*` (TS `validateTenantTemplate`).
pub fn def_of<'d>(defs: &'d [OpDef], op: &OpRef) -> Option<&'d OpDef> {
    let star = OpRef {
        variant: "*".to_owned(),
        ..op.clone()
    };
    defs.iter()
        .find(|d| same_op(&d.op_ref(), op))
        .or_else(|| defs.iter().find(|d| same_op(&d.op_ref(), &star)))
}

/// The catalogue a tenant template is validated and run against: the platform's [`op_defs`], then the template's
/// own definitions (0246) of the ops the platform does not know. An own op is at identity level `0` here: the job
/// that adds it chooses its level (`job_operation`).
pub fn defs_with_own(ops: &[TenantTemplateOp]) -> Vec<OpDef> {
    let mut defs = op_defs().to_vec();
    for o in ops {
        if let Some(d) = &o.def
            && def_of(op_defs(), &o.op_ref()).is_none()
        {
            defs.push(own_op_def(o, d));
        }
    }
    defs
}

/// The [`OpDef`] of a tenant's own definition `d` of op `o` (identity level `0`, see [`defs_with_own`]).
pub fn own_op_def(o: &TenantTemplateOp, d: &OwnOpDef) -> OpDef {
    OpDef {
        object: o.object.clone(),
        verb: o.verb.clone(),
        variant: o.variant.clone(),
        side_effect: d.side_effect,
        identity_level: agent_model::kinds::catalog::IdentityLevel::Public,
        input_fields: d.input_fields.clone(),
        output_fields: d.output_fields.clone(),
    }
}

/// The longest own-op description kept (characters).
pub const OWN_DESCRIPTION_MAX: usize = 1000;
/// The most input or output fields an own op declares.
pub const OWN_FIELDS_MAX: usize = 64;

/// Check a tenant's own op definition: field names are letters, digits or `_`, at most [`OWN_FIELDS_MAX`] each, no
/// duplicates; the description at most [`OWN_DESCRIPTION_MAX`] characters.
fn check_own(key: &str, d: &OwnOpDef) -> Result<(), String> {
    for (what, fields) in [("input", &d.input_fields), ("output", &d.output_fields)] {
        if fields.len() > OWN_FIELDS_MAX {
            return Err(format!(
                "{key}: an operation declares at most {OWN_FIELDS_MAX} {what} fields."
            ));
        }
        for (i, f) in fields.iter().enumerate() {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(format!(
                    "{key}: the {what} field \u{201c}{f}\u{201d} is letters, digits or \u{201c}_\u{201d}."
                ));
            }
            if fields[..i].contains(f) {
                return Err(format!("{key}: the {what} field {f} is listed twice."));
            }
        }
    }
    if d.description.chars().count() > OWN_DESCRIPTION_MAX {
        return Err(format!(
            "{key}: the description is at most {OWN_DESCRIPTION_MAX} characters."
        ));
    }
    Ok(())
}

/// The declarative base of every tenant template (OQ-3).
pub const BASE: &str = "generic_rest@2";
/// Tenant ops declare no timeout: every call gets this one.
pub const TIMEOUT_MS: u64 = 10_000;
/// The auth kinds a tenant template may use (`bundle_token` is the platform's own).
pub const AUTH_KINDS: [&str; 7] = [
    "none",
    "api_key_header",
    "basic",
    "bearer_static",
    "oauth2_client_credentials",
    "licence_bearer",
    "mtls",
];
/// The most operations one template may run as MCP tasks.
pub const MAX_TASK_TOOLS: usize = 16;

/// `task_tools` are op keys (`object.verb`), unique; with the template's `ops` known, each must name one.
pub fn check_task_tools(tools: &[String], ops: &[TenantTemplateOp]) -> Result<(), String> {
    if tools.len() > MAX_TASK_TOOLS {
        return Err(format!(
            "A template runs at most {MAX_TASK_TOOLS} operations as MCP tasks."
        ));
    }
    let known: BTreeSet<String> = ops.iter().map(|o| op_key(&o.op_ref())).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for tool in tools {
        let op = parse_op_key(tool);
        if op.object.is_empty() || op.verb.is_empty() {
            return Err(format!(
                "task_tools entry \u{201c}{tool}\u{201d} is not an operation key (object.verb)."
            ));
        }
        let key = op_key(&op);
        if !seen.insert(key.clone()) {
            return Err(format!("task_tools lists {key} twice."));
        }
        if !ops.is_empty() && !known.contains(&key) {
            return Err(format!(
                "task_tools names {key}, which this template does not implement."
            ));
        }
    }
    Ok(())
}

/// The `auth_params` the transport reads (crate::connectors::transport::{auth,rest}); anything else is refused.
pub const AUTH_PARAM_KEYS: [&str; 10] = [
    "client_auth",
    "csrf_fetch_path",
    "csrf_header",
    "header",
    "password_template",
    "scope_template",
    "static_headers",
    "token_url_template",
    "username_template",
    "value_template",
];
/// The methods a tenant op may use.
pub const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];
const MAX_OPS: usize = 64;
const MAX_TEXT: usize = 16 * 1024;
const MAX_SCHEMA: usize = 20;

pub fn template_not_found(key: &str, version: &str) -> ApiError {
    ApiError::Missing(
        "template_not_found",
        format!("This tenant has no template {key}@{version}."),
    )
}

fn internal(what: &str, e: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("{what}: {e}"))
}

/// One `tenant_connector_template` row with its operations (JSON, in author order).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TemplateRow {
    pub tenant_id: Uuid,
    pub key: String,
    pub version: String,
    pub label: String,
    pub kind: String,
    pub base: String,
    pub transport: String,
    pub auth_kind: String,
    pub auth_params: Value,
    pub credential_schema: Value,
    pub task_tools: Vec<String>,
    pub status: String,
    pub description: String,
    pub validation: Option<Value>,
    pub contract: Option<Value>,
    pub created_by: String,
    pub updated_at: DateTime<Utc>,
    pub ops: Value,
}

impl TemplateRow {
    /// `TenantConnectorTemplate` JSON (plus the server-side `auth_params`).
    pub fn json(&self) -> Value {
        json!({
            "tenant_id": self.tenant_id,
            "key": self.key,
            "version": self.version,
            "label": self.label,
            "kind": self.kind,
            "base": self.base,
            "transport": self.transport,
            "auth_kind": self.auth_kind,
            "auth_params": self.auth_params,
            "credential_schema": self.credential_schema,
            "task_tools": self.task_tools,
            "status": self.status,
            "description": self.description,
            "ops": self.ops,
            "validation": self.validation,
            "contract": self.contract,
            "created_by": self.created_by,
            "updated_at": self.updated_at,
        })
    }

    /// The row as the shared model (what the validator reads).
    pub fn model(&self) -> ApiResult<TenantConnectorTemplate> {
        serde_json::from_value(self.json())
            .map_err(|e| internal("a tenant_connector_template row does not parse", e))
    }

    /// `auth_params` as text pairs (the column holds an object of text values; anything else is skipped).
    pub fn auth_params(&self) -> BTreeMap<String, String> {
        self.auth_params
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The number of errors of the last validation (`None` = never validated since the last edit).
    pub fn validation_errors(&self) -> Option<usize> {
        self.validation
            .as_ref()
            .map(|v| v["errors"].as_array().map_or(0, Vec::len))
    }

    /// The last T1 run is green.
    pub fn contract_green(&self) -> bool {
        self.contract
            .as_ref()
            .is_some_and(|c| c["status"] == "green")
    }
}

macro_rules! template_select {
    ($tail:literal) => {
        concat!(
            "SELECT t.tenant_id, t.key, t.version, t.label, t.kind, t.base, t.transport, t.auth_kind, \
                    t.auth_params, t.credential_schema, t.task_tools, t.status, t.description, t.validation, \
                    t.contract, t.created_by, t.updated_at, \
                    coalesce((SELECT jsonb_agg(jsonb_build_object( \
                                  'object', o.object, 'verb', o.verb, 'variant', o.variant, 'method', o.method, \
                                  'path', o.path, 'request', o.request, 'field_map', o.field_map, \
                                  'status_map', o.status_map, 'idempotency', o.idempotency) \
                                  || CASE WHEN o.input_types = '{}'::jsonb THEN '{}'::jsonb \
                                          ELSE jsonb_build_object('input_types', o.input_types) END \
                                  || CASE WHEN o.def IS NULL THEN '{}'::jsonb \
                                          ELSE jsonb_build_object('def', o.def) END \
                                  ORDER BY o.ord) \
                                FROM tenant_connector_template_op o \
                               WHERE o.tenant_id = t.tenant_id AND o.key = t.key AND o.version = t.version), \
                             '[]'::jsonb) AS ops \
               FROM tenant_connector_template t ",
            $tail
        )
    };
}

fn status_rank(s: &str) -> u8 {
    match s {
        "active" => 0,
        "draft" => 1,
        _ => 2,
    }
}

/// Every template of the tenant: active, draft, retired; then key; then newest version first.
pub async fn list(conn: &mut PgConnection) -> ApiResult<Vec<TemplateRow>> {
    let mut rows = sqlx::query_as::<_, TemplateRow>(template_select!(""))
        .fetch_all(&mut *conn)
        .await?;
    rows.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| cmp_version(&b.version, &a.version))
    });
    Ok(rows)
}

/// One version, or `None`.
pub async fn one(
    conn: &mut PgConnection,
    key: &str,
    version: &str,
) -> ApiResult<Option<TemplateRow>> {
    Ok(
        sqlx::query_as::<_, TemplateRow>(template_select!("WHERE t.key = $1 AND t.version = $2"))
            .bind(key)
            .bind(version)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// One version, else 404 `template_not_found`.
pub async fn require(conn: &mut PgConnection, key: &str, version: &str) -> ApiResult<TemplateRow> {
    one(conn, key, version)
        .await?
        .ok_or_else(|| template_not_found(key, version))
}

/// `(version, status)` of every version of a tenant template key.
pub async fn versions_of(conn: &mut PgConnection, key: &str) -> ApiResult<Vec<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT version, status FROM tenant_connector_template WHERE key = $1")
            .bind(key)
            .fetch_all(&mut *conn)
            .await?,
    )
}

/// A platform template (any version) has this key.
pub async fn platform_key_exists(conn: &mut PgConnection, key: &str) -> ApiResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM connector_template WHERE key = $1)")
            .bind(key)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// Ids of the tenant's systems pinned to `key@version`.
pub async fn pinned_by(
    conn: &mut PgConnection,
    key: &str,
    version: &str,
) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM connector_instance \
          WHERE template_scope = 'tenant' AND template_key = $1 AND template_version = $2 ORDER BY id",
    )
    .bind(key)
    .bind(version)
    .fetch_all(&mut *conn)
    .await?)
}

/// One pin of a tenant template (`TenantTemplateUsage.pins[]`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PinRow {
    pub template_key: String,
    pub template_version: String,
    pub id: String,
    pub label: String,
    pub env: String,
    pub status: String,
}

/// Every system of the tenant pinned to one of its own templates.
pub async fn pins(conn: &mut PgConnection) -> ApiResult<Vec<PinRow>> {
    Ok(sqlx::query_as::<_, PinRow>(
        "SELECT template_key, template_version, id, label, env, status FROM connector_instance \
          WHERE template_scope = 'tenant' ORDER BY template_key, template_version, id",
    )
    .fetch_all(&mut *conn)
    .await?)
}

/// Store the result columns (`validation`, `contract`); allowed on every status. Results, not design (plan T11): no
/// ledger row. Every design write of these tables lives in crate::connectors::places.
pub async fn set_results(
    conn: &mut PgConnection,
    key: &str,
    version: &str,
    validation: Option<&Value>,
    contract: Option<&Value>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE tenant_connector_template SET validation = $3, contract = $4, updated_at = now() \
          WHERE key = $1 AND version = $2",
    )
    .bind(key)
    .bind(version)
    .bind(validation)
    .bind(contract)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The recorded responses of the tenant's systems pinned to `key` (any version), by op: the stored contract-test
/// response first, else the sample fetched for that op.
pub async fn recorded_responses(
    conn: &mut PgConnection,
    key: &str,
) -> ApiResult<BTreeMap<String, Value>> {
    let rows: Vec<(String, Value, i32)> = sqlx::query_as(
        "SELECT r.op, r.response, 0 FROM connector_contract_result r \
           JOIN connector_instance i ON i.tenant_id = r.tenant_id AND i.id = r.instance_id \
          WHERE i.template_scope = 'tenant' AND i.template_key = $1 AND r.response IS NOT NULL \
         UNION ALL \
         SELECT s.op, s.body, 1 FROM connector_sample s \
           JOIN connector_instance i ON i.tenant_id = s.tenant_id AND i.id = s.instance_id \
          WHERE i.template_scope = 'tenant' AND i.template_key = $1 \
          ORDER BY 3",
    )
    .bind(key)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = BTreeMap::new();
    for (op, body, _) in rows {
        out.entry(op).or_insert(body);
    }
    Ok(out)
}

// ---- request checks ---------------------------------------------------------------------------------------------

/// The key rule of 0092 and the fixture (`^[a-z][a-z0-9_]{2,40}$`).
pub fn valid_key(key: &str) -> bool {
    let b = key.as_bytes();
    (3..=41).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// The credential schema a new template starts with (the fixture): basic → username + password, mtls → client
/// certificate + key (+ optional CA), licence_bearer → none (minted per call), else one API key.
pub fn default_schema(auth_kind: &str) -> Vec<CredentialField> {
    let field = |name: &str, label: &str, secret: bool| CredentialField {
        name: name.to_owned(),
        label: label.to_owned(),
        secret,
        hint: None,
    };
    if auth_kind == "basic" {
        vec![
            field("username", "Username", false),
            field("password", "Password", true),
        ]
    } else if auth_kind == "mtls" {
        vec![
            field("client_cert_pem", "Client certificate (PEM)", true),
            field("client_key_pem", "Client private key (PEM)", true),
            field("ca_pem", "Server CA certificate (PEM, optional)", true),
        ]
    } else if auth_kind == "licence_bearer" {
        vec![]
    } else {
        vec![field("api_key", "API key", true)]
    }
}

fn snake(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Field names are snake_case and unique; at most 20 fields.
pub fn check_schema(schema: &[CredentialField]) -> Result<(), String> {
    if schema.len() > MAX_SCHEMA {
        return Err(format!(
            "A credential schema has at most {MAX_SCHEMA} fields."
        ));
    }
    for (i, f) in schema.iter().enumerate() {
        if !snake(&f.name) {
            return Err(format!(
                "Credential field \u{201c}{}\u{201d}: a name is lower-case letters, digits or \u{201c}_\u{201d}.",
                f.name
            ));
        }
        if schema[..i].iter().any(|g| g.name == f.name) {
            return Err(format!(
                "Credential field \u{201c}{}\u{201d} is listed twice.",
                f.name
            ));
        }
    }
    Ok(())
}

/// Only the parameters the transport reads, as short text.
pub fn check_auth_params(params: &BTreeMap<String, String>) -> Result<(), String> {
    for (k, v) in params {
        if !AUTH_PARAM_KEYS.contains(&k.as_str()) {
            return Err(format!(
                "auth_params.{k} is not an auth parameter ({}).",
                AUTH_PARAM_KEYS.join(", ")
            ));
        }
        if v.len() > 2000 {
            return Err(format!("auth_params.{k} is longer than 2000 characters."));
        }
        match k.as_str() {
            "static_headers" => check_static_headers(v)?,
            "header" | "csrf_header" if reserved_credential_header(v) => {
                return Err(format!(
                    "reserved_header: auth_params.{k} cannot be \u{201c}{}\u{201d}, a header the platform owns.",
                    v.trim()
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// `auth_params.static_headers` is a JSON object of header name → text, and sets no reserved header
/// ([`reserved_static_header`], the rule the transport also applies when it renders a stored template).
fn check_static_headers(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    let bad =
        || "auth_params.static_headers is a JSON object of header name \u{2192} text.".to_owned();
    let map: Map<String, Value> = serde_json::from_str(raw).map_err(|_| bad())?;
    for (name, value) in &map {
        let value = value.as_str().ok_or_else(bad)?;
        if let Some(why) = reserved_static_header(name, value) {
            return Err(format!(
                "reserved_header: auth_params.static_headers cannot set \u{201c}{}\u{201d}: {why}.",
                name.trim()
            ));
        }
    }
    Ok(())
}

/// Normalise and check the operations of a save: variant `*` when empty, method upper-case from [`METHODS`],
/// object/verb snake_case, one entry per op, bounded sizes. Catalogue checks are the validator's, not this.
pub fn check_ops(ops: Vec<TenantTemplateOp>) -> Result<Vec<TenantTemplateOp>, String> {
    if ops.len() > MAX_OPS {
        return Err(format!("A template has at most {MAX_OPS} operations."));
    }
    let mut out: Vec<TenantTemplateOp> = Vec::with_capacity(ops.len());
    for mut o in ops {
        o.object = o.object.trim().to_owned();
        o.verb = o.verb.trim().to_owned();
        o.variant = match o.variant.trim() {
            "" => "*".to_owned(),
            v => v.to_owned(),
        };
        o.method = o.method.trim().to_ascii_uppercase();
        let key = op_key(&o.op_ref());
        if !snake(&o.object) || !snake(&o.verb) {
            return Err(format!(
                "\u{201c}{key}\u{201d}: object and verb are lower-case letters, digits or \u{201c}_\u{201d}."
            ));
        }
        if !METHODS.contains(&o.method.as_str()) {
            return Err(format!(
                "{key}: the method is one of {}.",
                METHODS.join(", ")
            ));
        }
        if o.path.trim().is_empty() {
            return Err(format!("{key}: the path is empty."));
        }
        if o.path.len() > MAX_TEXT || o.request.len() > MAX_TEXT {
            return Err(format!("{key}: path and request are at most 16 KiB."));
        }
        if out.iter().any(|x| same_op(&x.op_ref(), &o.op_ref())) {
            return Err(format!("{key} is listed twice."));
        }
        // a platform op keeps the platform's definition: an own one is never stored for it
        if def_of(op_defs(), &o.op_ref()).is_some() {
            o.def = None;
        }
        if let Some(d) = &o.def {
            check_own(&key, d)?;
        }
        out.push(o);
    }
    Ok(out)
}

/// The trailing number of `s` plus one (TS `nextVersion`'s `/^(.*?)(\d+)$/`), if `s` ends in digits.
fn bump(s: &str) -> Option<String> {
    let digits = s.chars().rev().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let (head, num) = s.split_at(s.len() - digits);
    let n: u64 = num.parse().ok()?;
    Some(format!("{head}{}", n.checked_add(1)?))
}

/// TS `nextVersion`: `from` with its trailing number bumped (else `from.1`), skipping versions that exist.
pub fn next_version(existing: &[String], from: &str) -> String {
    let mut candidate = bump(from).unwrap_or_else(|| format!("{from}.1"));
    while existing.contains(&candidate) {
        candidate = bump(&candidate).unwrap_or_else(|| format!("{candidate}.1"));
    }
    candidate
}

// ---- the platform shape -----------------------------------------------------------------------------------------

/// A tenant op as a platform op: side effect from the catalogue `defs` (the platform's plus the template's own,
/// [`defs_with_own`]; unknown → `W`, the cautious one), no fault map
/// (the default fault set), no fixtures, the [`TIMEOUT_MS`] timeout.
pub fn connector_op(o: &TenantTemplateOp, defs: &[OpDef]) -> ConnectorTemplateOp {
    ConnectorTemplateOp {
        object: o.object.clone(),
        verb: o.verb.clone(),
        variant: o.variant.clone(),
        side_effect: def_of(defs, &o.op_ref()).map_or(SideEffect::W, |d| d.side_effect),
        op_kind: OpKind::Standard,
        method: o.method.clone(),
        path: o.path.clone(),
        request: o.request.clone(),
        field_map: o.field_map.clone(),
        status_map: o.status_map.clone(),
        fault_map: Vec::new(),
        pagination: None,
        idempotency: o.idempotency.clone(),
        verify_read: None,
        timeout_ms: TIMEOUT_MS,
        fixtures: Vec::new(),
        input_types: o.input_types.clone(),
    }
}

/// What the systems catalogue and the instance detail show as a tenant template's vendor.
pub fn vendor_label(kind: &str) -> String {
    format!("{kind} \u{b7} your template")
}

/// A tenant template in the platform template shape (instances pinned to it render, call and map through it).
pub fn as_connector_template(row: &TemplateRow) -> ApiResult<ConnectorTemplate> {
    let m = row.model()?;
    let defs = defs_with_own(&m.ops);
    let ops = m.ops.iter().map(|o| connector_op(o, &defs)).collect();
    Ok(ConnectorTemplate {
        vendor: vendor_label(&m.kind),
        key: m.key,
        version: m.version,
        label: m.label,
        system_class: String::new(),
        description: m.description,
        wave: String::new(),
        platform_connector: false,
        transport: m.transport,
        alt_transport: None,
        auth_kind: m.auth_kind,
        auth_detail: String::new(),
        auth_params: row.auth_params(),
        credential_schema: m.credential_schema,
        rate_limit_model: Value::Null,
        vendor_api_pin: String::new(),
        webhooks: Value::Null,
        status: m.status,
        t1: Value::Null,
        t2: Value::Null,
        published_by: None,
        published_at: None,
        drift: Value::Null,
        ops,
        // Canonical op keys: `check_task_tools` accepts `case.read{*}` as `case.read`, and the executor compares
        // against the template's ops, so the stored spelling is normalised here for every reader.
        task_tools: row
            .task_tools
            .iter()
            .map(|t| op_key(&parse_op_key(t)))
            .collect(),
    })
}

// ---- tenant T1 -------------------------------------------------------------------------------------------------

/// The value a T1 render puts in for a secret: never sent anywhere, and looked for in the rendered URL.
const SAMPLE_SECRET: &str = "t1-sample-secret-value";

/// A typed sample for a canonical input (the catalogue lists names only: numbers and flags by name).
fn sample_input(name: &str) -> Value {
    const NUMERIC: [&str; 6] = ["amount", "total", "price", "quantity", "count", "score"];
    if NUMERIC.iter().any(|n| name.contains(n)) {
        json!(1)
    } else if matches!(name, "granted" | "verified") {
        json!(true)
    } else {
        Value::String(format!("t1-{name}"))
    }
}

/// T1 for one op: it renders with sample inputs (every placeholder resolves, a JSON request stays JSON, the path is
/// relative, no secret reaches the URL) and, when `recorded` holds a response of this op, the field map extracts
/// every mapped field from it.
pub fn t1_op(
    op: &TenantTemplateOp,
    defs: &[OpDef],
    recorded: Option<&Value>,
) -> Result<(), String> {
    let cop = connector_op(op, defs);
    let mut canonical = Map::new();
    let mut config = Map::new();
    let mut secret = Map::new();
    for text in [op.path.as_str(), op.request.as_str()] {
        for (ns, name) in placeholders(text).iter().flat_map(|p| p.refs()) {
            match ns {
                "canonical" => {
                    canonical.insert(name.to_owned(), sample_input(name));
                }
                "secret" => {
                    secret.insert(name.to_owned(), Value::String(SAMPLE_SECRET.to_owned()));
                }
                _ => {
                    config.insert(name.to_owned(), Value::String(format!("t1-{name}")));
                }
            }
        }
    }
    let ctx = RenderCtx {
        canonical: &canonical,
        config: &config,
        env: &config,
        secret: &secret,
        idempotency_key: "t1-idempotency-key",
    };
    let req = mapping::render(&cop, &ctx).map_err(|e| format!("does not render: {}", e.detail))?;
    if !req.path.starts_with('/') || req.path.starts_with("//") || req.path.contains("://") {
        return Err(
            "the path must be relative to the system's base URL (start with /).".to_owned(),
        );
    }
    let in_url = req.path.contains(SAMPLE_SECRET)
        || req
            .query
            .iter()
            .any(|(k, v)| k.contains(SAMPLE_SECRET) || v.contains(SAMPLE_SECRET));
    if in_url {
        return Err("a secret reaches the URL; send it in a header or the body.".to_owned());
    }
    let request = op.request.trim();
    let json_template = request.starts_with('{') || request.starts_with('[');
    if json_template && matches!(req.body, Some(Value::String(_))) {
        return Err("the request is not valid JSON once rendered.".to_owned());
    }
    if let Some(body) = recorded {
        for f in &op.field_map {
            json_path(body, &f.path).map_err(|e| {
                format!(
                    "{} \u{2192} {} against the recorded response: {e}",
                    f.path, f.field
                )
            })?;
        }
    }
    Ok(())
}

/// One T1 run over every op: `(passed, total, failures as "op: why")`.
pub fn run_t1(
    t: &TenantConnectorTemplate,
    defs: &[OpDef],
    recorded: &BTreeMap<String, Value>,
) -> (usize, usize, Vec<String>) {
    let mut failures = Vec::new();
    for op in &t.ops {
        let key = op_key(&op.op_ref());
        if let Err(why) = t1_op(op, defs, recorded.get(&key)) {
            failures.push(format!("{key}: {why}"));
        }
    }
    let total = t.ops.len();
    (total - failures.len(), total, failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::model::{FieldMapEntry, Idempotency};

    fn op(path: &str, request: &str) -> TenantTemplateOp {
        TenantTemplateOp {
            input_types: Default::default(),
            object: "case".into(),
            verb: "read".into(),
            variant: "*".into(),
            method: "GET".into(),
            path: path.into(),
            request: request.into(),
            field_map: vec![FieldMapEntry {
                path: "$.data.name".into(),
                field: "case_id".into(),
                transform: Some("none".into()),
            }],
            status_map: vec![],
            idempotency: Idempotency::Nat,
            def: None,
        }
    }

    #[test]
    fn auth_params_refuse_reserved_headers() {
        let p = |k: &str, v: &str| BTreeMap::from([(k.to_owned(), v.to_owned())]);
        let sh = |headers: &str| p("static_headers", headers);
        // another tenant's demo partition
        let e = check_auth_params(&sh(
            r#"{"X-Demo-Tenant":"6f1c2d3e-0000-4000-8000-00000000beef"}"#,
        ))
        .unwrap_err();
        assert!(
            e.starts_with("reserved_header:") && e.contains("X-Demo-Tenant"),
            "{e}"
        );
        // a tenant-written stand-in is no better
        assert!(check_auth_params(&sh(r#"{"x-demo-tenant":"{{config.tenant}}"}"#)).is_err());
        // a static credential, any case
        let e = check_auth_params(&sh(r#"{"authorization":"Bearer x"}"#)).unwrap_err();
        assert!(
            e.starts_with("reserved_header:") && e.contains("authorization"),
            "{e}"
        );
        for name in ["Host", "Cookie", "Proxy-Authorization", "Transfer-Encoding"] {
            assert!(
                check_auth_params(&sh(&format!(r#"{{"{name}":"x"}}"#))).is_err(),
                "{name}"
            );
        }
        assert!(check_auth_params(&sh(r#"{"X-Demo-Env":"prod"}"#)).is_err());
        // the server-side values, and ordinary headers, are fine
        check_auth_params(&sh(r#"{"X-Demo-Tenant":"{{tenant.id}}"}"#)).unwrap();
        check_auth_params(&sh(
            r#"{"X-Demo-Tenant":"{{tenant.id}}","X-Demo-Env":"{{system.env}}","Prefer":"return=representation","X-Org":"{{config.org}}"}"#,
        ))
        .unwrap();
        check_auth_params(&sh("")).unwrap();
        assert!(check_auth_params(&sh("[1]")).is_err());
        // the credential header of api_key_header / the CSRF header never takes a reserved name
        assert!(check_auth_params(&p("header", "X-Demo-Tenant")).is_err());
        assert!(check_auth_params(&p("csrf_header", "host")).is_err());
        check_auth_params(&p("header", "Authorization")).unwrap();
        check_auth_params(&p("csrf_header", "x-csrf-token")).unwrap();
    }

    #[test]
    fn op_defs_parse_and_cover_the_fixture_ops() {
        let defs = op_defs();
        assert!(defs.len() > 20, "opdefs.json parses");
        let case_read = OpRef {
            object: "case".into(),
            verb: "read".into(),
            variant: "*".into(),
        };
        assert_eq!(
            def_of(defs, &case_read).map(|d| d.side_effect),
            Some(SideEffect::R)
        );
    }

    /// The loader swallows a parse error into an empty catalogue, so the contract is parsed strictly here: an
    /// entry without `identity_level` (or with one the wire does not know) fails this, not production.
    #[test]
    fn op_defs_strictly_parse_with_a_level_each() {
        let raw: Vec<serde_json::Value> = serde_json::from_str(OPDEFS_JSON).expect("json array");
        let parsed: Vec<OpDef> = serde_json::from_str(OPDEFS_JSON)
            .expect("every opdefs.json entry carries a valid identity_level");
        assert_eq!(parsed.len(), raw.len());
        assert_eq!(
            op_defs().len(),
            raw.len(),
            "the loader did not fall back to empty"
        );
        let mut without = raw[0].clone();
        without.as_object_mut().unwrap().remove("identity_level");
        assert!(
            serde_json::from_value::<OpDef>(without).is_err(),
            "an entry without identity_level does not parse"
        );
    }

    /// The platform catalogue knows no tenant's system (owner 2026-10-31): an fr tool a tenant connects is the
    /// tenant's own op, defined by its template ([`defs_with_own`]).
    #[test]
    fn op_defs_know_no_tenant_system() {
        assert!(op_defs().iter().all(|d| d.object != "fr"));
    }

    fn own_op(object: &str, verb: &str, def: Option<OwnOpDef>) -> TenantTemplateOp {
        TenantTemplateOp {
            object: object.into(),
            verb: verb.into(),
            variant: "*".into(),
            method: "POST".into(),
            path: format!("/{verb}"),
            request: String::new(),
            field_map: Vec::new(),
            status_map: Vec::new(),
            idempotency: Idempotency::Nat,
            input_types: BTreeMap::new(),
            def,
        }
    }

    fn own(se: SideEffect, inputs: &[&str]) -> OwnOpDef {
        OwnOpDef {
            side_effect: se,
            input_fields: inputs.iter().map(|s| (*s).to_owned()).collect(),
            output_fields: vec!["id".into()],
            description: "Starts a project.".into(),
        }
    }

    /// A template's own op joins the catalogue it is validated and run against, at level 0; a platform op keeps the
    /// platform's definition (an own one is dropped on save, never shadows it).
    #[test]
    fn own_ops_join_the_template_catalogue_and_never_shadow_a_platform_op() {
        let platform = op_defs()[0].clone();
        let ops = check_ops(vec![
            own_op("fr", "start_project", Some(own(SideEffect::W, &["title"]))),
            own_op(
                &platform.object,
                &platform.verb,
                Some(own(SideEffect::P, &["x"])),
            ),
        ])
        .expect("valid");
        assert_eq!(
            ops[1].def, None,
            "a platform op keeps the platform definition"
        );
        let defs = defs_with_own(&ops);
        assert_eq!(defs.len(), op_defs().len() + 1);
        let fr = def_of(&defs, &ops[0].op_ref()).expect("own op in the template catalogue");
        assert_eq!(fr.side_effect, SideEffect::W);
        assert_eq!(fr.input_fields, ["title"]);
        assert_eq!(serde_json::to_value(fr.identity_level).unwrap(), "0");
        let t = connector_op(&ops[0], &defs);
        assert_eq!(t.side_effect, SideEffect::W);
    }

    #[test]
    fn an_own_op_with_a_bad_field_name_is_refused() {
        let e = check_ops(vec![own_op(
            "erp",
            "post_invoice",
            Some(own(SideEffect::W, &["amount", "amount"])),
        )])
        .unwrap_err();
        assert!(e.contains("listed twice"), "{e}");
        let e = check_ops(vec![own_op(
            "erp",
            "post_invoice",
            Some(own(SideEffect::W, &["a-b"])),
        )])
        .unwrap_err();
        assert!(e.contains("letters, digits"), "{e}");
    }

    #[test]
    fn op_defs_keys_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for d in op_defs() {
            let key = op_key(&d.op_ref());
            assert!(seen.insert(key.clone()), "duplicate opdef {key}");
        }
    }

    #[test]
    fn keys_versions_and_ops() {
        assert!(valid_key("frappe_helpdesk"));
        assert!(!valid_key("ab"));
        assert!(!valid_key("Frappe"));
        assert!(!valid_key("my-key"));
        let have = vec!["1".to_owned(), "2".to_owned()];
        assert_eq!(next_version(&have, "1"), "3");
        assert_eq!(next_version(&have, "2"), "3");
        assert_eq!(next_version(&[], "beta"), "beta.1");
        let ok = check_ops(vec![op("/a", "")]).unwrap();
        assert_eq!(ok[0].method, "GET");
        assert!(check_ops(vec![op("/a", ""), op("/b", "")]).is_err());
        let mut bad = op("/a", "");
        bad.method = "TRACE".into();
        assert!(check_ops(vec![bad]).is_err());
        assert_eq!(default_schema("basic").len(), 2);
        assert_eq!(default_schema("api_key_header")[0].name, "api_key");
    }

    #[test]
    fn task_tools_are_unique_known_operation_keys() {
        let tools =
            |keys: &[&str]| -> Vec<String> { keys.iter().map(|k| (*k).to_owned()).collect() };
        check_task_tools(&tools(&["case.read", "case.create"]), &[]).expect("two valid keys");
        // malformed: no object.verb
        assert!(check_task_tools(&tools(&["case"]), &[]).is_err());
        assert!(check_task_tools(&tools(&["Case.read"]), &[]).is_err());
        // duplicate, `*` and empty variant being the same operation
        assert!(check_task_tools(&tools(&["case.read", "case.read{*}"]), &[]).is_err());
        // a key the template does not implement
        let read = op("/a", "");
        check_task_tools(&tools(&["case.read"]), std::slice::from_ref(&read)).expect("a known op");
        assert!(check_task_tools(&tools(&["case.create"]), &[read]).is_err());
        // one over the cap
        let verbs = [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q",
        ];
        let many: Vec<String> = verbs.iter().map(|v| format!("case.{v}")).collect();
        assert_eq!(many.len(), MAX_TASK_TOOLS + 1);
        assert!(check_task_tools(&many, &[]).is_err());
    }

    #[test]
    fn t1_renders_and_checks_the_recorded_response() {
        let defs = op_defs();
        assert_eq!(
            t1_op(&op("/api/{{canonical.case_id}}", ""), defs, None),
            Ok(())
        );
        assert!(t1_op(&op("https://evil.example/x", ""), defs, None).is_err());
        assert!(t1_op(&op("/x?k={{secret.api_key}}", ""), defs, None).is_err());
        assert!(t1_op(&op("/x", "{\"a\": {{canonical.case_id}"), defs, None).is_err());
        let hit = json!({"data": {"name": "C-1"}});
        let miss = json!({"data": {}});
        assert_eq!(t1_op(&op("/x", ""), defs, Some(&hit)), Ok(()));
        assert!(t1_op(&op("/x", ""), defs, Some(&miss)).is_err());
    }
}
