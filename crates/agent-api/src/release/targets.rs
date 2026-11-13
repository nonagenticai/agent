//! B2's six design targets (spec B2 §2.2, §2.5 rows 1–6): the Rust form of the fixture `TARGETS` entries
//! `languages`, `setting`, `overlay`, `rule_values`, `rule_passage` and `selection`
//! (`web/src/lib/fixtures/ledger.ts` lines 153–300). Each copies the fixture's `key`, `proj`, `text` and `places`;
//! `get` returns the fixture's `get` JSON over B2's working rows (0047 + `jobs.languages`).
//!
//! | kind | key | working rows | places |
//! |---|---|---|---|
//! | `languages` | `languages` | `jobs.languages` | `[{}]` |
//! | `setting` | `setting:<key>` | `job_setting_override` | `actor, stance` ∪ override keys (KH T4: no `preamble`) |
//! | `overlay` | `overlay:<object>:<key>` | `job_overlay_row` | every tenant row `{object, key}` |
//! | `rule_values` | `rule:<id>` | `business_rule.slot_values` | rule slots of the draft-compiled bundle ∪ stored rows |
//! | `rule_passage` | `rule-passage:<id>` | `business_rule.passage` | as `rule_values` |
//! | `selection` | `selection` | `job_target.selection` | `[{}]` |
//!
//! `set` is the restore path (discard, follow-live after apply/rollback, a write back): it writes exactly the value
//! `get` returns. Handlers write their rows inside the `mutate` closure they pass to `design::track`; neither a
//! target nor a handler bumps `jobs.draft_rev` (`track` does). A row `set` creates that its value does not name the
//! writer of is stamped [`SET_BY`]; the ledger and the audit trail name the person.
//!
//! Where a value can be "nothing" two ways, the target keeps them apart only when the fixture does:
//! - `setting`: no override (`None`, "release value") ≠ an override of `null` (`Some({value: null, ..})`, "hidden").
//!   Unlike the fixture (whose value is the bare string), the value is the row `{value, base_hash}` and `proj` compares
//!   both, as `overlay` does: a rebase ("Keep mine") that only re-stamps `base_hash` onto the new base is a draft change
//!   (T8 review I1 ruling), and `set` (restore) writes `base_hash` back (T6 review M3);
//! - `rule_values`: no `business_rule` row, or a row with no values → `None` (T2 review M5);
//! - `rule_passage`: no row, or a row with no passage → `None` ("no passage");
//! - `selection`: no `job_target` row, or no selection chosen → `None`.
use std::collections::BTreeMap;
use std::sync::Arc;

use agent_model::compile::Selection;
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::compile::{CompileError, TemplateCompiler};
use super::overlay::KNOWLEDGE_PACK;
use crate::design::hooks::is_template_refusal;
use crate::design::{DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap};
use crate::error::{ApiError, ApiResult};
use crate::knowledge::kinds;
use crate::platform::templates::TemplatePin;

/// `updated_by` of a working row that `set` creates and whose value does not name its writer.
pub const SET_BY: &str = "design";

/// The job-setting keys a tenant may override (fixture `syncSettings` / `setting.places`). KH T4 (D108): `preamble`
/// is no longer one — it is the job layer's `identity` instruction, which tenants never refine.
pub const SETTING_KEYS: [&str; 2] = ["actor", "stance"];

/// The setting bases a release carries and the runtime reads (`bundle.settings`, `RuntimeDesign::settings`):
/// [`SETTING_KEYS`] plus `preamble`, the folded `identity` instruction (D108).
pub const BASE_SETTING_KEYS: [&str; 3] = ["preamble", "actor", "stance"];

/// Setting keys whose tenant overrides KH T4 retired (0133 deleted the rows): a stored snapshot that still names one
/// (a rollback or restore to a release cut before T4) writes nothing for it.
pub const RETIRED_SETTING_KEYS: [&str; 1] = ["preamble"];

/// Register B2's six targets (one line in `design::registry()`). The rule targets compile the job's draft selection
/// with `compiler` to find the template's rule slots.
pub fn register(r: &mut Registry, compiler: Arc<dyn TemplateCompiler>) {
    r.register(Arc::new(Languages));
    r.register(Arc::new(Setting));
    r.register(Arc::new(Overlay));
    r.register(Arc::new(RuleValues {
        compiler: compiler.clone(),
    }));
    r.register(Arc::new(RulePassage { compiler }));
    r.register(Arc::new(SelectionTarget));
}

// ---- helpers --------------------------------------------------------------------------------------------------------

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn job_of(at: Place<'_>) -> ApiResult<Uuid> {
    at.job
        .map(|j| j.id)
        .ok_or_else(|| internal("a B2 design target is read and written at a job place".into()))
}

fn field<'r>(r: &'r DraftRef, f: &str) -> &'r str {
    r.get(f).and_then(Value::as_str).unwrap_or("")
}

fn one(f: &str, v: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert(f.to_string(), Value::String(v.to_string()));
    m
}

/// JavaScript `String(v)`.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Fixture `list`: a non-empty array joined with `, ` (JavaScript `join`: null → ""), else `none`.
fn list(v: &Value) -> String {
    match v.as_array() {
        Some(xs) if !xs.is_empty() => xs
            .iter()
            .map(|x| {
                if x.is_null() {
                    String::new()
                } else {
                    js_string(x)
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        _ => "none".to_string(),
    }
}

/// A non-null value, else `None` (the kinds where `null` and "nothing" are one state).
fn present(v: &Snap) -> Option<&Value> {
    v.as_ref().filter(|x| !x.is_null())
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

// ---- languages (ledger.ts 155–167) ----------------------------------------------------------------------------------

/// D59: the languages a job serves (`jobs.languages`).
pub struct Languages;

impl DesignTarget for Languages {
    fn kind(&self) -> &'static str {
        "languages"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, _r: &DraftRef) -> String {
        "languages".to_string()
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let langs: Option<Vec<String>> =
                sqlx::query_scalar("SELECT languages FROM jobs WHERE id = $1")
                    .bind(job)
                    .fetch_optional(&mut *c)
                    .await?;
            Ok(langs.map(|l| json!(l)))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            // fixture: `if (job && s)` — removing the languages place changes nothing
            let Some(v) = v else {
                return Ok(());
            };
            let langs: Vec<String> = serde_json::from_value(v.clone())
                .map_err(|e| internal(format!("languages value is not a string list: {e}")))?;
            sqlx::query("UPDATE jobs SET languages = $2 WHERE id = $1")
                .bind(job)
                .bind(langs)
                .execute(&mut *c)
                .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(Some(vec![DraftRef::new()])) })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(match v {
            Some(x) => list(x),
            None => "none".to_string(),
        })
    }
}

// ---- setting (ledger.ts 168–191) ------------------------------------------------------------------------------------

/// A job's override of a template setting (`job_setting_override`; `value` NULL = hide the base value). The value is
/// `{value: string | null, base_hash: string | null}`; `base_hash` = the hash of the base value the override was made
/// over (`release::hash::bundle_hash` of the base string), so moving it onto a new base is a change.
pub struct Setting;

/// The `{value, base_hash}` of a setting snap, else a 500 (a corrupt value).
fn setting_row(v: &Value) -> ApiResult<(Option<String>, Option<String>)> {
    let text = |f: &str| -> ApiResult<Option<String>> {
        match v.get(f) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => Err(internal(format!(
                "setting {f} is not a string or null: {other}"
            ))),
        }
    };
    if !v.is_object() {
        return Err(internal(format!(
            "setting value is not {{value, base_hash}}: {v}"
        )));
    }
    Ok((text("value")?, text("base_hash")?))
}

impl DesignTarget for Setting {
    fn kind(&self) -> &'static str {
        "setting"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("setting:{}", field(r, "key"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT value, base_hash FROM job_setting_override WHERE job_id = $1 AND key = $2",
            )
            .bind(job)
            .bind(field(r, "key"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(|(value, base_hash)| json!({"value": value, "base_hash": base_hash})))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let key = field(r, "key");
            if v.is_some() && RETIRED_SETTING_KEYS.contains(&key) {
                tracing::warn!(key = %key, "a pre-KH-T4 snapshot names a retired setting override; not restored");
                return Ok(());
            }
            let Some(v) = v else {
                sqlx::query("DELETE FROM job_setting_override WHERE job_id = $1 AND key = $2")
                    .bind(job)
                    .bind(key)
                    .execute(&mut *c)
                    .await?;
                return Ok(());
            };
            let (value, base_hash) = setting_row(v)?;
            sqlx::query(
                "INSERT INTO job_setting_override (job_id, key, value, base_hash, updated_by) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (tenant_id, job_id, key) DO UPDATE \
                    SET value = EXCLUDED.value, base_hash = EXCLUDED.base_hash, updated_at = clock_timestamp()",
            )
            .bind(job)
            .bind(key)
            .bind(value)
            .bind(base_hash)
            .bind(SET_BY)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let stored: Vec<String> = sqlx::query_scalar(
                "SELECT key FROM job_setting_override WHERE job_id = $1 ORDER BY key",
            )
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
            let mut keys: Vec<String> = SETTING_KEYS.iter().map(|k| k.to_string()).collect();
            for k in stored {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            Ok(Some(keys.iter().map(|k| one("key", k)).collect()))
        })
    }
    /// `(value, base_hash)`; no override (`None`) stays apart from a hidden one (`{value: null}`).
    fn proj(&self, v: &Snap) -> Value {
        match v {
            None => Value::Null,
            Some(r) => {
                let pick = |f: &str| r.get(f).cloned().unwrap_or(Value::Null);
                json!({"value": pick("value"), "base_hash": pick("base_hash")})
            }
        }
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(match v.as_ref().map(|r| r.get("value")) {
            None => "release value".to_string(),
            Some(None | Some(Value::Null)) => "hidden".to_string(),
            Some(Some(x)) => js_string(x),
        })
    }
}

/// The template bases a design VIEW shows at a place the tenant has not changed ([`view_text`]): one bundle's
/// `settings` (key → base; a key without a base is absent) and `objects` (overlay object → its base items).
#[derive(Debug, Clone, Default)]
pub struct ViewBases {
    settings: BTreeMap<String, String>,
    objects: Map<String, Value>,
}

impl ViewBases {
    /// The bases a bundle carries (`job_release.bundle`, or a compiled `Bundle` as JSON).
    pub fn of_bundle(bundle: &Value) -> Self {
        let settings = bundle
            .get("settings")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let objects = bundle
            .get("objects")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Self { settings, objects }
    }

    /// Setting `key` without an override: its base, `not set` when there is none.
    fn setting(&self, key: &str) -> String {
        self.settings
            .get(key)
            .cloned()
            .unwrap_or_else(|| "not set".to_string())
    }

    /// Overlay row `key` of `object` without a tenant row: the base item (`base “<label>”`), `none` when the base
    /// has no such item.
    fn overlay(&self, object: &str, key: &str) -> String {
        let item = self
            .objects
            .get(object)
            .and_then(Value::as_array)
            .and_then(|xs| {
                xs.iter()
                    .find(|i| i.get("key").and_then(Value::as_str) == Some(key))
            });
        let Some(item) = item else {
            return "none".to_string();
        };
        match item
            .get("label")
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty())
        {
            Some(l) => format!("base “{l}”"),
            None => "base".to_string(),
        }
    }
}

/// Whether a VIEW line of `kind` at value `v` shows a base ([`view_text`]): a setting without an override, an
/// overlay place without a tenant row.
pub fn shows_base(kind: &str, v: &Snap) -> bool {
    match kind {
        "setting" => v.is_none(),
        "overlay" => present(v).is_none(),
        _ => false,
    }
}

/// Whether any VIEW line of `kind` can show a base, i.e. whether reading [`view_bases`] is worth it.
pub fn may_show_base(kind: &str) -> bool {
    shows_base(kind, &None)
}

/// A design VIEW's line for place `r` — any field that is a VALUE: live / draft / release design, design diff, the
/// draft's `live` / `draft`, a rollback preview's `now` / `after`. The target's text, except where [`shows_base`]:
/// [`Setting::text`] and [`Overlay::text`] say "release value" there, which suits a ledger line ("was release value
/// → now …") but is not a value; a view shows the base in force (`bases`, [`ViewBases`]). An override of null stays
/// "hidden" (live acceptance 2026-10-21: every setting, then every draft row and refinement, read "release value").
pub fn view_text(
    t: &dyn DesignTarget,
    v: &Snap,
    r: &DraftRef,
    bases: &ViewBases,
) -> Option<String> {
    if shows_base(t.kind(), v) {
        return Some(match t.kind() {
            "overlay" => bases.overlay(field(r, "object"), field(r, "key")),
            _ => bases.setting(field(r, "key")),
        });
    }
    t.text(v, r)
}

/// The bases in force for `of`: release `id`'s bundle (`Live` = the current release); else (`Draft`, or a job never
/// released) the bundle the DRAFT selection compiles to — none without a selection or when it does not compile
/// ([`super::overlay::draft_bundle`]).
pub async fn view_bases(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<ViewBases> {
    let release = match of {
        DesignOf::Live => super::summary::current_ref(&mut *c, job)
            .await?
            .map(|r| r.id),
        DesignOf::Release(id) => Some(id.clone()),
        DesignOf::Draft => None,
    };
    if let Some(id) = release {
        let bundle: Option<Value> = sqlx::query_scalar(
            "SELECT bundle FROM job_release WHERE job_id = $1 AND release_id = $2",
        )
        .bind(job.id)
        .bind(id)
        .fetch_optional(&mut *c)
        .await?;
        return Ok(bundle
            .as_ref()
            .map(ViewBases::of_bundle)
            .unwrap_or_default());
    }
    match super::overlay::draft_bundle(&mut *c, compiler, job.id).await? {
        Some(b) => {
            let v = serde_json::to_value(&b)
                .map_err(|e| internal(format!("the draft bundle does not serialise: {e}")))?;
            Ok(ViewBases::of_bundle(&v))
        }
        None => Ok(ViewBases::default()),
    }
}

/// The [`view_bases`] of the live design and of the draft (an unpublished change's `live` / `draft`).
#[derive(Debug, Clone, Default)]
pub struct LiveDraftBases {
    pub live: ViewBases,
    pub draft: ViewBases,
}

impl LiveDraftBases {
    /// Both, read only when `needed` (some line [`may_show_base`]); else both empty.
    pub async fn read(
        c: &mut PgConnection,
        compiler: &dyn TemplateCompiler,
        job: &JobRef,
        needed: bool,
    ) -> ApiResult<Self> {
        if !needed {
            return Ok(Self::default());
        }
        Ok(Self {
            live: view_bases(&mut *c, compiler, job, &DesignOf::Live).await?,
            draft: view_bases(&mut *c, compiler, job, &DesignOf::Draft).await?,
        })
    }
}

// ---- overlay (ledger.ts 192–227) ------------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct OverlayStored {
    key: String,
    kind: String,
    item: Option<Value>,
    base_hash: Option<String>,
    content_hash: Option<String>,
    updated_by: String,
    updated_at: DateTime<Utc>,
}

/// PK-2: after a `knowledge_pack` row was set (discard, restore, apply, rollback), the template-sourced pins of the
/// kinds its key covers follow the template minus the job's tombstones again, in the same transaction, so kind pins
/// never go stale. Any other object: nothing.
async fn refresh_pack_pins(
    c: &mut PgConnection,
    at: Place<'_>,
    object: &str,
    key: &str,
) -> ApiResult<()> {
    if object != KNOWLEDGE_PACK {
        return Ok(());
    }
    let job = at
        .job
        .ok_or_else(|| internal("a B2 design target is read and written at a job place".into()))?;
    kinds::refresh_template_packs(c, job, key).await
}

/// One tenant refinement row of a refinable object (`job_overlay_row`); the value is the fixture `TenantRow`
/// `{key, kind, item, base_hash, content_hash, updated_by, updated_at}`.
pub struct Overlay;

impl DesignTarget for Overlay {
    fn kind(&self) -> &'static str {
        "overlay"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("overlay:{}:{}", field(r, "object"), field(r, "key"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let row = sqlx::query_as::<_, OverlayStored>(
                "SELECT key, kind, item, base_hash, content_hash, updated_by, updated_at \
                   FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(job)
            .bind(field(r, "object"))
            .bind(field(r, "key"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(|o| {
                json!({
                    "key": o.key,
                    "kind": o.kind,
                    "item": o.item,
                    "base_hash": o.base_hash,
                    "content_hash": o.content_hash,
                    "updated_by": o.updated_by,
                    "updated_at": iso(o.updated_at),
                })
            }))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let (object, key) = (field(r, "object"), field(r, "key"));
            let Some(v) = present(v) else {
                sqlx::query(
                    "DELETE FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
                )
                .bind(job)
                .bind(object)
                .bind(key)
                .execute(&mut *c)
                .await?;
                return refresh_pack_pins(c, at, object, key).await;
            };
            let Some(row) = v.as_object() else {
                return Err(internal(format!("overlay value is not an object: {v}")));
            };
            let str_at = |f: &str| row.get(f).and_then(Value::as_str).map(str::to_owned);
            let kind =
                str_at("kind").ok_or_else(|| internal("overlay value has no kind".to_string()))?;
            let item: Option<Value> = row.get("item").filter(|x| !x.is_null()).cloned();
            let updated_by = str_at("updated_by")
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| SET_BY.to_string());
            let updated_at: Option<DateTime<Utc>> = row
                .get("updated_at")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            sqlx::query(
                "INSERT INTO job_overlay_row \
                        (job_id, object, key, kind, item, base_hash, content_hash, updated_by, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($9, clock_timestamp())) \
                 ON CONFLICT (tenant_id, job_id, object, key) DO UPDATE \
                    SET kind = EXCLUDED.kind, item = EXCLUDED.item, base_hash = EXCLUDED.base_hash, \
                        content_hash = EXCLUDED.content_hash, updated_by = EXCLUDED.updated_by, \
                        updated_at = EXCLUDED.updated_at",
            )
            .bind(job)
            .bind(object)
            .bind(key)
            .bind(kind)
            .bind(item)
            .bind(str_at("base_hash"))
            .bind(str_at("content_hash"))
            .bind(updated_by)
            .bind(updated_at)
            .execute(&mut *c)
            .await?;
            refresh_pack_pins(c, at, object, key).await
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let rows: Vec<(String, String)> = sqlx::query_as(
                // activity rows are the `activity` target's places (journey::refine, Q-R3 (a)); listing
                // them here too would snapshot and diff every activity refinement twice
                "SELECT object, key FROM job_overlay_row WHERE job_id = $1 AND object <> 'activity' \
                  ORDER BY object, key",
            )
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                rows.into_iter()
                    .map(|(object, key)| {
                        let mut m = one("object", &object);
                        m.insert("key".to_string(), Value::String(key));
                        m
                    })
                    .collect(),
            ))
        })
    }
    /// Fixture: `r && {kind: r.kind, item: r.item, base_hash: r.base_hash}`.
    fn proj(&self, v: &Snap) -> Value {
        match present(v) {
            None => Value::Null,
            Some(r) => {
                let pick = |f: &str| r.get(f).cloned().unwrap_or(Value::Null);
                json!({"kind": pick("kind"), "item": pick("item"), "base_hash": pick("base_hash")})
            }
        }
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Some(r) = present(v) else {
            return Some("release value".to_string());
        };
        let kind = r.get("kind").map(js_string).unwrap_or_default();
        if kind == "tombstone" {
            return Some("hidden".to_string());
        }
        let label = r
            .get("item")
            .and_then(|i| i.get("label"))
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty());
        Some(match label {
            Some(l) => format!("{kind} “{l}”"),
            None => kind,
        })
    }
}

// ---- rule_values + rule_passage (ledger.ts 228–260) -----------------------------------------------------------------

/// The rule ids a job has (fixture `w.release.rules` of the job): the rule slots of the bundle its DRAFT selection
/// compiles to, then every stored `business_rule` row not among them (by id). A job with no selection chosen, or
/// whose selection does not compile (a 422 `compile_failed`, or a template refusal such as 404 `profile_not_found`,
/// `design::hooks::is_template_refusal`), has no template slots; any other error propagates.
async fn draft_rule_ids(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: Uuid,
) -> ApiResult<Vec<String>> {
    let stored_sel: Option<Option<Value>> =
        sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
            .bind(job)
            .fetch_optional(&mut *c)
            .await?;
    let mut ids: Vec<String> = Vec::new();
    if let Some(sel) = stored_sel.flatten() {
        let sel: Selection = serde_json::from_value(sel)
            .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))?;
        let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(&mut *c)
            .await?;
        let pins: Vec<TemplatePin> = serde_json::from_value(pins)
            .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
        match compiler.compile(&mut *c, &sel, &pins).await {
            Ok(compiled) => {
                for r in &compiled.bundle.rules {
                    if !ids.contains(&r.id) {
                        ids.push(r.id.clone());
                    }
                }
            }
            Err(CompileError::Api(e)) if !is_template_refusal(&e) => return Err(*e),
            Err(_) => {}
        }
    }
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT rule_id FROM business_rule WHERE job_id = $1 ORDER BY rule_id")
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
    for id in stored {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let authored: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM job_overlay_row WHERE job_id = $1 AND object = 'rule_slot' ORDER BY key",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    for id in authored {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// A job's values for one template business rule (`business_rule.slot_values`, `{slot: string | null}`).
pub struct RuleValues {
    compiler: Arc<dyn TemplateCompiler>,
}

impl DesignTarget for RuleValues {
    fn kind(&self) -> &'static str {
        "rule_values"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("rule:{}", field(r, "id"))
    }
    /// `None` when the job has no row for the rule, or a row with no values (T2 review M5).
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let vals: Option<Value> = sqlx::query_scalar(
                "SELECT slot_values FROM business_rule WHERE job_id = $1 AND rule_id = $2",
            )
            .bind(job)
            .bind(field(r, "id"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(vals.filter(|v| v.as_object().is_some_and(|m| !m.is_empty())))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let id = field(r, "id");
            let Some(v) = present(v) else {
                // no values: a row that holds nothing else goes, a row with a passage keeps it
                sqlx::query(
                    "DELETE FROM business_rule WHERE job_id = $1 AND rule_id = $2 AND passage IS NULL",
                )
                .bind(job)
                .bind(id)
                .execute(&mut *c)
                .await?;
                sqlx::query(
                    "UPDATE business_rule SET slot_values = '{}', updated_at = clock_timestamp() \
                      WHERE job_id = $1 AND rule_id = $2",
                )
                .bind(job)
                .bind(id)
                .execute(&mut *c)
                .await?;
                return Ok(());
            };
            let ok = v
                .as_object()
                .is_some_and(|m| m.values().all(|x| x.is_string() || x.is_null()));
            if !ok {
                return Err(internal(format!(
                    "rule values are not an object of string | null: {v}"
                )));
            }
            sqlx::query(
                "INSERT INTO business_rule (job_id, rule_id, slot_values, updated_by) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (tenant_id, job_id, rule_id) DO UPDATE \
                    SET slot_values = EXCLUDED.slot_values, updated_at = clock_timestamp()",
            )
            .bind(job)
            .bind(id)
            .bind(v.clone())
            .bind(SET_BY)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let ids = draft_rule_ids(&mut *c, self.compiler.as_ref(), job).await?;
            Ok(Some(ids.iter().map(|id| one("id", id)).collect()))
        })
    }
    /// Fixture: `k = v` per slot (`not set` for null), joined with ` · `; nothing → `null`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let m: &Map<String, Value> = v.as_ref()?.as_object()?;
        Some(
            m.iter()
                .map(|(k, x)| {
                    let shown = if x.is_null() {
                        "not set".to_string()
                    } else {
                        js_string(x)
                    };
                    format!("{k} = {shown}")
                })
                .collect::<Vec<_>>()
                .join(" · "),
        )
    }
}

/// The PolicyPassage copy a job keeps with one business rule (`business_rule.passage`).
pub struct RulePassage {
    compiler: Arc<dyn TemplateCompiler>,
}

impl DesignTarget for RulePassage {
    fn kind(&self) -> &'static str {
        "rule_passage"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("rule-passage:{}", field(r, "id"))
    }
    /// `None` when the job has no row for the rule, or a row without a passage.
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let p: Option<Option<Value>> = sqlx::query_scalar(
                "SELECT passage FROM business_rule WHERE job_id = $1 AND rule_id = $2",
            )
            .bind(job)
            .bind(field(r, "id"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(p.flatten())
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let id = field(r, "id");
            let Some(v) = present(v) else {
                // no passage: a row that holds no values goes, a row with values keeps them
                sqlx::query(
                    "DELETE FROM business_rule WHERE job_id = $1 AND rule_id = $2 AND slot_values = '{}'",
                )
                .bind(job)
                .bind(id)
                .execute(&mut *c)
                .await?;
                sqlx::query(
                    "UPDATE business_rule SET passage = NULL, updated_at = clock_timestamp() \
                      WHERE job_id = $1 AND rule_id = $2",
                )
                .bind(job)
                .bind(id)
                .execute(&mut *c)
                .await?;
                return Ok(());
            };
            if !v.is_object() {
                return Err(internal(format!("rule passage is not an object: {v}")));
            }
            sqlx::query(
                "INSERT INTO business_rule (job_id, rule_id, passage, updated_by) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (tenant_id, job_id, rule_id) DO UPDATE \
                    SET passage = EXCLUDED.passage, updated_at = clock_timestamp()",
            )
            .bind(job)
            .bind(id)
            .bind(v.clone())
            .bind(SET_BY)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let ids = draft_rule_ids(&mut *c, self.compiler.as_ref(), job).await?;
            Ok(Some(ids.iter().map(|id| one("id", id)).collect()))
        })
    }
    /// Fixture: `v(s)?.title ?? 'no passage'`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(
            present(v)
                .and_then(|p| p.get("title"))
                .filter(|t| !t.is_null())
                .map_or_else(|| "no passage".to_string(), js_string),
        )
    }
}

// ---- selection (ledger.ts 261–282) ----------------------------------------------------------------------------------

/// The job's template selection (D65, `job_target.selection`).
pub struct SelectionTarget;

/// Fixture `selectionValues(sel, key)` for `SELECTION_VALUE_KEYS` (`area`, `channel`, `region`): the list, the one
/// `region` (D111) as a list of one.
fn selection_values(sel: &Value, key: &str) -> Option<Value> {
    match key {
        "area" => sel.get("areas").cloned(),
        "region" => sel
            .get("region")
            .filter(|r| r.is_string())
            .map(|r| Value::Array(vec![r.clone()])),
        _ => sel.get("dimensions").and_then(|d| d.get(key)).cloned(),
    }
}

impl DesignTarget for SelectionTarget {
    fn kind(&self) -> &'static str {
        "selection"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, _r: &DraftRef) -> String {
        "selection".to_string()
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let sel: Option<Option<Value>> =
                sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
                    .bind(job)
                    .fetch_optional(&mut *c)
                    .await?;
            Ok(sel.flatten())
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let sel: Option<Value> = present(v).cloned();
            if sel.as_ref().is_some_and(|s| !s.is_object()) {
                return Err(internal("selection value is not an object".to_string()));
            }
            // KH T1: a value from before 0132 (a pre-KH release's snapshot, restored by a rollback / discard /
            // restore) is rewritten into the hierarchy shape first; one without the hierarchy is a 422, never the
            // job_target_selection_v2 CHECK's 500.
            let sel = match sel {
                Some(raw) => {
                    let v2 = crate::platform::selection::selection_v2(&raw);
                    let parsed: Selection = serde_json::from_value(v2.clone()).map_err(|e| {
                        ApiError::Unprocessable(
                            "invalid_selection",
                            format!("The selection cannot be read: {e}"),
                        )
                    })?;
                    crate::platform::templates::hierarchy_refusal(&parsed)?;
                    crate::platform::templates::region_refusal(&mut *c, &parsed).await?;
                    Some(v2)
                }
                None => None,
            };
            sqlx::query(
                "INSERT INTO job_target (job_id, selection, updated_by, updated_at) \
                 VALUES ($1, $2, $3, clock_timestamp()) \
                 ON CONFLICT (tenant_id, job_id) DO UPDATE \
                    SET selection = EXCLUDED.selection, updated_by = EXCLUDED.updated_by, \
                        updated_at = EXCLUDED.updated_at",
            )
            .bind(job)
            .bind(sel)
            .bind(SET_BY)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(Some(vec![DraftRef::new()])) })
    }
    /// Fixture: `${profile} · ${dimension}: ${list} · …` over the non-empty `area`/`channel`/`region` values;
    /// nothing → `null`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        // R15: a pre-KH release's snapshot reads in the hierarchy shape (`dimensions.region[]` → `region`)
        let sel = &crate::platform::selection::selection_v2(present(v)?);
        let profile = sel
            .get("profile")
            .map_or_else(|| "undefined".to_string(), js_string);
        let dims: Vec<String> = ["area", "channel", "region"]
            .iter()
            .filter_map(|d| {
                let xs = selection_values(sel, d)?;
                xs.as_array()
                    .is_some_and(|a| !a.is_empty())
                    .then(|| format!("{d}: {}", list(&xs)))
            })
            .collect();
        Some(format!("{profile} · {}", dims.join(" · ")))
    }
}
