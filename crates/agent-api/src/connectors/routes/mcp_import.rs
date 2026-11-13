//! `POST /api/connections/templates/{key}/{version}/mcp-import` (integrations_manager): turns the tools of an MCP
//! server into operations of a draft tenant template (caller identity Part B, B1).
//!
//! Body `{"system": "<instance id>"}` names an instance pinned to this template; its `tools/list` is read (the
//! platform MCP server when the template's auth kind is `none`, else the external server at the instance's base URL,
//! crate::mcp_client::external). Each tool becomes one op:
//!
//! - key: the tool's `_meta.operation` when it is a valid op key, else `tool.<snake_name>`; a consolidated tool with
//!   `_meta.operations` (`{"card.move": {"action": "move"}}`) becomes one op per entry, whose request holds the fixed
//!   arguments as constants (they are not inputs);
//! - `path` = `/<tool name>`, method POST; the request maps every `inputSchema` property from the canonical input of
//!   the same name (`{{canonical.p}}`, `{{canonical.p?}}` when the schema does not require it);
//! - side effect R when `annotations.readOnlyHint` is true, else W (`idempotency` nat);
//! - `field_map` = the top-level `outputSchema` properties as they are, none without an output schema.
//!
//! An op the platform catalogue (`contracts/opdefs.json`) does not know is the TENANT's own op (0246, owner
//! 2026-10-31: the platform is general purpose, a tenant's tools are never platform catalogue): it is stored with its
//! own definition (`def`: the side effect above, the input and output names, the tool's description). A platform op
//! keeps the platform's definition. Each op is validated alone first (against the platform catalogue plus its own
//! definition), replacing same-keyed ops of the draft (others stay); an op that would fail /validate is returned
//! under `invalid_operations` and never stored. 404 `template_not_found`, 409 `template_immutable`, 422 `invalid_input`.
//! The answer lists the ops (`own`: the tenant defines it) and, per op, the declared inputs (name, type, description,
//! required).
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::audit::Event;
use crate::connectors::custody;
use crate::connectors::instances as inst_db;
use crate::connectors::model::{
    FieldMapEntry, Idempotency, InputType, OwnOpDef, SideEffect, TenantTemplateOp,
    Transport as Wire,
};
use crate::connectors::opkey::{op_key, parse_op_key, same_op};
use crate::connectors::places;
use crate::connectors::tenant_templates::{
    self as db, OWN_DESCRIPTION_MAX, defs_with_own, op_defs,
};
use crate::connectors::transport::{AuthSpec, CallTarget};
use crate::connectors::validate::validate_tenant_template_server;
use crate::connectors::wire::{AuthKind, CallSecrets, OpError};

use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::mcp_client::external::ExtTool;
use crate::state::AppState;
use crate::tenancy::{Requires, role};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/connections/templates/{key}/{version}/mcp-import",
        post(import),
    )
}

#[derive(Deserialize)]
struct KeyVersion {
    key: String,
    version: String,
}

#[derive(Deserialize)]
struct Body {
    system: String,
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_input", msg.into())
}

fn op_failed(e: OpError) -> ApiError {
    ApiError::Unprocessable(e.code, e.detail)
}

/// One declared input of an imported op.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub name: String,
    pub kind: String,
    pub description: String,
    pub required: bool,
}

/// One op an import produces.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub op: TenantTemplateOp,
    pub key: String,
    pub tool: String,
    pub side_effect: SideEffect,
    pub inputs: Vec<Input>,
    pub outputs: Vec<String>,
    /// The tool's description, at most [`OWN_DESCRIPTION_MAX`] characters (an own op's definition keeps it).
    pub description: String,
}

impl Planned {
    /// The tenant's own definition of this op (0246): what the tool declares.
    pub fn own_def(&self) -> OwnOpDef {
        OwnOpDef {
            side_effect: self.side_effect,
            input_fields: self.inputs.iter().map(|i| i.name.clone()).collect(),
            output_fields: self.outputs.clone(),
            description: self.description.clone(),
        }
    }
}

/// `Order-Items2` -> `order_items`: lower-case letters and `_` only (what an op key allows).
fn snake(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = if c.is_ascii_alphabetic() {
            c.to_ascii_lowercase()
        } else {
            '_'
        };
        if c != '_' || !(out.is_empty() || out.ends_with('_')) {
            out.push(c);
        }
    }
    out.trim_end_matches('_').to_owned()
}

/// A valid `object.verb` key (no variant) as parsed by [`parse_op_key`].
fn valid_key(key: &str) -> bool {
    let r = parse_op_key(key);
    !r.object.is_empty() && !r.verb.is_empty() && r.variant == "*" && op_key(&r) == key
}

fn name_ok(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The longest input description kept (characters).
const DESCRIPTION_MAX: usize = 300;

/// The inputs a tool's `inputSchema` declares, without the fixed arguments of its op.
fn inputs_of(schema: &Value, fixed: &Map<String, Value>) -> Vec<Input> {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    schema["properties"]
        .as_object()
        .map(|p| {
            p.iter()
                .filter(|(n, _)| !fixed.contains_key(*n) && name_ok(n))
                .map(|(n, s)| Input {
                    name: n.clone(),
                    kind: match &s["type"] {
                        Value::String(t) => t.clone(),
                        Value::Array(a) => a
                            .iter()
                            .filter_map(Value::as_str)
                            .find(|t| *t != "null")
                            .unwrap_or("string")
                            .to_owned(),
                        _ => "string".to_owned(),
                    },
                    // what the model reads of it: one short line, never the server's whole text
                    description: s["description"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(DESCRIPTION_MAX)
                        .collect(),
                    required: required.contains(&n.as_str()),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The request template: the fixed arguments as constants, then one member per input.
fn request_of(fixed: &Map<String, Value>, inputs: &[Input]) -> String {
    let mut members: Vec<String> = fixed
        .iter()
        .map(|(k, v)| format!("{}:{v}", Value::String(k.clone())))
        .collect();
    for i in inputs {
        let q = if i.required { "" } else { "?" };
        members.push(format!(
            "{}:\"{{{{canonical.{}{q}}}}}\"",
            Value::String(i.name.clone()),
            i.name
        ));
    }
    format!("{{{}}}", members.join(","))
}

/// The ops of `tools`, and what was left out (with the reason).
pub fn plan(tools: &[ExtTool]) -> (Vec<Planned>, Vec<String>) {
    let mut out: Vec<Planned> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for t in tools {
        let served: Vec<(String, Map<String, Value>)> = if !t.operations.is_empty() {
            t.operations
                .iter()
                .map(|(k, f)| (k.clone(), f.clone()))
                .collect()
        } else {
            let key = t
                .operation
                .clone()
                .filter(|k| valid_key(k))
                .unwrap_or_else(|| format!("tool.{}", snake(&t.name)));
            vec![(key, Map::new())]
        };
        for (key, fixed) in served {
            if !valid_key(&key) {
                skipped.push(format!("{key}: not a valid operation key"));
                continue;
            }
            if out.iter().any(|p| p.key == key) {
                skipped.push(format!("{key}: more than one tool serves it"));
                continue;
            }
            let r = parse_op_key(&key);
            let inputs = inputs_of(&t.input_schema, &fixed);
            let outputs: Vec<String> = t
                .output_schema
                .as_ref()
                .and_then(|s| s["properties"].as_object())
                .map(|p| p.keys().filter(|n| name_ok(n)).cloned().collect())
                .unwrap_or_default();
            let side_effect = if t.read_only {
                SideEffect::R
            } else {
                SideEffect::W
            };
            out.push(Planned {
                op: TenantTemplateOp {
                    object: r.object,
                    verb: r.verb,
                    variant: "*".to_owned(),
                    method: "POST".to_owned(),
                    path: format!("/{}", t.name),
                    request: request_of(&fixed, &inputs),
                    field_map: outputs
                        .iter()
                        .map(|n| FieldMapEntry {
                            path: format!("$.{n}"),
                            field: n.clone(),
                            transform: Some("none".to_owned()),
                        })
                        .collect(),
                    status_map: Vec::new(),
                    idempotency: Idempotency::Nat,
                    input_types: inputs
                        .iter()
                        .map(|i| {
                            (
                                i.name.clone(),
                                InputType {
                                    kind: i.kind.clone(),
                                    description: i.description.clone(),
                                },
                            )
                        })
                        .collect(),
                    def: None,
                },
                key,
                tool: t.name.clone(),
                side_effect,
                inputs,
                outputs,
                description: t
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .chars()
                    .take(OWN_DESCRIPTION_MAX)
                    .collect(),
            });
        }
    }
    (out, skipped)
}

async fn import(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
    ApiJson(b): ApiJson<Body>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let row = db::require(&mut tx, &p.key, &p.version).await?;
    if row.status != "draft" {
        return Err(ApiError::Conflict(
            "template_immutable",
            format!(
                "{}@{} is {}: a published version never changes. Start a new draft version.",
                row.key, row.version, row.status
            ),
        ));
    }
    let model = row.model()?;
    if model.transport != Wire::Mcp {
        return Err(invalid(
            "Only a template with the mcp transport imports tools.",
        ));
    }
    let inst = inst_db::require(&mut tx, b.system.trim()).await?;
    if inst.template_key != row.key || inst.template_version != row.version {
        return Err(invalid(format!(
            "The system {} is not pinned to {}@{}.",
            inst.id, row.key, row.version
        )));
    }
    if inst.stub {
        return Err(invalid("A stub system has no tools to import."));
    }
    let secret =
        custody::open_credential(&mut tx, st.tenant_keyring.as_deref(), t.tenant, &inst.id)
            .await?
            .unwrap_or_default();
    // no transaction is open around the network call
    tx.commit().await?;

    let timeout = std::time::Duration::from_secs(15);
    let tools = if model.auth_kind == AuthKind::None {
        let mcp = st.connectors.mcp().ok_or_else(|| {
            invalid("MCP_URL is not set: the platform MCP server cannot be read.")
        })?;
        mcp.tools(t.tenant)
            .await
            .map_err(op_failed)?
            .into_iter()
            .map(|x| ExtTool {
                name: x.name,
                description: x.description,
                input_schema: x.input_schema,
                output_schema: x.output_schema,
                read_only: x.read_only,
                operation: x.operation,
                operations: x.operations,
            })
            .collect()
    } else {
        let target = CallTarget {
            tenant: t.tenant,
            instance_id: inst.id.clone(),
            env: if inst.env == "prod" { "prod" } else { "test" },
            credential_version: inst.cred_version.unwrap_or(0),
            base_url: inst.base_url.clone(),
            stub: false,
        };
        let auth = AuthSpec {
            kind: model.auth_kind,
            params: row.auth_params(),
        };
        let mut secrets = CallSecrets {
            config: inst.env_map(),
            secret,
        };
        // A licence-bearer system has no stored credential: without the minted token the listing below would be
        // an unauthenticated `tools/list`, so the import would refuse rather than import nothing.
        if model.auth_kind == AuthKind::LicenceBearer {
            secrets.secret.insert(
                "licence_token".to_owned(),
                Value::String(
                    crate::connectors::runtime::mint_licence_token(&st, t.tenant)
                        .await
                        .map_err(op_failed)?,
                ),
            );
        }
        st.connectors
            .external_mcp()
            .list(&st.connectors, &target, &auth, &secrets, timeout)
            .await
            .map_err(op_failed)?
    };

    let (mut planned, skipped) = plan(&tools);
    // An op the platform does not know is the tenant's own: it carries the tool's definition (0246).
    for p in &mut planned {
        if db::def_of(op_defs(), &p.op.op_ref()).is_none() {
            p.op.def = Some(p.own_def());
        }
    }
    let mut writable: Vec<&Planned> = Vec::new();
    let mut invalid_ops: Vec<Value> = Vec::new();
    for p in &planned {
        let mut one = model.clone();
        one.ops = vec![p.op.clone()];
        let issues = validate_tenant_template_server(&one, &defs_with_own(&one.ops));
        if issues.is_empty() {
            writable.push(p);
        } else {
            invalid_ops.push(
                json!({"key": p.key, "issues": serde_json::to_value(&issues).unwrap_or_default()}),
            );
        }
    }
    if writable.is_empty() {
        return Ok(Json(json!({
            "ops": [], "invalid_operations": invalid_ops, "skipped": skipped,
        })));
    }
    // same-keyed ops are replaced in place, the template's other ops stay
    let mut ops = model.ops.clone();
    let mut actions: Vec<&'static str> = Vec::new();
    for p in &writable {
        match ops
            .iter()
            .position(|o| same_op(&o.op_ref(), &p.op.op_ref()))
        {
            Some(i) => {
                ops[i] = p.op.clone();
                actions.push("updated");
            }
            None => {
                ops.push(p.op.clone());
                actions.push("created");
            }
        }
    }
    let ops = db::check_ops(ops).map_err(invalid)?;
    let count = writable.len();

    let mut tx = t.begin(&st.db).await?;
    let again = db::require(&mut tx, &p.key, &p.version).await?;
    if again.status != "draft" {
        return Err(ApiError::Conflict(
            "template_immutable",
            format!("{}@{} is no longer a draft.", again.key, again.version),
        ));
    }
    let (key, version, tenant) = (row.key.clone(), row.version.clone(), t.tenant);
    let (label, description) = (row.label.clone(), row.description.clone());
    let (schema, auth_params) = (row.credential_schema.clone(), row.auth_params.clone());
    // an import writes operations only: the template's task tools stay as they are
    let task_tools = row.task_tools.clone();
    let e = super::tenant_templates::on(
        Event::new(
            "connector.tenant_template.mcp_import",
            "tenant_connector_template",
            &row.key,
        ),
        &row.key,
        &row.version,
    )
    .detail("ops", count);
    super::tenant_templates::design_write(
        &mut tx,
        &st,
        &t,
        &row.key,
        &row.version,
        format!("Import MCP tools into {}@{}", row.key, row.version),
        e,
        move |c| {
            Box::pin(async move {
                places::update_template(
                    &mut *c,
                    &key,
                    &version,
                    &label,
                    &description,
                    &schema,
                    &auth_params,
                    &task_tools,
                )
                .await?;
                places::replace_ops(&mut *c, tenant, &key, &version, &ops).await
            })
        },
    )
    .await?;
    tx.commit().await?;

    let listed: Vec<Value> = writable
        .iter()
        .zip(&actions)
        .map(|(p, action)| {
            json!({
                "op": p.key,
                "action": action,
                "tool": p.tool,
                "side_effect": format!("{:?}", p.side_effect),
                "own": p.op.def.is_some(),
                "inputs": p.inputs.iter().map(|i| json!({
                    "name": i.name, "type": i.kind, "description": i.description, "required": i.required,
                })).collect::<Vec<_>>(),
                "outputs": p.outputs,
            })
        })
        .collect();
    Ok(Json(json!({
        "ops": listed, "invalid_operations": invalid_ops, "skipped": skipped,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<ExtTool> {
        ExtTool::parse_list(&json!({"tools": [
            {"name": "card", "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string"}, "card_id": {"type": "string", "description": "The card."},
                "title": {"type": "string"}}, "required": ["card_id"]},
             "_meta": {"operations": {"card.move": {"action": "move"}}}},
            {"name": "Board-List2", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": true},
             "outputSchema": {"type": "object", "properties": {"boards": {"type": "array"}}}}
        ]}))
    }

    #[test]
    fn fixed_arguments_are_constants_and_not_inputs() {
        let (p, skipped) = plan(&tools());
        assert!(skipped.is_empty(), "{skipped:?}");
        let mv = &p[0];
        assert_eq!(mv.key, "card.move");
        assert_eq!(mv.op.path, "/card");
        let names: Vec<&str> = mv.inputs.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["card_id", "title"]);
        assert_eq!(
            mv.op.request,
            r#"{"action":"move","card_id":"{{canonical.card_id}}","title":"{{canonical.title?}}"}"#
        );
        assert_eq!(mv.side_effect, SideEffect::W);
        let list = &p[1];
        assert_eq!(list.key, "tool.board_list");
        assert_eq!(list.side_effect, SideEffect::R);
        assert_eq!(list.op.field_map[0].path, "$.boards");
    }

    #[test]
    fn the_op_keeps_each_input_type_and_a_short_description() {
        let long = "x".repeat(DESCRIPTION_MAX + 50);
        let t = ExtTool::parse_list(&json!({"tools": [
            {"name": "card", "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string"}, "card_id": {"type": "string", "description": "The card."},
                "position": {"type": ["integer", "null"], "description": long}}},
             "_meta": {"operations": {"card.move": {"action": "move"}}}}
        ]}));
        let (p, _) = plan(&t);
        let ty = &p[0].op.input_types;
        assert_eq!(
            ty.keys().collect::<Vec<_>>(),
            ["card_id", "position"],
            "a fixed argument has no type"
        );
        assert_eq!(ty["card_id"].kind, "string");
        assert_eq!(ty["card_id"].description, "The card.");
        assert_eq!(ty["position"].kind, "integer");
        assert_eq!(ty["position"].description.chars().count(), DESCRIPTION_MAX);
    }
}
