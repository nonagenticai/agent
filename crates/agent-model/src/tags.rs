//! Template tag logic (plan B6-4, B6-5, §4; port of `web/src/lib/model/logic.ts`): a tag's contents as UI
//! `TagItem`s, the diff between two contents, the content hash, the derived draft op of an untagged row, the
//! composition stack of a template layer and the catch-up a pin takes when it moves to the latest tag. Pure.
//!
//! This module is the ONE definition of the tag content hash ([`content_hash`]) and of the draft op
//! ([`draft_op`]) (Ruling 16): `agent-api` re-exports [`TagMember`] / [`content_hash`] and its SQL draft-op `CASE`
//! must agree with [`draft_op`].
//!
//! Sorting is bytewise (Rust `str` order) where the web uses `localeCompare`; the keys are ASCII identifiers.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical::{canonical_json, fields_hash, sha256_hex};
use crate::compile::Row;
use crate::kinds::{self, Kind, Op};
use crate::layer::Scope;

// --- content hash + draft op ----------------------------------------------------------------------------------------

/// What a tag's content hash covers of one member row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct TagMember {
    /// The member's layer key.
    pub layer: String,
    pub kind: String,
    pub key: String,
    pub op: String,
    pub fields_hash: String,
}

/// A member's identity in a tag: (layer key, kind, key).
pub type MemberKey = (String, String, String);

impl TagMember {
    /// (layer key, kind, key).
    pub fn member_key(&self) -> MemberKey {
        (self.layer.clone(), self.kind.clone(), self.key.clone())
    }
}

/// `sha256:<hex>` of the canonical JSON of the members `[{layer, kind, key, op, fields_hash}]`, sorted by
/// (layer, kind, key, op, fields_hash). Pure and order-independent (B6-4; written by T7, moved here by Ruling 16).
pub fn content_hash(members: &[TagMember]) -> String {
    let mut sorted: Vec<&TagMember> = members.iter().collect();
    sorted.sort_unstable();
    let list = Value::Array(
        sorted
            .iter()
            .map(|m| {
                json!({
                    "layer": m.layer,
                    "kind": m.kind,
                    "key": m.key,
                    "op": m.op,
                    "fields_hash": m.fields_hash,
                })
            })
            .collect(),
    );
    format!("sha256:{}", sha256_hex(&canonical_json(&list)))
}

/// The (layer, kind, key) set of a tag's members: what [`draft_op`] looks a row up in.
pub fn member_keys(members: &[TagMember]) -> BTreeSet<MemberKey> {
    members.iter().map(TagMember::member_key).collect()
}

/// The draft op of an untagged current row (B6-5, P6 `DraftRevision.op`), in this order: `tombstone` when the row
/// is a tombstone; else `add` when its (layer, kind, key) is absent from the family's latest tag
/// (`latest_tag_keys`, empty when the family has no tag); else `patch`. Tombstone first, so a tombstone of a key
/// that only a lower layer defines reads as a tombstone, not as an add (T7 decision 4, Ruling 16).
pub fn draft_op(
    op: Op,
    layer: &str,
    kind: &str,
    key: &str,
    latest_tag_keys: &BTreeSet<MemberKey>,
) -> Op {
    if op == Op::Tombstone {
        return Op::Tombstone;
    }
    let id: MemberKey = (layer.to_owned(), kind.to_owned(), key.to_owned());
    if latest_tag_keys.contains(&id) {
        Op::Patch
    } else {
        Op::Add
    }
}

// --- contents + diff ------------------------------------------------------------------------------------------------

/// One authored item of a tag (UI `TagItem`). `fields_hash` is the row's [`fields_hash`]: when both sides of a
/// [`diff`] have one, it alone decides `changed` (B2 `release_conflict` needs field-level changes, Ruling 19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagItem {
    pub kind: Kind,
    pub key: String,
    pub label: String,
    /// The item's authored content in one line: the row's `summary`, else [`kinds::summary`].
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields_hash: Option<String>,
    /// D111: the region of the region sub-layer the item comes from (`nl` for `industry/retail/region/nl`); absent
    /// for the family's other layers. A region item is its own item: it never replaces the parent's item of the
    /// same (kind, key), which every job not selecting that region still runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// An item's identity in [`contents`] and [`diff`]: (kind, key, region).
type ItemId = (Kind, String, Option<String>);

impl TagItem {
    /// The item a row (add or patch) contributes.
    pub fn of_row(row: &Row) -> TagItem {
        let detail = match row.fields.get("summary").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => kinds::summary(row.kind, &row.fields),
        };
        TagItem {
            kind: row.kind,
            key: row.key.clone(),
            label: kinds::label(row.kind, &row.key, &row.fields),
            detail,
            fields_hash: Some(fields_hash(&row.fields)),
            region: None,
        }
    }

    fn id(&self) -> ItemId {
        (self.kind, self.key.clone(), self.region.clone())
    }
}

/// The items `rows` (a tag's members, or a family's current rows) hold, keyed by (kind, key) across the family's
/// layers, in first-appearance order: an add or patch sets the item (a later row of the same (kind, key) replaces
/// it in place), a tombstone removes it (web `contentsAt`: a tombstoned item is not in the tag). Pass the rows in
/// apply order (head layer first). Every row counts as a non-region row ([`contents_by_region`]).
pub fn contents(rows: &[Row]) -> Vec<TagItem> {
    contents_by_region(rows, |_| None)
}

/// [`contents`] with D111 region sub-layers kept apart: `region_of(row)` is the region of the row's layer (`None` for
/// a layer without one), and items are keyed by (kind, key, region), so a region override and the parent's item of
/// the same (kind, key) are two items ([`TagItem::region`]).
pub fn contents_by_region(
    rows: &[Row],
    region_of: impl Fn(&Row) -> Option<String>,
) -> Vec<TagItem> {
    let mut order: Vec<ItemId> = Vec::new();
    let mut items: BTreeMap<ItemId, TagItem> = BTreeMap::new();
    for row in rows {
        let region = region_of(row);
        let id = (row.kind, row.key.clone(), region.clone());
        if row.op == Op::Tombstone {
            if items.remove(&id).is_some() {
                order.retain(|k| *k != id);
            }
            continue;
        }
        let item = TagItem {
            region,
            ..TagItem::of_row(row)
        };
        if items.insert(id.clone(), item).is_none() {
            order.push(id);
        }
    }
    order
        .into_iter()
        .filter_map(|id| items.remove(&id))
        .collect()
}

/// What happened to an item between two contents. Ordered as the diff sorts: added, changed, removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffChange {
    Added,
    Changed,
    Removed,
}

/// One diff row (UI `TagDiffRow`, plus the `fields_hash` before / after that plan §4 `ItemChange` carries).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDiffRow {
    pub kind: Kind,
    pub key: String,
    pub label: String,
    pub change: DiffChange,
    pub before: Option<String>,
    pub after: Option<String>,
    #[serde(default)]
    pub before_hash: Option<String>,
    #[serde(default)]
    pub after_hash: Option<String>,
    /// D111: the region of a region sub-layer item ([`TagItem::region`]); absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// What changed from `before` to `after`, keyed by (kind, key) — and the D111 region, so a region item and its
/// parent's item are compared separately — (web `diffItems`): `added`, `changed` (when both
/// sides have a `fields_hash`, only the hashes are compared, Ruling 19; else the detail), `removed`; sorted by
/// change (added, changed, removed), then kind, then key. An added or changed row carries the new label, a removed one the old.
pub fn diff(before: &[TagItem], after: &[TagItem]) -> Vec<TagDiffRow> {
    let a: BTreeMap<ItemId, &TagItem> = before.iter().map(|i| (i.id(), i)).collect();
    let b: BTreeMap<ItemId, &TagItem> = after.iter().map(|i| (i.id(), i)).collect();
    let mut rows: Vec<TagDiffRow> = Vec::new();
    for (k, new) in &b {
        match a.get(k) {
            None => rows.push(TagDiffRow {
                kind: new.kind,
                key: new.key.clone(),
                label: new.label.clone(),
                change: DiffChange::Added,
                before: None,
                after: Some(new.detail.clone()),
                before_hash: None,
                after_hash: new.fields_hash.clone(),
                region: new.region.clone(),
            }),
            Some(old) => {
                // Ruling 19: with both hashes the fields decide alone (a detail-only change raises no B2
                // base_changed); without one, the detail decides (web `diffItems`).
                let changed = match (&old.fields_hash, &new.fields_hash) {
                    (Some(x), Some(y)) => x != y,
                    _ => old.detail != new.detail,
                };
                if changed {
                    rows.push(TagDiffRow {
                        kind: new.kind,
                        key: new.key.clone(),
                        label: new.label.clone(),
                        change: DiffChange::Changed,
                        before: Some(old.detail.clone()),
                        after: Some(new.detail.clone()),
                        before_hash: old.fields_hash.clone(),
                        after_hash: new.fields_hash.clone(),
                        region: new.region.clone(),
                    });
                }
            }
        }
    }
    for (k, old) in &a {
        if !b.contains_key(k) {
            rows.push(TagDiffRow {
                kind: old.kind,
                key: old.key.clone(),
                label: old.label.clone(),
                change: DiffChange::Removed,
                before: Some(old.detail.clone()),
                after: None,
                before_hash: old.fields_hash.clone(),
                after_hash: None,
                region: old.region.clone(),
            });
        }
    }
    rows.sort_by(|x, y| {
        x.change
            .cmp(&y.change)
            .then_with(|| x.kind.as_str().cmp(y.kind.as_str()))
            .then_with(|| x.key.cmp(&y.key))
            .then_with(|| x.region.cmp(&y.region))
    });
    rows
}

/// The diff a pin on `from` takes when it is re-pointed to `to` (the latest tag) and cut (UI `TagCatchUp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCatchUp {
    pub from: i32,
    pub to: i32,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
}

/// Per distinct pinned version below `latest`, ascending, how much changes when that pin moves to `latest`
/// (web `tagContext.catch_up`). `contents_at(v)` returns the contents of tag `v` (see [`contents`]).
pub fn catch_up<F>(pinned: &[i32], latest: i32, mut contents_at: F) -> Vec<TagCatchUp>
where
    F: FnMut(i32) -> Vec<TagItem>,
{
    let versions: BTreeSet<i32> = pinned.iter().copied().filter(|v| *v < latest).collect();
    if versions.is_empty() {
        return Vec::new();
    }
    let to = contents_at(latest);
    versions
        .into_iter()
        .map(|from| {
            let d = diff(&contents_at(from), &to);
            let n = |c: DiffChange| d.iter().filter(|r| r.change == c).count();
            TagCatchUp {
                from,
                to: latest,
                added: n(DiffChange::Added),
                changed: n(DiffChange::Changed),
                removed: n(DiffChange::Removed),
            }
        })
        .collect()
}

// --- composition stack ----------------------------------------------------------------------------------------------

/// A template layer as the composition stack reads it (web `StackLayer`). `industry` is the layer's industry (an
/// industry / industry_dept key part, a job head's `template_selection.industry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackLayer {
    pub layer_key: String,
    pub scope: Scope,
    pub industry: Option<String>,
}

/// The web `SCOPE_RANK` over template scopes; a profile layer is not a template layer and composes with nothing.
fn template_rank(scope: Scope) -> Option<u8> {
    match scope {
        Scope::Base => Some(0),
        Scope::Industry => Some(1),
        Scope::BaseDept => Some(2),
        Scope::IndustryDept => Some(3),
        Scope::Job => Some(4),
        Scope::Profile => None,
    }
}

/// Does `lower` sit under `upper` in a job's composition (base and base_dept (D116) under every higher template
/// rank whatever its industry, else the same industry)? So base_dept sits under every industry_dept and job, never
/// under an industry.
fn composes_under(lower: &StackLayer, upper: &StackLayer) -> bool {
    let (Some(lo), Some(up)) = (template_rank(lower.scope), template_rank(upper.scope)) else {
        return false;
    };
    lo < up
        && (matches!(lower.scope, Scope::Base | Scope::BaseDept)
            || (lower.industry.is_some() && lower.industry == upper.industry))
}

fn by_rank_then_key(a: &StackLayer, b: &StackLayer) -> std::cmp::Ordering {
    template_rank(a.scope)
        .cmp(&template_rank(b.scope))
        .then_with(|| a.layer_key.cmp(&b.layer_key))
}

/// The layers `layer` composes with from the bottom up, ending with itself (web `layerStack`: base → industry →
/// base_dept → department → job).
pub fn layer_stack<'a>(layers: &'a [StackLayer], layer: &'a StackLayer) -> Vec<&'a StackLayer> {
    let mut out: Vec<&StackLayer> = layers.iter().filter(|x| composes_under(x, layer)).collect();
    out.push(layer);
    out.sort_by(|a, b| by_rank_then_key(a, b));
    out
}

/// The layers that compose on top of `layer` and so inherit what it contributes (web `composedOnBy`).
pub fn composed_on_by<'a>(layers: &'a [StackLayer], layer: &StackLayer) -> Vec<&'a StackLayer> {
    let mut out: Vec<&StackLayer> = layers.iter().filter(|x| composes_under(layer, x)).collect();
    out.sort_by(|a, b| by_rank_then_key(a, b));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack(key: &str, scope: Scope, industry: Option<&str>) -> StackLayer {
        StackLayer {
            layer_key: key.to_owned(),
            scope,
            industry: industry.map(str::to_owned),
        }
    }

    /// D116 (S2): base_dept composes under every industry_dept and job whatever the industry, never under an
    /// industry; an industry never stacks base_dept.
    #[test]
    fn base_dept_composes_under_every_department_and_job() {
        let base_dept = stack("base_dept/customer_service", Scope::BaseDept, None);
        let base = stack("base", Scope::Base, None);
        let retail = stack("industry/retail", Scope::Industry, Some("retail"));
        let retail_cs = stack(
            "industry_dept/retail/customer_service",
            Scope::IndustryDept,
            Some("retail"),
        );
        let telecom_cs = stack(
            "industry_dept/telecom/customer_service",
            Scope::IndustryDept,
            Some("telecom"),
        );
        let job = stack("job/care", Scope::Job, Some("retail"));
        let job_no_industry = stack("job/faq", Scope::Job, None);
        let profile = stack("profile/bundles", Scope::Profile, None);

        for upper in [&retail_cs, &telecom_cs, &job, &job_no_industry] {
            assert!(composes_under(&base_dept, upper), "{}", upper.layer_key);
        }
        assert!(!composes_under(&base_dept, &retail));
        assert!(!composes_under(&retail, &base_dept));
        assert!(!composes_under(&base_dept, &base));
        assert!(composes_under(&base, &base_dept));
        assert!(!composes_under(&base_dept, &base_dept));
        assert!(!composes_under(&base_dept, &profile));
        assert!(!composes_under(&profile, &base_dept));
        assert!(!composes_under(&retail_cs, &base_dept));

        let layers = [
            job.clone(),
            retail_cs.clone(),
            base_dept.clone(),
            base.clone(),
            retail.clone(),
        ];
        let keys: Vec<&str> = layer_stack(&layers, &layers[0])
            .into_iter()
            .map(|l| l.layer_key.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "base",
                "industry/retail",
                "base_dept/customer_service",
                "industry_dept/retail/customer_service",
                "job/care"
            ]
        );
        let on: Vec<&str> = composed_on_by(&layers, &base_dept)
            .into_iter()
            .map(|l| l.layer_key.as_str())
            .collect();
        assert_eq!(on, ["industry_dept/retail/customer_service", "job/care"]);
    }
}
