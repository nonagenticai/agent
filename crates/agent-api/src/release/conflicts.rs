//! Release conflicts (spec B2 §5 "Conflict detector", §2.7 "conflict detectors"; 05 §4.6, J16, D15): a tenant
//! refinement whose base moved underneath it. A port of care-bus-agents@45ee128 `release/conflicts.rs` `detect` /
//! `setting_hash`, over B2's working rows (`job_overlay_row`, `job_setting_override`) and the stored bundle JSON
//! (`objects`, `settings`) instead of materialised base tables.
//!
//! - [`ConflictDetector`]: one source of conflicts. B2 ships [`OverlayDetector`] (the five overlay objects + the job
//!   settings); B5 adds `crate::job_design::conflicts::JobDesignDetector` (its refinements and extensions against
//!   the bases at the release's pins) through [`ConflictDetectors::register`].
//! - [`ConflictDetectors`]: the registry (`AppState.conflict_detectors`, built by [`registry`]).
//! - [`on_apply`]: §5 apply step 7 — carried decisions resolve, every other open conflict is superseded, and the
//!   detectors recompute the open set against the applied release's bundle.
//! - [`raise_upgrade`]: §5 approve step 11 (D15) — a cut that adopts newer template pins opens its conflicts at the
//!   cut (`raised_by_cut`), before it is applied.
//!
//! Detection is state-based (as cb): a tenant row conflicts with the bundle it now runs on, whatever happened before.
use std::collections::BTreeSet;
use std::sync::Arc;

use agent_model::kinds::knowledge_pack;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::bundle::OVERLAY_OBJECTS;
use super::hash::bundle_hash;
use super::overlay::KNOWLEDGE_PACK;
use super::overlay::base_hash;
use super::overlay::item_hash;
use super::targets::SETTING_KEYS;
use crate::design::JobRef;
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::TemplatePin;

/// The object a job-setting conflict names (and its `kind` for a changed base value).
pub const JOB_SETTING: &str = "job_setting";

/// One conflict a detector found, before it is stored (`release_conflict` columns).
#[derive(Clone, Debug, PartialEq)]
pub struct NewConflict {
    /// `base_changed | base_removed | base_added | parent_missing | base_shadowed | job_setting |
    /// excluded_by_rule` (D112, informational) | `required_kind_introduced` (D110, informational, KH T8).
    pub kind: String,
    /// `activity | context_space | ontology_attribute | gate_option | knowledge_store | job_setting`.
    pub object: String,
    pub key: String,
    /// `override | extension | tombstone`.
    pub tenant_row: String,
    /// One line on what changed underneath (no person-typed text).
    pub detail: String,
    pub base_hash_before: Option<String>,
    pub base_hash_after: Option<String>,
    pub base_before: Option<Value>,
    pub base_after: Option<Value>,
}

/// A source of release conflicts (§2.7). `from` = the bundle Production ran before (`None`: nothing was live, or a
/// cut has no current release), `to` = the bundle being applied (or cut), `release_id` = that release (its
/// `job_release` row exists: B5 reads its pins and selection, which the bundle does not carry). LOCAL STATE ONLY: no
/// upstream call.
pub trait ConflictDetector: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn detect<'a>(
        &'a self,
        c: &'a mut PgConnection,
        job: &'a JobRef,
        from: Option<&'a Value>,
        to: &'a Value,
        release_id: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<NewConflict>>>;
}

/// The registered detectors (`AppState.conflict_detectors`).
#[derive(Default)]
pub struct ConflictDetectors {
    list: Vec<Arc<dyn ConflictDetector>>,
}

impl ConflictDetectors {
    /// Panics on a second detector with the same name (a programming error caught at boot).
    pub fn register(&mut self, d: Arc<dyn ConflictDetector>) {
        let name = d.name();
        assert!(
            !self.list.iter().any(|x| x.name() == name),
            "conflict detector `{name}` is registered twice"
        );
        self.list.push(d);
    }

    /// Every detector's conflicts, in registration order; a `(kind, object, key)` seen twice keeps the first.
    pub async fn detect(
        &self,
        c: &mut PgConnection,
        job: &JobRef,
        from: Option<&Value>,
        to: &Value,
        release_id: &str,
    ) -> ApiResult<Vec<NewConflict>> {
        let mut out: Vec<NewConflict> = Vec::new();
        let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
        for d in &self.list {
            for n in d.detect(&mut *c, job, from, to, release_id).await? {
                if seen.insert((n.kind.clone(), n.object.clone(), n.key.clone())) {
                    out.push(n);
                }
            }
        }
        Ok(out)
    }
}

/// The production detectors. Each plan adds one line here.
pub fn registry() -> ConflictDetectors {
    let mut r = ConflictDetectors::default();
    r.register(Arc::new(OverlayDetector));
    r.register(Arc::new(crate::job_design::conflicts::JobDesignDetector));
    r
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// The hash of one base job setting (cb `setting_hash`; = T8 `job_setting_override.base_hash`).
pub fn setting_hash(value: &str) -> ApiResult<String> {
    bundle_hash(&Value::String(value.to_owned()))
        .map_err(|e| internal(format!("a setting value does not hash: {e}")))
}

/// The base items of `object` in a stored bundle (read leniently: anything that is not an object is skipped).
fn base_items<'b>(bundle: &'b Value, object: &str) -> Vec<&'b Map<String, Value>> {
    bundle
        .get("objects")
        .and_then(|o| o.get(object))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

fn find<'b>(items: &[&'b Map<String, Value>], key: &str) -> Option<&'b Map<String, Value>> {
    items
        .iter()
        .find(|i| i.get("key").and_then(Value::as_str) == Some(key))
        .copied()
}

/// The base value of setting `key` in a stored bundle.
fn base_setting<'b>(bundle: &'b Value, key: &str) -> Option<&'b str> {
    bundle.get("settings")?.get(key)?.as_str()
}

/// `{<key>: <value>}` (fixture `j16.base_before` / `base_after` of a setting conflict).
fn one_setting(key: &str, value: &str) -> Value {
    let mut m = Map::new();
    m.insert(key.to_owned(), Value::String(value.to_owned()));
    Value::Object(m)
}

/// The entity names a bundle's ontology base knows: the entity of every `ontology_attribute` (`<Entity>.<attr>`)
/// and every entity a base context space names (as `overlay::entities` until B5).
fn base_entities(bundle: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for a in base_items(bundle, "ontology_attribute") {
        if let Some((e, _)) = a
            .get("key")
            .and_then(Value::as_str)
            .and_then(|k| k.split_once('.'))
        {
            out.insert(e.to_owned());
        }
    }
    for s in base_items(bundle, "context_space") {
        for e in s
            .get("entities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(e) = e.as_str() {
                out.insert(e.to_owned());
            }
        }
    }
    out
}

#[derive(sqlx::FromRow)]
struct OverlayRow {
    object: String,
    key: String,
    kind: String,
    item: Option<Value>,
    base_hash: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SettingRow {
    key: String,
    value: Option<String>,
    base_hash: Option<String>,
}

/// B2's detector: `job_overlay_row` vs `bundle.objects` and `job_setting_override` vs `bundle.settings`.
///
/// | kind | raised when |
/// |---|---|
/// | `base_changed` | a visible override whose `base_hash` ≠ the hash of the new base item |
/// | `base_removed` | an override or tombstone whose key is gone from the new base |
/// | `base_added` | an extension whose key now exists in the base |
/// | `parent_missing` | a visible `ontology_attribute` whose entity the release removed (it was in `from`'s base) and that neither the new base nor a tenant context space names |
/// | `job_setting` | a setting override whose `base_hash` ≠ the hash of the new base value (`base_removed` / `base_added` as for overlays) |
pub struct OverlayDetector;

impl ConflictDetector for OverlayDetector {
    fn name(&self) -> &'static str {
        "b2.overlay"
    }

    fn detect<'a>(
        &'a self,
        c: &'a mut PgConnection,
        job: &'a JobRef,
        from: Option<&'a Value>,
        to: &'a Value,
        _release_id: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<NewConflict>>> {
        Box::pin(detect_b2(c, job.id, from, to))
    }
}

async fn detect_b2(
    c: &mut PgConnection,
    job: Uuid,
    from: Option<&Value>,
    to: &Value,
) -> ApiResult<Vec<NewConflict>> {
    let rows = sqlx::query_as::<_, OverlayRow>(
        "SELECT object, key, kind, item, base_hash FROM job_overlay_row WHERE job_id = $1 \
          ORDER BY object, key",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    let settings = sqlx::query_as::<_, SettingRow>(
        "SELECT key, value, base_hash FROM job_setting_override WHERE job_id = $1 ORDER BY key",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    classify(&rows, &settings, from, to)
}

/// The pure part of [`OverlayDetector`]: the job's working rows (`rows`, `settings`) against the bundle that was live
/// (`from`) and the one being applied or cut (`to`). Overlay conflicts come first (in row order), then settings.
fn classify(
    rows: &[OverlayRow],
    settings: &[SettingRow],
    from: Option<&Value>,
    to: &Value,
) -> ApiResult<Vec<NewConflict>> {
    let mut out = Vec::new();
    // entities a tenant context space names (its own parents for an attribute)
    let mut tenant_entities: BTreeSet<String> = BTreeSet::new();
    for r in rows.iter().filter(|r| r.object == "context_space") {
        for e in r
            .item
            .as_ref()
            .and_then(|i| i.get("entities"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(e) = e.as_str() {
                tenant_entities.insert(e.to_owned());
            }
        }
    }
    let entities_after = base_entities(to);
    let entities_before = from.map(base_entities).unwrap_or_default();
    for r in rows {
        if !OVERLAY_OBJECTS.contains(&r.object.as_str()) {
            continue;
        }
        // D112 (KH T3): a wildcard pack tombstone (`*` stage or kind) names no one base item
        if r.object == "knowledge_pack"
            && let Some(rule) = PackRule::wildcard(&r.key)
        {
            if r.kind == "tombstone" {
                out.extend(rule.findings(r, from, to)?);
            }
            continue;
        }
        let tomb = r.kind == "tombstone";
        let after_items = base_items(to, &r.object);
        let after = find(&after_items, &r.key);
        let after_hash = after.map(|a| base_hash(&r.object, a)).transpose()?;
        let before = from.and_then(|f| find(&base_items(f, &r.object), &r.key).cloned());
        let label = r.object.replace('_', " ");
        // D112 (KH T3): a pack renamed with `replaces: <this key>` keeps the job's tombstone (it follows the rename)
        let followed = r.object == "knowledge_pack"
            && after_items
                .iter()
                .any(|i| i.get("replaces").and_then(Value::as_str) == Some(r.key.as_str()));
        let found: Option<(&str, String)> = match (&r.base_hash, &after_hash) {
            (Some(_), None) if followed => None,
            (Some(_), None) => Some((
                "base_removed",
                format!(
                    "The release removed the base {label} “{}”; your {} has no base any more.",
                    r.key, r.kind
                ),
            )),
            (Some(b), Some(a)) if !tomb && b != a => Some((
                "base_changed",
                format!(
                    "The release changed the base {label} “{}”; your {} was made on the old one.",
                    r.key, r.kind
                ),
            )),
            (None, Some(_)) if !tomb => Some((
                "base_added",
                format!(
                    "The release added a base {label} “{}”; your extension now shadows it.",
                    r.key
                ),
            )),
            _ => None,
        };
        if let Some((kind, detail)) = found {
            out.push(NewConflict {
                kind: kind.to_owned(),
                object: r.object.clone(),
                key: r.key.clone(),
                tenant_row: r.kind.clone(),
                detail,
                base_hash_before: r.base_hash.clone(),
                base_hash_after: after_hash.clone(),
                base_before: before.clone().map(Value::Object),
                base_after: after.cloned().map(Value::Object),
            });
        }
        if r.object == "ontology_attribute"
            && !tomb
            && let Some((entity, _)) = r.key.split_once('.')
            && entities_before.contains(entity)
            && !entities_after.contains(entity)
            && !tenant_entities.contains(entity)
        {
            out.push(NewConflict {
                kind: "parent_missing".to_owned(),
                object: r.object.clone(),
                key: r.key.clone(),
                tenant_row: r.kind.clone(),
                detail: format!(
                    "The release removed entity {entity}; your attribute “{}” has no parent.",
                    r.key
                ),
                base_hash_before: r.base_hash.clone(),
                base_hash_after: None,
                base_before: before.map(Value::Object),
                base_after: None,
            });
        }
    }
    for s in settings {
        if !SETTING_KEYS.contains(&s.key.as_str()) {
            continue;
        }
        let now = base_setting(to, &s.key);
        let current = now.map(setting_hash).transpose()?;
        let tenant_row = match (&s.value, &s.base_hash) {
            (None, _) => "tombstone",
            (Some(_), None) => "extension",
            (Some(_), Some(_)) => "override",
        };
        let kind = match (&s.base_hash, &current) {
            (Some(_), None) => Some("base_removed"),
            (Some(b), Some(a)) if s.value.is_some() && b != a => Some(JOB_SETTING),
            (None, Some(_)) if s.value.is_some() => Some("base_added"),
            _ => None,
        };
        let Some(kind) = kind else {
            continue;
        };
        let was = from.and_then(|f| base_setting(f, &s.key));
        let detail = match kind {
            "base_removed" => format!("The release removed the base {}.", s.key),
            "base_added" => format!("The release added a base {} under your setting.", s.key),
            _ => format!(
                "The release changed the base {} under your override.",
                s.key
            ),
        };
        out.push(NewConflict {
            kind: kind.to_owned(),
            object: JOB_SETTING.to_owned(),
            key: s.key.clone(),
            tenant_row: tenant_row.to_owned(),
            detail,
            base_hash_before: s.base_hash.clone(),
            base_hash_after: current,
            base_before: was.map(|v| one_setting(&s.key, v)),
            base_after: now.map(|v| one_setting(&s.key, v)),
        });
    }
    Ok(out)
}

/// The conflict kind of D112's informational finding: a wildcard pack tombstone of the job also excludes a pack the
/// release adds under it ("excluded by your rule"). It never blocks an approve or an apply (no conflict does) and
/// offers no resolution: it is listed for the approver.
pub const EXCLUDED_BY_RULE: &str = "excluded_by_rule";

/// A job's wildcard `knowledge_pack` tombstone `stage|kind|pack_key` (stage or kind `*`).
struct PackRule<'k> {
    stage: &'k str,
    kind: &'k str,
    pack: &'k str,
}

impl<'k> PackRule<'k> {
    fn wildcard(key: &'k str) -> Option<Self> {
        let (stage, kind, pack) = knowledge_pack::split_key(key)?;
        (stage == knowledge_pack::ANY || kind == knowledge_pack::ANY).then_some(PackRule {
            stage,
            kind,
            pack,
        })
    }

    /// Whether the rule covers pack reference key `key` (`stage|kind|pack_key`).
    fn covers(&self, key: &str) -> bool {
        let any = knowledge_pack::ANY;
        knowledge_pack::split_key(key).is_some_and(|(s, k, p)| {
            p == self.pack
                && (self.stage == any || s == self.stage)
                && (self.kind == any || k == self.kind)
        })
    }

    /// `excluded_by_rule` for every item of `to` the rule covers that `from` did not have; `base_removed` (an
    /// orphaned rule, as for an exact tombstone) when `from` had covered items, `to` has none, and no `to` item
    /// `replaces` a covered key (a rename the rule follows).
    fn findings(
        &self,
        r: &OverlayRow,
        from: Option<&Value>,
        to: &Value,
    ) -> ApiResult<Vec<NewConflict>> {
        let key_of =
            |i: &Map<String, Value>| i.get("key").and_then(Value::as_str).map(str::to_owned);
        let after = base_items(to, KNOWLEDGE_PACK);
        let before = from
            .map(|f| base_items(f, KNOWLEDGE_PACK))
            .unwrap_or_default();
        let before_keys: BTreeSet<String> = before.iter().filter_map(|i| key_of(i)).collect();
        let mut out = Vec::new();
        let mut covered_after = 0;
        for item in &after {
            let Some(key) = key_of(item).filter(|k| self.covers(k)) else {
                continue;
            };
            covered_after += 1;
            if before_keys.contains(&key) {
                continue;
            }
            out.push(NewConflict {
                kind: EXCLUDED_BY_RULE.to_owned(),
                object: KNOWLEDGE_PACK.to_owned(),
                detail: format!(
                    "The release adds the pack reference “{key}”; your rule “{}” excludes it too.",
                    r.key
                ),
                key,
                tenant_row: r.kind.clone(),
                base_hash_before: None,
                base_hash_after: Some(item_hash(item)?),
                base_before: None,
                base_after: Some(Value::Object(Map::clone(item))),
            });
        }
        let had = before_keys.iter().any(|k| self.covers(k));
        let renamed = after.iter().any(|i| {
            i.get("replaces")
                .and_then(Value::as_str)
                .is_some_and(|old| self.covers(old))
        });
        if had && covered_after == 0 && !renamed {
            out.push(NewConflict {
                kind: "base_removed".to_owned(),
                object: KNOWLEDGE_PACK.to_owned(),
                key: r.key.clone(),
                tenant_row: r.kind.clone(),
                detail: format!(
                    "The release removes every pack reference your rule “{}” excluded, and no pack replaces them.",
                    r.key
                ),
                base_hash_before: r.base_hash.clone(),
                base_hash_after: None,
                base_before: None,
                base_after: None,
            });
        }
        Ok(out)
    }
}

/// D110 (KH T8): the informational finding of an upgrade that makes a knowledge kind required for a department
/// (`object` [`KNOWLEDGE_KIND`], `key` `<department>:<stage>|<kind>`, `tenant_row` `none`). It never blocks and offers
/// no resolution: readiness refuses the apply of the release until the kind resolves (crate::knowledge::readiness).
pub const REQUIRED_KIND_INTRODUCED: &str = "required_kind_introduced";

/// The object of a [`REQUIRED_KIND_INTRODUCED`] finding.
pub const KNOWLEDGE_KIND: &str = "knowledge_kind";

/// [`REQUIRED_KIND_INTRODUCED`] for every required kind a department of cut `cut`'s selection has under the `new`
/// pins and did not have under the `old` ones (the template fold of each, crate::knowledge::packs::template); none
/// for a selection without the hierarchy.
async fn required_introduced(
    c: &mut PgConnection,
    job: &JobRef,
    cut: &str,
    old: &[TemplatePin],
    new: &[TemplatePin],
) -> ApiResult<Vec<NewConflict>> {
    use crate::knowledge::packs::template as template_packs;
    let stored: Option<Value> = sqlx::query_scalar(
        "SELECT selection FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job.id)
    .bind(cut)
    .fetch_optional(&mut *c)
    .await?;
    let Some(sel) = stored.and_then(|v| crate::design::read::selection_of_value(&v).ok()) else {
        return Ok(Vec::new());
    };
    if sel.industry.is_empty() || sel.departments.is_empty() {
        return Ok(Vec::new());
    }
    let before = template_packs::knowledge(&mut *c, old, &sel).await?.kinds;
    let after = template_packs::knowledge(&mut *c, new, &sel).await?.kinds;
    let mut out = Vec::new();
    for dept in &sel.departments {
        let had: BTreeSet<String> = before
            .iter()
            .filter(|k| k.required && k.of_department(dept))
            .map(|k| k.key())
            .collect();
        for k in after
            .iter()
            .filter(|k| k.required && k.of_department(dept) && !had.contains(&k.key()))
        {
            out.push(NewConflict {
                kind: REQUIRED_KIND_INTRODUCED.to_owned(),
                object: KNOWLEDGE_KIND.to_owned(),
                key: format!("{dept}:{}", k.key()),
                tenant_row: "none".to_owned(),
                detail: format!(
                    "The release makes knowledge kind {} ({} stage) required for department {dept}: bind it before \
                     this release can be applied.",
                    k.kind, k.stage
                ),
                base_hash_before: None,
                base_hash_after: None,
                base_before: None,
                base_after: Some(json!({
                    "stage": k.stage,
                    "kind": k.kind,
                    "label": k.label,
                    "tenant_data": k.tenant_data,
                    "layer": k.layer,
                })),
            });
        }
    }
    Ok(out)
}

/// J16 `ConflictCause` of a conflict a release raises: the first template layer whose pin moved
/// (`template_upgrade {layer, from: "v<n>", to: "v<m>"}`), else `new_cut`.
pub fn cause(from: &[TemplatePin], to: &[TemplatePin]) -> Value {
    for p in to {
        let was = from.iter().find(|x| x.layer == p.layer).map(|x| x.version);
        if let Some(w) = was
            && w != p.version
        {
            return json!({
                "kind": "template_upgrade",
                "layer": p.layer,
                "from": format!("v{w}"),
                "to": format!("v{}", p.version),
            });
        }
    }
    json!({"kind": "new_cut"})
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    c: &mut PgConnection,
    tenant: Uuid,
    job: &JobRef,
    release_id: &str,
    raised_by_cut: Option<&str>,
    n: &NewConflict,
    cause: &Value,
    decision: Option<&Value>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO release_conflict \
                (tenant_id, job_id, release_id, kind, object, key, tenant_row, detail, base_hash_before, \
                 base_hash_after, base_before, base_after, cause, raised_by_cut, decision) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(tenant)
    .bind(job.id)
    .bind(release_id)
    .bind(&n.kind)
    .bind(&n.object)
    .bind(&n.key)
    .bind(&n.tenant_row)
    .bind(&n.detail)
    .bind(&n.base_hash_before)
    .bind(&n.base_hash_after)
    .bind(&n.base_before)
    .bind(&n.base_after)
    .bind(cause)
    .bind(raised_by_cut)
    .bind(decision)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// What applying a release does to the job's conflicts (counts, for tests and logs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recomputed {
    pub resolved: usize,
    pub superseded: usize,
    pub opened: usize,
}

#[derive(sqlx::FromRow)]
struct Superseded {
    kind: String,
    object: String,
    key: String,
    decision: Option<Value>,
    cause: Option<Value>,
}

/// §5 apply step 7 (fixture `applyIn`, J16), run after the release is current and the working rows followed live:
/// 1. an open conflict whose `decision.draft_key` this cut bundled (`carried`) is `resolved` (resolution = the
///    choice, `decision.published_in` = the release, `draft_id` = null);
/// 2. every other open conflict is `superseded`;
/// 3. the detectors run against the applied bundle (`to`; `from` = the bundle that was live) and each conflict is
///    stored `open` under this release. One that a superseded conflict already named (same kind, object, key) keeps
///    its `cause` and `decision` (fixture: the clone); a new one gets [`cause`] of the pin move.
#[allow(clippy::too_many_arguments)]
pub async fn on_apply(
    c: &mut PgConnection,
    detectors: &ConflictDetectors,
    tenant: Uuid,
    job: &JobRef,
    release_id: &str,
    carried: &[String],
    from: Option<&Value>,
    to: &Value,
    pins: (&[TemplatePin], &[TemplatePin]),
) -> ApiResult<Recomputed> {
    let resolved = sqlx::query(
        "UPDATE release_conflict \
            SET status = 'resolved', resolution = decision ->> 'choice', resolved_by = decision ->> 'by', \
                resolved_at = clock_timestamp(), \
                decision = decision || jsonb_build_object('draft_id', NULL::text, 'published_in', $3::text) \
          WHERE job_id = $1 AND status = 'open' AND decision IS NOT NULL \
            AND decision ->> 'draft_key' = ANY($2)",
    )
    .bind(job.id)
    .bind(carried.to_vec())
    .bind(release_id)
    .execute(&mut *c)
    .await?
    .rows_affected() as usize;
    let old = sqlx::query_as::<_, Superseded>(
        "UPDATE release_conflict SET status = 'superseded' \
          WHERE job_id = $1 AND status = 'open' \
          RETURNING kind, object, key, decision, cause",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let fresh = detectors.detect(&mut *c, job, from, to, release_id).await?;
    let new_cause = cause(pins.0, pins.1);
    for n in &fresh {
        let prior = old
            .iter()
            .find(|o| o.kind == n.kind && o.object == n.object && o.key == n.key);
        let cause = prior
            .and_then(|o| o.cause.clone())
            .unwrap_or_else(|| new_cause.clone());
        let decision = prior.and_then(|o| o.decision.as_ref());
        insert(&mut *c, tenant, job, release_id, None, n, &cause, decision).await?;
    }
    Ok(Recomputed {
        resolved,
        superseded: old.len(),
        opened: fresh.len(),
    })
}

/// §5 approve step 11 (D15, fixture `raiseUpgradeConflicts`): a cut that adopts newer template pins (`old` → `new`)
/// opens the conflicts its bundle (`to`) raises against the tenant's rows at the cut — `release_id` and
/// `raised_by_cut` = the cut, cause `template_upgrade` — unless an open conflict already names the same kind, object
/// and key. Nothing happens when the pins did not move. Returns how many were opened.
#[allow(clippy::too_many_arguments)]
pub async fn raise_upgrade(
    c: &mut PgConnection,
    detectors: &ConflictDetectors,
    tenant: Uuid,
    job: &JobRef,
    cut: &str,
    old: &[TemplatePin],
    new: &[TemplatePin],
    from: Option<&Value>,
    to: &Value,
) -> ApiResult<usize> {
    if old == new {
        return Ok(0);
    }
    let cause = cause(old, new);
    let open: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT kind, object, key FROM release_conflict WHERE job_id = $1 AND status = 'open'",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let mut found = detectors.detect(&mut *c, job, from, to, cut).await?;
    found.extend(required_introduced(&mut *c, job, cut, old, new).await?);
    let mut n = 0;
    for f in found {
        let dup = open
            .iter()
            .any(|(k, o, key)| *k == f.kind && *o == f.object && *key == f.key);
        if dup {
            continue;
        }
        insert(&mut *c, tenant, job, cut, Some(cut), &f, &cause, None).await?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(layer: &str, version: i32) -> TemplatePin {
        TemplatePin {
            layer: layer.to_string(),
            version,
        }
    }

    #[test]
    fn cause_names_the_first_moved_layer() {
        let a = [pin("job/x", 3), pin("industry/y", 1)];
        let b = [pin("job/x", 4), pin("industry/y", 1)];
        assert_eq!(
            cause(&a, &b),
            json!({"kind": "template_upgrade", "layer": "job/x", "from": "v3", "to": "v4"})
        );
        assert_eq!(cause(&a, &a), json!({"kind": "new_cut"}));
    }

    #[test]
    fn bundle_reads_are_lenient() {
        let b = json!({
            "objects": {"ontology_attribute": [{"key": "Order.id"}, 5], "context_space": [{"key": "s", "entities": ["Customer"]}]},
            "settings": {"preamble": "hi"}
        });
        let items = base_items(&b, "ontology_attribute");
        assert_eq!(items.len(), 1);
        assert!(find(&items, "Order.id").is_some());
        assert_eq!(base_setting(&b, "preamble"), Some("hi"));
        assert_eq!(base_setting(&b, "actor"), None);
        let e: Vec<String> = base_entities(&b).into_iter().collect();
        assert_eq!(e, ["Customer", "Order"]);
        assert!(base_items(&json!({}), "activity").is_empty());
    }

    // ---- classify: one case per kind (T12 review I1) ----

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("an object").clone()
    }

    fn ih(v: Value) -> String {
        item_hash(&obj(v)).expect("hashes")
    }

    fn row(
        object: &str,
        key: &str,
        kind: &str,
        item: Option<Value>,
        base: Option<String>,
    ) -> OverlayRow {
        OverlayRow {
            object: object.to_string(),
            key: key.to_string(),
            kind: kind.to_string(),
            item,
            base_hash: base,
        }
    }

    fn setting(key: &str, value: Option<&str>, base: Option<&str>) -> SettingRow {
        SettingRow {
            key: key.to_string(),
            value: value.map(str::to_string),
            base_hash: base.map(|b| setting_hash(b).expect("hashes")),
        }
    }

    fn bundle(objects: Value, settings: Value) -> Value {
        json!({"objects": objects, "settings": settings})
    }

    fn kinds(out: &[NewConflict]) -> Vec<(&str, &str, &str)> {
        out.iter()
            .map(|n| (n.kind.as_str(), n.object.as_str(), n.key.as_str()))
            .collect()
    }

    #[test]
    fn base_changed_is_an_override_on_a_moved_base() {
        let old = json!({"key": "a", "label": "Old"});
        let new = json!({"key": "a", "label": "New"});
        let from = bundle(json!({"activity": [old.clone()]}), json!({}));
        let to = bundle(json!({"activity": [new.clone()]}), json!({}));
        let rows = [row(
            "activity",
            "a",
            "override",
            Some(json!({"key": "a"})),
            Some(ih(old.clone())),
        )];
        let out = classify(&rows, &[], Some(&from), &to).unwrap();
        assert_eq!(kinds(&out), [("base_changed", "activity", "a")]);
        let n = &out[0];
        assert_eq!(n.tenant_row, "override");
        assert_eq!(n.base_hash_before, Some(ih(old.clone())));
        assert_eq!(n.base_hash_after, Some(ih(new.clone())));
        assert_eq!(n.base_before, Some(old.clone()));
        assert_eq!(n.base_after, Some(new));
        // the same base: nothing; a tombstone on a moved base: nothing (it hides whatever the base is)
        let same = bundle(json!({"activity": [old.clone()]}), json!({}));
        assert!(classify(&rows, &[], Some(&from), &same).unwrap().is_empty());
        let tomb = [row("activity", "a", "tombstone", None, Some(ih(old)))];
        assert!(classify(&tomb, &[], Some(&from), &to).unwrap().is_empty());
    }

    #[test]
    fn base_removed_is_an_override_or_tombstone_whose_base_is_gone() {
        let old = json!({"key": "g", "label": "Old"});
        let from = bundle(json!({"gate_option": [old.clone()]}), json!({}));
        let to = bundle(json!({"gate_option": []}), json!({}));
        let rows = [
            row(
                "gate_option",
                "g",
                "override",
                Some(json!({"key": "g"})),
                Some(ih(old.clone())),
            ),
            row(
                "gate_option",
                "h",
                "tombstone",
                None,
                Some(ih(json!({"key": "h"}))),
            ),
        ];
        let out = classify(&rows, &[], Some(&from), &to).unwrap();
        assert_eq!(
            kinds(&out),
            [
                ("base_removed", "gate_option", "g"),
                ("base_removed", "gate_option", "h")
            ]
        );
        assert_eq!(out[0].base_before, Some(old));
        assert_eq!(out[0].base_after, None);
        assert_eq!(out[0].base_hash_after, None);
        assert_eq!(out[1].tenant_row, "tombstone");
    }

    #[test]
    fn base_added_is_an_extension_the_base_now_has() {
        let to = bundle(
            json!({"context_space": [{"key": "s", "label": "Base"}]}),
            json!({}),
        );
        let rows = [
            row(
                "context_space",
                "s",
                "extension",
                Some(json!({"key": "s"})),
                None,
            ),
            row(
                "context_space",
                "t",
                "extension",
                Some(json!({"key": "t"})),
                None,
            ),
        ];
        let out = classify(&rows, &[], None, &to).unwrap();
        assert_eq!(kinds(&out), [("base_added", "context_space", "s")]);
        assert_eq!(out[0].tenant_row, "extension");
        assert_eq!(out[0].base_hash_before, None);
        assert_eq!(out[0].base_before, None);
        assert_eq!(
            out[0].base_after,
            Some(json!({"key": "s", "label": "Base"}))
        );
    }

    #[test]
    fn parent_missing_only_when_the_release_removed_the_entity() {
        let attr = || {
            row(
                "ontology_attribute",
                "Order.note",
                "extension",
                Some(json!({"key": "Order.note"})),
                None,
            )
        };
        let from = bundle(
            json!({"ontology_attribute": [{"key": "Order.id"}]}),
            json!({}),
        );
        let to = bundle(
            json!({"ontology_attribute": [{"key": "Customer.id"}]}),
            json!({}),
        );
        // raised: Order was in the old base and is in neither the new base nor a tenant context space
        let out = classify(&[attr()], &[], Some(&from), &to).unwrap();
        assert_eq!(
            kinds(&out),
            [("parent_missing", "ontology_attribute", "Order.note")]
        );
        assert_eq!(out[0].base_after, None);
        // not raised: a tenant context space names the entity
        let space = row(
            "context_space",
            "mine",
            "extension",
            Some(json!({"key": "mine", "entities": ["Order"]})),
            None,
        );
        assert!(
            classify(&[attr(), space], &[], Some(&from), &to)
                .unwrap()
                .is_empty()
        );
        // not raised: the entity was never in the old base (or nothing was live)
        let never = bundle(
            json!({"ontology_attribute": [{"key": "Customer.id"}]}),
            json!({}),
        );
        assert!(
            classify(&[attr()], &[], Some(&never), &to)
                .unwrap()
                .is_empty()
        );
        assert!(classify(&[attr()], &[], None, &to).unwrap().is_empty());
        // not raised: the new base still names it through a base context space
        let kept = bundle(
            json!({"context_space": [{"key": "b", "entities": ["Order"]}]}),
            json!({}),
        );
        assert!(
            classify(&[attr()], &[], Some(&from), &kept)
                .unwrap()
                .is_empty()
        );
        // not raised for a tombstone
        let tomb = row("ontology_attribute", "Order.note", "tombstone", None, None);
        assert!(classify(&[tomb], &[], Some(&from), &to).unwrap().is_empty());
    }

    #[test]
    fn other_objects_are_ignored() {
        let to = bundle(json!({"selection": [{"key": "x"}]}), json!({}));
        let rows = [row("selection", "x", "extension", None, None)];
        assert!(classify(&rows, &[], None, &to).unwrap().is_empty());
    }

    /// KH T4: an instruction override is made against the folded text, so re-tagging a contributing layer with the
    /// same text is no conflict; another text is `base_changed`.
    #[test]
    fn an_instruction_override_conflicts_on_its_text_only() {
        let tone = |text: &str, tag: &str| {
            json!({"key": "tone", "text": text,
                   "sections": [{"layer": "job/x", "tag": tag, "text": text}]})
        };
        let v4 = tone("Warm.", "job/x@v4");
        let made_on = base_hash("instruction", &obj(v4.clone())).expect("hashes");
        let rows = [row(
            "instruction",
            "tone",
            "override",
            Some(json!({"key": "tone", "text": "Brisk."})),
            Some(made_on),
        )];
        let from = bundle(json!({"instruction": [v4]}), json!({}));
        let retagged = bundle(
            json!({"instruction": [tone("Warm.", "job/x@v5")]}),
            json!({}),
        );
        assert!(
            classify(&rows, &[], Some(&from), &retagged)
                .unwrap()
                .is_empty()
        );
        let changed = bundle(
            json!({"instruction": [tone("Cool.", "job/x@v5")]}),
            json!({}),
        );
        let out = classify(&rows, &[], Some(&from), &changed).unwrap();
        assert_eq!(kinds(&out), [("base_changed", "instruction", "tone")]);
    }

    #[test]
    fn setting_kinds() {
        let from = bundle(
            json!({}),
            json!({"preamble": "old", "actor": "a", "stance": "s"}),
        );
        let to = bundle(json!({}), json!({"preamble": "new", "stance": "s2"}));
        let settings = [
            setting("actor", Some("mine"), Some("a")),
            setting("stance", Some("mine"), Some("s")),
            // KH T4: `preamble` is no tenant setting any more (the identity instruction); a stray row raises nothing
            setting("preamble", Some("mine"), Some("old")),
        ];
        let out = classify(&[], &settings, Some(&from), &to).unwrap();
        assert_eq!(
            kinds(&out),
            [
                ("base_removed", JOB_SETTING, "actor"),
                (JOB_SETTING, JOB_SETTING, "stance"),
            ]
        );
        assert_eq!(out[0].tenant_row, "override");
        assert_eq!(out[0].base_after, None);
        let p = &out[1];
        assert_eq!(p.tenant_row, "override");
        assert_eq!(p.base_hash_before, Some(setting_hash("s").unwrap()));
        assert_eq!(p.base_hash_after, Some(setting_hash("s2").unwrap()));
        assert_eq!(p.base_before, Some(json!({"stance": "s"})));
        assert_eq!(p.base_after, Some(json!({"stance": "s2"})));
        // an extension over a base the release adds is base_added
        let added = [setting("actor", Some("mine"), None)];
        let out = classify(
            &[],
            &added,
            Some(&to),
            &bundle(json!({}), json!({"actor": "a2"})),
        )
        .unwrap();
        assert_eq!(kinds(&out), [("base_added", JOB_SETTING, "actor")]);
        assert_eq!(out[0].tenant_row, "extension");
        // an unchanged base, a tombstone on a moved base and an unknown key raise nothing
        let quiet = [
            setting("stance", Some("mine"), Some("s2")),
            setting("actor", None, Some("a")),
            setting("unknown", Some("mine"), None),
        ];
        let to2 = bundle(
            json!({}),
            json!({"actor": "a3", "stance": "s2", "unknown": "u"}),
        );
        assert!(classify(&[], &quiet, Some(&from), &to2).unwrap().is_empty());
        // a tombstone whose base is gone is base_removed
        let gone = [setting("stance", None, Some("s"))];
        let out = classify(&[], &gone, Some(&from), &bundle(json!({}), json!({}))).unwrap();
        assert_eq!(kinds(&out), [("base_removed", JOB_SETTING, "stance")]);
        assert_eq!(out[0].tenant_row, "tombstone");
    }
}
