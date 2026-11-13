//! J12 context spaces (plan B5 T7, R2/R12): the job's spaces as the pages show them — the bases at the design's pins
//! (B6a `job_content.context_spaces`, never copied) merged with B2's `overlay` places of object `context_space`
//! (`job_overlay_row`: override / extension / tombstone). B5 stores nothing of its own here: the tenant's `required`
//! is a field of the overlay item, written by B2's `PUT /api/jobs/{job}/context-spaces/{key}` (kind `space_required`
//! for a required-only patch, `release::overlay::save_context_space`).
//!
//! - A base space in the bundle shape (as B2's `objects.context_space`): `{key, ..fields}`, `slots` → `required`,
//!   `label` defaults to the key, `required` / `entities` to `[]` ([`bundle_item`]). Its hash is B2's `base_hash`.
//! - Effective = the base item with the tenant item's fields over it; an item without `required` (a `null` save) runs
//!   the base's slots (= the template). A tombstoned space is left out; an extension has no base (`origin: tenant`).
//! - `conflict`: an override whose base is gone (`base_removed`) or whose `base_hash` is no longer the base's hash
//!   (`base_changed`); else null.
//! - `stage` / `activity` / `request_type` / `procedure` are the item's fields when it has them. The pinned templates
//!   do not carry them today (`context_space` rows hold `label`, `area`, `knowledge_kinds`, `slots`), so they are
//!   `""` until a template or a tenant item names them — except a base space's `activity` (R15; seed v8 sets it on the retail spaces,
//!   [`Bases::space_activity`]). The space's activity is what a procedure's grant check
//!   reads ([`activity_ops`]).
use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sqlx::PgConnection;

use super::base::{self, Bases};
use super::op_label;
use super::types::{ContextSpace, Origin, SpaceConflict};
use crate::design::{self, DesignOf, JobRef, Registry};
use crate::error::ApiResult;
use crate::release::compile::TemplateCompiler;
use crate::release::overlay::{CONTEXT_SPACE, item_hash};

const TOMBSTONE: &str = "tombstone";
const OVERRIDE: &str = "override";

/// The tenant's overlay row of one space, as a design holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct SpaceRow {
    pub kind: String,
    pub item: Option<Map<String, Value>>,
    pub base_hash: Option<String>,
}

/// One effective space of a design.
#[derive(Clone, Debug, PartialEq)]
pub struct EffSpace {
    pub key: String,
    /// The base's fields with the tenant's item over them.
    pub item: Map<String, Value>,
    /// The base item (bundle shape), `None` for a tenant extension.
    pub base: Option<Map<String, Value>>,
    pub row: Option<SpaceRow>,
    /// The base's activity from the seed (R15) when the base item names none; `None` for an extension.
    pub seed_activity: Option<String>,
}

impl EffSpace {
    fn strings(m: &Map<String, Value>, f: &str) -> Vec<String> {
        m.get(f)
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn text(&self, f: &str) -> String {
        self.item
            .get(f)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    pub fn label(&self) -> String {
        let l = self.text("label");
        if l.is_empty() { self.key.clone() } else { l }
    }

    /// The effective required slots.
    pub fn required(&self) -> Vec<String> {
        Self::strings(&self.item, "required")
    }

    /// The base's slots (`[]` for an extension).
    pub fn base_required(&self) -> Vec<String> {
        self.base
            .as_ref()
            .map(|b| Self::strings(b, "required"))
            .unwrap_or_default()
    }

    /// The activity this space serves (`support:start_return`): the item's, else the base's from the seed (R15);
    /// `""` when neither names one.
    pub fn activity(&self) -> String {
        match self.text("activity") {
            a if !a.is_empty() => a,
            _ => self.seed_activity.clone().unwrap_or_default(),
        }
    }

    pub fn conflict(&self) -> ApiResult<Option<SpaceConflict>> {
        let Some(row) = self.row.as_ref().filter(|r| r.kind == OVERRIDE) else {
            return Ok(None);
        };
        Ok(match &self.base {
            None => Some(SpaceConflict::BaseRemoved),
            Some(b) => (row.base_hash.as_deref() != Some(item_hash(b)?.as_str()))
                .then_some(SpaceConflict::BaseChanged),
        })
    }

    /// The wire row (`live_required` set by the caller).
    pub fn wire(&self, tenant: &str, job: &str) -> ApiResult<ContextSpace> {
        let activity = self.activity();
        let stage = match self.text("stage") {
            s if !s.is_empty() => s,
            _ => activity
                .split_once(':')
                .map(|(s, _)| s.to_owned())
                .unwrap_or_default(),
        };
        let required = self.required();
        let base_required = self.base_required();
        Ok(ContextSpace {
            tenant_id: tenant.to_owned(),
            job: job.to_owned(),
            key: self.key.clone(),
            label: self.label(),
            stage,
            activity,
            request_type: self.text("request_type"),
            entities: Self::strings(&self.item, "entities"),
            overridden: self.base.is_some() && self.row.is_some() && required != base_required,
            required,
            base_required,
            live_required: None,
            origin: if self.base.is_some() {
                Origin::Base
            } else {
                Origin::Tenant
            },
            conflict: self.conflict()?,
            procedure: self.text("procedure"),
        })
    }
}

/// A base space in B2's bundle shape (`release::compile` `context_space_item`): `{key, ..fields}`, `slots` →
/// `required`, `label` defaults to the key, `required` / `entities` to `[]`.
pub fn bundle_item(key: &str, fields: &Map<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".to_owned(), Value::String(key.to_owned()));
    for (k, v) in fields {
        match k.as_str() {
            "key" => {}
            "slots" => {
                m.insert("required".to_owned(), v.clone());
            }
            _ => {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    m.entry("label")
        .or_insert_with(|| Value::String(key.to_owned()));
    m.entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    m.entry("entities")
        .or_insert_with(|| Value::Array(Vec::new()));
    m
}

/// The tenant's `context_space` overlay rows of design `of`, by key.
pub async fn rows(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<BTreeMap<String, SpaceRow>> {
    let mut out = BTreeMap::new();
    for r in design::values_of_kind(&mut *c, reg, job, of, "overlay").await? {
        if r.r#ref.get("object").and_then(Value::as_str) != Some(CONTEXT_SPACE) {
            continue;
        }
        let Some(v) = r.snap.filter(|v| !v.is_null()) else {
            continue;
        };
        let Some(key) = r.r#ref.get("key").and_then(Value::as_str) else {
            continue;
        };
        out.insert(
            key.to_owned(),
            SpaceRow {
                kind: v
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                item: v.get("item").and_then(Value::as_object).cloned(),
                base_hash: v
                    .get("base_hash")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        );
    }
    Ok(out)
}

/// The effective spaces of `bases` + the tenant's `rows` (pure; sorted by key; tombstoned spaces left out).
pub fn merge(bases: &Bases, rows: &BTreeMap<String, SpaceRow>) -> Vec<EffSpace> {
    let mut out: BTreeMap<String, EffSpace> = BTreeMap::new();
    for (key, fields) in &bases.context_spaces {
        let base = bundle_item(key, fields);
        out.insert(
            key.clone(),
            EffSpace {
                key: key.clone(),
                item: base.clone(),
                base: Some(base),
                row: None,
                seed_activity: bases.space_activity(key),
            },
        );
    }
    for (key, row) in rows {
        if row.kind == TOMBSTONE {
            out.remove(key);
            continue;
        }
        let e = out.entry(key.clone()).or_insert_with(|| EffSpace {
            key: key.clone(),
            item: Map::new(),
            base: None,
            row: None,
            seed_activity: None,
        });
        for (k, v) in row.item.iter().flatten() {
            e.item.insert(k.clone(), v.clone());
        }
        e.item.insert("key".to_owned(), Value::String(key.clone()));
        e.row = Some(row.clone());
    }
    out.into_values().collect()
}

/// The effective spaces of design `of` (bases at its pins + its overlay rows).
pub async fn of_design(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<(std::sync::Arc<Bases>, Vec<EffSpace>)> {
    let bases = base::at(&mut *c, reg, compiler, job, of).await?;
    let rows = rows(&mut *c, reg, job, of).await?;
    let spaces = merge(&bases, &rows);
    Ok((bases, spaces))
}

/// `contextSpaces`: the DRAFT spaces, each with `live_required` from the LIVE design (a space not live yet keeps
/// its draft slots).
pub async fn list(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    tenant: &str,
) -> ApiResult<Vec<ContextSpace>> {
    let (_, draft) = of_design(&mut *c, reg, compiler, job, &DesignOf::Draft).await?;
    let (_, live) = of_design(&mut *c, reg, compiler, job, &DesignOf::Live).await?;
    draft
        .iter()
        .map(|s| {
            let mut w = s.wire(tenant, &job.key)?;
            w.live_required = Some(
                live.iter()
                    .find(|l| l.key == s.key)
                    .map(EffSpace::required)
                    .unwrap_or_else(|| w.required.clone()),
            );
            Ok(w)
        })
        .collect()
}

/// The grant of a space's activity: the activity's operations as `object.verb{variant}` labels (fixture
/// `activity_grant`); empty when the space names no activity or the bases do not know it.
pub fn activity_ops(bases: &Bases, space: Option<&EffSpace>) -> Vec<String> {
    let Some(s) = space else {
        return Vec::new();
    };
    let activity = s.activity();
    bases
        .activity(&activity)
        .map(|a| {
            a.ops
                .iter()
                .map(|o| op_label(&o.op, o.variant.as_deref()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn bases() -> Bases {
        let mut b = Bases::default();
        b.context_spaces.insert(
            "retail:order-support:returns".into(),
            obj(json!({"label": "Returns", "area": "order-support", "slots": ["order_id", "reason"]})),
        );
        b.context_spaces.insert(
            "retail:faq:shipping".into(),
            obj(json!({"label": "Shipping", "area": "faq", "slots": []})),
        );
        b
    }

    #[test]
    fn bundle_item_renames_slots_and_defaults() {
        let i = bundle_item("a:b:c", &obj(json!({"slots": ["x"], "area": "b"})));
        assert_eq!(
            Value::Object(i),
            json!({"key": "a:b:c", "required": ["x"], "area": "b", "label": "a:b:c", "entities": []})
        );
    }

    #[test]
    fn merge_overlays_items_and_drops_tombstones() {
        let b = bases();
        let returns = "retail:order-support:returns";
        let base_hash = item_hash(&bundle_item(returns, &b.context_spaces[returns])).unwrap();
        let mut rows = BTreeMap::new();
        rows.insert(
            returns.to_owned(),
            SpaceRow {
                kind: "override".into(),
                item: Some(obj(
                    json!({"key": returns, "label": "Returns", "required": ["order_id"]}),
                )),
                base_hash: Some(base_hash),
            },
        );
        rows.insert(
            "retail:faq:shipping".to_owned(),
            SpaceRow {
                kind: "tombstone".into(),
                item: None,
                base_hash: None,
            },
        );
        rows.insert(
            "retail:faq:gifts".to_owned(),
            SpaceRow {
                kind: "extension".into(),
                item: Some(obj(
                    json!({"label": "Gifts", "required": ["card"], "activity": "support:gift"}),
                )),
                base_hash: None,
            },
        );
        let m = merge(&b, &rows);
        let keys: Vec<&str> = m.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["retail:faq:gifts", returns]);
        let r = m[1].wire("t", "j").unwrap();
        assert_eq!(r.required, ["order_id"]);
        assert_eq!(r.base_required, ["order_id", "reason"]);
        assert!(r.overridden);
        assert_eq!(r.origin, Origin::Base);
        assert_eq!(r.conflict, None);
        let g = m[0].wire("t", "j").unwrap();
        assert_eq!(g.origin, Origin::Tenant);
        assert_eq!(g.stage, "support");
        assert_eq!(g.activity, "support:gift");
        assert!(!g.overridden);
    }

    #[test]
    fn a_base_space_without_activity_takes_the_seed_s() {
        let mut b = bases();
        // R2: the base item itself names the activity it serves (seed v8 `context_space.activity`)
        b.context_spaces
            .get_mut("retail:order-support:returns")
            .unwrap()
            .insert("activity".into(), json!("support:return"));
        let m = merge(&b, &BTreeMap::new());
        let returns = m
            .iter()
            .find(|s| s.key == "retail:order-support:returns")
            .unwrap();
        let w = returns.wire("t", "j").unwrap();
        assert_eq!(w.activity, "support:return");
        assert_eq!(w.stage, "support");
        // the item names no activity for shipping: no activity, an empty grant
        let shipping = m.iter().find(|s| s.key == "retail:faq:shipping").unwrap();
        assert_eq!(shipping.activity(), "");
        assert!(activity_ops(&b, Some(shipping)).is_empty());
    }

    #[test]
    fn an_item_without_required_runs_the_template_slots() {
        let b = bases();
        let returns = "retail:order-support:returns";
        let mut rows = BTreeMap::new();
        rows.insert(
            returns.to_owned(),
            SpaceRow {
                kind: "override".into(),
                item: Some(obj(json!({"key": returns, "label": "Mine"}))),
                base_hash: Some("sha256:stale".into()),
            },
        );
        let m = merge(&b, &rows);
        let r = m
            .iter()
            .find(|s| s.key == returns)
            .unwrap()
            .wire("t", "j")
            .unwrap();
        assert_eq!(r.required, ["order_id", "reason"]);
        assert!(!r.overridden);
        assert_eq!(r.label, "Mine");
        assert_eq!(r.conflict, Some(SpaceConflict::BaseChanged));
    }

    #[test]
    fn an_override_without_base_is_base_removed() {
        let mut rows = BTreeMap::new();
        rows.insert(
            "x:y:z".to_owned(),
            SpaceRow {
                kind: "override".into(),
                item: Some(obj(json!({"label": "Gone", "required": []}))),
                base_hash: Some("sha256:0".into()),
            },
        );
        let m = merge(&Bases::default(), &rows);
        assert_eq!(m[0].conflict().unwrap(), Some(SpaceConflict::BaseRemoved));
    }
}
