//! P16 coverage and P17 usage of the platform connector templates (B7 T7), super_admin reads.
//!
//! - [`overview`] is pure: the TS fixture's `connectorTemplateOverview` (`web/src/lib/api/areas/connectors.ts`) over
//!   explicit inputs. Templates are taken as their JSON (the TS type), so the
//!   `templates` summaries are the template minus `ops`, plus `implements` and `instances`, exactly as TS builds them.
//! - [`head_catalogue`] reads its catalogue inputs from B6's head catalogue (`layer_head`, plan B7 "Cross-plan
//!   interfaces" B6a): canonical object states, the operation catalogue, and the job archetypes with their expected
//!   operations (`operation_expectation` rows folded over the job's template stack, [`stack_of`]).
//! - [`tenant_rows`] reads the cross-tenant rows (platform-scope instances, tenant templates and their base) through
//!   the `agent_platform`-only definers `agent_connector_pins()` and `agent_tenant_connector_bases()` (0152).
//! - [`routed_and_overrides`] is the T13 seam: job routes (0094 `operation_route`) and mapping overrides
//!   (`field_mapping_override`) do not exist yet.
use std::collections::BTreeMap;

use agent_model::compile::{Selection, parse_department};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::validate::GENERIC_REST;
use crate::error::ApiResult;
use crate::platform_db;

/// One platform-scope connector instance of some tenant (P16 `pinned_by`, P17 "who runs it").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub tenant_id: String,
    pub tenant: String,
    pub instance_id: String,
    pub env: String,
    pub key: String,
    pub version: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: String,
}

/// One tenant-private template and the platform template it is built on (`base` = `key@version`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Base {
    pub tenant: String,
    pub key: String,
    pub version: String,
    pub status: String,
    pub base: String,
}

/// Everything [`overview`] computes from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OverviewInput {
    /// Every platform template version, as the TS `ConnectorTemplate` JSON (with `ops`).
    pub templates: Vec<Value>,
    /// Object → its canonical statuses (`states[].canonical`), in state order.
    pub canonical_states: BTreeMap<String, Vec<String>>,
    pub pins: Vec<Pin>,
    pub bases: Vec<Base>,
    /// The canonical operation catalogue: `{key, object, side_effect}`.
    pub catalogue: Vec<Value>,
    /// The job archetypes: `{key, label, ops: [{key, required}]}`.
    pub jobs: Vec<Value>,
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// TS `opKey`: `object.verb`, or `object.verb{variant}` when the variant is not `*`.
fn op_key_of(op: &Value) -> String {
    let (object, verb) = (text(op, "object"), text(op, "verb"));
    match text(op, "variant") {
        "" | "*" => format!("{object}.{verb}"),
        v => format!("{object}.{verb}{{{v}}}"),
    }
}

fn ops_of(t: &Value) -> &[Value] {
    t.get("ops")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// Platform-scope instances pinned to exactly `key@version`.
fn pins_of<'p>(pins: &'p [Pin], key: &str, version: &str) -> impl Iterator<Item = &'p Pin> {
    pins.iter()
        .filter(move |p| p.key == key && p.version == version)
}

/// `ConnectorTemplateSummary`: the template without `ops`, plus `implements` (OpRefs) and `instances`.
pub fn summary(t: &Value, instances: usize) -> Value {
    let implements: Vec<Value> = ops_of(t)
        .iter()
        .map(|o| json!({"object": o["object"], "verb": o["verb"], "variant": o["variant"]}))
        .collect();
    let mut v = t.clone();
    if let Value::Object(m) = &mut v {
        m.remove("ops");
        m.insert("implements".into(), Value::Array(implements));
        m.insert("instances".into(), json!(instances));
    }
    v
}

/// TS `needsKey(side_effect) && idempotency.kind === 'none'`.
fn unkeyed(op: &Value) -> bool {
    text(op, "side_effect") != "R" && op["idempotency"]["kind"] == "none"
}

/// One `TemplateCoverage` row.
fn coverage_of(t: &Value, input: &OverviewInput) -> Value {
    let (key, version) = (text(t, "key"), text(t, "version"));
    let ops = ops_of(t);
    // object → mapped canonical targets, both in first-seen order (the TS Map / Set)
    let mut by_object: Vec<(String, Vec<String>)> = Vec::new();
    for o in ops {
        let status_map = o
            .get("status_map")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        if text(o, "op_kind") == "builtin" || status_map.is_empty() {
            continue;
        }
        let object = text(o, "object").to_owned();
        let at = match by_object.iter().position(|(ob, _)| *ob == object) {
            Some(i) => i,
            None => {
                by_object.push((object, Vec::new()));
                by_object.len() - 1
            }
        };
        let set = &mut by_object[at].1;
        // `invalid_state:*` rows map a vendor refusal to an error, not to a status of the object.
        for m in status_map {
            let c = text(m, "canonical");
            if !c.contains(':') && !set.iter().any(|x| x == c) {
                set.push(c.to_owned());
            }
        }
    }
    let status_maps: Vec<Value> = by_object
        .iter()
        .filter_map(|(object, targets)| {
            let canonical = input.canonical_states.get(object).filter(|c| !c.is_empty())?;
            let mapped: Vec<&String> = canonical.iter().filter(|s| targets.contains(s)).collect();
            let unknown: Vec<&String> = targets.iter().filter(|s| !canonical.contains(s)).collect();
            Some(json!({"object": object, "canonical": canonical, "mapped": mapped, "unknown": unknown}))
        })
        .collect();
    let mut publish_blockers: Vec<String> = Vec::new();
    if ops.is_empty() && key != GENERIC_REST {
        publish_blockers.push("no operations".into());
    }
    let bad: Vec<String> = ops.iter().filter(|o| unkeyed(o)).map(op_key_of).collect();
    if !bad.is_empty() {
        publish_blockers.push(format!(
            "idempotency \u{201c}none\u{201d} on {} (W/X/P)",
            bad.join(", ")
        ));
    }
    let pinned_by: Vec<Value> = pins_of(&input.pins, key, version)
        .map(|p| {
            json!({"tenant_id": p.tenant_id, "tenant": p.tenant, "instance_id": p.instance_id, "env": p.env})
        })
        .collect();
    let base = format!("{key}@{version}");
    let bases_of: Vec<Value> = input
        .bases
        .iter()
        .filter(|b| b.base == base)
        .map(
            |b| json!({"tenant": b.tenant, "key": b.key, "version": b.version, "status": b.status}),
        )
        .collect();
    json!({
        "key": key,
        "version": version,
        "any": key == GENERIC_REST,
        "implements": ops.iter().map(op_key_of).collect::<Vec<_>>(),
        "status_maps": status_maps,
        "publish_blockers": publish_blockers,
        "pinned_by": pinned_by,
        "bases_of": bases_of,
    })
}

/// `ConnectorTemplateOverview` (TS `connectorTemplateOverview`).
pub fn overview(input: &OverviewInput) -> Value {
    let templates: Vec<Value> = input
        .templates
        .iter()
        .map(|t| {
            let n = pins_of(&input.pins, text(t, "key"), text(t, "version")).count();
            summary(t, n)
        })
        .collect();
    let coverage: Vec<Value> = input
        .templates
        .iter()
        .map(|t| coverage_of(t, input))
        .collect();
    json!({
        "templates": templates,
        "coverage": coverage,
        "catalogue": input.catalogue,
        "jobs": input.jobs,
    })
}

// ---- B6 head catalogue ------------------------------------------------------------------------------------------

/// The catalogue half of [`OverviewInput`] from B6's head catalogue.
#[derive(Debug, Clone, Default)]
pub struct HeadCatalogue {
    pub canonical_states: BTreeMap<String, Vec<String>>,
    pub catalogue: Vec<Value>,
    pub jobs: Vec<Value>,
}

/// `(layer_key, kind, key, op, fields)` of one `layer_head` row.
pub(crate) type HeadRow = (String, String, String, String, Value);

/// Current rows of every family of `kind`, folded by key over layers in `layer_key` order: `add` sets the fields,
/// `patch` merges its top-level fields over them, `tombstone` removes the key.
pub(crate) fn fold(rows: &[HeadRow], kind: &str) -> Vec<(String, Map<String, Value>)> {
    let mut out: Vec<(String, Map<String, Value>)> = Vec::new();
    for (_, _, key, op, fields) in rows.iter().filter(|r| r.1 == kind) {
        let at = out.iter().position(|(x, _)| x == key);
        let incoming = fields.as_object().cloned().unwrap_or_default();
        match (op.as_str(), at) {
            ("tombstone", Some(i)) => {
                out.remove(i);
            }
            ("tombstone", None) => {}
            ("patch", Some(i)) => out[i].1.extend(incoming),
            (_, Some(i)) => out[i].1 = incoming,
            (_, None) => out.push((key.clone(), incoming)),
        }
    }
    out
}

/// The canonical states, the operation catalogue and the job archetypes of the head catalogue (`layer_head`;
/// `agent_app` reads it, 0041).
pub async fn head_catalogue(conn: &mut PgConnection) -> ApiResult<HeadCatalogue> {
    let rows: Vec<HeadRow> = sqlx::query_as(
        "SELECT layer_key, kind, key, op, fields FROM layer_head \
          WHERE kind IN ('object', 'operation', 'operation_expectation', 'job') \
          ORDER BY layer_key, kind, key",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut canonical_states = BTreeMap::new();
    for (object, f) in fold(&rows, "object") {
        let states: Vec<String> = f
            .get("states")
            .and_then(Value::as_array)
            .map(|s| {
                s.iter()
                    .filter(|x| x["canonical"] == true)
                    .filter_map(|x| x["status"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        canonical_states.insert(object, states);
    }
    let ops = fold(&rows, "operation");
    let side_of = |key: &str| -> Option<String> {
        ops.iter().find(|(k, _)| k == key).and_then(|(_, f)| {
            f.get("attrs")?
                .get("side_effect")?
                .as_str()
                .map(str::to_owned)
        })
    };
    let catalogue: Vec<Value> = ops
        .iter()
        .map(|(key, _)| {
            let object = key.split('.').next().unwrap_or_default();
            // a variant without its own side effect inherits the unqualified operation's (TS `sideOf`)
            let unqualified = key.split('{').next().unwrap_or(key);
            let side = side_of(key).or_else(|| side_of(unqualified));
            json!({"key": key, "object": object, "side_effect": side})
        })
        .collect();
    // KC: a job's template stack, not only its own layer, carries its expected operations
    let selections: Vec<(String, Value)> = sqlx::query_as(
        "SELECT key, template_selection FROM layer WHERE scope = 'job' AND template_selection IS NOT NULL",
    )
    .fetch_all(&mut *conn)
    .await?;
    let stacks: BTreeMap<String, Vec<String>> = selections
        .iter()
        .map(|(key, sel)| (key.clone(), stack_of(key, sel)))
        .collect();
    let jobs = jobs_of(&rows, &stacks);
    Ok(HeadCatalogue {
        canonical_states,
        catalogue,
        jobs,
    })
}

/// The layers a job's template folds, in fold order (`agent_model::compile::fold_order`'s layer keys): base,
/// industry, then per selected department its `base_dept/<dept>` and `industry_dept/<industry>/<dept>`, each with
/// the selection's region sub-layer, and the job layer last. A selection that does not read is the job layer alone.
fn stack_of(job_layer: &str, selection: &Value) -> Vec<String> {
    let sel: Selection = serde_json::from_value(selection.clone()).unwrap_or_default();
    let region = sel.region.as_deref().filter(|r| !r.is_empty());
    let mut stack: Vec<String> = Vec::new();
    let mut add = |key: String| {
        if !stack.contains(&key) {
            if let Some(r) = region {
                stack.push(key.clone());
                stack.push(format!("{key}/region/{r}"));
            } else {
                stack.push(key);
            }
        }
    };
    add("base".to_owned());
    if !sel.industry.is_empty() {
        add(format!("industry/{}", sel.industry));
        for dept in &sel.departments {
            if let Some((_, name)) = parse_department(dept) {
                add(format!("base_dept/{name}"));
                add(dept.clone());
            }
        }
    }
    stack.push(job_layer.to_owned());
    stack
}

/// The job archetypes with their expected operations: `operation_expectation` rows folded over the job's stack
/// (`stacks`: job layer key -> [`stack_of`]; a job without one reads its own layer only). A later layer replaces
/// an earlier one's expectation of the same operation; a tombstone removes it.
fn jobs_of(rows: &[HeadRow], stacks: &BTreeMap<String, Vec<String>>) -> Vec<Value> {
    let expectations: Vec<&HeadRow> = rows
        .iter()
        .filter(|r| r.1 == "operation_expectation")
        .collect();
    fold(rows, "job")
        .into_iter()
        .filter(|(_, f)| f.contains_key("label"))
        .map(|(key, f)| {
            let layer = format!("job/{key}");
            let own = [layer.clone()];
            let stack = stacks.get(&layer).map_or(&own[..], Vec::as_slice);
            let mut folded: Vec<(String, String)> = Vec::new();
            for l in stack {
                for r in expectations.iter().filter(|r| &r.0 == l) {
                    folded.retain(|(op, _)| op != &r.2);
                    if r.3 != "tombstone" {
                        folded.push((r.2.clone(), text(&r.4, "summary").to_owned()));
                    }
                }
            }
            let mut ops: Vec<Value> = Vec::new();
            for (op, summary) in &folded {
                let required = summary.trim_start().starts_with("required");
                ops.push(json!({"key": op, "required": required}));
                for v in variants_in(summary) {
                    ops.push(json!({"key": format!("{op}{{{v}}}"), "required": required}));
                }
            }
            json!({"key": key, "label": f.get("label").cloned().unwrap_or(Value::Null), "ops": ops})
        })
        .collect()
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

// ---- cross-tenant rows ------------------------------------------------------------------------------------------

/// One `agent_connector_pins()` row: tenant id and name, instance id, env, template key and version, label, status.
type PinRow = (Uuid, String, String, String, String, String, String, String);

/// Every tenant's platform-scope instances and tenant templates, in the caller's transaction. Both reads are
/// `agent_platform`-only definers (0152), so they run under [`platform_db::enter`] and the transaction returns to
/// `agent_app` after. Order: tenant (name, id), then instance id / template (key, version).
pub async fn tenant_rows(conn: &mut PgConnection) -> ApiResult<(Vec<Pin>, Vec<Base>)> {
    platform_db::enter(&mut *conn).await?;
    let rows: Vec<PinRow> = sqlx::query_as(
        "SELECT tenant_id, tenant_name, instance_id, env, template_key, template_version, label, status \
           FROM agent_connector_pins() ORDER BY tenant_name, tenant_id, instance_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let pins: Vec<Pin> = rows
        .into_iter()
        .map(
            |(tenant_id, tenant, instance_id, env, key, version, label, status)| Pin {
                tenant_id: tenant_id.to_string(),
                tenant,
                instance_id,
                env,
                key,
                version,
                label,
                status,
            },
        )
        .collect();
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT tenant_name, key, version, status, base FROM agent_tenant_connector_bases() \
          ORDER BY tenant_name, tenant_id, key, version",
    )
    .fetch_all(&mut *conn)
    .await?;
    platform_db::leave(&mut *conn).await?;
    let bases: Vec<Base> = rows
        .into_iter()
        .map(|(tenant, key, version, status, base)| Base {
            tenant,
            key,
            version,
            status,
            base,
        })
        .collect();
    Ok((pins, bases))
}

/// The ops jobs route to `instance` of `tenant` (draft working rows or any live snapshot) and the tenant's JSONPath
/// overrides on it (`{op, field}`).
///
/// T13 fills this: `operation_route` and `field_mapping_override` (0094) need B2's draft ledger and B4's runner,
/// neither merged yet, so no route or override can exist and both lists are empty.
pub async fn routed_and_overrides(
    conn: &mut PgConnection,
    tenant: &str,
    instance: &str,
) -> ApiResult<(Vec<String>, Vec<Value>)> {
    let _ = (conn, tenant, instance);
    Ok((Vec::new(), Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(layer: &str, kind: &str, key: &str, op: &str, fields: Value) -> HeadRow {
        (layer.into(), kind.into(), key.into(), op.into(), fields)
    }

    #[test]
    fn a_job_expects_the_operations_of_its_whole_stack() {
        let rows = vec![
            head(
                "job/ins_agent",
                "job",
                "ins_agent",
                "add",
                json!({"label": "Insurance agent"}),
            ),
            head(
                "base_dept/customer_service",
                "operation_expectation",
                "case.create",
                "add",
                json!({"summary": "optional"}),
            ),
            head(
                "industry_dept/insurance/customer_service",
                "operation_expectation",
                "claim.create",
                "add",
                json!({"summary": "required \u{b7} {claim}"}),
            ),
            head(
                "industry_dept/insurance/customer_service",
                "operation_expectation",
                "case.create",
                "add",
                json!({"summary": "required"}),
            ),
            head(
                "industry_dept/telecom/customer_service",
                "operation_expectation",
                "plan.change",
                "add",
                json!({"summary": "required"}),
            ),
        ];
        let sel = json!({"industry": "insurance", "departments": ["industry_dept/insurance/customer_service"]});
        let stacks =
            BTreeMap::from([("job/ins_agent".to_owned(), stack_of("job/ins_agent", &sel))]);
        assert_eq!(
            stack_of("job/ins_agent", &sel),
            [
                "base",
                "industry/insurance",
                "base_dept/customer_service",
                "industry_dept/insurance/customer_service",
                "job/ins_agent"
            ]
        );
        let jobs = jobs_of(&rows, &stacks);
        let keys: Vec<(&str, bool)> = jobs[0]["ops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| (o["key"].as_str().unwrap(), o["required"].as_bool().unwrap()))
            .collect();
        // the department's layer overrides (and re-orders) the base department's case.create; telecom's plan.change is not on the stack
        assert_eq!(
            keys,
            [
                ("claim.create", true),
                ("claim.create{claim}", true),
                ("case.create", true)
            ]
        );
        // without a stored selection only the job layer counts (legacy)
        assert!(
            jobs_of(&rows, &BTreeMap::new())[0]["ops"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn variants_of_an_expectation_summary() {
        assert_eq!(
            variants_in("required \u{b7} {return, warranty_claim}"),
            vec!["return".to_owned(), "warranty_claim".to_owned()]
        );
        assert!(variants_in("optional").is_empty());
    }

    #[test]
    fn coverage_counts_pins_and_blockers() {
        let t = json!({
            "key": "acme", "version": "1", "status": "draft",
            "ops": [{"object": "case", "verb": "create", "variant": "*", "side_effect": "W", "op_kind": "standard",
                     "idempotency": {"kind": "none"},
                     "status_map": [{"system": "Open", "canonical": "open"}, {"system": "x", "canonical": "invalid_state:x"},
                                    {"system": "Odd", "canonical": "odd"}]}]
        });
        let input = OverviewInput {
            templates: vec![t],
            canonical_states: BTreeMap::from([("case".into(), vec!["new".into(), "open".into()])]),
            pins: vec![Pin {
                tenant_id: "t".into(),
                tenant: "T".into(),
                instance_id: "i".into(),
                env: "test".into(),
                key: "acme".into(),
                version: "1".into(),
                label: String::new(),
                status: String::new(),
            }],
            ..OverviewInput::default()
        };
        let o = overview(&input);
        assert_eq!(o["templates"][0]["instances"], 1);
        assert!(o["templates"][0].get("ops").is_none());
        let c = &o["coverage"][0];
        assert_eq!(c["implements"], json!(["case.create"]));
        assert_eq!(c["status_maps"][0]["mapped"], json!(["open"]));
        assert_eq!(c["status_maps"][0]["unknown"], json!(["odd"]));
        assert_eq!(
            c["publish_blockers"],
            json!(["idempotency \u{201c}none\u{201d} on case.create (W/X/P)"])
        );
        assert_eq!(c["pinned_by"][0]["instance_id"], "i");
    }
}
