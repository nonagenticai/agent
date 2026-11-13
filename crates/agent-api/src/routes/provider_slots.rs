//! `/api/providers/slots`: the tenant's rag/ingest provider slots (plan B3 §5.1, T4; D69 draft, interim for W5
//! A1/A2 — G2). Shapes = `web/src/lib/api/types/tenant.ts` (`ProviderSlot`, `SlotInput`); refusals = the fixture
//! (`web/src/lib/api/areas/tenant.ts` `providerSlots` / `saveProviderSlot` / `acceptSlotDrift`).
//!
//! - `GET /api/providers/slots` (member) → `ProviderSlot[]` in slot order. Refreshes drift from both services'
//!   `GET /api/tenant/providers` first (crate::providers::slots); a service that is down or not provisioned only
//!   leaves its slots' drift as last observed, never a 5xx. The D70 internal tenant sees its platform kinds
//!   (`mlx_api`, `pgx_vllm`), configured from what strata serves it.
//! - `PUT /api/providers/slots/{slot}` `SlotInput` (integrations_manager) → `ProviderSlot`. 404 `slot_not_found`;
//!   422 `internal_uses_mlx_api`, `internal_provider_only`; 409 `VERSION_CONFLICT` (open drift, or a stale
//!   `expected_version`); 422 `unknown_kind`, `model_required`, `invalid_base_url`, `invalid_target`,
//!   `secret_required`, `slot_test_failed` (live probe: authenticated `GET {base}/models`, 5 s; the message names
//!   the host, never the key). D69: written to the draft (`design::track_tenant`, kind `provider_slot`), pushed to
//!   strata only when a job publishes it; the save itself makes no strata call.
//! - `POST /api/providers/slots/{slot}/adopt-remote` (integrations_manager) → `ProviderSlot`. 404; 409 `no_drift`.
//!   The registration another app wrote becomes the draft (its key stays in strata: `secret_id` null).
//! - `POST /api/providers/slots/{slot}/reset` `{expected_version}` (integrations_manager; F-08 "Use default", T4b)
//!   → `ProviderSlot` + `model_change_id`. 404 `slot_not_found`; 422 `internal_uses_mlx_api`; 409 `VERSION_CONFLICT`
//!   (open drift or stale version), `already_default` (default already, or never configured),
//!   `no_deployment_default` (`providers::kinds::Slot::deployment_default`). One transaction: the working row becomes
//!   `source = 'default'` (no kind, endpoint, model or key; version + 1) through `design::track_tenant` (D54), the
//!   jobs pinning the old model get a model change to the default (D93, `model_changes::on_slot_saved`, `to` =
//!   `None`), and outbox `connections_put` removes the tenant registration at once (no `api_key` anywhere; strata
//!   falls back to its deployment default). Audit `provider_slot.reset` `{slot, from_source}`.
//! - `POST /api/providers/slots/{slot}/test` `{kind, base_url, model, secret?}` (integrations_manager; F-09 "Test",
//!   T4c) → `{ok, at, detail, dims?}`. Validation as the save (404 `slot_not_found`; 422 `internal_uses_mlx_api`,
//!   `internal_provider_only`, `unknown_kind`, `model_required`, `invalid_base_url`, `invalid_target`); the key is the
//!   one typed, else the slot's stored key for the same host (a key strata keeps for an adopted drift: none is sent
//!   and strata uses its own for that host), else 422 `secret_required_for_new_host`. The slot's service probes it
//!   (`POST /api/tenant/providers/test`, dialect from crate::providers::kinds); a failing provider is `ok: false`,
//!   never a 5xx; the route not deployed (404) is 503 `slot_test_unavailable`; strata's 422 codes pass through with
//!   agent's messages. Stores nothing: no row, key, ledger entry, outbox row or audit row.
//! - `POST /api/providers/slots/{slot}/push` (integrations_manager; review I4) → `ProviderSlot` + `push_id`: queue again
//!   every published entry of the slot's service strata has not confirmed (a failed, dead or withheld push —
//!   crate::providers::slots::requeue); the drainer sends it. 404 `slot_not_found`; 422 `internal_uses_mlx_api`; 409
//!   `nothing_to_push`. Audit `provider_slot.push`.
//!
//! A save or reset fans a model change out to every job whose pinned generations run another model (D93,
//! crate::knowledge::model_changes::on_slot_saved). Only the embed model is pinned per store; the other slots go
//! tenant-wide when a job publishes them (D69, plan §0 R12).
//!
//! No transaction is held across the probe or a strata call.
use axum::extract::State;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::design::{self, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::netguard;
use crate::providers::kinds::{self, Slot};
use crate::providers::llm::{self, Kind};
use crate::providers::secret::{self, SecretKind};
use crate::providers::slots::{self, RemoteRow, SlotDesign, SlotRow};
use crate::providers::targets::{self, slot_ref};
use crate::providers::{UrlField, check_url};
use crate::state::AppState;
use crate::strata::{self, Service, ingest, rag};
use crate::tenancy::{self, Member, Requires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/providers/slots", get(list))
        .route("/api/providers/slots/{slot}", put(save))
        .route("/api/providers/slots/{slot}/adopt-remote", post(adopt))
        .route("/api/providers/slots/{slot}/reset", post(reset))
        .route("/api/providers/slots/{slot}/test", post(test_draft))
        .route("/api/providers/slots/{slot}/push", post(push))
}

/// The ledger area and owning page of a slot (fixture `trackTenant({area: 'providers', page: 'T6'})`).
const AREA: &str = "providers";
const PAGE: &str = "T6";
const MAX_URL_CHARS: usize = 2048;
const MAX_MODEL_CHARS: usize = 200;
const MAX_SECRET_CHARS: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotInput {
    kind: String,
    base_url: String,
    model: String,
    /// The key; absent, `null` or blank = keep the stored one (allowed only for the same kind and endpoint).
    #[serde(default)]
    secret: Option<String>,
    expected_version: i64,
}

/// D70: nonagentic's own tenant's slots are the platform's (mlx_api, pgx vLLM).
fn internal_uses_mlx_api() -> ApiError {
    ApiError::Unprocessable(
        "internal_uses_mlx_api",
        "nonagentic\u{2019}s own tenant runs on mlx_api and the pgx vLLM (D70); its providers are managed by the \
         platform."
            .into(),
    )
}

/// D70: the internal kinds serve nonagentic's own tenant only.
fn internal_provider_only(kind: &str) -> ApiError {
    ApiError::Unprocessable(
        "internal_provider_only",
        format!(
            "{} serves nonagentic\u{2019}s own tenant only. Pick an external provider and enter its URL and token.",
            kinds::kind_label(kind)
        ),
    )
}

/// The fixture's refusals before anything is written: open drift, then a stale version (both 409
/// `VERSION_CONFLICT`, nothing written).
fn check_current(
    slot: Slot,
    cur: Option<&SlotRow>,
    remote: Option<&RemoteRow>,
    expected: i64,
) -> ApiResult<()> {
    if let Some(r) = remote.filter(|r| r.drifted()) {
        return Err(ApiError::Conflict(
            "VERSION_CONFLICT",
            format!(
                "The {} slot was changed by another app ({}, v{}). Nothing was overwritten: load the current \
                 version first.",
                slot.label(),
                slots::ANOTHER_APP,
                r.pushed_version + 1
            ),
        ));
    }
    let version = i64::from(cur.map_or(0, |c| c.version));
    if expected != version {
        return Err(ApiError::Conflict(
            "VERSION_CONFLICT",
            format!(
                "The {} slot is at v{version}, you edited v{expected}. Nothing was overwritten.",
                slot.label()
            ),
        ));
    }
    Ok(())
}

async fn list(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let internal = {
        let mut tx = t.begin(&st.db).await?;
        let internal = llm::platform_owned(&mut tx, &st, t.tenant).await?;
        tx.commit().await?;
        internal
    };
    // no transaction is open across these calls
    let rag = slots::fetch_remote(&st, t.tenant, Service::Rag).await;
    let ing = slots::fetch_remote(&st, t.tenant, Service::Ingest).await;
    let view_of = |s: Slot| match s.service() {
        Service::Rag => rag.as_ref(),
        Service::Ingest => ing.as_ref(),
    };
    if internal {
        let out: Vec<Value> = Slot::ALL
            .into_iter()
            .map(|s| {
                slots::internal_json(
                    s,
                    view_of(s).and_then(|v| v.get(s.as_str())),
                    &st.cfg.internal_providers,
                )
            })
            .collect();
        return Ok(Json(Value::Array(out)));
    }
    let mut tx = t.begin(&st.db).await?;
    for s in Slot::ALL {
        if let Some(v) = view_of(s) {
            slots::record_observed(&mut tx, t.tenant, s, v.get(s.as_str())).await?;
        }
    }
    let out = slots::views(&mut tx, t.tenant).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(out)))
}

/// What a save writes, once checked.
struct Checked {
    kind: String,
    base_url: String,
    model: String,
    /// The key typed in this request.
    typed: Option<String>,
}

fn check_input(st: &AppState, tenant: Uuid, slot: Slot, i: &SlotInput) -> ApiResult<Checked> {
    let kind = i.kind.trim().to_string();
    if slot.dialect(&kind).is_none() {
        return Err(ApiError::Unprocessable(
            "unknown_kind",
            format!(
                "{} is not a {} kind for this tenant.",
                kinds::kind_label(&kind),
                slot.label()
            ),
        ));
    }
    let model = i.model.trim().to_string();
    if model.is_empty() {
        return Err(ApiError::Unprocessable(
            "model_required",
            "Name the model.".into(),
        ));
    }
    if model.chars().count() > MAX_MODEL_CHARS || model.contains('@') {
        return Err(ApiError::Unprocessable(
            "model_required",
            "A model name has at most 200 characters and no \u{201c}@\u{201d}.".into(),
        ));
    }
    let base_url = i.base_url.trim().trim_end_matches('/').to_string();
    if base_url.chars().count() > MAX_URL_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_base_url",
            "The URL is too long.".into(),
        ));
    }
    check_url(&base_url, st.connectors.policy(), tenant, UrlField::BaseUrl)?;
    let typed = i
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if typed
        .as_ref()
        .is_some_and(|s| s.chars().count() > MAX_SECRET_CHARS)
    {
        return Err(ApiError::Unprocessable(
            "secret_required",
            "The key is too long.".into(),
        ));
    }
    Ok(Checked {
        kind,
        base_url,
        model,
        typed,
    })
}

/// `saveProviderSlot` (D69): validated, live-probed, then written to the draft of every job.
async fn save(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(slot): ApiPath<String>,
    ApiJson(i): ApiJson<SlotInput>,
) -> ApiResult<Json<Value>> {
    let slot = slots::parse_slot(&slot)?;
    // 1. the fixture's refusals, read-only (nothing is held across the probe)
    let (c, cur_key, stored_key) = {
        let mut tx = t.begin(&st.db).await?;
        if llm::platform_owned(&mut tx, &st, t.tenant).await? {
            return Err(internal_uses_mlx_api());
        }
        if kinds::is_internal_kind(i.kind.trim()) {
            return Err(internal_provider_only(i.kind.trim()));
        }
        let cur = slots::row(&mut tx, t.tenant, slot, false).await?;
        let remote = slots::remote(&mut tx, t.tenant, slot).await?;
        check_current(slot, cur.as_ref(), remote.as_ref(), i.expected_version)?;
        let c = check_input(&st, t.tenant, slot, &i)?;
        let cur_design = cur.as_ref().and_then(SlotRow::design);
        let same_endpoint = cur_design.as_ref().is_some_and(|d| {
            d.kind == c.kind && d.base_url.as_deref() == Some(c.base_url.as_str())
        });
        let key_set = cur_design.as_ref().is_some_and(SlotDesign::key_set);
        if c.typed.is_none() && !(key_set && same_endpoint) {
            return Err(ApiError::Unprocessable(
                "secret_required",
                "Enter the key: a new kind or endpoint needs it, and keys are never reused.".into(),
            ));
        }
        let mut stored = None;
        if c.typed.is_some() {
            // 503 before the probe when keys cannot be sealed here
            secret::keyring(st.provider_keyring.as_deref())?;
        } else if let Some(d) = &cur_design
            && let Some(id) = d.secret_id
        {
            let sealed: String = sqlx::query_scalar(
                "SELECT key_sealed FROM provider_slot_secret WHERE tenant_id = $1 AND id = $2",
            )
            .bind(t.tenant)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            stored = Some(secret::open(
                st.provider_keyring.as_deref(),
                t.tenant,
                SecretKind::SlotKey,
                id,
                &sealed,
            )?);
        }
        tx.commit().await?;
        // the key kept when none is typed (same kind and endpoint)
        let cur_key = cur_design.map(|d| (d.secret_id, d.key_kept));
        (c, cur_key, stored)
    };
    // 2. the live probe, with the key that will be used; a key strata holds and agent never saw cannot be probed here
    let probe_key = c
        .typed
        .as_deref()
        .or_else(|| stored_key.as_ref().map(|k| k.expose()));
    if let Some(key) = probe_key {
        let note = if c.typed.is_some() {
            "token: the key you typed"
        } else {
            "token: the stored key"
        };
        let r = llm::probe_tenant(
            st.connectors.policy(),
            t.tenant,
            Kind::Openai,
            &c.base_url,
            Some(key),
            note,
        )
        .await?;
        if !r.ok {
            return Err(ApiError::Unprocessable(
                "slot_test_failed",
                format!(
                    "Live test failed: {} \u{2014} {}",
                    slots::host_of(&c.base_url),
                    r.detail
                ),
            ));
        }
    }
    // the ledger value never carries a secret (plan §1)
    let shown = json!({"kind": c.kind, "base_url": c.base_url, "model": c.model});
    if audit::scrub::check(&shown).is_err() {
        return Err(ApiError::Unprocessable(
            "secret_rejected",
            "The URL or model looks like a secret; keys go in the key field.".into(),
        ));
    }
    // 3. the draft write: lock order tenants → the slot row → every job (track_tenant)
    let mut tx = t.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let cur = slots::row(&mut tx, t.tenant, slot, true).await?;
    let remote = slots::remote(&mut tx, t.tenant, slot).await?;
    check_current(slot, cur.as_ref(), remote.as_ref(), i.expected_version)?;
    // B3 T4a (D83/D93): a new model proposes a model change to every job pinning the old one (before the row moves)
    let from_model = cur
        .as_ref()
        .and_then(SlotRow::design)
        .and_then(|d| d.model().map(str::to_string));
    crate::knowledge::model_changes::on_slot_saved(
        &mut tx,
        t.tenant,
        slot,
        from_model.as_deref(),
        Some(c.model.as_str()),
        &t.caller.sub,
    )
    .await?;
    let (secret_id, key_kept, key_detail) = match &c.typed {
        Some(k) => {
            let id = Uuid::new_v4();
            let sealed = secret::seal(
                st.provider_keyring.as_deref(),
                t.tenant,
                SecretKind::SlotKey,
                id,
                k,
            )?;
            sqlx::query(
                "INSERT INTO provider_slot_secret (id, slot, key_sealed, fingerprint, created_by) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(id)
            .bind(slot.as_str())
            .bind(&sealed.sealed)
            .bind(&sealed.fingerprint)
            .bind(&t.caller.sub)
            .execute(&mut *tx)
            .await?;
            (Some(id), false, "set")
        }
        None => match cur_key {
            Some((id, kept)) => (id, kept, "kept"),
            None => (None, false, "none"),
        },
    };
    let design = SlotDesign {
        kind: c.kind.clone(),
        base_url: Some(c.base_url.clone()),
        model: c.model.clone(),
        secret_id,
        key_kept,
        fingerprint: None,
        default: false,
    };
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "provider_slot",
        r#ref: slot_ref(slot),
        label: Label::Now(format!(
            "{} slot: {} \u{b7} {}",
            slot.label(),
            c.kind,
            c.model
        )),
    };
    let (tenant, by) = (t.tenant, t.caller.sub.clone());
    design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |conn| {
        Box::pin(async move { targets::write_slot(conn, tenant, slot, Some(&design), &by).await })
    })
    .await?;
    let view = slots::view(&mut tx, t.tenant, slot).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "provider_slot.save",
            "provider_slot",
            format!("{}:{}", slots::target_of(slot).as_str(), slot.as_str()),
        )
        .detail("version", view["version"].clone())
        .detail("kind", c.kind.as_str())
        .detail("key", key_detail)
        .detail("draft", true),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(view))
}

/// `acceptSlotDrift` (D69): the registration another app wrote becomes the draft; strata keeps its key.
async fn adopt(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(slot): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let slot = slots::parse_slot(&slot)?;
    let mut tx = t.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    // the slot row before the jobs (track_tenant): serialises with a concurrent save
    slots::row(&mut tx, t.tenant, slot, true).await?;
    let remote = slots::remote(&mut tx, t.tenant, slot)
        .await?
        .filter(RemoteRow::drifted)
        .ok_or_else(|| {
            ApiError::Conflict(
                "no_drift",
                format!("The {} slot has not drifted.", slot.label()),
            )
        })?;
    let adopted = slots::remote_design(slot, &remote);
    let label = format!(
        "{} slot: adopted v{} from {}",
        slot.label(),
        remote.pushed_version + 1,
        slots::ANOTHER_APP
    );
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "provider_slot",
        r#ref: slot_ref(slot),
        label: Label::Now(label),
    };
    let (tenant, by) = (t.tenant, t.caller.sub.clone());
    design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |conn| {
        Box::pin(
            async move { targets::write_slot(conn, tenant, slot, adopted.as_ref(), &by).await },
        )
    })
    .await?;
    slots::accept_observed(&mut tx, t.tenant, slot).await?;
    let view = slots::view(&mut tx, t.tenant, slot).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "provider_slot.adopt_remote",
            "provider_slot",
            format!("{}:{}", slots::target_of(slot).as_str(), slot.as_str()),
        )
        .detail("version", view["version"].clone())
        .detail("draft", true),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(view))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetInput {
    expected_version: i64,
}

/// `resetProviderSlot` (F-08, T4b): the slot uses the deployment default (module doc).
async fn reset(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(slot): ApiPath<String>,
    ApiJson(i): ApiJson<ResetInput>,
) -> ApiResult<Json<Value>> {
    let slot = slots::parse_slot(&slot)?;
    let mut tx = t.begin(&st.db).await?;
    if llm::platform_owned(&mut tx, &st, t.tenant).await? {
        return Err(internal_uses_mlx_api());
    }
    // lock order: tenants → the slot row → every job (track_tenant) → the push's enqueue lock
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let cur = slots::row(&mut tx, t.tenant, slot, true).await?;
    let remote = slots::remote(&mut tx, t.tenant, slot).await?;
    check_current(slot, cur.as_ref(), remote.as_ref(), i.expected_version)?;
    let Some(from) = cur
        .as_ref()
        .and_then(SlotRow::design)
        .filter(|d| !d.default)
    else {
        return Err(ApiError::Conflict(
            "already_default",
            format!(
                "The {} slot already uses the deployment default.",
                slot.label()
            ),
        ));
    };
    if slot.deployment_default().is_none() {
        return Err(ApiError::Conflict(
            "no_deployment_default",
            format!(
                "This platform deploys no default for the {} slot; keep a provider configured.",
                slot.label()
            ),
        ));
    }
    // D93: the jobs pinning the old model get a model change to the deployment default (before the row moves)
    let change = crate::knowledge::model_changes::on_slot_saved(
        &mut tx,
        t.tenant,
        slot,
        Some(from.model.as_str()),
        None,
        &t.caller.sub,
    )
    .await?;
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "provider_slot",
        r#ref: slot_ref(slot),
        label: Label::Now(format!("{} slot: deployment default", slot.label())),
    };
    let (tenant, by) = (t.tenant, t.caller.sub.clone());
    design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |conn| {
        Box::pin(async move {
            let d = SlotDesign::deployment_default();
            targets::write_slot(conn, tenant, slot, Some(&d), &by).await
        })
    })
    .await?;
    let version = slots::row(&mut tx, t.tenant, slot, false)
        .await?
        .map_or(0, |r| r.version);
    // pushed at once: the capability left out of the tenant set, no api_key (strata serves its default)
    slots::enqueue_push(
        &mut tx,
        t.tenant,
        slot,
        Some(&SlotDesign::deployment_default()),
        version,
        None,
    )
    .await?;
    let mut view = slots::view(&mut tx, t.tenant, slot).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "provider_slot.reset",
            "provider_slot",
            format!("{}:{}", slots::target_of(slot).as_str(), slot.as_str()),
        )
        .detail("slot", slot.as_str())
        .detail("from_source", "tenant")
        .detail("version", version),
    )
    .await?;
    tx.commit().await?;
    view["model_change_id"] = json!(change);
    Ok(Json(view))
}

/// `pushProviderSlot` (review I4): retry what strata has not confirmed for the slot's service (module doc).
async fn push(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(slot): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let slot = slots::parse_slot(&slot)?;
    let mut tx = t.begin(&st.db).await?;
    if llm::platform_owned(&mut tx, &st, t.tenant).await? {
        return Err(internal_uses_mlx_api());
    }
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let target = slots::target_of(slot);
    let id = slots::requeue(&mut tx, t.tenant, target)
        .await?
        .ok_or_else(|| {
            ApiError::Conflict(
                "nothing_to_push",
                format!(
                    "Everything published for {} is already in place; publish a job to push a saved slot.",
                    slot.service().name()
                ),
            )
        })?;
    let mut view = slots::view(&mut tx, t.tenant, slot).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "provider_slot.push",
            "provider_slot",
            format!("{}:{}", target.as_str(), slot.as_str()),
        )
        .detail("slot", slot.as_str())
        .detail("outbox_id", id.to_string()),
    )
    .await?;
    tx.commit().await?;
    view["push_id"] = json!(id);
    Ok(Json(view))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestInput {
    kind: String,
    base_url: String,
    model: String,
    /// The key to try; absent, `null` or blank = the slot's stored key, for the same host only.
    #[serde(default)]
    secret: Option<String>,
}

/// `testProviderSlotDraft` (F-09, T4c): probe the drawer's values through the slot's service; stores nothing
/// (module doc).
async fn test_draft(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(slot): ApiPath<String>,
    ApiJson(i): ApiJson<TestInput>,
) -> ApiResult<Json<Value>> {
    let slot = slots::parse_slot(&slot)?;
    // 1. read-only: the save's refusals, the key, the data-plane gate (nothing is held across the probe)
    let mut tx = t.begin(&st.db).await?;
    if llm::platform_owned(&mut tx, &st, t.tenant).await? {
        return Err(internal_uses_mlx_api());
    }
    if kinds::is_internal_kind(i.kind.trim()) {
        return Err(internal_provider_only(i.kind.trim()));
    }
    let c = check_input(
        &st,
        t.tenant,
        slot,
        &SlotInput {
            kind: i.kind,
            base_url: i.base_url,
            model: i.model,
            secret: i.secret,
            expected_version: 0,
        },
    )?;
    let cur = slots::row(&mut tx, t.tenant, slot, false)
        .await?
        .and_then(|r| r.design())
        .filter(|d| !d.default);
    let same_host = cur
        .as_ref()
        .filter(|d| d.base_url.as_deref() == Some(c.base_url.as_str()));
    let mut stored = None;
    if c.typed.is_none() {
        let Some(d) = same_host else {
            return Err(ApiError::Unprocessable(
                "secret_required_for_new_host",
                "Enter the key: a stored key is only sent to the host it was saved for.".into(),
            ));
        };
        // without a sealed key (a key strata keeps for an adopted drift, or none) no api_key is sent: strata uses
        // its own key for this host
        if let Some(id) = d.secret_id {
            let sealed: String = sqlx::query_scalar(
                "SELECT key_sealed FROM provider_slot_secret WHERE tenant_id = $1 AND id = $2",
            )
            .bind(t.tenant)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            stored = Some(secret::open(
                st.provider_keyring.as_deref(),
                t.tenant,
                SecretKind::SlotKey,
                id,
                &sealed,
            )?);
        }
    }
    let plane = strata::gate(&st, &mut tx, t.tenant, slot.service()).await?;
    tx.commit().await?;
    // 2. the probe, in strata; no transaction is open
    let key = c
        .typed
        .as_deref()
        .or_else(|| stored.as_ref().map(|k| k.expose()));
    let mut body = json!({
        "capability": slot.as_str(),
        "dialect": slot.dialect(&c.kind).unwrap_or_default(),
        "base_url": c.base_url,
        "model": c.model,
    });
    if let Some(k) = key {
        body["api_key"] = json!(k);
    }
    let answer = match slot.service() {
        Service::Rag => rag::providers_test(&st.http, &plane, body).await?,
        Service::Ingest => ingest::providers_test(&st.http, &plane, body).await?,
    };
    let Some(r) = answer else {
        return Err(ApiError::Unavailable(
            "slot_test_unavailable",
            format!(
                "{} cannot test a provider yet; save the slot to test it live.",
                slot.service().name()
            ),
            None,
        ));
    };
    let detail = r
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("the knowledge service gave no detail");
    let detail = key.map_or_else(|| detail.to_string(), |k| netguard::redact_text(detail, k));
    let mut out = json!({
        "ok": r.get("ok").and_then(Value::as_bool).unwrap_or(false),
        "at": Utc::now(),
        "detail": detail,
    });
    if let Some(d) = r.get("dims").filter(|d| !d.is_null()) {
        out["dims"] = d.clone();
    }
    Ok(Json(out))
}
