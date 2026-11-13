//! B5's release conflict detector (plan B5 T4, R3; spec 05 §4.6): the job-design rows a tenant made against the
//! bases of one template stack, checked against the bases of the release being cut or applied
//! ([`base::of_release`]: its pins, selection and stored bundle). Registered once in
//! `release::conflicts::registry()`. LOCAL STATE ONLY (a compile of tagged layers, no upstream call).
//!
//! | Row | Object · key | Raised |
//! |---|---|---|
//! | `ontology_entity_refinement` (override) | `entity` · `<name>` | `parent_missing` when the base entity is gone; `base_changed` when its [`entity_hash`] ≠ the row's `base_hash` |
//! | `ontology_entity` (extension) | `entity` · `<name>` | `base_shadowed` when the base now has an entity of that name |
//! | `job_gate_option_refinement` (override) | `gate` · `<gate>:<option>` | `parent_missing` when the gate or the option is gone; `base_changed` on a [`gate_option_hash`] mismatch |
//! | `job_gate_rule_refinement` with a `base_hash` (override) | `gate` · `<gate>:threshold` | `parent_missing` when the gate is gone; `base_changed` on a [`gate_rule_hash`] mismatch |
//! | `job_gate_question` (extension) | `gate_question` · `<key>` | `base_shadowed` when the library now has that key |
//! | `toolset_narrowing` (tombstone) | `narrowing` · `<role>:<activity>:<op>` | `parent_missing` when the role or the activity is gone |
//! | `procedure_version` (extension) | `procedure` · `<name>@<version>` | `base_shadowed` when the base now has that version of the name |
//!
//! A release whose template does not compile says nothing about what moved: no conflicts are raised from it (its
//! cut is refused by B2 before this runs; a stored one keeps its conflicts from the last cut that compiled).
//! B2's own `ontology_attribute` / `gate_option` / `context_space` / `activity` overlays stay with B2's
//! `OverlayDetector`.
use std::collections::BTreeSet;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use sqlx::PgConnection;

use super::base::{self, Bases, entity_hash, gate_option_hash, gate_rule_hash};
use crate::design::JobRef;
use crate::error::ApiResult;
use crate::release::conflicts::{ConflictDetector, NewConflict};

/// The detector's registry name.
pub const NAME: &str = "job_design";

/// B5's conflict source (`release::conflicts::registry`).
pub struct JobDesignDetector;

impl ConflictDetector for JobDesignDetector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn detect<'a>(
        &'a self,
        c: &'a mut PgConnection,
        job: &'a JobRef,
        _from: Option<&'a Value>,
        _to: &'a Value,
        release_id: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<NewConflict>>> {
        Box::pin(async move {
            let Some(bases) = base::of_release(&mut *c, job, release_id).await? else {
                return Ok(Vec::new());
            };
            if !bases.template_errors.is_empty() {
                return Ok(Vec::new());
            }
            let rows = Rows::read(&mut *c, job).await?;
            classify(&bases, &rows)
        })
    }
}

/// The job's B5 rows the detector compares (working rows, as B2's detector reads `job_overlay_row`).
#[derive(Clone, Debug, Default)]
pub struct Rows {
    /// Tenant entity names.
    pub entities: Vec<String>,
    /// `(name, base_hash)`.
    pub entity_refinements: Vec<(String, String)>,
    /// `(gate, option, base_hash)`.
    pub option_refinements: Vec<(String, String, String)>,
    /// `(gate, base_hash)`; a NULL `base_hash` (a tenant question's rule) is not a base refinement.
    pub rule_refinements: Vec<(String, Option<String>)>,
    /// Tenant gate question keys.
    pub questions: Vec<String>,
    /// `(role, activity, op)`.
    pub narrowings: Vec<(String, String, String)>,
    /// `(name, version)` of every tenant procedure version.
    pub procedures: Vec<(String, i32)>,
}

impl Rows {
    pub async fn read(c: &mut PgConnection, job: &JobRef) -> ApiResult<Rows> {
        let id = job.id;
        Ok(Rows {
            entities: sqlx::query_scalar(
                "SELECT name FROM ontology_entity WHERE job_id = $1 ORDER BY name",
            )
            .bind(id)
            .fetch_all(&mut *c)
            .await?,
            entity_refinements: sqlx::query_as(
                "SELECT name, base_hash FROM ontology_entity_refinement WHERE job_id = $1 ORDER BY name",
            )
            .bind(id)
            .fetch_all(&mut *c)
            .await?,
            option_refinements: sqlx::query_as(
                "SELECT gate, option, base_hash FROM job_gate_option_refinement WHERE job_id = $1 \
                  ORDER BY gate, option",
            )
            .bind(id)
            .fetch_all(&mut *c)
            .await?,
            rule_refinements: sqlx::query_as(
                "SELECT gate, base_hash FROM job_gate_rule_refinement WHERE job_id = $1 ORDER BY gate",
            )
            .bind(id)
            .fetch_all(&mut *c)
            .await?,
            questions: sqlx::query_scalar(
                "SELECT key FROM job_gate_question WHERE job_id = $1 ORDER BY key",
            )
            .bind(id)
            .fetch_all(&mut *c)
            .await?,
            narrowings: sqlx::query_as(
                "SELECT role, activity, op FROM toolset_narrowing WHERE job_id = $1 \
                  ORDER BY role, activity, op",
            )
            .bind(id)
            .fetch_all(&mut *c)
            .await?,
            procedures: sqlx::query_as(
                "SELECT name, version FROM procedure_version WHERE job_id = $1 ORDER BY name, version",
            )
            .bind(id)
            .fetch_all(&mut *c)
            .await?,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn conflict(
    kind: &str,
    object: &str,
    key: String,
    tenant_row: &str,
    detail: String,
    before: Option<&str>,
    after: Option<String>,
    base_after: Option<&Map<String, Value>>,
) -> NewConflict {
    NewConflict {
        kind: kind.to_owned(),
        object: object.to_owned(),
        key,
        tenant_row: tenant_row.to_owned(),
        detail,
        base_hash_before: before.map(str::to_owned),
        base_hash_after: after,
        base_before: None,
        base_after: base_after.cloned().map(Value::Object),
    }
}

/// The pure part of [`JobDesignDetector`]: `rows` against `bases` (module docs), in table order.
pub fn classify(bases: &Bases, rows: &Rows) -> ApiResult<Vec<NewConflict>> {
    let mut out = Vec::new();
    for (name, hash) in &rows.entity_refinements {
        match bases.entity(name) {
            None => out.push(conflict(
                "parent_missing",
                "entity",
                name.clone(),
                "override",
                format!("The release removed entity {name}; your refinement of it has no base."),
                Some(hash.as_str()),
                None,
                None,
            )),
            Some(item) => {
                let now = entity_hash(item)?;
                if now != *hash {
                    out.push(conflict(
                        "base_changed",
                        "entity",
                        name.clone(),
                        "override",
                        format!(
                            "The release changed entity {name}; your refinement was made on the old one."
                        ),
                        Some(hash.as_str()),
                        Some(now),
                        Some(item),
                    ));
                }
            }
        }
    }
    for name in &rows.entities {
        if let Some(item) = bases.entity(name) {
            out.push(conflict(
                "base_shadowed",
                "entity",
                name.clone(),
                "extension",
                format!("The release added a base entity {name}; your entity of that name now shadows it."),
                None,
                Some(entity_hash(item)?),
                Some(item),
            ));
        }
    }
    for (gate, option, hash) in &rows.option_refinements {
        let key = format!("{gate}:{option}");
        if bases.gate(gate).is_none() {
            out.push(conflict(
                "parent_missing",
                "gate",
                key,
                "override",
                format!("The release removed gate {gate}; your refinement of option “{option}” has no base."),
                Some(hash.as_str()),
                None,
                None,
            ));
            continue;
        }
        match bases.gate_option(gate, option) {
            None => out.push(conflict(
                "parent_missing",
                "gate",
                key,
                "override",
                format!("The release removed option “{option}” of gate {gate}; your refinement has no base."),
                Some(hash.as_str()),
                None,
                None,
            )),
            Some(o) => {
                let now = gate_option_hash(o)?;
                if now != *hash {
                    out.push(conflict(
                        "base_changed",
                        "gate",
                        key,
                        "override",
                        format!(
                            "The release changed option “{option}” of gate {gate}; your refinement was made on the old one."
                        ),
                        Some(hash.as_str()),
                        Some(now),
                        Some(o),
                    ));
                }
            }
        }
    }
    for (gate, hash) in &rows.rule_refinements {
        let Some(hash) = hash else {
            continue;
        };
        let key = format!("{gate}:threshold");
        match bases.gate(gate) {
            None => out.push(conflict(
                "parent_missing",
                "gate",
                key,
                "override",
                format!("The release removed gate {gate}; your threshold or rule has no base."),
                Some(hash.as_str()),
                None,
                None,
            )),
            Some(g) => {
                let now = gate_rule_hash(g)?;
                if now != *hash {
                    out.push(conflict(
                        "base_changed",
                        "gate",
                        key,
                        "override",
                        format!(
                            "The release changed the rule of gate {gate}; your tuning was made on the old one."
                        ),
                        Some(hash.as_str()),
                        Some(now),
                        Some(&g.fields),
                    ));
                }
            }
        }
    }
    for q in &rows.questions {
        if let Some(g) = bases.gate(q) {
            out.push(conflict(
                "base_shadowed",
                "gate_question",
                q.clone(),
                "extension",
                format!("The release added a library gate {q}; your question of that key now shadows it."),
                None,
                None,
                Some(&g.fields),
            ));
        }
    }
    let roles: BTreeSet<&str> = bases.agent_roles.iter().map(|r| r.key.as_str()).collect();
    for (role, activity, op) in &rows.narrowings {
        let key = format!("{role}:{activity}:{op}");
        let missing = if !roles.contains(role.as_str()) {
            Some(format!("role {role}"))
        } else if bases.activity(activity).is_none() {
            Some(format!("activity {activity}"))
        } else {
            None
        };
        if let Some(what) = missing {
            out.push(conflict(
                "parent_missing",
                "narrowing",
                key,
                "tombstone",
                format!("The release removed {what}; your narrowing of {op} has no base."),
                None,
                None,
                None,
            ));
        }
    }
    for (name, version) in &rows.procedures {
        let Ok(v) = u32::try_from(*version) else {
            continue;
        };
        if bases.procedure_versions(name).contains(&v) {
            out.push(conflict(
                "base_shadowed",
                "procedure",
                format!("{name}@{version}"),
                "extension",
                format!(
                    "The release added version {version} of procedure {name}; your version of that number now shadows it."
                ),
                None,
                None,
                bases.procedures.get(&format!("{name}@{version}")),
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::compile::CatalogueItem;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn item(key: &str, fields: Value) -> CatalogueItem {
        CatalogueItem {
            key: key.into(),
            row_id: "r".into(),
            layer: "base".into(),
            fields: obj(fields),
        }
    }

    fn bases() -> Bases {
        let mut b = Bases::default();
        b.entities.insert(
            "Order".into(),
            obj(json!({"name": "Order", "label": "Order"})),
        );
        b.gates.push(item(
            "GT",
            json!({"options": [{"key": "need", "label": "Need"}], "threshold_value": 0.5, "rule": null}),
        ));
        b.agent_roles.push(item("procedure", json!({})));
        b.activities.push(base::BaseActivity {
            key: "change:cancel_order".into(),
            item: obj(json!({"key": "change:cancel_order"})),
            ops: Vec::new(),
        });
        b.procedures.insert("return@2".into(), Map::new());
        b
    }

    fn kinds(out: &[NewConflict]) -> Vec<(String, String, String)> {
        out.iter()
            .map(|c| (c.kind.clone(), c.object.clone(), c.key.clone()))
            .collect()
    }

    fn t(kind: &str, object: &str, key: &str) -> (String, String, String) {
        (kind.into(), object.into(), key.into())
    }

    #[test]
    fn unchanged_rows_raise_nothing() {
        let b = bases();
        let rows = Rows {
            entity_refinements: vec![("Order".into(), entity_hash(&b.entities["Order"]).unwrap())],
            option_refinements: vec![(
                "GT".into(),
                "need".into(),
                gate_option_hash(b.gate_option("GT", "need").unwrap()).unwrap(),
            )],
            rule_refinements: vec![
                (
                    "GT".into(),
                    Some(gate_rule_hash(b.gate("GT").unwrap()).unwrap()),
                ),
                ("TQ1".into(), None),
            ],
            questions: vec!["TQ1".into()],
            narrowings: vec![(
                "procedure".into(),
                "change:cancel_order".into(),
                "order.cancel".into(),
            )],
            procedures: vec![("return".into(), 3)],
            entities: vec!["GiftCard".into()],
        };
        assert!(classify(&b, &rows).unwrap().is_empty());
    }

    #[test]
    fn every_kind_is_raised() {
        let b = bases();
        let rows = Rows {
            entity_refinements: vec![
                ("Gone".into(), "sha256:x".into()),
                ("Order".into(), "sha256:stale".into()),
            ],
            entities: vec!["Order".into()],
            option_refinements: vec![
                ("ZZ".into(), "a".into(), "sha256:x".into()),
                ("GT".into(), "gone".into(), "sha256:x".into()),
                ("GT".into(), "need".into(), "sha256:stale".into()),
            ],
            rule_refinements: vec![
                ("ZZ".into(), Some("sha256:x".into())),
                ("GT".into(), Some("sha256:stale".into())),
            ],
            questions: vec!["GT".into()],
            narrowings: vec![
                (
                    "ghost".into(),
                    "change:cancel_order".into(),
                    "order.cancel".into(),
                ),
                ("procedure".into(), "x:y".into(), "order.cancel".into()),
            ],
            procedures: vec![("return".into(), 2)],
        };
        let out = classify(&b, &rows).unwrap();
        assert_eq!(
            kinds(&out),
            [
                t("parent_missing", "entity", "Gone"),
                t("base_changed", "entity", "Order"),
                t("base_shadowed", "entity", "Order"),
                t("parent_missing", "gate", "ZZ:a"),
                t("parent_missing", "gate", "GT:gone"),
                t("base_changed", "gate", "GT:need"),
                t("parent_missing", "gate", "ZZ:threshold"),
                t("base_changed", "gate", "GT:threshold"),
                t("base_shadowed", "gate_question", "GT"),
                t(
                    "parent_missing",
                    "narrowing",
                    "ghost:change:cancel_order:order.cancel"
                ),
                t("parent_missing", "narrowing", "procedure:x:y:order.cancel"),
                t("base_shadowed", "procedure", "return@2"),
            ]
        );
        let changed = &out[1];
        assert_eq!(changed.base_hash_before.as_deref(), Some("sha256:stale"));
        assert_eq!(
            changed.base_hash_after,
            Some(entity_hash(&b.entities["Order"]).unwrap())
        );
        assert_eq!(out[0].tenant_row, "override");
        assert_eq!(out[2].tenant_row, "extension");
        assert_eq!(out[9].tenant_row, "tombstone");
    }
}
