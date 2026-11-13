//! Provider slots (plan B3 T4, D69 draft, interim for W5 A1/A2 — G2): the tenant's rag/ingest providers
//! (`embed`, `rerank`, `chat` in strata_rag; `ocr`, `vision`, `transcribe` in strata_ingest).
//!
//! - The working row (`provider_slot`, migration 0051) is the draft of the tenant-wide ledger kind `provider_slot`
//!   (crate::providers::targets): a save writes it through `design::track_tenant`, never strata.
//! - When a job publishes a release that bundles a slot change, the kind's `published` hook queues outbox op
//!   `connections_put` ([`enqueue_push`]); the sender (crate::provision::senders::connections) replaces the
//!   service's whole provider set in strata with the published value and records what it pushed ([`record_pushed`]).
//! - Drift (G2): `GET /api/tenant/providers` of each service is fingerprinted ([`remote_fpr`]) and kept in
//!   `provider_slot_remote` ([`record_observed`]); a slot whose observed fingerprint differs from the pushed one has
//!   drifted (`changed_by: "another app"`, `remote_version = pushed_version + 1`, synthetic until A2).
//! - A push carries every published entry of the service strata has not confirmed yet (`unsent`, review I2): the
//!   pending row's, an in-flight row's, failed / dead rows' and the entries the last successful push withheld; an entry
//!   strata confirmed is kept as strata reports it (a later drift is never overwritten). One pending row per service:
//!   a new publish UPDATEs its payload (crate::provision::enqueue).
//! - A withheld embed entry (crate::provision::senders::connections) shows on the embed slot (`withheld`, [`withheld`])
//!   and as a readiness warning; [`requeue_withheld`] queues it again (a published or accepted model change, the
//!   model-change sweeper, `POST /api/providers/slots/{slot}/push`).
//!
//! Keys: sealed in `provider_slot_secret` (crate::providers::secret, AAD `<tenant>:slot_key:<id>`); a value, a view,
//! an outbox payload and an audit row carry `{set, fingerprint}` or the secret row id only.
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use super::kinds::{self, Slot};
use crate::audit::Actor;
use crate::error::{ApiError, ApiResult};
use crate::provision::{self, NewRow, Target};
use crate::state::AppState;
use crate::strata::{self, Service, ingest, rag};

/// The outbox op that pushes slots to strata (plan §0 R5).
pub const OP: &str = "connections_put";
/// The fingerprint of "no tenant registration" (strata `source` `default` / `unconfigured`).
pub const NONE_FPR: &str = "none";
/// Who queues a push when a job publishes (the audit actor of `provision.enqueued`).
pub const PUBLISHER: &str = "service:agent-api-release";
/// `changed_by` of a drift until W5 A2 names the writer.
pub const ANOTHER_APP: &str = "another app";

/// The design value of a configured slot (the fixture's `SlotDesign` + `secret_id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotDesign {
    pub kind: String,
    pub base_url: Option<String>,
    pub model: String,
    /// The `provider_slot_secret` row whose key the slot uses.
    pub secret_id: Option<Uuid>,
    /// strata holds a key agent never saw (an adopted drift): pushes send no `api_key` and strata keeps its own.
    pub key_kept: bool,
    /// The fingerprint of `secret_id`'s key (read from the secret row; never written from a value).
    pub fingerprint: Option<String>,
    /// The slot uses the deployment default (F-08 "Use default", T4b; 0057 `source = 'default'`): no kind, endpoint,
    /// model or key of the tenant's (`kind` / `model` are empty).
    pub default: bool,
}

impl SlotDesign {
    /// The value of a slot reset to the deployment default.
    pub fn deployment_default() -> Self {
        SlotDesign {
            kind: String::new(),
            base_url: None,
            model: String::new(),
            secret_id: None,
            key_kept: false,
            fingerprint: None,
            default: true,
        }
    }

    pub fn key_set(&self) -> bool {
        self.secret_id.is_some() || self.key_kept
    }

    /// The tenant's own model, `None` for the deployment default.
    pub fn model(&self) -> Option<&str> {
        (!self.default).then_some(self.model.as_str())
    }

    /// `{configured, kind, base_url, model, secret: {set, fingerprint}, secret_id}`; the deployment default is
    /// `{configured: true, source: "default"}` with every registration field null.
    pub fn to_value(&self) -> Value {
        if self.default {
            return json!({
                "configured": true,
                "source": "default",
                "kind": null,
                "base_url": null,
                "model": null,
                "secret": {"set": false, "fingerprint": null},
                "secret_id": null,
            });
        }
        json!({
            "configured": true,
            "kind": self.kind,
            "base_url": self.base_url,
            "model": self.model,
            "secret": {"set": self.key_set(), "fingerprint": self.fingerprint},
            "secret_id": self.secret_id.map(|u| u.to_string()),
        })
    }

    /// The inverse of [`SlotDesign::to_value`]; `null`, an absent value or `configured: false` is `None` (nothing at
    /// the place). A value of another shape is a corrupt snapshot (500).
    pub fn from_value(v: Option<&Value>) -> ApiResult<Option<Self>> {
        let Some(v) = v.filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        if v.get("configured").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        if v.get("source").and_then(Value::as_str) == Some("default") {
            return Ok(Some(SlotDesign::deployment_default()));
        }
        let bad = |f: &str| internal(format!("provider_slot value: {f} is missing or not valid"));
        let text = |f: &str| v.get(f).and_then(Value::as_str).map(str::to_string);
        let kind = text("kind").ok_or_else(|| bad("kind"))?;
        let model = text("model").ok_or_else(|| bad("model"))?;
        let base_url = match v.get("base_url") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(bad("base_url")),
        };
        let secret_id = match v.get("secret_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(Uuid::parse_str(s).map_err(|_| bad("secret_id"))?),
            Some(_) => return Err(bad("secret_id")),
        };
        let set = v
            .pointer("/secret/set")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let fingerprint = v
            .pointer("/secret/fingerprint")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Some(SlotDesign {
            kind,
            base_url,
            model,
            key_kept: set && secret_id.is_none(),
            secret_id,
            fingerprint,
            default: false,
        }))
    }
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// 404 `slot_not_found`.
pub fn parse_slot(s: &str) -> ApiResult<Slot> {
    Slot::parse(s)
        .ok_or_else(|| ApiError::Missing("slot_not_found", "No such provider slot.".into()))
}

/// The service name stored in `provider_slot.service` / an outbox row's target.
pub fn target_of(slot: Slot) -> Target {
    slot.service().target()
}

/// A working row with its key's fingerprint.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct SlotRow {
    pub slot: String,
    pub configured: bool,
    pub kind: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub region: Option<String>,
    pub version: i32,
    pub secret_id: Option<Uuid>,
    pub key_kept: bool,
    pub fingerprint: Option<String>,
    pub secret_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    /// `tenant` | `default` (0057).
    pub source: String,
}

impl SlotRow {
    /// The slot uses the deployment default (F-08).
    pub fn is_default(&self) -> bool {
        self.source == "default"
    }

    /// The design value (`None` while not configured).
    pub fn design(&self) -> Option<SlotDesign> {
        if !self.configured {
            return None;
        }
        if self.is_default() {
            return Some(SlotDesign::deployment_default());
        }
        Some(SlotDesign {
            kind: self.kind.clone().unwrap_or_default(),
            base_url: self.base_url.clone(),
            model: self.model.clone().unwrap_or_default(),
            secret_id: self.secret_id,
            key_kept: self.key_kept,
            fingerprint: self.fingerprint.clone(),
            default: false,
        })
    }
}

/// The working row of `slot`; `lock` takes it `FOR UPDATE`.
pub async fn row(
    c: &mut PgConnection,
    tenant: Uuid,
    slot: Slot,
    lock: bool,
) -> ApiResult<Option<SlotRow>> {
    let q = if lock {
        "SELECT s.slot, s.configured, s.kind, s.base_url, s.model, s.region, s.version, s.secret_id, s.key_kept, \
                k.fingerprint, k.created_at AS secret_at, s.updated_at, s.source \
           FROM provider_slot s \
           LEFT JOIN provider_slot_secret k ON k.tenant_id = s.tenant_id AND k.id = s.secret_id \
          WHERE s.tenant_id = $1 AND s.slot = $2 \
            FOR UPDATE OF s"
    } else {
        "SELECT s.slot, s.configured, s.kind, s.base_url, s.model, s.region, s.version, s.secret_id, s.key_kept, \
                k.fingerprint, k.created_at AS secret_at, s.updated_at, s.source \
           FROM provider_slot s \
           LEFT JOIN provider_slot_secret k ON k.tenant_id = s.tenant_id AND k.id = s.secret_id \
          WHERE s.tenant_id = $1 AND s.slot = $2"
    };
    Ok(sqlx::query_as(q)
        .bind(tenant)
        .bind(slot.as_str())
        .fetch_optional(&mut *c)
        .await?)
}

async fn rows(c: &mut PgConnection, tenant: Uuid) -> ApiResult<Vec<SlotRow>> {
    Ok(sqlx::query_as(
        "SELECT s.slot, s.configured, s.kind, s.base_url, s.model, s.region, s.version, s.secret_id, s.key_kept, \
                k.fingerprint, k.created_at AS secret_at, s.updated_at, s.source \
           FROM provider_slot s \
           LEFT JOIN provider_slot_secret k ON k.tenant_id = s.tenant_id AND k.id = s.secret_id \
          WHERE s.tenant_id = $1",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?)
}

/// What agent last pushed to strata for a slot and what strata last reported.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RemoteRow {
    pub slot: String,
    pub pushed_version: i32,
    pub pushed_fpr: String,
    pub pushed_at: Option<DateTime<Utc>>,
    pub observed_fpr: Option<String>,
    pub observed: Option<Value>,
    pub observed_at: Option<DateTime<Utc>>,
}

impl RemoteRow {
    /// strata reports something other than what agent pushed (G2).
    pub fn drifted(&self) -> bool {
        self.observed_fpr
            .as_deref()
            .is_some_and(|o| o != self.pushed_fpr)
    }
}

/// The remote row of `slot`.
pub async fn remote(
    c: &mut PgConnection,
    tenant: Uuid,
    slot: Slot,
) -> ApiResult<Option<RemoteRow>> {
    Ok(sqlx::query_as(
        "SELECT slot, pushed_version, pushed_fpr, pushed_at, observed_fpr, observed, observed_at \
           FROM provider_slot_remote WHERE tenant_id = $1 AND slot = $2",
    )
    .bind(tenant)
    .bind(slot.as_str())
    .fetch_optional(&mut *c)
    .await?)
}

async fn remotes(c: &mut PgConnection, tenant: Uuid) -> ApiResult<Vec<RemoteRow>> {
    Ok(sqlx::query_as(
        "SELECT slot, pushed_version, pushed_fpr, pushed_at, observed_fpr, observed, observed_at \
           FROM provider_slot_remote WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?)
}

/// A strata registration's fingerprint: SHA-256 of `base_url|dialect|model|has_api_key` (first 16 hex), or
/// [`NONE_FPR`] when the capability is not a tenant registration (`default`, `unconfigured`, absent).
pub fn remote_fpr(cap: Option<&Value>) -> String {
    let Some(c) = cap.filter(|c| c.get("source").and_then(Value::as_str) == Some("tenant")) else {
        return NONE_FPR.to_string();
    };
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let base = s("base_url");
    let has_key = c
        .get("has_api_key")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut h = Sha256::new();
    h.update(
        format!(
            "{}|{}|{}|{has_key}",
            base.trim_end_matches('/'),
            s("dialect"),
            s("model")
        )
        .as_bytes(),
    );
    let mut hex = hex::encode(h.finalize());
    hex.truncate(16);
    hex
}

/// What is kept of a reported capability: where it comes from and the registration (strata never reports a key).
pub fn observed_of(cap: Option<&Value>) -> Value {
    let Some(c) = cap else {
        return json!({"source": "unconfigured"});
    };
    let mut out = serde_json::Map::new();
    for k in ["source", "base_url", "dialect", "model", "has_api_key"] {
        if let Some(v) = c.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// Record what strata reports for `slot` (`cap` = the capability object of its service's view). `observed_at` moves
/// only when the fingerprint changes.
pub async fn record_observed(
    c: &mut PgConnection,
    tenant: Uuid,
    slot: Slot,
    cap: Option<&Value>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO provider_slot_remote (tenant_id, slot, observed_fpr, observed, observed_at) \
         VALUES ($1, $2, $3, $4, clock_timestamp()) \
         ON CONFLICT (tenant_id, slot) DO UPDATE \
            SET observed = EXCLUDED.observed, \
                observed_at = CASE WHEN provider_slot_remote.observed_fpr IS NOT DISTINCT FROM EXCLUDED.observed_fpr \
                                   THEN provider_slot_remote.observed_at ELSE EXCLUDED.observed_at END, \
                observed_fpr = EXCLUDED.observed_fpr",
    )
    .bind(tenant)
    .bind(slot.as_str())
    .bind(remote_fpr(cap))
    .bind(observed_of(cap))
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Record a successful push of `entry`: `cap` = the capability as strata stored it (the PUT's answer).
pub async fn record_pushed(
    c: &mut PgConnection,
    tenant: Uuid,
    entry: &PushSlot,
    cap: Option<&Value>,
) -> ApiResult<()> {
    let Some(slot) = Slot::parse(&entry.slot) else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO provider_slot_remote (tenant_id, slot, pushed_version, pushed_fpr, pushed_at, pushed_release, \
                                           observed_fpr, observed, observed_at, pushed_key_ref) \
         VALUES ($1, $2, $3, $4, clock_timestamp(), $5, $4, $6, clock_timestamp(), $7) \
         ON CONFLICT (tenant_id, slot) DO UPDATE \
            SET pushed_version = EXCLUDED.pushed_version, pushed_fpr = EXCLUDED.pushed_fpr, \
                pushed_at = EXCLUDED.pushed_at, pushed_release = EXCLUDED.pushed_release, \
                observed_fpr = EXCLUDED.observed_fpr, observed = EXCLUDED.observed, \
                observed_at = EXCLUDED.observed_at, pushed_key_ref = EXCLUDED.pushed_key_ref",
    )
    .bind(tenant)
    .bind(slot.as_str())
    .bind(entry.version)
    .bind(remote_fpr(cap))
    .bind(&entry.release)
    .bind(observed_of(cap))
    .bind(entry.key_ref.filter(|_| entry.configured))
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// After adopting a drift: what strata holds is what agent now has; `pushed_version` takes the synthetic remote
/// version. Agent never saw the key strata holds now: no `pushed_key_ref`.
pub async fn accept_observed(c: &mut PgConnection, tenant: Uuid, slot: Slot) -> ApiResult<()> {
    sqlx::query(
        "UPDATE provider_slot_remote \
            SET pushed_fpr = observed_fpr, pushed_version = pushed_version + 1, pushed_key_ref = NULL \
          WHERE tenant_id = $1 AND slot = $2 AND observed_fpr IS NOT NULL",
    )
    .bind(tenant)
    .bind(slot.as_str())
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The registration a drift reports, as the slot's design (adopting it): `None` = strata has no tenant registration
/// for the slot (it was removed).
pub fn remote_design(slot: Slot, r: &RemoteRow) -> Option<SlotDesign> {
    let o = r.observed.as_ref()?;
    if o.get("source").and_then(Value::as_str) != Some("tenant") {
        return None;
    }
    let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
    Some(SlotDesign {
        kind: slot.kind_of_dialect(&s("dialect").unwrap_or_default()),
        base_url: s("base_url"),
        model: s("model").unwrap_or_default(),
        secret_id: None,
        key_kept: o
            .get("has_api_key")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        fingerprint: None,
        default: false,
    })
}

/// The `drift` of a slot view, or null.
fn drift_json(slot: Slot, r: Option<&RemoteRow>) -> Value {
    let Some(r) = r.filter(|r| r.drifted()) else {
        return Value::Null;
    };
    let mut d = json!({
        "remote_version": r.pushed_version + 1,
        "changed_by": ANOTHER_APP,
        "at": r.observed_at,
    });
    if let Some(x) = remote_design(slot, r) {
        d["remote"] = json!({"kind": x.kind, "base_url": x.base_url, "model": x.model});
    }
    d
}

/// The web `ProviderSlot` of a customer tenant's slot. `source` = `default` when the slot was reset to the
/// deployment default (F-08): configured (deployed), no kind, endpoint or key, `model` = the default's. `withheld`
/// (embed only, [`withheld`]): the published embed model strata does not run yet, and the roots that hold it back.
pub fn slot_json(
    slot: Slot,
    row: Option<&SlotRow>,
    r: Option<&RemoteRow>,
    withheld: Option<&Value>,
) -> Value {
    let design = row.and_then(SlotRow::design);
    let default = design.as_ref().is_some_and(|d| d.default);
    let own = design.as_ref().filter(|d| !d.default);
    let source = if default { "default" } else { "tenant" };
    let model = if default {
        slot.deployment_default().map(str::to_string)
    } else {
        own.map(|d| d.model.clone())
    };
    json!({
        "slot": slot.as_str(),
        "service": target_of(slot).as_str(),
        "label": slot.label(),
        "purpose": slot.purpose(),
        "kinds": slot.external_kinds(),
        "source": source,
        "configured": design.is_some(),
        "kind": own.map(|d| d.kind.clone()),
        "base_url": own.and_then(|d| d.base_url.clone()),
        "model": model,
        "version": row.map_or(0, |x| x.version),
        "secret": {
            "set": design.as_ref().is_some_and(SlotDesign::key_set),
            "updated_at": row.and_then(|x| x.secret_at),
            "fingerprint": row.and_then(|x| x.fingerprint.clone()),
        },
        "updated_at": row.map(|x| x.updated_at),
        "drift": drift_json(slot, r),
        "used_by": slot.used_by(),
        "region": row.and_then(|x| x.region.clone()),
        "withheld": withheld.cloned().unwrap_or(Value::Null),
    })
}

/// The web `ProviderSlot` of nonagentic's own tenant (D70): the platform's internal kind, read from what strata
/// serves the tenant (`cap`, a `default` source); nothing is stored per tenant.
pub fn internal_json(
    slot: Slot,
    cap: Option<&Value>,
    ip: &crate::config::InternalProvidersConfig,
) -> Value {
    let configured = cap
        .and_then(|c| c.get("source"))
        .and_then(Value::as_str)
        .is_none_or(|s| s != "unconfigured");
    let model = cap
        .and_then(|c| c.get("model"))
        .and_then(Value::as_str)
        .unwrap_or(slot.internal_model());
    json!({
        "slot": slot.as_str(),
        "service": target_of(slot).as_str(),
        "label": slot.label(),
        "purpose": slot.purpose(),
        "kinds": [slot.internal_kind()],
        "configured": configured,
        "kind": slot.internal_kind(),
        "base_url": slot.internal_base_url(ip),
        "model": model,
        "version": 0,
        "secret": {"set": false, "updated_at": null, "fingerprint": null},
        "updated_at": null,
        "drift": null,
        "used_by": slot.used_by(),
        "source": "default",
        "region": kinds::INTERNAL_REGION,
    })
}

/// Every slot of a customer tenant, in slot order, from the working and remote rows.
pub async fn views(c: &mut PgConnection, tenant: Uuid) -> ApiResult<Vec<Value>> {
    let rs = rows(&mut *c, tenant).await?;
    let rm = remotes(&mut *c, tenant).await?;
    let held = withheld(&mut *c, tenant).await?;
    Ok(Slot::ALL
        .into_iter()
        .map(|s| {
            slot_json(
                s,
                rs.iter().find(|x| x.slot == s.as_str()),
                rm.iter().find(|x| x.slot == s.as_str()),
                held.as_ref().filter(|_| s == Slot::Embed),
            )
        })
        .collect())
}

/// One slot's view.
pub async fn view(c: &mut PgConnection, tenant: Uuid, slot: Slot) -> ApiResult<Value> {
    let r = row(&mut *c, tenant, slot, false).await?;
    let rm = remote(&mut *c, tenant, slot).await?;
    let held = if slot == Slot::Embed {
        withheld(&mut *c, tenant).await?
    } else {
        None
    };
    Ok(slot_json(slot, r.as_ref(), rm.as_ref(), held.as_ref()))
}

/// strata's `GET /api/tenant/providers` of `service` for `tenant`, or `None` (not configured here, no token pair,
/// unreachable, refused): a slot view never fails because one service is down.
pub async fn fetch_remote(st: &AppState, tenant: Uuid, service: Service) -> Option<Value> {
    let p = strata::plane(st, tenant, service).await.ok()?;
    let r = match service {
        Service::Rag => rag::providers_get(&st.http, &p).await,
        Service::Ingest => ingest::providers_get(&st.http, &p).await,
    };
    match r {
        Ok(v) if v.is_object() => Some(v),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                upstream = service.name(),
                code = e.code(),
                "slot drift refresh skipped"
            );
            None
        }
    }
}

/// strata's `stores.embed_model` identity of a reported embed capability that names a model (`tenant`, `store` or
/// `default` source): its `model_id`, else `<model>@<dims>` (strata `ProviderSpec::embed_model_id`).
pub fn embed_id(cap: Option<&Value>) -> Option<String> {
    let c = cap.filter(|c| {
        matches!(
            c.get("source").and_then(Value::as_str),
            Some("tenant" | "store" | "default")
        )
    })?;
    if let Some(id) = c
        .get("model_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(id.to_string());
    }
    let model = c.get("model").and_then(Value::as_str)?;
    let dims = c
        .get("dims")
        .and_then(Value::as_i64)
        .unwrap_or(kinds::EMBED_DIMS);
    Some(format!("{model}@{dims}"))
}

/// The embed model the tenant row runs now (`cap` = the `embed` of strata's tenant view): [`embed_id`], and for a
/// `default` source that names no model, the deployment default's.
pub fn current_embed_id(cap: Option<&Value>) -> Option<String> {
    embed_id(cap).or_else(|| {
        cap.filter(|c| c.get("source").and_then(Value::as_str) == Some("default"))
            .and(Slot::Embed.deployment_default())
            .map(Slot::embed_model_id)
    })
}

/// The tenant embed follows strata's deployment default (`source: default`, no tenant registration).
pub fn follows_default(cap: Option<&Value>) -> bool {
    cap.and_then(|c| c.get("source"))
        .and_then(Value::as_str)
        .is_some_and(|s| s == "default")
}

/// The host of a URL, for messages (never the path, never a credential).
pub fn host_of(u: &str) -> String {
    Url::parse(u)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "the provider".to_string())
}

/// One slot of a `connections_put` payload: the published value, never a key (`key_ref` names the sealed key row).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSlot {
    pub slot: String,
    /// `false` = remove the tenant registration (strata falls back to its default).
    pub configured: bool,
    pub kind: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// The `provider_slot_secret` row whose key is sent as `api_key`; `None` = no `api_key` (strata keeps the key it
    /// holds for the same `base_url`).
    pub key_ref: Option<Uuid>,
    /// The working row's version when it was published.
    pub version: i32,
    /// The release that published it.
    pub release: Option<String>,
}

/// The payload of a `connections_put` row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPayload {
    pub slots: Vec<PushSlot>,
}

/// The outbox dedupe key of a service's pushes.
pub fn dedupe_key(target: Target) -> String {
    format!("{}:{OP}", target.as_str())
}

/// The advisory-lock name the sender holds while it reads and replaces a service's provider set.
pub fn push_lock(tenant: Uuid, target: Target) -> String {
    format!("{OP}:{tenant}:{}", target.as_str())
}

/// Queue the push of `slot`'s published value `design` in the caller's transaction: `None` (not configured) and the
/// deployment default (F-08) remove the tenant registration (no `api_key`). The row carries every other entry of the
/// service strata has not confirmed yet (`unsent`); this entry replaces one of its slot. `release` = the release that
/// published it (`None`: a reset, pushed at once).
pub async fn enqueue_push(
    c: &mut PgConnection,
    tenant: Uuid,
    slot: Slot,
    design: Option<&SlotDesign>,
    version: i32,
    release: Option<&str>,
) -> ApiResult<Uuid> {
    let design = design.filter(|d| !d.default);
    let entry = PushSlot {
        slot: slot.as_str().to_string(),
        configured: design.is_some(),
        kind: design.map(|d| d.kind.clone()),
        base_url: design.and_then(|d| d.base_url.clone()),
        model: design.map(|d| d.model.clone()),
        key_ref: design.and_then(|d| d.secret_id),
        version,
        release: release.map(str::to_string),
    };
    enqueue_entry(c, tenant, target_of(slot), Some(entry))
        .await?
        .ok_or_else(|| internal("a push with an entry queued nothing".to_string()))
}

/// The result field of a push naming the entries it withheld (crate::provision::senders::connections).
pub const WITHHELD: &str = "withheld";
/// The result field naming the strata roots that held a withheld entry back.
pub const WITHHELD_ROOTS: &str = "withheld_roots";

fn payload_of(v: &Value) -> ApiResult<PushPayload> {
    serde_json::from_value(v.clone())
        .map_err(|e| internal(format!("connections_put payload is not valid: {e}")))
}

/// The entries a push result withheld (none when it withheld nothing).
fn withheld_entries(result: Option<&Value>) -> ApiResult<Vec<PushSlot>> {
    match result.and_then(|r| r.get(WITHHELD)) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            internal(format!(
                "connections_put withheld entries are not valid: {e}"
            ))
        }),
    }
}

/// How far back `unsent` reads a service's rows (each row carries every older unsent entry, so the newest few
/// suffice; the bound keeps a long history cheap).
const UNSENT_DEPTH: i64 = 50;

/// Every entry of `key`'s pushes that strata has not confirmed: newest row first, back to the newest `succeeded` one
/// (only the entries it withheld count there); a cancelled row (offboarding) is skipped. A slot's entry is taken from
/// the newest row naming it. A payload that does not parse fails loud (500), it is never dropped (review M10).
async fn unsent(c: &mut PgConnection, tenant: Uuid, key: &str) -> ApiResult<Vec<PushSlot>> {
    let rows: Vec<(String, Value, Option<Value>)> = sqlx::query_as(
        "SELECT status::text, payload, result FROM provision_outbox \
          WHERE tenant_id = $1 AND dedupe_key = $2 AND status <> 'cancelled' \
          ORDER BY created_at DESC, id DESC LIMIT $3",
    )
    .bind(tenant)
    .bind(key)
    .bind(UNSENT_DEPTH)
    .fetch_all(&mut *c)
    .await?;
    let mut out: Vec<PushSlot> = Vec::new();
    for (status, payload, result) in rows {
        let done = status == "succeeded";
        let entries = if done {
            withheld_entries(result.as_ref())?
        } else {
            payload_of(&payload)?.slots
        };
        for e in entries {
            if !out.iter().any(|o| o.slot == e.slot) {
                out.push(e);
            }
        }
        if done {
            break;
        }
    }
    Ok(out)
}

/// Queue a push of `target` carrying `unsent` plus `entry` (it replaces an entry of its slot): the pending row's
/// payload is replaced (one pending row per service), failed / dead rows are superseded (their entries are in the new
/// payload). `None` = nothing to push.
async fn enqueue_entry(
    c: &mut PgConnection,
    tenant: Uuid,
    target: Target,
    entry: Option<PushSlot>,
) -> ApiResult<Option<Uuid>> {
    let key = dedupe_key(target);
    // serialise concurrent publishes of one tenant's service, so neither drops the other's slot
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{OP}_enqueue:{tenant}:{}", target.as_str()))
        .execute(&mut *c)
        .await?;
    let mut slots = unsent(&mut *c, tenant, &key).await?;
    if let Some(e) = entry {
        slots.retain(|s| s.slot != e.slot);
        slots.push(e);
    }
    if slots.is_empty() {
        return Ok(None);
    }
    slots.sort_by_key(|s| Slot::parse(&s.slot));
    let payload =
        serde_json::to_value(&PushPayload { slots }).map_err(|e| ApiError::Internal(e.into()))?;
    provision::enqueue(
        c,
        tenant,
        NewRow {
            target,
            op: OP,
            dedupe_key: key,
            payload,
        },
        &Actor::system(PUBLISHER),
    )
    .await
    .map(Some)
}

/// Queue again what strata has not confirmed for `target` (`unsent`): the retry of a failed, dead or withheld push
/// (`POST /api/providers/slots/{slot}/push`). The row id, `None` = nothing is waiting.
pub async fn requeue(
    c: &mut PgConnection,
    tenant: Uuid,
    target: Target,
) -> ApiResult<Option<Uuid>> {
    enqueue_entry(c, tenant, target, None).await
}

/// A succeeded push: its result and when it completed.
type PushDone = (Option<Value>, Option<DateTime<Utc>>);

/// The newest succeeded strata_rag push.
async fn last_rag_push(c: &mut PgConnection, tenant: Uuid) -> ApiResult<Option<PushDone>> {
    Ok(sqlx::query_as(
        "SELECT result, completed_at FROM provision_outbox \
          WHERE tenant_id = $1 AND dedupe_key = $2 AND status = 'succeeded' \
          ORDER BY completed_at DESC NULLS LAST, created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(dedupe_key(target_of(Slot::Embed)))
    .fetch_optional(&mut *c)
    .await?)
}

/// The embed entry the newest successful push withheld (review I4): `{model, version, release, roots, at}` — the
/// published model strata does not run yet and the strata roots (names) that hold it back. `None` = nothing withheld.
pub async fn withheld(c: &mut PgConnection, tenant: Uuid) -> ApiResult<Option<Value>> {
    let Some((result, at)) = last_rag_push(&mut *c, tenant).await? else {
        return Ok(None);
    };
    let Some(e) = withheld_entries(result.as_ref())?
        .into_iter()
        .find(|e| e.slot == Slot::Embed.as_str())
    else {
        return Ok(None);
    };
    let roots = result
        .as_ref()
        .and_then(|r| r.get(WITHHELD_ROOTS))
        .cloned()
        .unwrap_or_else(|| json!([]));
    Ok(Some(json!({
        "model": e.model,
        "configured": e.configured,
        "version": e.version,
        "release": e.release,
        "roots": roots,
        "at": at,
    })))
}

/// Queue again the embed entry the newest successful push withheld (ruling 2026-10-21, revised;
/// crate::provision::senders::connections), with everything else strata has not confirmed: when a job publishes or
/// accepts a model change (crate::knowledge::model_changes) and on every model-change sweep (a blocking root or job
/// deleted). The sender withholds it again while a job still reads a root on the deployment default. The row id,
/// `None` = nothing was withheld.
pub async fn requeue_withheld(c: &mut PgConnection, tenant: Uuid) -> ApiResult<Option<Uuid>> {
    if withheld(&mut *c, tenant).await?.is_none() {
        return Ok(None);
    }
    enqueue_entry(c, tenant, target_of(Slot::Embed), None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_value_round_trips() {
        let id = Uuid::new_v4();
        let d = SlotDesign {
            kind: "voyage".into(),
            base_url: Some("https://api.voyageai.com/v1".into()),
            model: "voyage-3.5".into(),
            secret_id: Some(id),
            key_kept: false,
            fingerprint: Some("0a1b2c3d".into()),
            default: false,
        };
        let v = d.to_value();
        assert_eq!(v["secret"], json!({"set": true, "fingerprint": "0a1b2c3d"}));
        assert_eq!(SlotDesign::from_value(Some(&v)).unwrap(), Some(d));
        let kept = json!({"configured": true, "kind": "cohere", "base_url": null, "model": "rerank-v3.5",
                          "secret": {"set": true, "fingerprint": null}, "secret_id": null});
        let k = SlotDesign::from_value(Some(&kept)).unwrap().unwrap();
        assert!(k.key_kept && k.key_set() && k.secret_id.is_none());
        assert_eq!(k.to_value(), kept);
        assert_eq!(SlotDesign::from_value(None).unwrap(), None);
        assert_eq!(
            SlotDesign::from_value(Some(&json!({"configured": false}))).unwrap(),
            None
        );
        assert!(SlotDesign::from_value(Some(&json!({"configured": true}))).is_err());
        let def = SlotDesign::deployment_default();
        let dv = def.to_value();
        assert_eq!(dv["source"], "default");
        assert_eq!(dv["model"], Value::Null);
        assert_eq!(
            SlotDesign::from_value(Some(&dv)).unwrap(),
            Some(def.clone())
        );
        assert_eq!(def.model(), None);
        assert!(!def.key_set());
    }

    #[test]
    fn fingerprint_ignores_everything_but_the_registration() {
        let a = json!({"source": "tenant", "base_url": "https://x.example/v1/", "dialect": "openai",
                       "model": "m", "has_api_key": true, "updated_at": "t1"});
        let b = json!({"source": "tenant", "base_url": "https://x.example/v1", "dialect": "openai",
                       "model": "m", "has_api_key": true, "updated_at": "t2", "regions": ["eu"]});
        assert_eq!(remote_fpr(Some(&a)), remote_fpr(Some(&b)));
        let c = json!({"source": "tenant", "base_url": "https://x.example/v1", "dialect": "openai",
                       "model": "m2", "has_api_key": true});
        assert_ne!(remote_fpr(Some(&a)), remote_fpr(Some(&c)));
        assert_eq!(
            remote_fpr(Some(&json!({"source": "default", "model": "m"}))),
            NONE_FPR
        );
        assert_eq!(remote_fpr(None), NONE_FPR);
        assert_eq!(remote_fpr(Some(&a)).len(), 16);
    }
}
