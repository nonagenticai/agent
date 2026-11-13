//! The template knowledge of a job's pinned tags (plan PK-1/PK-2, KH T3: D107, D112, D115): its effective knowledge
//! kinds and `knowledge_pack` references, and a kind's pack pins.
//!
//! - [`load`]: the layers of the pinned families (as of their tags) and the member rows the knowledge fold reads
//!   (`stage_knowledge_kind`, `knowledge_pack`, `department_archetype`).
//! - [`knowledge`]: `agent_model::compile::fold_knowledge` over them — THE fold order of D112/D115
//!   (`agent_model::compile::fold_order`, the compile's own), archetype kinds before their department, sibling
//!   departments never overriding each other; its errors are logged, never fatal here (the compile reports them).
//! - [`fold`]: the folded pack references ([`PackRef`]), by row key. A row with a bad key or version is skipped.
//! - [`pins_for`]: one seeded kind's pins from the folded references. Per pack key the most specific reference wins
//!   (exact `stage|kind` > `*|kind` > `stage|*` > `*|*`); a job tombstone (`job_overlay_row`, object `knowledge_pack`, key
//!   `stage|kind|pack_key`, either part may be `*`) removes the pack — also a pack that `replaces` the tombstoned key
//!   (D112: the tombstone follows a rename); sorted by key; at most [`MAX_PINS`](crate::knowledge::kinds::MAX_PINS)
//!   (more are dropped with a warning, never refused).
//! - [`job_tombstones`]: the job's tombstoned keys (tenant-scoped read).
//! - [`check_declared`]: D107's authoring check — 422 `pack_kind_undeclared`.
use std::collections::BTreeMap;

use agent_model::compile::{self, CompiledKnowledge, FoldedPack, LayerMeta, Row, Selection};
use agent_model::kinds::knowledge_pack::{ANY, split_key};
use agent_model::kinds::stage_knowledge::{archetype_covers, declares};
use agent_model::kinds::{Kind, Op, split_stage_knowledge_key};
use agent_model::layer::LayerKey;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::knowledge::kinds::MAX_PINS;
use crate::platform::db::layers::{self, Layer};
use crate::platform::templates::TemplatePin;
use crate::release::compile::job_key;

/// The `job_overlay_row` object of a job-level pack exclusion (PK-2).
pub const OBJECT: &str = "knowledge_pack";

/// One folded pack reference: the pack `pack_key@version` for `stage` / `kind` (either may be `*`), from row `row_id`
/// of layer `layer`; `replaces` = the pack key it was renamed from (D112).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRef {
    pub stage: String,
    pub kind: String,
    pub pack_key: String,
    pub version: i32,
    pub layer: String,
    pub row_id: Uuid,
    pub replaces: Option<String>,
}

impl PackRef {
    /// The row key `stage|kind|pack_key`.
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.stage, self.kind, self.pack_key)
    }

    /// `pack_key@version`.
    pub fn pin(&self) -> String {
        format!("{}@{}", self.pack_key, self.version)
    }

    /// The reference of a folded pack (`None` for a version outside `i32`, which the row validator refuses).
    pub fn of(p: &FoldedPack) -> Option<PackRef> {
        Some(PackRef {
            stage: p.stage.clone(),
            kind: p.kind.clone(),
            pack_key: p.pack_key.clone(),
            version: i32::try_from(p.version).ok()?,
            layer: p.layer.clone(),
            row_id: Uuid::parse_str(&p.row_id).unwrap_or_default(),
            replaces: p.replaces.clone(),
        })
    }
}

/// What the knowledge fold reads from a set of pinned tags.
#[derive(Clone, Debug, Default)]
pub struct PinnedKnowledge {
    /// Every layer of a pinned family that its tag contains (created no later than the tag), by key (bytewise).
    pub layers: Vec<LayerMeta>,
    /// The member rows of kinds `stage_knowledge_kind`, `knowledge_pack` and `department_archetype`, by (layer key,
    /// kind, key) bytewise.
    pub rows: Vec<Row>,
}

/// One member row of a pinned tag with its layer key.
#[derive(sqlx::FromRow)]
struct MemberRow {
    id: Uuid,
    layer_id: Uuid,
    layer_key: String,
    kind: String,
    key: String,
    op: String,
    fields: Value,
    rev: i32,
}

fn parse_op(op: &str) -> Option<Op> {
    match op {
        "add" => Some(Op::Add),
        "patch" => Some(Op::Patch),
        "tombstone" => Some(Op::Tombstone),
        _ => None,
    }
}

fn meta(id: Uuid, key: &str, extends: Option<String>) -> Option<LayerMeta> {
    match LayerKey::parse_label(key) {
        Ok(k) => Some(LayerMeta {
            id: id.to_string(),
            key: k,
            extends,
            tag: None,
            tagged_at: None,
        }),
        Err(e) => {
            tracing::warn!(layer = %key, error = %e, "a pinned layer key does not parse; skipped");
            None
        }
    }
}

/// The layers and knowledge member rows of the pinned tags ([`PinnedKnowledge`]).
pub async fn load(c: &mut PgConnection, pins: &[TemplatePin]) -> ApiResult<PinnedKnowledge> {
    if pins.is_empty() {
        return Ok(PinnedKnowledge::default());
    }
    let families: Vec<String> = pins.iter().map(|p| p.layer.clone()).collect();
    let versions: Vec<i32> = pins.iter().map(|p| p.version).collect();
    let found: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT l.id, l.key, l.extends \
           FROM unnest($1::text[], $2::int[]) AS p(family, version) \
           JOIN layer_tag t ON t.family = p.family AND t.version = p.version \
           JOIN layer l ON l.family = p.family AND l.created_at <= t.created_at",
    )
    .bind(&families)
    .bind(&versions)
    .fetch_all(&mut *c)
    .await?;
    let mut layers: Vec<LayerMeta> = found
        .into_iter()
        .filter_map(|(id, key, extends)| meta(id, &key, extends))
        .collect();
    layers.sort_by_key(LayerMeta::label);
    let members: Vec<MemberRow> = sqlx::query_as(
        "SELECT DISTINCT ON (l.key, r.kind, r.key) r.id, r.layer_id, l.key AS layer_key, r.kind, r.key, r.op, \
                r.fields, r.rev \
           FROM unnest($1::text[], $2::int[]) AS p(family, version) \
           JOIN layer_tag_row m ON m.family = p.family AND m.version = p.version \
           JOIN layer_row r ON r.id = m.row_id \
           JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind IN ('stage_knowledge_kind', 'knowledge_pack', 'department_archetype') \
          ORDER BY l.key, r.kind, r.key, r.rev DESC",
    )
    .bind(&families)
    .bind(&versions)
    .fetch_all(&mut *c)
    .await?;
    let mut members = members;
    // SQL returns rows in the database collation: the fold order is bytewise (as the compile's)
    members.sort_by(|a, b| {
        (a.layer_key.as_str(), a.kind.as_str(), a.key.as_str()).cmp(&(
            b.layer_key.as_str(),
            b.kind.as_str(),
            b.key.as_str(),
        ))
    });
    let rows = members
        .into_iter()
        .filter_map(|m| {
            let (Some(kind), Some(op)) = (Kind::parse(&m.kind), parse_op(&m.op)) else {
                tracing::warn!(kind = %m.kind, op = %m.op, "a member row has an unknown kind or op; skipped");
                return None;
            };
            Some(Row {
                id: m.id.to_string(),
                layer_id: m.layer_id.to_string(),
                kind,
                key: m.key,
                op,
                fields: m.fields,
                rev: m.rev,
            })
        })
        .collect();
    Ok(PinnedKnowledge { layers, rows })
}

/// The effective knowledge of the pinned tags under `sel` (the job is the first `job/*` pin's): see the module docs.
/// Fold errors (`department_conflict`, `kind_required_downgrade`, `pack_version_conflict`, a bad row) are logged and
/// never fail seeding or an approve here: the compile of the same pins reports them.
pub async fn knowledge(
    c: &mut PgConnection,
    pins: &[TemplatePin],
    sel: &Selection,
) -> ApiResult<CompiledKnowledge> {
    if pins.is_empty() {
        return Ok(CompiledKnowledge::default());
    }
    let loaded = load(&mut *c, pins).await?;
    let (k, errors) = compile::fold_knowledge(&loaded.layers, &loaded.rows, sel, job_key(pins));
    for e in &errors {
        tracing::warn!(code = %e.code, key = %e.key, "template knowledge fold: {}", e.message);
    }
    Ok(k)
}

/// The `knowledge_pack` references of the pinned tags under `sel`, folded (see the module docs), sorted by row key.
pub async fn fold(
    c: &mut PgConnection,
    pins: &[TemplatePin],
    sel: &Selection,
) -> ApiResult<Vec<PackRef>> {
    Ok(refs_of(&knowledge(c, pins, sel).await?))
}

/// The pack references of a folded knowledge, by row key.
pub fn refs_of(k: &CompiledKnowledge) -> Vec<PackRef> {
    k.packs.iter().filter_map(PackRef::of).collect()
}

/// Whether a job tombstone key (`stage|kind|pack_key`, either part may be `*`) removes `pack_key` from
/// `stage` / `kind`.
pub fn tombstoned(keys: &[String], stage: &str, kind: &str, pack_key: &str) -> bool {
    keys.iter().any(|k| {
        split_key(k).is_some_and(|(s, kd, p)| {
            p == pack_key && (s == ANY || s == stage) && (kd == ANY || kd == kind)
        })
    })
}

/// The specificity of a reference for `stage` / `kind`: exact 4, `*|kind` 3 (KH T3), `stage|*` 2, `*|*` 1; `None`
/// when it does not apply.
fn tier(r: &PackRef, stage: &str, kind: &str) -> Option<u8> {
    if r.stage == stage && r.kind == kind {
        Some(4)
    } else if r.stage == ANY && r.kind == kind {
        Some(3)
    } else if r.stage == stage && r.kind == ANY {
        Some(2)
    } else if r.stage == ANY && r.kind == ANY {
        Some(1)
    } else {
        None
    }
}

/// The pins (`pack_key@version`) of seeded kind `stage` / `kind` (see the module docs).
pub fn pins_for(
    refs: &[PackRef],
    stage: &str,
    kind: &str,
    tombstoned_keys: &[String],
) -> Vec<String> {
    // pack key → (tier, version, the key it replaces)
    let mut best: BTreeMap<&str, (u8, i32, Option<&str>)> = BTreeMap::new();
    for r in refs {
        let Some(t) = tier(r, stage, kind) else {
            continue;
        };
        if best
            .get(r.pack_key.as_str())
            .is_some_and(|(seen, _, _)| *seen >= t)
        {
            continue;
        }
        best.insert(r.pack_key.as_str(), (t, r.version, r.replaces.as_deref()));
    }
    let mut out: Vec<String> = best
        .into_iter()
        .filter(|(key, (_, _, replaces))| {
            !tombstoned(tombstoned_keys, stage, kind, key)
                && !replaces.is_some_and(|old| tombstoned(tombstoned_keys, stage, kind, old))
        })
        .map(|(key, (_, version, _))| format!("{key}@{version}"))
        .collect();
    if out.len() > MAX_PINS {
        tracing::warn!(
            stage = %stage,
            kind = %kind,
            pins = out.len(),
            max = MAX_PINS,
            "a knowledge kind's template packs exceed the pin limit; the rest are dropped"
        );
        out.truncate(MAX_PINS);
    }
    out
}

/// The keys of the job's `knowledge_pack` tombstones (`job_overlay_row`; the caller's connection is scoped to the
/// job's tenant).
pub async fn job_tombstones(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT key FROM job_overlay_row \
          WHERE job_id = $1 AND object = 'knowledge_pack' AND kind = 'tombstone' ORDER BY key",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?)
}

// ---------------------------------------------------------------------------------------------------------------
// D107 authoring check

/// 422 `pack_kind_undeclared`.
fn undeclared(layer: &str, key: &str) -> ApiError {
    ApiError::Unprocessable(
        "pack_kind_undeclared",
        format!(
            "The knowledge pack row {key} on {layer} names a knowledge kind that neither {layer}, a layer above it, \
             nor a department archetype it extends declares. Declare the kind (a stage_knowledge_kind row) first, \
             or use `*` for the kind."
        ),
    )
}

/// The layer keys a `knowledge_pack` row of `layer` may draw its kinds from (its own layer and the layers above it),
/// and the archetypes it extends: base ← industry ← department (+ its archetype and, D116, its `base_dept/<a>` layer;
/// a base_dept layer draws from base and its own archetype); a job layer (or a job × dimension
/// layer) draws from base, its own job layer and the industry and departments of its template's selection (and, D111,
/// their region sub-layers of every region: the region is each job's own choice). D111: a region sub-layer draws from
/// its parent's ancestry, itself and the sub-layers of the same region above it (`base/region/<r>`,
/// `industry/<i>/region/<r>`) — as its parent's rows would, plus its region.
async fn ancestry(c: &mut PgConnection, layer: &Layer) -> ApiResult<(Vec<String>, Vec<String>)> {
    if let Some(region) = &layer.region {
        let (mut keys, archetypes) = match layers::find_layer_by_key(&mut *c, &layer.family).await?
        {
            Some(parent) if parent.region.is_none() => head_ancestry(&mut *c, &parent).await?,
            _ => (vec!["base".to_owned()], Vec::new()),
        };
        keys.push(layer.key.clone());
        keys.push(format!("base/region/{region}"));
        if let Some(i) = &layer.industry {
            keys.push(format!("industry/{i}/region/{region}"));
        }
        // D116: the same region's sub-layer of every base_dept the parent draws from
        for a in &archetypes {
            keys.push(format!("base_dept/{a}/region/{region}"));
        }
        keys.sort();
        keys.dedup();
        return Ok((keys, archetypes));
    }
    head_ancestry(c, layer).await
}

/// [`ancestry`] of a layer that is not a region sub-layer.
async fn head_ancestry(
    c: &mut PgConnection,
    layer: &Layer,
) -> ApiResult<(Vec<String>, Vec<String>)> {
    let mut keys: Vec<String> = vec!["base".to_owned(), layer.key.clone()];
    let mut archetypes: Vec<String> = Vec::new();
    match layer.scope.as_str() {
        // D116: a base_dept layer is its archetype's layer: it draws from base and the archetype's own declarations
        "base_dept" => {
            archetypes.extend(layer.department.clone().filter(|a| !a.is_empty()));
        }
        "industry_dept" => {
            if let Some(i) = &layer.industry {
                keys.push(format!("industry/{i}"));
            }
            if let Some(a) = layer.extends.clone().filter(|a| !a.is_empty()) {
                // D116: and from the base_dept layer of the archetype it extends
                keys.push(format!("base_dept/{a}"));
                archetypes.push(a);
            }
        }
        "job" => {
            keys.push(layer.family.clone());
            let head = if layer.family == layer.key {
                Some(layer.clone())
            } else {
                layers::find_layer_by_key(&mut *c, &layer.family).await?
            };
            let sel: Option<Selection> = head
                .and_then(|h| h.template_selection)
                .and_then(|v| serde_json::from_value(v).ok());
            if let Some(sel) = sel.filter(|s| !s.industry.is_empty()) {
                keys.push(format!("industry/{}", sel.industry));
                let mut base_depts: Vec<String> = Vec::new();
                for d in &sel.departments {
                    keys.push(d.clone());
                    if let Some(a) = layers::find_layer_by_key(&mut *c, d)
                        .await?
                        .and_then(|l| l.extends)
                        .filter(|a| !a.is_empty())
                    {
                        // D116: and the base_dept layer of every archetype its departments extend
                        base_depts.push(format!("base_dept/{a}"));
                        archetypes.push(a);
                    }
                }
                keys.extend(base_depts.iter().cloned());
                // D111: a job layer is a template — each job made from it picks its own region (its target's
                // `selection.region`, which can change after this row is written), so no one region is "the job's"
                // here. The row may name a kind any region sub-layer of its base, industry or departments declares;
                // for a job of another region the pack is then `pack_not_applicable` (D107: listed, never an error).
                let mut heads = vec!["base".to_owned(), format!("industry/{}", sel.industry)];
                heads.extend(sel.departments.iter().cloned());
                heads.extend(base_depts);
                let regional: Vec<String> = sqlx::query_scalar(
                    "SELECT key FROM layer WHERE region IS NOT NULL AND family = ANY($1) ORDER BY key",
                )
                .bind(&heads)
                .fetch_all(&mut *c)
                .await?;
                keys.extend(regional);
            }
        }
        _ => {}
    }
    keys.sort();
    keys.dedup();
    Ok((keys, archetypes))
}

/// D107 (1): a `knowledge_pack` row `key` written to `layer` must name a `stage|kind` declared (a current
/// `stage_knowledge_kind` row) in its own layer, an ancestor layer ([`ancestry`]) or an archetype it extends (its
/// `knowledge_kinds`, a `<family>.*` pattern covering the family); `*` matches any. Otherwise 422
/// `pack_kind_undeclared`. A tombstone is never checked (removing a reference always works); a key that is not a
/// pack key is the validator's `invalid_key`, not this check's.
pub async fn check_declared(c: &mut PgConnection, layer: &Layer, key: &str) -> ApiResult<()> {
    let Some((stage, kind, _)) = split_key(key) else {
        return Ok(());
    };
    if kind == ANY {
        return Ok(());
    }
    let (keys, archetypes) = ancestry(&mut *c, layer).await?;
    let declared: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM layer_head \
          WHERE kind = 'stage_knowledge_kind' AND op <> 'tombstone' AND layer_key = ANY($1)",
    )
    .bind(&keys)
    .fetch_all(&mut *c)
    .await?;
    if declared
        .iter()
        .any(|d| split_stage_knowledge_key(d).is_some_and(|(ds, dk)| declares(ds, dk, stage, kind)))
    {
        return Ok(());
    }
    if !archetypes.is_empty() {
        let defs: Vec<Value> = sqlx::query_scalar(
            "SELECT fields FROM layer_head \
              WHERE kind = 'department_archetype' AND op <> 'tombstone' AND layer_key = 'base' AND key = ANY($1)",
        )
        .bind(&archetypes)
        .fetch_all(&mut *c)
        .await?;
        let covered = defs.iter().any(|f| {
            f.get("knowledge_kinds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|entry| archetype_covers(entry, kind))
        });
        if covered {
            return Ok(());
        }
    }
    Err(undeclared(&layer.key, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::compile::fold_knowledge;
    use serde_json::json;

    fn layer(id: &str, key: &str, extends: Option<&str>) -> LayerMeta {
        LayerMeta {
            id: id.to_owned(),
            key: LayerKey::parse_label(key).expect("a layer key"),
            extends: extends.map(str::to_owned),
            tag: None,
            tagged_at: None,
        }
    }

    fn pack(layer: &str, key: &str, op: Op, version: i64) -> Row {
        Row {
            id: Uuid::new_v4().to_string(),
            layer_id: layer.to_owned(),
            kind: Kind::KnowledgePack,
            key: key.to_owned(),
            op,
            fields: if op == Op::Tombstone {
                json!({})
            } else {
                json!({ "version": version })
            },
            rev: 1,
        }
    }

    fn world() -> Vec<LayerMeta> {
        vec![
            layer("L-base", "base", None),
            layer("L-ret", "industry/retail", None),
            layer(
                "L-cs",
                "industry_dept/retail/customer_service",
                Some("customer_service"),
            ),
            layer("L-hr", "industry_dept/retail/hr", None),
            layer("L-job", "job/care", None),
        ]
    }

    fn sel() -> Selection {
        serde_json::from_value(json!({
            "industry": "retail",
            "departments": ["industry_dept/retail/customer_service"],
        }))
        .unwrap()
    }

    fn refs(rows: &[Row], sel: &Selection, job: Option<&str>) -> Vec<PackRef> {
        refs_of(&fold_knowledge(&world(), rows, sel, job).0)
    }

    fn pins(refs: &[PackRef], stage: &str, kind: &str, tombs: &[&str]) -> Vec<String> {
        let tombs: Vec<String> = tombs.iter().map(|t| (*t).to_owned()).collect();
        pins_for(refs, stage, kind, &tombs)
    }

    /// The brief's fold: base `*|*|base@1`, industry `*|*|industry/retail@1`, job `support|faq|industry/retail@2`,
    /// and a job-layer tombstone of `*|*|base`.
    fn brief_rows() -> Vec<Row> {
        vec![
            pack("L-base", "*|*|base", Op::Add, 1),
            pack("L-ret", "*|*|industry/retail", Op::Add, 1),
            pack("L-job", "support|faq|industry/retail", Op::Add, 2),
            pack("L-job", "*|*|base", Op::Tombstone, 0),
        ]
    }

    #[test]
    fn fold_replaces_in_layer_order_and_tombstones_remove() {
        let r = refs(&brief_rows(), &sel(), Some("care"));
        let keys: Vec<String> = r.iter().map(PackRef::key).collect();
        assert_eq!(
            keys,
            ["*|*|industry/retail", "support|faq|industry/retail"],
            "the job layer's tombstone removed base"
        );
        assert_eq!(pins(&r, "support", "faq", &[]), ["industry/retail@2"]);
        assert_eq!(pins(&r, "support", "other", &[]), ["industry/retail@1"]);
        // a later layer replaces the same key
        let r = refs(
            &[
                pack("L-base", "*|*|base", Op::Add, 1),
                pack("L-ret", "*|*|base", Op::Add, 3),
            ],
            &sel(),
            None,
        );
        assert_eq!(pins(&r, "s", "k", &[]), ["base@3"]);
    }

    #[test]
    fn specificity_exact_beats_stage_wildcard_beats_any() {
        let r = refs(
            &[
                pack("L-base", "*|*|p", Op::Add, 1),
                pack("L-ret", "support|*|p", Op::Add, 2),
                pack("L-cs", "support|faq|p", Op::Add, 3),
            ],
            &sel(),
            None,
        );
        assert_eq!(pins(&r, "support", "faq", &[]), ["p@3"]);
        assert_eq!(pins(&r, "support", "other", &[]), ["p@2"]);
        assert_eq!(pins(&r, "exit", "faq", &[]), ["p@1"]);
        // KH T3: `*|kind` (one kind at every stage) beats `stage|*`, loses to the exact key
        let r = refs(
            &[
                pack("L-base", "*|*|p", Op::Add, 1),
                pack("L-base", "*|faq|p", Op::Add, 5),
                pack("L-ret", "support|*|p", Op::Add, 2),
                pack("L-cs", "exit|faq|p", Op::Add, 3),
            ],
            &sel(),
            None,
        );
        assert_eq!(pins(&r, "support", "faq", &[]), ["p@5"]);
        assert_eq!(pins(&r, "exit", "faq", &[]), ["p@3"]);
        assert_eq!(pins(&r, "support", "other", &[]), ["p@2"]);
    }

    #[test]
    fn job_tombstones_remove_one_kind_or_every_kind_and_follow_a_rename() {
        let r = refs(&brief_rows(), &sel(), Some("care"));
        assert!(pins(&r, "support", "faq", &["support|faq|industry/retail"]).is_empty());
        assert_eq!(
            pins(&r, "support", "other", &["support|faq|industry/retail"]),
            ["industry/retail@1"],
            "another kind keeps it"
        );
        assert!(pins(&r, "exit", "x", &["*|*|industry/retail"]).is_empty());
        assert!(pins(&r, "support", "x", &["support|*|industry/retail"]).is_empty());
        assert_eq!(
            pins(&r, "exit", "x", &["support|*|industry/retail"]),
            ["industry/retail@1"]
        );
        // D112: a pack that replaces the tombstoned key stays excluded
        let mut renamed = pack("L-ret", "support|faq|industry/shop", Op::Add, 1);
        renamed.fields = json!({"version": 1, "replaces": "industry/retail"});
        let r = refs(&[renamed], &sel(), None);
        assert_eq!(r[0].replaces.as_deref(), Some("industry/retail"));
        assert!(pins(&r, "support", "faq", &["support|faq|industry/retail"]).is_empty());
        assert_eq!(pins(&r, "support", "faq", &[]), ["industry/shop@1"]);
    }

    #[test]
    fn bad_rows_are_skipped_and_pins_are_capped() {
        let mut rows = vec![
            pack("L-base", "*|faq", Op::Add, 1),
            pack("L-base", "bad key", Op::Add, 1),
            pack("L-base", "*|*|q", Op::Add, 0),
        ];
        for i in 0..20 {
            rows.push(pack("L-base", &format!("*|*|p{i:02}"), Op::Add, 1));
        }
        let r = refs(&rows, &sel(), None);
        assert!(
            r.iter()
                .all(|r| r.pack_key.starts_with('p') && r.pack_key != "p")
        );
        let got = pins(&r, "s", "k", &[]);
        assert_eq!(got.len(), MAX_PINS);
        assert_eq!(got[0], "p00@1");
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted);
    }

    #[test]
    fn the_fold_follows_the_selection() {
        let mut sel: Selection = serde_json::from_value(json!({
            "industry": "retail",
            "departments": ["industry_dept/retail/customer_service"],
            "dimensions": {"channel": ["email"]},
            "region": "eu",
        }))
        .unwrap();
        let rows = [
            pack("L-ret", "*|*|i", Op::Add, 1),
            pack("L-hr", "*|*|hr", Op::Add, 1),
            pack("L-cs", "*|*|cs", Op::Add, 1),
            pack("L-job", "*|*|j", Op::Add, 1),
        ];
        let keys = |sel: &Selection, job| -> Vec<String> {
            refs(&rows, sel, job)
                .iter()
                .map(|r| r.pack_key.clone())
                .collect()
        };
        assert_eq!(keys(&sel, Some("care")), ["cs", "i", "j"]);
        assert_eq!(keys(&sel, Some("other")), ["cs", "i"]);
        sel.industry = "bank".into();
        assert!(
            keys(&sel, None).is_empty(),
            "another industry: neither layer"
        );
    }
}
