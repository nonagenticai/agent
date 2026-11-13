//! A job's setting overrides and languages (B2 T7, spec §4): `jobSettingsView`, `saveJobSettingOverride`,
//! `revertJobSettingOverride`, `jobLanguages`, `saveJobLanguages`.
//!
//! - `GET /api/jobs/{job}/settings` — any member: `JobSettingsView {base, override, effective, draft_rev}`. `base` =
//!   the settings of the bundle the job's DRAFT selection compiles to (null while there is no selection or it does
//!   not compile); `override` = the job's `job_setting_override` rows (null = hidden).
//! - `PUT /api/jobs/{job}/settings` `{<key>: string | null}` — `job_designer`: one draft write per key (kind
//!   `setting`, page J13); 422 `unknown_setting`, `invalid_body`. The value is stored trimmed; null hides the base.
//! - `DELETE /api/jobs/{job}/settings/{key}` — `job_designer`: back to the release value through
//!   `release::overlay::refine(Op::Revert, job_setting)` (open conflicts of the setting are decided `reverted` and
//!   linked to the ledger row); 409 `nothing_to_revert`, 422 `unknown_setting`.
//! - `GET /api/jobs/{job}/languages` — any member: `JobLanguagesView {live, draft, options, unpublished}`.
//! - `PUT /api/jobs/{job}/languages` `{languages}` — `job_designer`: the draft languages (kind `languages`, page
//!   J13); 422 `invalid_body`, `unknown_language`.
//!
//! A port of the fixture handlers (`web/src/lib/api/areas/release.ts`). Every handler runs `ensure_job` first in its
//! job-scoped transaction; `design::track` records each write and bumps `draft_rev` (the handler never does). A write
//! locks the job row before it reads anything its write depends on (the same lock `track` takes again).
use std::collections::BTreeMap;

use agent_model::compile::Selection;
use axum::extract::State;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::design::hooks::is_template_refusal;
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label, Registry, coverage};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::platform::templates::TemplatePin;
use crate::release::compile::{CompileError, TemplateCompiler};
use crate::release::hash::bundle_hash;
use crate::release::overlay::{self, Op};
use crate::release::targets::SETTING_KEYS;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/settings",
            get(job_settings_view).put(save_job_setting_override),
        )
        .route(
            "/api/jobs/{job}/settings/{key}",
            delete(revert_job_setting_override),
        )
        .route(
            "/api/jobs/{job}/languages",
            get(job_languages).put(save_job_languages),
        )
}

/// The platform languages a job may serve, in the fixture's order (`LANGUAGE_LABEL`, `types/core.ts`).
pub const LANGUAGES: [(&str, &str); 4] = [
    ("nl", "Dutch"),
    ("en", "English"),
    ("fr", "French"),
    ("de", "German"),
];

/// The longest setting value, in UTF-16 code units (fixture `v.length > 2000`).
pub const MAX_SETTING_LEN: usize = 2000;

/// The base value of each setting key, in [`SETTING_KEYS`] order.
pub(super) type Bases = Vec<(&'static str, Option<String>)>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveLanguages {
    languages: Vec<String>,
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

fn key_ref(key: String) -> DraftRef {
    let mut r = DraftRef::new();
    r.insert("key".to_string(), Value::String(key));
    r
}

fn text_or_null(v: Option<String>) -> Value {
    v.map_or(Value::Null, Value::String)
}

/// The `base_hash` of an override of a setting whose base is `v` (cb `setting_hash`).
fn setting_hash(v: &str) -> ApiResult<String> {
    bundle_hash(&Value::String(v.to_owned()))
        .map_err(|e| internal(format!("a setting value does not hash: {e}")))
}

/// Serialise with every other write of the job's design before reading what the write depends on (`design::track`
/// takes the same lock again, which is free in the same transaction).
async fn lock_job(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job)
        .execute(&mut *c)
        .await?;
    Ok(())
}

fn base_of<'b>(bases: &'b Bases, key: &str) -> Option<&'b str> {
    bases
        .iter()
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.as_deref())
}

// ---- settings -------------------------------------------------------------------------------------------------------

/// The base of every setting key: the settings of the bundle the job's DRAFT selection compiles to. No selection,
/// a selection that does not compile (422 `compile_failed`) or a template refusal (`is_template_refusal`) → all
/// null; any other error propagates (it has aborted the transaction).
pub(super) async fn draft_bases(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
) -> ApiResult<Bases> {
    let mut out: Bases = SETTING_KEYS.iter().map(|k| (*k, None)).collect();
    let stored: Option<Option<Value>> =
        sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
            .bind(job.id)
            .fetch_optional(&mut *c)
            .await?;
    let Some(sel) = stored.flatten() else {
        return Ok(out);
    };
    let sel: Selection = serde_json::from_value(sel)
        .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))?;
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_one(&mut *c)
        .await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    match TemplateCompiler::compile(compiler, &mut *c, &sel, &pins).await {
        Ok(compiled) => {
            for (k, v) in out.iter_mut() {
                *v = compiled.bundle.settings.get(k).map(str::to_owned);
            }
        }
        Err(CompileError::Api(e)) if !is_template_refusal(&e) => return Err(*e),
        Err(_) => {}
    }
    Ok(out)
}

/// The `JobSettingsView` (fixture `settingsView`), over the [`draft_bases`] the caller already computed.
async fn settings_view(c: &mut PgConnection, bases: Bases, job: &JobRef) -> ApiResult<Value> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT key, value FROM job_setting_override WHERE job_id = $1 ORDER BY key",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let overrides: Map<String, Value> = rows
        .into_iter()
        .map(|(k, v)| (k, text_or_null(v)))
        .collect();
    let mut base = Map::new();
    let mut effective = Map::new();
    for (k, b) in bases {
        let eff = match overrides.get(k) {
            Some(o) => o.clone(),
            None => text_or_null(b.clone()),
        };
        effective.insert(k.to_string(), eff);
        base.insert(k.to_string(), text_or_null(b));
    }
    let draft_rev = design::draft_rev(&mut *c, job).await?;
    Ok(json!({
        "base": base,
        "override": overrides,
        "effective": effective,
        "draft_rev": draft_rev,
    }))
}

async fn job_settings_view(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let bases = draft_bases(&mut tx, st.compiler.as_ref(), &jr).await?;
    let v = settings_view(&mut tx, bases, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// Refuse the whole patch before any write (fixture: every key is checked first).
fn check_patch(patch: &BTreeMap<String, Option<String>>) -> ApiResult<()> {
    for (k, v) in patch {
        if !SETTING_KEYS.contains(&k.as_str()) {
            return Err(ApiError::Unprocessable(
                "unknown_setting",
                format!("No setting “{k}”."),
            ));
        }
        if let Some(v) = v {
            if v.trim().is_empty() {
                return Err(ApiError::Unprocessable(
                    "invalid_body",
                    format!("{k} cannot be empty; use Hide or Revert."),
                ));
            }
            if v.encode_utf16().count() > MAX_SETTING_LEN {
                return Err(ApiError::Unprocessable(
                    "invalid_body",
                    format!("{k} is at most {MAX_SETTING_LEN} characters."),
                ));
            }
        }
    }
    Ok(())
}

async fn save_job_setting_override(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(patch): ApiJson<BTreeMap<String, Option<String>>>,
) -> ApiResult<Json<Value>> {
    check_patch(&patch)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    lock_job(&mut tx, jr.id).await?;
    // an override never changes the draft bundle, so these bases also serve the view below
    let bases = draft_bases(&mut tx, st.compiler.as_ref(), &jr).await?;
    for (k, v) in patch {
        let label = match v {
            None => format!("Setting {k} hidden"),
            Some(_) => format!("Setting {k} changed"),
        };
        let base_hash = base_of(&bases, &k).map(setting_hash).transpose()?;
        let value = v.map(|s| s.trim().to_string());
        let (job_id, key, by) = (jr.id, k.clone(), j.t.caller.sub.clone());
        let w = DraftWrite {
            area: "settings",
            page: "J13",
            kind: "setting",
            r#ref: key_ref(k),
            label: Label::Now(label),
        };
        design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO job_setting_override (job_id, key, value, base_hash, updated_by) \
                     VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (tenant_id, job_id, key) DO UPDATE \
                        SET value = EXCLUDED.value, base_hash = EXCLUDED.base_hash, \
                            updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
                )
                .bind(job_id)
                .bind(key)
                .bind(value)
                .bind(base_hash)
                .bind(by)
                .execute(&mut *c)
                .await?;
                Ok::<(), ApiError>(())
            })
        })
        .await?;
    }
    let v = settings_view(&mut tx, bases, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// Fixture `refineIn(…, 'revert', 'job_setting', key)`: drop the override (the release's value applies again), decide
/// the setting's open conflicts `reverted` and link them to the ledger row — `release::overlay::refine` does all of it.
async fn revert_job_setting_override(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    overlay::refine(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &j,
        Op::Revert,
        overlay::JOB_SETTING,
        &key,
    )
    .await?;
    let bases = draft_bases(&mut tx, st.compiler.as_ref(), &jr).await?;
    let v = settings_view(&mut tx, bases, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

// ---- languages ------------------------------------------------------------------------------------------------------

/// What Production serves (the live `languages` value), `[]` when none.
async fn live_languages(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Vec<String>> {
    let snap = design::value(
        &mut *c,
        reg,
        job,
        &DesignOf::Live,
        "languages",
        &DraftRef::new(),
    )
    .await?;
    match snap.filter(|v| !v.is_null()) {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| internal(format!("the live languages are unreadable: {e}"))),
        None => Ok(Vec::new()),
    }
}

/// Fixture `languageChange`: `+ de · − fr`, else `default x` when only the first language moved, else `unchanged`.
fn language_change(old: &[String], next: &[String]) -> String {
    let mut parts: Vec<String> = next
        .iter()
        .filter(|l| !old.contains(*l))
        .map(|l| format!("+ {l}"))
        .collect();
    parts.extend(
        old.iter()
            .filter(|l| !next.contains(*l))
            .map(|l| format!("− {l}")),
    );
    if parts.is_empty() && old.first() != next.first() {
        let first = next.first().map_or("undefined", String::as_str);
        parts.push(format!("default {first}"));
    }
    if parts.is_empty() {
        "unchanged".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Fixture `saveJobLanguages` input: trimmed, lowercased, first occurrence kept.
fn normalise_languages(languages: Vec<String>) -> ApiResult<Vec<String>> {
    let mut next: Vec<String> = Vec::new();
    for l in languages {
        let l = l.trim().to_lowercase();
        if !next.contains(&l) {
            next.push(l);
        }
    }
    if next.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            "A job serves at least one language.".into(),
        ));
    }
    let bad: Vec<&str> = next
        .iter()
        .map(String::as_str)
        .filter(|l| !LANGUAGES.iter().any(|(k, _)| k == l))
        .collect();
    if !bad.is_empty() {
        return Err(ApiError::Unprocessable(
            "unknown_language",
            format!("No platform language “{}”.", bad.join(", ")),
        ));
    }
    Ok(next)
}

/// The `JobLanguagesView` (fixture `languagesView`).
async fn languages_view(c: &mut PgConnection, reg: &Registry, job: &JobRef) -> ApiResult<Value> {
    let live = live_languages(&mut *c, reg, job).await?;
    let draft: Vec<String> = sqlx::query_scalar("SELECT languages FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_one(&mut *c)
        .await?;
    let mut options = Vec::with_capacity(LANGUAGES.len());
    for (key, label) in LANGUAGES {
        let knowledge = coverage::language_level(&mut *c, job, key).await?;
        options.push(json!({"key": key, "label": label, "knowledge": knowledge}));
    }
    Ok(json!({
        "unpublished": live != draft,
        "live": live,
        "draft": draft,
        "options": options,
    }))
}

async fn job_languages(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = languages_view(&mut tx, &st.design, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn save_job_languages(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<SaveLanguages>,
) -> ApiResult<Json<Value>> {
    let next = normalise_languages(b.languages)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    lock_job(&mut tx, jr.id).await?;
    let live = live_languages(&mut tx, &st.design, &jr).await?;
    let w = DraftWrite {
        area: "settings",
        page: "J13",
        kind: "languages",
        r#ref: DraftRef::new(),
        label: Label::Now(format!("Languages: {}", language_change(&live, &next))),
    };
    let job_id = jr.id;
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query("UPDATE jobs SET languages = $2 WHERE id = $1")
                .bind(job_id)
                .bind(next)
                .execute(&mut *c)
                .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let v = languages_view(&mut tx, &st.design, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn language_change_matches_the_fixture() {
        assert_eq!(
            language_change(&langs(&["en", "fr"]), &langs(&["en", "de"])),
            "+ de · − fr"
        );
        assert_eq!(
            language_change(&langs(&["en", "nl"]), &langs(&["nl", "en"])),
            "default nl"
        );
        assert_eq!(
            language_change(&langs(&["en"]), &langs(&["en"])),
            "unchanged"
        );
    }

    #[test]
    fn languages_are_normalised_then_checked() {
        assert_eq!(
            normalise_languages(langs(&[" DE ", "en", "de"])).unwrap(),
            langs(&["de", "en"])
        );
        assert!(matches!(
            normalise_languages(Vec::new()),
            Err(ApiError::Unprocessable("invalid_body", _))
        ));
        match normalise_languages(langs(&["xx", "de", "yy"])) {
            Err(ApiError::Unprocessable("unknown_language", m)) => {
                assert_eq!(m, "No platform language “xx, yy”.")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_patch_is_checked_before_any_write() {
        let patch = |k: &str, v: Option<&str>| {
            let mut p = BTreeMap::new();
            p.insert(k.to_string(), v.map(str::to_string));
            p
        };
        assert!(check_patch(&patch("actor", Some("care"))).is_ok());
        assert!(check_patch(&patch("actor", None)).is_ok());
        assert!(matches!(
            check_patch(&patch("tone", None)),
            Err(ApiError::Unprocessable("unknown_setting", _))
        ));
        assert!(matches!(
            check_patch(&patch("actor", Some("  "))),
            Err(ApiError::Unprocessable("invalid_body", _))
        ));
        let long = "x".repeat(MAX_SETTING_LEN + 1);
        assert!(matches!(
            check_patch(&patch("actor", Some(long.as_str()))),
            Err(ApiError::Unprocessable("invalid_body", _))
        ));
    }
}
