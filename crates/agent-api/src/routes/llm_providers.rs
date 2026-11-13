//! `/api/providers`: the tenant's LLM providers (D70, live data) and the uses bound to them (D69 design, kind
//! `llm_use`). Shapes = `web/src/lib/api/types/tenant.ts` (`LlmProviders`, `LlmProvider`, `LlmProviderInput`,
//! `LlmProviderTestInput`, `LlmProviderTestResult`); refusals = the fixture (`web/src/lib/api/areas/tenant.ts` T6) and
//! plan B3 §5.1. Ported from care-bus-agents@96c608c `routes/llm_providers.rs`.
//!
//! - `GET /api/providers` (member) → `LlmProviders`. The platform-owned tenant (D70 `internal`) sees one virtual,
//!   read-only `pgx vLLM (internal)` provider from config (no row) serving its OpenAI-compatible uses.
//! - `POST /api/providers` (integrations_manager) → 201 `LlmProvider`. 422 `internal_uses_mlx_api`,
//!   `invalid_provider`, `invalid_base_url`, `invalid_target`, `model_required`, `invalid_kind`; 409 `provider_exists`.
//! - `PUT /api/providers/{id}` (integrations_manager) → `LlmProvider`. As create + 404 `provider_not_found`; 409
//!   `provider_in_use` (a kind change while a use the new kind cannot serve binds it: in the draft, live in a job or
//!   in an approved, unapplied cut); 422 `secret_required_for_new_host`. A bound model that is no longer listed
//!   re-binds the use to `models[0]` as a draft (`design::track_tenant`, kind `llm_use`).
//! - `DELETE /api/providers/{id}` (integrations_manager) → 204: a SOFT delete (0050 `deleted_at`, the key dropped);
//!   the provider leaves the list and cannot be bound, and a snapshot that still names it resolves to 409
//!   `provider_deleted`. 404; 409 `provider_in_use` (bound in the draft, live in any job, or in an approved, unapplied
//!   cut: crate::providers::llm::holders, read with every job locked FOR SHARE); 422 `internal_uses_mlx_api`.
//! - The view's `live_problems` lists the live bindings that cannot serve (`provider_deleted`, a kind mismatch).
//! - `POST /api/providers/{id}/test` (integrations_manager) → `{ok, at, detail}`, stored as `last_test`.
//! - `POST /api/providers/test` (integrations_manager) → `{ok, at, detail, models}`; stores nothing (no row, no
//!   `last_test`, no audit).
//! - `PUT /api/providers/uses/{use}` `{provider_id, model, voice?}` (integrations_manager) → `LlmProviders`. D69: a
//!   draft (`design::track_tenant`), never live on save. 404 `use_not_found`, `provider_not_found`; 422
//!   `provider_kind_mismatch`, `kind_not_valid_for_use` (w2v on an LLM use, Laya on a speech use; B3 T4d),
//!   `unknown_model`, `voice_only_for_tts`, `invalid_voice`. Uses: `qualify`, `gates`, `generator`, and the speech uses
//!   `asr` / `tts` (D101/D102, crate::voice::ProviderSpeech; `w2v` or OpenAI-compatible providers).
//!
//! The key is write-only end to end: sealed (crate::providers::secret, AAD `<tenant>:llm_key:<id>`), never in a
//! response (`secret: {set, updated_at, fingerprint}`), an audit row (`key: set|none|kept|cleared|replaced`), a ledger
//! value or a log line. No transaction is held across a provider call.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use reqwest::Url;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::design::{self, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::providers::llm::{
    self, INTERNAL_PROVIDER_ID, INTERNAL_PROVIDER_NAME, INTERNAL_REGION, Kind, Lock, ProbeResult,
    ProviderRow, Use,
};
use crate::providers::secret::{self, SecretKind};
use crate::providers::targets::{self, use_ref};
use crate::providers::{UrlField, check_url};
use crate::state::AppState;
use crate::tenancy::{self, Member, Requires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/providers", get(list).post(create))
        .route("/api/providers/{id}", put(update).delete(remove))
        // static, so it never reaches `/api/providers/{id}` (which has no POST)
        .route("/api/providers/test", post(test_draft))
        .route("/api/providers/{id}/test", post(test_saved))
        .route("/api/providers/uses/{use}", put(bind))
}

/// The ledger area and owning page of a use binding (fixture `trackTenant({area: 'providers', page: 'T6'})`).
const AREA: &str = "providers";
const PAGE: &str = "T6";
const MAX_URL_CHARS: usize = 2048;
const MAX_SECRET_CHARS: usize = 4096;
const MAX_MODEL_CHARS: usize = 200;
const MAX_MODELS: usize = 100;
const MAX_REGION_CHARS: usize = 64;

/// `secret`: absent, `""` = keep the stored key; `null` = clear it; a string = replace it.
fn tri<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderInput {
    name: String,
    /// Absent: `openai` on create, the stored kind on update.
    #[serde(default)]
    kind: Option<String>,
    base_url: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default, deserialize_with = "tri")]
    secret: Option<Option<String>>,
    #[serde(default)]
    region: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestInput {
    kind: String,
    base_url: String,
    #[serde(default, deserialize_with = "tri")]
    secret: Option<Option<String>>,
    #[serde(default)]
    provider_id: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UseInput {
    #[serde(default)]
    provider_id: Option<Uuid>,
    #[serde(default)]
    model: Option<String>,
    /// `tts` only (B3 T4d): the voice the answer is spoken in; absent = the gateway's default.
    #[serde(default)]
    voice: Option<String>,
}

/// What an input does to the stored key.
enum SecretChange {
    Keep,
    Clear,
    Replace(String),
}

fn secret_change(s: &Option<Option<String>>) -> ApiResult<SecretChange> {
    Ok(match s {
        None => SecretChange::Keep,
        Some(None) => SecretChange::Clear,
        Some(Some(v)) => match v.trim() {
            "" => SecretChange::Keep,
            t if t.chars().count() > MAX_SECRET_CHARS => {
                return Err(invalid_provider("The key is too long."));
            }
            t => SecretChange::Replace(t.to_string()),
        },
    })
}

fn invalid_provider(msg: &str) -> ApiError {
    ApiError::Unprocessable("invalid_provider", msg.into())
}

fn invalid_kind() -> ApiError {
    ApiError::Unprocessable("invalid_kind", "A provider is openai, laya or w2v.".into())
}

fn provider_not_found() -> ApiError {
    ApiError::Missing("provider_not_found", "No provider with that id.".into())
}

fn provider_exists(name: &str) -> ApiError {
    ApiError::Conflict(
        "provider_exists",
        format!("A provider called \u{201c}{name}\u{201d} already exists."),
    )
}

fn new_host() -> ApiError {
    ApiError::Unprocessable(
        "secret_required_for_new_host",
        "Enter the key again: a stored key is only sent to the host it was saved for.".into(),
    )
}

/// D70: nonagentic's own tenant runs on mlx_api + the pgx vLLM; its providers are the platform's.
async fn refuse_internal(c: &mut PgConnection, st: &AppState, tenant: Uuid) -> ApiResult<()> {
    if llm::platform_owned(c, st, tenant).await? {
        return Err(ApiError::Unprocessable(
            "internal_uses_mlx_api",
            "nonagentic\u{2019}s own tenant runs on mlx_api and the pgx vLLM (D70); its providers are managed by \
             the platform."
                .into(),
        ));
    }
    Ok(())
}

/// A base URL without its trailing `/`, checked by netguard (https only, public host).
fn checked_base_url(st: &AppState, tenant: Uuid, raw: &str) -> ApiResult<String> {
    let base = raw.trim().trim_end_matches('/').to_string();
    if base.chars().count() > MAX_URL_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_base_url",
            "The URL is too long.".into(),
        ));
    }
    check_url(&base, st.connectors.policy(), tenant, UrlField::BaseUrl)?;
    Ok(base)
}

/// `(host, port)` of a stored or entered base URL: a stored key goes to that host only.
fn host_of(u: &str) -> Option<(String, Option<u16>)> {
    Url::parse(u).ok().map(|u| {
        (
            u.host_str().unwrap_or("").to_ascii_lowercase(),
            u.port_or_known_default(),
        )
    })
}

/// `host[:port]` of a base URL, for audit details (never the path: a key may be embedded in it).
fn audit_host(u: &str) -> String {
    match host_of(u) {
        Some((h, Some(p))) => format!("{h}:{p}"),
        Some((h, None)) => h,
        None => String::new(),
    }
}

/// A provider input, validated (fixture `providerInput`; the internal refusal and the name clash need the database).
struct Checked {
    name: String,
    kind: Option<Kind>,
    base_url: String,
    models: Vec<String>,
    region: Option<String>,
}

fn check(st: &AppState, tenant: Uuid, i: &ProviderInput) -> ApiResult<Checked> {
    let name = i.name.trim().to_string();
    if name.is_empty() {
        return Err(invalid_provider("Give the provider a name."));
    }
    if name.chars().count() > 120 {
        return Err(invalid_provider(
            "A provider name has at most 120 characters.",
        ));
    }
    let base_url = checked_base_url(st, tenant, &i.base_url)?;
    let mut models: Vec<String> = Vec::new();
    for m in i.models.iter().map(|m| m.trim()).filter(|m| !m.is_empty()) {
        if m.chars().count() > MAX_MODEL_CHARS {
            return Err(invalid_provider("A model name has at most 200 characters."));
        }
        if !models.iter().any(|x| x.as_str() == m) {
            models.push(m.to_string());
        }
    }
    if models.is_empty() {
        return Err(ApiError::Unprocessable(
            "model_required",
            "Name at least one model.".into(),
        ));
    }
    if models.len() > MAX_MODELS {
        return Err(invalid_provider("A provider lists at most 100 models."));
    }
    let kind = match i.kind.as_deref() {
        None => None,
        Some(k) => Some(Kind::parse(k.trim()).ok_or_else(invalid_kind)?),
    };
    let region = match i.region.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(r) if r.chars().count() > MAX_REGION_CHARS => {
            return Err(invalid_provider("A region has at most 64 characters."));
        }
        Some(r) => Some(r.to_string()),
    };
    Ok(Checked {
        name,
        kind,
        base_url,
        models,
        region,
    })
}

async fn name_taken(c: &mut PgConnection, name: &str, other_than: Option<Uuid>) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM llm_provider \
                         WHERE name = $1 AND id IS DISTINCT FROM $2 AND deleted_at IS NULL)",
    )
    .bind(name)
    .bind(other_than)
    .fetch_one(&mut *c)
    .await?)
}

/// 23505 on `(tenant_id, name)` of live providers (a concurrent create) is 409 `provider_exists`.
fn on_write(e: sqlx::Error, name: &str) -> ApiError {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => provider_exists(name),
        _ => e.into(),
    }
}

/// `LlmProvider` of a stored row: never the key, only `{set, updated_at, fingerprint}`.
fn provider_json(p: &ProviderRow) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "kind": p.kind,
        "base_url": p.base_url,
        "token_mode": "tenant_key",
        "secret": {
            "set": p.secret_sealed.is_some(),
            "updated_at": p.secret_updated_at,
            "fingerprint": p.secret_fpr,
        },
        "models": p.models,
        "updated_at": p.updated_at,
        "updated_by": p.updated_by,
        "last_test": p.last_test,
        "region": p.region,
    })
}

/// D70: the virtual internal provider of the platform-owned tenant (config, read-only, no row).
fn internal_json(st: &AppState) -> Value {
    let ip = &st.cfg.internal_providers;
    json!({
        "id": INTERNAL_PROVIDER_ID,
        "name": INTERNAL_PROVIDER_NAME,
        "kind": Kind::Openai.as_str(),
        "base_url": ip.llm_url,
        "token_mode": "internal",
        "secret": {"set": ip.llm_key.is_some(), "updated_at": null, "fingerprint": null},
        "models": [ip.llm_model],
        "updated_at": Utc::now(),
        "updated_by": "platform",
        "last_test": null,
        "region": INTERNAL_REGION,
    })
}

#[derive(sqlx::FromRow)]
struct Binding {
    use_name: String,
    provider_id: Option<Uuid>,
    model: Option<String>,
    voice: Option<String>,
}

async fn bindings(c: &mut PgConnection) -> ApiResult<Vec<Binding>> {
    Ok(sqlx::query_as(
        "SELECT \"use\" AS use_name, provider_id, model, voice FROM llm_use_binding ORDER BY \"use\"",
    )
    .fetch_all(&mut *c)
    .await?)
}

/// The draft bindings of `provider_id`.
async fn bound_to(c: &mut PgConnection, provider_id: Uuid) -> ApiResult<Vec<Binding>> {
    Ok(sqlx::query_as(
        "SELECT \"use\" AS use_name, provider_id, model, voice FROM llm_use_binding \
          WHERE provider_id = $1 ORDER BY \"use\"",
    )
    .bind(provider_id)
    .fetch_all(&mut *c)
    .await?)
}

fn use_label(u: &str) -> &str {
    match Use::parse(u) {
        Some(known) => known.label(),
        None => u,
    }
}

/// `LlmProviders` (fixture `llmView`): providers, every use (an unbound one with nulls) and the warning while
/// `qualify` resolves to no usable provider.
async fn llm_view(c: &mut PgConnection, st: &AppState, tenant: Uuid) -> ApiResult<Value> {
    if llm::platform_owned(&mut *c, st, tenant).await? {
        let model = &st.cfg.internal_providers.llm_model;
        let uses: Vec<Value> = Use::ALL
            .iter()
            .map(|u| {
                let internal = u.internal();
                json!({
                    "use": u.as_str(),
                    "label": u.label(),
                    "provider_id": internal.then_some(INTERNAL_PROVIDER_ID),
                    "model": internal.then(|| model.clone()),
                    "voice": null,
                })
            })
            .collect();
        return Ok(json!({
            "hosting": "internal",
            "providers": [internal_json(st)],
            "uses": uses,
            "warning": null,
            "live_problems": [],
        }));
    }
    let providers = llm::providers(&mut *c).await?;
    let bound = bindings(&mut *c).await?;
    // I1/I3: a live binding that cannot serve (its provider deleted, a kind mismatch) is shown, never silent
    let mut live_problems = Vec::new();
    for j in llm::jobs(&mut *c).await? {
        live_problems.extend(llm::live_problems(&mut *c, &st.design, &j).await?);
    }
    let uses: Vec<Value> = Use::ALL
        .iter()
        .map(|u| {
            let b = bound.iter().find(|b| b.use_name == u.as_str());
            json!({
                "use": u.as_str(),
                "label": u.label(),
                "provider_id": b.and_then(|b| b.provider_id),
                "model": b.and_then(|b| b.model.clone()),
                "voice": b.and_then(|b| b.voice.clone()),
            })
        })
        .collect();
    let qualify = bound
        .iter()
        .find(|b| b.use_name == Use::Qualify.as_str())
        .and_then(|b| b.provider_id)
        .and_then(|pid| providers.iter().find(|p| p.id == pid));
    // what an unusable `qualify` really does (llm::resolve, turn::drain): an unbound use runs on the platform's
    // inference-router under the tenant's own licence (503 `llm_inference_not_provisioned` while it has none); a
    // bound provider without a key is not answered (`TURN_FAILED`). There is no keyword fallback.
    let warning = match qualify {
        None => Value::Null,
        Some(p) if p.secret_sealed.is_none() => json!({
            "code": "key_required",
            "message": format!(
                "{}: key required. Until a key is stored, turns cannot be answered, every customer message gets \"the assistant is unavailable\".",
                p.name
            ),
        }),
        Some(_) => Value::Null,
    };
    Ok(json!({
        "hosting": "external",
        "providers": providers.iter().map(provider_json).collect::<Vec<_>>(),
        "uses": uses,
        "warning": warning,
        "live_problems": live_problems,
    }))
}

async fn list(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let view = llm_view(&mut tx, &st, t.tenant).await?;
    tx.commit().await?;
    Ok(Json(view))
}

async fn create(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<ProviderInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut tx = t.begin(&st.db).await?;
    refuse_internal(&mut tx, &st, t.tenant).await?;
    let c = check(&st, t.tenant, &input)?;
    if name_taken(&mut tx, &c.name, None).await? {
        return Err(provider_exists(&c.name));
    }
    let kind = c.kind.unwrap_or(Kind::Openai);
    let id = Uuid::new_v4();
    let sealed = match secret_change(&input.secret)? {
        SecretChange::Replace(s) => Some(secret::seal(
            st.provider_keyring.as_deref(),
            t.tenant,
            SecretKind::LlmKey,
            id,
            &s,
        )?),
        SecretChange::Keep | SecretChange::Clear => None,
    };
    sqlx::query(
        "INSERT INTO llm_provider (id, name, kind, base_url, models, region, secret_sealed, secret_fpr, \
                                   secret_updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CASE WHEN $7::text IS NULL THEN NULL ELSE clock_timestamp() END, $9)",
    )
    .bind(id)
    .bind(&c.name)
    .bind(kind.as_str())
    .bind(&c.base_url)
    .bind(&c.models)
    .bind(&c.region)
    .bind(sealed.as_ref().map(|s| s.sealed.as_str()))
    .bind(sealed.as_ref().map(|s| s.fingerprint.as_str()))
    .bind(&t.caller.sub)
    .execute(&mut *tx)
    .await
    .map_err(|e| on_write(e, &c.name))?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("provider.create", "llm_provider", id.to_string())
            .detail("host", audit_host(&c.base_url))
            .detail("kind", kind.as_str())
            .detail("key", if sealed.is_some() { "set" } else { "none" }),
    )
    .await?;
    let row = llm::provider(&mut tx, id, Lock::None)
        .await?
        .ok_or_else(provider_not_found)?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(provider_json(&row))))
}

async fn update(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<ProviderInput>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    // lock order: tenants → the provider row → every job (track_tenant)
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    refuse_internal(&mut tx, &st, t.tenant).await?;
    let cur = llm::provider(&mut tx, id, Lock::Update)
        .await?
        .ok_or_else(provider_not_found)?;
    let c = check(&st, t.tenant, &input)?;
    if name_taken(&mut tx, &c.name, Some(id)).await? {
        return Err(provider_exists(&c.name));
    }
    let kind = match c.kind {
        Some(k) => k,
        None => cur.kind()?,
    };
    let bound = bound_to(&mut tx, id).await?;
    if kind.as_str() != cur.kind {
        let cannot = |u: &str| Use::parse(u).is_none_or(|u| !u.accepts(kind));
        let blocked: Vec<&str> = bound
            .iter()
            .filter(|b| cannot(b.use_name.as_str()))
            .map(|b| b.use_name.as_str())
            .collect();
        if !blocked.is_empty() {
            return Err(ApiError::Conflict(
                "provider_in_use",
                format!(
                    "{} serves {}; unbind it before changing its kind.",
                    cur.name,
                    blocked.join(", ")
                ),
            ));
        }
        // I3: a job's Production (or an approved cut) that binds it for a use the new kind cannot serve. Every job
        // FOR UPDATE (track_tenant below takes the same locks), so no apply lands between this read and the commit.
        llm::lock_jobs(&mut tx, true).await?;
        let mut jobs: Vec<String> = Vec::new();
        let mut uses: Vec<String> = Vec::new();
        for h in llm::holders(&mut tx, &st.design, id).await? {
            if !cannot(h.use_name.as_str()) {
                continue;
            }
            if !jobs.contains(&h.job) {
                jobs.push(h.job);
            }
            if !uses.contains(&h.use_name) {
                uses.push(h.use_name);
            }
        }
        if !jobs.is_empty() {
            return Err(ApiError::Conflict(
                "provider_in_use",
                format!(
                    "{} still serves {} for {}: publish the unbind there before changing its kind.",
                    cur.name,
                    uses.join(", "),
                    jobs.join(", ")
                ),
            )
            .with("jobs", jobs));
        }
    }
    let change = secret_change(&input.secret)?;
    if matches!(change, SecretChange::Keep)
        && cur.secret_sealed.is_some()
        && host_of(&c.base_url) != host_of(&cur.base_url)
    {
        return Err(new_host());
    }
    let (sealed, fpr, key_detail) = match change {
        SecretChange::Keep => (cur.secret_sealed.clone(), cur.secret_fpr.clone(), "kept"),
        SecretChange::Clear => (None, None, "cleared"),
        SecretChange::Replace(s) => {
            let s = secret::seal(
                st.provider_keyring.as_deref(),
                t.tenant,
                SecretKind::LlmKey,
                id,
                &s,
            )?;
            (Some(s.sealed), Some(s.fingerprint), "replaced")
        }
    };
    let key_changed = key_detail != "kept";
    sqlx::query(
        "UPDATE llm_provider \
            SET name = $2, kind = $3, base_url = $4, models = $5, region = $6, \
                secret_sealed = $7, secret_fpr = $8, \
                secret_updated_at = CASE WHEN NOT $9 THEN secret_updated_at \
                                         WHEN $7::text IS NULL THEN NULL ELSE clock_timestamp() END, \
                last_test = CASE WHEN $9 OR base_url <> $4 OR kind <> $3 THEN NULL ELSE last_test END, \
                updated_at = clock_timestamp(), updated_by = $10 \
          WHERE id = $1",
    )
    .bind(id)
    .bind(&c.name)
    .bind(kind.as_str())
    .bind(&c.base_url)
    .bind(&c.models)
    .bind(&c.region)
    .bind(&sealed)
    .bind(&fpr)
    .bind(key_changed)
    .bind(&t.caller.sub)
    .execute(&mut *tx)
    .await
    .map_err(|e| on_write(e, &c.name))?;
    // a bound model that is no longer listed: the use re-binds to models[0], as a draft (fixture updateLlmProvider)
    for b in &bound {
        let Some(old) = b.model.as_deref() else {
            continue;
        };
        if c.models.iter().any(|m| m.as_str() == old) {
            continue;
        }
        let next = c.models[0].clone();
        let label = format!(
            "{}: {} \u{b7} {next} (model {old} removed)",
            use_label(&b.use_name),
            c.name
        );
        // the ledger `after` value never carries a secret (plan §1), as in bind
        let after = targets::binding_value(id, Some(next.as_str()), None, Some(c.name.as_str()));
        if audit::scrub::check(&after).is_err() {
            return Err(ApiError::Unprocessable(
                "secret_rejected",
                "The model or provider name looks like a secret.".into(),
            ));
        }
        let w = DraftWrite {
            area: AREA,
            page: PAGE,
            kind: "llm_use",
            r#ref: use_ref(&b.use_name),
            label: Label::Now(label),
        };
        let (tenant, use_name, by) = (t.tenant, b.use_name.clone(), t.caller.sub.clone());
        let voice: Option<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT voice FROM llm_use_binding WHERE \"use\" = $1",
        )
        .bind(&use_name)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |c| {
            Box::pin(async move {
                targets::upsert_binding(
                    c,
                    tenant,
                    &use_name,
                    id,
                    Some(next.as_str()),
                    voice.as_deref(),
                    &by,
                )
                .await
            })
        })
        .await?;
    }
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("provider.update", "llm_provider", id.to_string())
            .detail("host", audit_host(&c.base_url))
            .detail("kind", kind.as_str())
            .detail("key", key_detail),
    )
    .await?;
    let row = llm::provider(&mut tx, id, Lock::None)
        .await?
        .ok_or_else(provider_not_found)?;
    tx.commit().await?;
    Ok(Json(provider_json(&row)))
}

async fn remove(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = t.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    refuse_internal(&mut tx, &st, t.tenant).await?;
    // FOR UPDATE before reading the bindings: a concurrent bind holds the row FOR KEY SHARE (see 0050); then every
    // job FOR SHARE, so no apply (which locks its job FOR UPDATE) makes it live between the reads and the commit
    let p = llm::provider(&mut tx, id, Lock::Update)
        .await?
        .ok_or_else(provider_not_found)?;
    llm::lock_jobs(&mut tx, false).await?;
    let bound = bound_to(&mut tx, id).await?;
    if !bound.is_empty() {
        let uses: Vec<&str> = bound.iter().map(|b| b.use_name.as_str()).collect();
        return Err(ApiError::Conflict(
            "provider_in_use",
            format!(
                "{} is bound to {}; unbind it first.",
                p.name,
                uses.join(", ")
            ),
        ));
    }
    // D69: an unbind is a draft; a job's Production keeps calling the provider until that job publishes it, and an
    // approved cut that binds it would make it live again when applied
    let mut jobs: Vec<String> = Vec::new();
    let mut pending = false;
    for h in llm::holders(&mut tx, &st.design, id).await? {
        pending |= !h.live;
        if !jobs.contains(&h.job) {
            jobs.push(h.job);
        }
    }
    if !jobs.is_empty() {
        let what = if pending {
            "is still live, or bound by an approved release, for"
        } else {
            "is still live for"
        };
        return Err(ApiError::Conflict(
            "provider_in_use",
            format!(
                "{} {what} {}: publish the unbind there first.",
                p.name,
                jobs.join(", ")
            ),
        )
        .with("jobs", jobs));
    }
    // soft delete (0050): a snapshot that still names it resolves to provider_deleted; the key goes now
    sqlx::query(
        "UPDATE llm_provider \
            SET deleted_at = clock_timestamp(), secret_sealed = NULL, secret_fpr = NULL, secret_updated_at = NULL, \
                last_test = NULL, updated_at = clock_timestamp(), updated_by = $2 \
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(&t.caller.sub)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("provider.delete", "llm_provider", id.to_string())
            .detail("kind", p.kind.as_str()),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn saved_result(r: &ProbeResult) -> Value {
    json!({"ok": r.ok, "at": r.at, "detail": r.detail})
}

/// `testLlmProvider`: probe a stored provider with its stored key; the result is its `last_test`. A provider failure
/// (401, timeout, no key) is `ok: false`, never a 5xx.
async fn test_saved(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let (internal, row) = {
        let mut tx = t.begin(&st.db).await?;
        let internal = llm::platform_owned(&mut tx, &st, t.tenant).await?;
        let row = if internal {
            None
        } else {
            llm::provider(&mut tx, id, Lock::None).await?
        };
        tx.commit().await?;
        (internal, row)
    };
    let result = if internal {
        if id != INTERNAL_PROVIDER_ID {
            return Err(provider_not_found());
        }
        llm::probe_internal(&st).await
    } else {
        let p = row.ok_or_else(provider_not_found)?;
        let kind = p.kind()?;
        match llm::open_key(st.provider_keyring.as_deref(), t.tenant, &p)? {
            None => ProbeResult::failed("422 key_required: store the provider key first."),
            Some(key) => {
                let note = format!(
                    "token: stored key {}",
                    p.secret_fpr.as_deref().unwrap_or("")
                );
                llm::probe_tenant(
                    st.connectors.policy(),
                    t.tenant,
                    kind,
                    &p.base_url,
                    Some(key.expose()),
                    &note,
                )
                .await?
            }
        }
    };
    let out = saved_result(&result);
    let mut tx = t.begin(&st.db).await?;
    if !internal {
        // the row may have been deleted meanwhile: then there is nothing to store
        sqlx::query("UPDATE llm_provider SET last_test = $2 WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .bind(&out)
            .execute(&mut *tx)
            .await?;
    }
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("provider.test", "llm_provider", id.to_string())
            .detail("ok", result.ok)
            .detail("internal", internal)
            .detail("models", result.models.len()),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `testLlmProviderDraft`: probe the drawer's values. Stores nothing: no provider, key, `last_test` or audit row.
/// The key is the one typed; else, with `provider_id` and no explicit `secret: null`, that provider's stored key,
/// but only for its stored host (422 `secret_required_for_new_host`); else none.
async fn test_draft(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiJson(i): ApiJson<TestInput>,
) -> ApiResult<Json<Value>> {
    let kind = Kind::parse(i.kind.trim()).ok_or_else(invalid_kind)?;
    let base = checked_base_url(&st, t.tenant, &i.base_url)?;
    let typed = match secret_change(&i.secret)? {
        SecretChange::Replace(s) => Some(s),
        SecretChange::Keep | SecretChange::Clear => None,
    };
    let explicit_none = matches!(i.secret, Some(None));
    let (key, note) = {
        let mut tx = t.begin(&st.db).await?;
        refuse_internal(&mut tx, &st, t.tenant).await?;
        let picked = if let Some(k) = typed {
            (Some(k), "token: the key you typed".to_string())
        } else if !explicit_none && let Some(pid) = i.provider_id {
            let p = llm::provider(&mut tx, pid, Lock::None)
                .await?
                .ok_or_else(provider_not_found)?;
            if host_of(&p.base_url) != host_of(&base) {
                return Err(new_host());
            }
            match llm::open_key(st.provider_keyring.as_deref(), t.tenant, &p)? {
                Some(k) => (
                    Some(k.expose().to_string()),
                    format!(
                        "token: stored key {}",
                        p.secret_fpr.as_deref().unwrap_or("")
                    ),
                ),
                None => (None, "no key sent".to_string()),
            }
        } else {
            (None, "no key sent".to_string())
        };
        tx.commit().await?;
        picked
    };
    let mut r = llm::probe_tenant(
        st.connectors.policy(),
        t.tenant,
        kind,
        &base,
        key.as_deref(),
        &note,
    )
    .await?;
    r.detail.push_str(". Nothing was saved.");
    Ok(Json(json!({
        "ok": r.ok,
        "at": r.at,
        "detail": r.detail,
        "models": r.models,
    })))
}

/// `bindLlmUse`: D69 — which provider serves a use is design: the working row is written inside
/// `design::track_tenant` (ledger `llm_use` in every job of the tenant, `draft_rev` bumped by the engine), live per
/// job when that job publishes. Answers `LlmProviders`.
async fn bind(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(use_name): ApiPath<String>,
    ApiJson(i): ApiJson<UseInput>,
) -> ApiResult<Json<Value>> {
    let u = Use::parse(&use_name)
        .ok_or_else(|| ApiError::Missing("use_not_found", "No such LLM use.".into()))?;
    // B3 T4d: only a `tts` binding carries a voice
    let voice = match i.voice.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(_) if u != Use::Tts => {
            return Err(ApiError::Unprocessable(
                "voice_only_for_tts",
                "Only text to speech (tts) takes a voice.".into(),
            ));
        }
        Some(v) if !crate::voice::valid_voice(v) => {
            return Err(ApiError::Unprocessable(
                "invalid_voice",
                format!(
                    "A voice is at most {} characters of [A-Za-z0-9_-].",
                    crate::voice::routes::MAX_VOICE
                ),
            ));
        }
        Some(v) => Some(v.to_string()),
    };
    let mut tx = t.begin(&st.db).await?;
    // lock order: tenants → the provider row (FOR KEY SHARE: a concurrent delete waits) → every job (track_tenant)
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let target = match i.provider_id {
        None => None,
        Some(pid) => {
            let p = llm::provider(&mut tx, pid, Lock::KeyShare)
                .await?
                .ok_or_else(provider_not_found)?;
            if let Some((code, why)) = u.refusal(p.kind()?) {
                return Err(ApiError::Unprocessable(code, why));
            }
            let model = i.model.as_deref().map(str::trim).unwrap_or("");
            if !p.models.iter().any(|m| m.as_str() == model) {
                let shown = if model.is_empty() { "(none)" } else { model };
                return Err(ApiError::Unprocessable(
                    "unknown_model",
                    format!("{} does not list the model {shown}.", p.name),
                ));
            }
            Some((pid, model.to_string(), p.name))
        }
    };
    let label = match &target {
        Some((_, model, name)) => format!("{}: {name} \u{b7} {model}", u.label()),
        None => format!("{}: unbound", u.label()),
    };
    // the ledger `after` value never carries a secret (plan §1)
    if let Some((pid, model, name)) = &target {
        let after = targets::binding_value(
            *pid,
            Some(model.as_str()),
            voice.as_deref(),
            Some(name.as_str()),
        );
        if audit::scrub::check(&after).is_err() {
            return Err(ApiError::Unprocessable(
                "secret_rejected",
                "The model or provider name looks like a secret.".into(),
            ));
        }
    }
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "llm_use",
        r#ref: use_ref(u.as_str()),
        label: Label::Now(label),
    };
    let (tenant, use_str, by) = (t.tenant, u.as_str(), t.caller.sub.clone());
    let write = target
        .as_ref()
        .map(|(pid, model, _)| (*pid, model.clone(), voice.clone()));
    design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |c| {
        Box::pin(async move {
            match write {
                Some((pid, model, voice)) => {
                    targets::upsert_binding(
                        c,
                        tenant,
                        use_str,
                        pid,
                        Some(model.as_str()),
                        voice.as_deref(),
                        &by,
                    )
                    .await
                }
                None => targets::delete_binding(c, tenant, use_str).await,
            }
        })
    })
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("provider.bind", "llm_use", u.as_str())
            .detail(
                "provider_id",
                target.as_ref().map(|(pid, _, _)| pid.to_string()),
            )
            .detail("model", target.as_ref().map(|(_, m, _)| m.clone()))
            .detail("voice", target.as_ref().and(voice.clone()))
            .detail("draft", true),
    )
    .await?;
    let view = llm_view(&mut tx, &st, t.tenant).await?;
    tx.commit().await?;
    Ok(Json(view))
}
