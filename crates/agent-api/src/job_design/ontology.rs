//! J11: the job's effective ontology (plan B5 "Snapshot targets", effective entity merge, T6). The base entities,
//! attributes and relations at the design's pins ([`Bases`], plan R1) with the tenant's rows on top:
//!
//! | Rows | Effect |
//! |---|---|
//! | `ontology_entity` | tenant entities, `origin: tenant`, their attributes from the row's `attributes` |
//! | `ontology_entity_refinement` | a base entity hidden, synonyms added (`synonyms_added` after the base's) |
//! | B2 `job_overlay_row` object `ontology_attribute` (key `Entity.attr`) | `extension`: an attribute the tenant added to a base entity (`origin: tenant`); `override`: a base attribute changed; `tombstone`: a base attribute hidden; an `override` whose base is gone shows with `conflict: parent_missing` |
//! | `ontology_relation` | tenant relations, `origin: tenant` |
//!
//! The folds ([`entities`], [`relations`]) are pure over plain rows, so the routes (DRAFT working rows, [`draft_rows`])
//! and the runtime (a snapshot's values, T12) cannot drift. A base entity's `scope`, `industry`, `department` and
//! `layer` come from the layer that first wrote it ([`Bases::authored_in`]).
use std::collections::BTreeMap;

use agent_model::compile::LayerMeta;
use agent_model::layer::Scope as LayerScope;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::base::Bases;
use super::types::{
    AttributeConflict, Cardinality, OntologyAttribute, OntologyEntity, OntologyRelation,
    OntologyScope, Origin,
};
use crate::error::ApiResult;
use crate::release::overlay::ONTOLOGY_ATTRIBUTE;

/// Fixture `createOntologyEntity`: `^[A-Z][A-Za-z0-9]{0,63}$`.
pub fn is_entity_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_uppercase()
        && b.iter().all(u8::is_ascii_alphanumeric)
}

/// Fixture `addOntologyAttribute`: `^[a-z][a-z0-9_]{0,63}$`.
pub fn is_attribute_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Fixture `createOntologyRelation`: `^[a-z][A-Za-z0-9]{0,63}$`.
pub fn is_predicate(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter().all(u8::is_ascii_alphanumeric)
}

/// The base relation key `Subject|predicate|Object`.
pub fn relation_key(subject: &str, predicate: &str, object: &str) -> String {
    format!("{subject}|{predicate}|{object}")
}

// ---- the working rows ---------------------------------------------------------------------------------------------

/// One `ontology_entity` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TenantEntityRow {
    pub name: String,
    pub label: String,
    pub scope: String,
    pub industry: Option<String>,
    pub department: Option<String>,
    pub synonyms: Vec<String>,
    pub attributes: Value,
    pub hidden: bool,
    pub object_key: Option<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// One `ontology_entity_refinement` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RefinementRow {
    pub name: String,
    pub hidden: bool,
    pub synonyms_added: Vec<String>,
}

/// One B2 overlay row of object `ontology_attribute`.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AttributeRow {
    pub key: String,
    pub kind: String,
    pub item: Option<Value>,
}

/// One `ontology_relation` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RelationRow {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub cardinality: String,
}

/// The tenant's ontology rows of one design.
#[derive(Clone, Debug, Default)]
pub struct Rows {
    pub entities: Vec<TenantEntityRow>,
    pub refinements: Vec<RefinementRow>,
    pub attributes: Vec<AttributeRow>,
    pub relations: Vec<RelationRow>,
}

impl Rows {
    pub fn tenant_entity(&self, name: &str) -> Option<&TenantEntityRow> {
        self.entities.iter().find(|e| e.name == name)
    }

    pub fn refinement(&self, name: &str) -> Option<&RefinementRow> {
        self.refinements.iter().find(|r| r.name == name)
    }

    pub fn tenant_relation(
        &self,
        subject: &str,
        predicate: &str,
        object: &str,
    ) -> Option<&RelationRow> {
        self.relations
            .iter()
            .find(|r| r.subject == subject && r.predicate == predicate && r.object == object)
    }
}

/// The job's DRAFT ontology rows (the working rows), in the caller's job-scoped transaction.
pub async fn draft_rows(c: &mut PgConnection, job: Uuid) -> ApiResult<Rows> {
    let entities = sqlx::query_as::<_, TenantEntityRow>(
        "SELECT name, label, scope, industry, department, synonyms, attributes, hidden, object_key, created_by, \
                created_at \
           FROM ontology_entity WHERE job_id = $1 ORDER BY name",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    let refinements = sqlx::query_as::<_, RefinementRow>(
        "SELECT name, hidden, synonyms_added FROM ontology_entity_refinement WHERE job_id = $1 ORDER BY name",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    let attributes = sqlx::query_as::<_, AttributeRow>(
        "SELECT key, kind, item FROM job_overlay_row WHERE job_id = $1 AND object = $2 ORDER BY key",
    )
    .bind(job)
    .bind(ONTOLOGY_ATTRIBUTE)
    .fetch_all(&mut *c)
    .await?;
    let relations = sqlx::query_as::<_, RelationRow>(
        "SELECT subject, predicate, object, cardinality FROM ontology_relation WHERE job_id = $1 \
          ORDER BY subject, predicate, object",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    Ok(Rows {
        entities,
        refinements,
        attributes,
        relations,
    })
}

// ---- the folds ----------------------------------------------------------------------------------------------------

fn wire<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(Value::String(s.to_owned())).ok()
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn text(m: &Map<String, Value>, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// An attribute from a base item (B6a names: `data_type`) or an overlay item (bundle names: `type`).
fn attribute(name: &str, m: &Map<String, Value>, origin: Origin) -> OntologyAttribute {
    OntologyAttribute {
        name: name.to_owned(),
        data_type: text(m, "data_type")
            .or_else(|| text(m, "type"))
            .unwrap_or_else(|| "string".to_owned()),
        pii: m.get("pii").and_then(Value::as_bool).unwrap_or(false),
        synonyms: strings(m.get("synonyms")),
        origin,
        conflict: None,
    }
}

/// `layer@tag` (web `Layer` label), or the bare layer label without a tag.
fn layer_label(l: &LayerMeta) -> String {
    match &l.tag {
        Some(t) if !t.is_empty() => format!("{}@{t}", l.label()),
        _ => l.label(),
    }
}

fn scope_of(l: Option<&LayerMeta>) -> OntologyScope {
    match l.map(|l| l.key.scope) {
        // D116: a base_dept layer is department-level content (industry-independent), shown as the department scope
        Some(LayerScope::IndustryDept | LayerScope::BaseDept) => OntologyScope::IndustryDept,
        Some(LayerScope::Industry) => OntologyScope::Industry,
        Some(LayerScope::Job) if l.is_some_and(|l| l.key.department.is_some()) => {
            OntologyScope::IndustryDept
        }
        Some(LayerScope::Job) if l.is_some_and(|l| l.key.industry.is_some()) => {
            OntologyScope::Industry
        }
        _ => OntologyScope::Generic,
    }
}

/// A base entity's attributes: the base ones (B2 overrides applied, tombstones dropped), then the tenant's extensions,
/// then overrides whose base attribute is gone (`conflict: parent_missing`).
fn base_attributes(bases: &Bases, rows: &Rows, entity: &str) -> Vec<OntologyAttribute> {
    let prefix = format!("{entity}.");
    let overlay: BTreeMap<&str, &AttributeRow> = rows
        .attributes
        .iter()
        .filter(|r| r.key.starts_with(&prefix))
        .map(|r| (r.key.as_str(), r))
        .collect();
    let mut out = Vec::new();
    for (key, fields) in bases.attributes.range(prefix.clone()..) {
        let Some(name) = key.strip_prefix(&prefix) else {
            break;
        };
        match overlay.get(key.as_str()) {
            Some(r) if r.kind == "tombstone" => {}
            Some(r) if r.kind == "override" => {
                let item = r.item.as_ref().and_then(Value::as_object).unwrap_or(fields);
                out.push(attribute(name, item, Origin::Base));
            }
            _ => out.push(attribute(name, fields, Origin::Base)),
        }
    }
    for (key, r) in &overlay {
        if bases.attributes.contains_key(*key) || r.kind == "tombstone" {
            continue;
        }
        let Some(item) = r.item.as_ref().and_then(Value::as_object) else {
            continue;
        };
        let mut a = attribute(&key[prefix.len()..], item, Origin::Tenant);
        if r.kind != "extension" {
            a.conflict = Some(AttributeConflict::ParentMissing);
        }
        out.push(a);
    }
    out
}

fn tenant_attributes(v: &Value) -> Vec<OntologyAttribute> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|m| Some(attribute(m.get("name")?.as_str()?, m, Origin::Tenant)))
        .collect()
}

fn base_entity(
    bases: &Bases,
    rows: &Rows,
    tenant: Uuid,
    job_key: &str,
    name: &str,
    fields: &Map<String, Value>,
) -> OntologyEntity {
    let layer = bases.authored_in.get(&format!("entity:{name}"));
    let refinement = rows.refinement(name);
    let mut synonyms = strings(fields.get("synonyms"));
    if let Some(r) = refinement {
        synonyms.extend(r.synonyms_added.iter().cloned());
    }
    let scope = scope_of(layer);
    OntologyEntity {
        tenant_id: tenant.to_string(),
        job: job_key.to_owned(),
        name: name.to_owned(),
        label: text(fields, "label").unwrap_or_else(|| name.to_owned()),
        scope,
        industry: match scope {
            OntologyScope::Generic => None,
            _ => layer
                .and_then(|l| l.key.industry.clone())
                .or_else(|| Some(bases.selection.industry.clone()).filter(|i| !i.is_empty())),
        },
        department: match scope {
            OntologyScope::IndustryDept => layer.and_then(|l| l.key.department.clone()),
            _ => None,
        },
        synonyms,
        attributes: base_attributes(bases, rows, name),
        origin: Origin::Base,
        hidden: refinement.is_some_and(|r| r.hidden),
        object_key: text(fields, "object_key"),
        layer: layer.map(layer_label).unwrap_or_else(|| "base".to_owned()),
        created_by: None,
        created_at: layer
            .and_then(|l| l.tagged_at.as_deref())
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or(DateTime::UNIX_EPOCH),
    }
}

fn tenant_entity(e: &TenantEntityRow, tenant: Uuid, job_key: &str) -> OntologyEntity {
    OntologyEntity {
        tenant_id: tenant.to_string(),
        job: job_key.to_owned(),
        name: e.name.clone(),
        label: e.label.clone(),
        scope: wire(&e.scope).unwrap_or(OntologyScope::Generic),
        industry: e.industry.clone(),
        department: e.department.clone(),
        synonyms: e.synonyms.clone(),
        attributes: tenant_attributes(&e.attributes),
        origin: Origin::Tenant,
        hidden: e.hidden,
        object_key: e.object_key.clone(),
        layer: "refinement".to_owned(),
        created_by: Some(e.created_by.clone()),
        created_at: e.created_at,
    }
}

/// Every entity of the design (hidden ones included), by name. A tenant entity whose name the base now also has
/// (a later template, `base_shadowed`) shows as the tenant's.
pub fn entities(bases: &Bases, rows: &Rows, tenant: Uuid, job_key: &str) -> Vec<OntologyEntity> {
    let mut out: BTreeMap<String, OntologyEntity> = bases
        .entities
        .iter()
        .map(|(name, fields)| {
            (
                name.clone(),
                base_entity(bases, rows, tenant, job_key, name, fields),
            )
        })
        .collect();
    for e in &rows.entities {
        out.insert(e.name.clone(), tenant_entity(e, tenant, job_key));
    }
    out.into_values().collect()
}

/// Entity `name` of the design (hidden or not), or `None`.
pub fn entity(
    bases: &Bases,
    rows: &Rows,
    tenant: Uuid,
    job_key: &str,
    name: &str,
) -> Option<OntologyEntity> {
    if let Some(e) = rows.tenant_entity(name) {
        return Some(tenant_entity(e, tenant, job_key));
    }
    bases
        .entity(name)
        .map(|fields| base_entity(bases, rows, tenant, job_key, name, fields))
}

/// Every relation of the design: the base relations (key `Subject|predicate|Object`), then the tenant's.
pub fn relations(bases: &Bases, rows: &Rows, tenant: Uuid, job_key: &str) -> Vec<OntologyRelation> {
    let mut out: Vec<OntologyRelation> = bases
        .relations
        .iter()
        .filter_map(|(key, fields)| {
            let mut parts = key.splitn(3, '|');
            let (s, p, o) = (parts.next()?, parts.next()?, parts.next()?);
            Some(OntologyRelation {
                tenant_id: tenant.to_string(),
                job: job_key.to_owned(),
                subject: s.to_owned(),
                predicate: p.to_owned(),
                object: o.to_owned(),
                cardinality: fields
                    .get("cardinality")
                    .and_then(Value::as_str)
                    .and_then(wire)
                    .unwrap_or(Cardinality::ManyToMany),
                origin: Origin::Base,
            })
        })
        .collect();
    out.extend(
        rows.relations
            .iter()
            .map(|r| tenant_relation(r, tenant, job_key)),
    );
    out
}

/// One tenant relation on the wire.
pub fn tenant_relation(r: &RelationRow, tenant: Uuid, job_key: &str) -> OntologyRelation {
    OntologyRelation {
        tenant_id: tenant.to_string(),
        job: job_key.to_owned(),
        subject: r.subject.clone(),
        predicate: r.predicate.clone(),
        object: r.object.clone(),
        cardinality: wire(&r.cardinality).unwrap_or(Cardinality::ManyToMany),
        origin: Origin::Tenant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn names_follow_the_fixture_patterns() {
        assert!(is_entity_name("GiftCard"));
        assert!(!is_entity_name("giftCard"));
        assert!(!is_entity_name("Gift_Card"));
        assert!(!is_entity_name(&format!("A{}", "b".repeat(64))));
        assert!(is_attribute_name("gift_message"));
        assert!(!is_attribute_name("GiftMessage"));
        assert!(!is_attribute_name("_x"));
        assert!(is_predicate("paidBy"));
        assert!(!is_predicate("PaidBy"));
        assert!(!is_predicate("paid_by"));
    }

    #[test]
    fn base_entities_fold_refinements_and_attribute_overlays() {
        let mut b = Bases::default();
        b.entities.insert(
            "Order".into(),
            obj(json!({"label": "Order", "synonyms": ["purchase"]})),
        );
        b.attributes
            .insert("Order.order_id".into(), obj(json!({"data_type": "string"})));
        b.attributes
            .insert("Order.total".into(), obj(json!({"data_type": "money"})));
        b.attributes
            .insert("OrderLine.qty".into(), obj(json!({"data_type": "int"})));
        b.relations.insert(
            "Order|paidBy|Payment".into(),
            obj(json!({"cardinality": "1:n"})),
        );
        let rows = Rows {
            entities: vec![TenantEntityRow {
                name: "Warranty".into(),
                label: "Warranty claim".into(),
                scope: "generic".into(),
                industry: None,
                department: None,
                synonyms: vec![],
                attributes: json!([{"name": "serial", "data_type": "string", "pii": false, "synonyms": []}]),
                hidden: true,
                object_key: None,
                created_by: "u".into(),
                created_at: Utc::now(),
            }],
            refinements: vec![RefinementRow {
                name: "Order".into(),
                hidden: false,
                synonyms_added: vec!["bestelling".into()],
            }],
            attributes: vec![
                AttributeRow {
                    key: "Order.total".into(),
                    kind: "tombstone".into(),
                    item: None,
                },
                AttributeRow {
                    key: "Order.gift_note".into(),
                    kind: "extension".into(),
                    item: Some(
                        json!({"key": "Order.gift_note", "type": "text", "pii": true, "synonyms": []}),
                    ),
                },
            ],
            relations: vec![],
        };
        let t = Uuid::nil();
        let es = entities(&b, &rows, t, "j1");
        assert_eq!(
            es.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["Order", "Warranty"]
        );
        let order = &es[0];
        assert_eq!(order.origin, Origin::Base);
        assert_eq!(order.synonyms, ["purchase", "bestelling"]);
        let attrs: Vec<(&str, Origin, &str)> = order
            .attributes
            .iter()
            .map(|a| (a.name.as_str(), a.origin, a.data_type.as_str()))
            .collect();
        assert_eq!(
            attrs,
            [
                ("order_id", Origin::Base, "string"),
                ("gift_note", Origin::Tenant, "text")
            ]
        );
        assert!(order.attributes[1].pii);
        assert!(es[1].hidden);
        assert_eq!(es[1].attributes[0].name, "serial");
        let rs = relations(&b, &rows, t, "j1");
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].cardinality, Cardinality::OneToMany);
        assert_eq!(rs[0].origin, Origin::Base);
    }
}
