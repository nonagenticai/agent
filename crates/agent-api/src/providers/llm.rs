//! LLM providers (B3 T3; D70, D69, D89; port of care-bus-agents@96c608c `routes/llm_providers.rs`): the provider
//! records a tenant enters (live data, table `llm_provider`) and the uses bound to them (design, kind `llm_use`,
//! crate::providers::targets).
//!
//! - [`resolve`] (B4 runtime, B5 gates, B3 T4d speech + interaction backend): the provider + model + key serving one
//!   use of one job in a design (`DesignOf::Live` = the job's active snapshot, `Draft` = the working rows). The
//!   platform-owned tenant (D70 `gpuHostingOf = internal`) stores no provider: its OpenAI-compatible LLM uses resolve
//!   to the internal pgx vLLM from config (`INTERNAL_LLM_*`), called with the platform service key; its `gates` use
//!   resolves to Laya through the public gateway (`INTERNAL_LAYA_*`, B5 T9) when `INTERNAL_LAYA_KEY` is set; its
//!   speech uses resolve to nothing (its speech is the platform gateway, `VOICE_PLATFORM_TENANTS`, D102). [`resolve_in`] is the
//!   same with its inputs spelled out ([`ResolveCtx`]), for callers that hold no `AppState` (the interaction backend).
//!   A bound use whose provider was deleted (a rollback or an old cut brought it back) is 409 `provider_deleted`, and
//!   one whose provider's kind cannot serve the use is 409 with [`Use::refusal`]'s code: both are counted
//!   (`agent_llm_resolve_refused_total{code}`) and logged, never read as "unbound" (no silent keyword fallback).
//! - [`resolve_by_id`] (B8 generator): one provider record by id.
//! - [`live_users`]: the jobs whose live design binds a provider; [`holders`]: every job + use that binds it live or
//!   in an approved, unapplied cut (delete and a kind change refuse while one does).
//! - [`live_problems`]: a job's live bindings that cannot serve (the providers view, T12 readiness).
//! - [`probe_tenant`] / [`probe_internal`]: the provider test (`GET {base}/models`, Laya `GET {base}/v1/models`, 5 s).
//!   A tenant-entered host is called only through `netguard::pinned_client`; a provider failure is `ok: false`,
//!   never a 5xx. The detail names the status, the model count, the time and which token was sent, never the URL,
//!   the token, an upstream body or an error text.
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::secret::{self, SecretKind};
use super::targets::use_ref;
use super::{UrlField, check_url};
use crate::config::InternalProvidersConfig;
use crate::crypto::keyring::Keyring;
use crate::design::{self, DesignOf, JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::netguard::{self, Policy};
use crate::state::AppState;
use crate::tenants::app_licence::{self, App, AppToken};

/// The provider test's whole budget (connect + answer).
pub const TEST_TIMEOUT: Duration = Duration::from_secs(5);
/// At most this many model names come back from a test.
const MAX_MODELS: usize = 200;
/// The id of the virtual internal provider (D70) the platform-owned tenant sees: no row has it.
pub const INTERNAL_PROVIDER_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0070);
/// Its display name and region.
pub const INTERNAL_PROVIDER_NAME: &str = "pgx vLLM (internal)";
pub const INTERNAL_REGION: &str = "EU (platform)";

/// A provider's API dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// OpenAI-compatible (vLLM, …): models at `{base_url}/models`; as a speech provider the OpenAI audio API
    /// (`/audio/transcriptions`, `/audio/speech`).
    Openai,
    /// Laya System-1: models at `{base_url}/v1/models`.
    Laya,
    /// A w2v speech gateway (B3 T4d, D101/D102; crate::voice `w2v` contract): `asr` / `tts` only; probed at
    /// `{base_url}/health`.
    W2v,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Openai => "openai",
            Kind::Laya => "laya",
            Kind::W2v => "w2v",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Kind::Openai),
            "laya" => Some(Kind::Laya),
            "w2v" => Some(Kind::W2v),
            _ => None,
        }
    }
    /// Fixture `LLM_KIND_LABEL`.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Openai => "OpenAI-compatible",
            Kind::Laya => "Laya",
            Kind::W2v => "w2v speech",
        }
    }
    /// What a probe reads: the model list (OpenAI-compatible, Laya), or the w2v gateway's `/health`.
    pub fn models_url(self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        match self {
            Kind::Openai => format!("{base}/models"),
            Kind::Laya => format!("{base}/v1/models"),
            Kind::W2v => format!("{base}/health"),
        }
    }
}

/// What agent calls a provider for (fixture `LLM_USE_RULES` + D89 `generator`, + the speech uses `asr` / `tts`
/// of B3 T4d, D101/D102: crate::voice::ProviderSpeech).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    Qualify,
    Gates,
    Generator,
    /// Speech to text (a voice turn's transcription).
    Asr,
    /// Text to speech (a voice turn's answer); its binding may carry a `voice`.
    Tts,
}

/// Every use 0050 stores (`llm_use_binding.use`): what [`live_users`] scans.
pub const STORED_USES: [&str; 5] = ["qualify", "gates", "generator", "asr", "tts"];

impl Use {
    pub const ALL: [Use; 5] = [Use::Qualify, Use::Gates, Use::Generator, Use::Asr, Use::Tts];
    pub fn as_str(self) -> &'static str {
        match self {
            Use::Qualify => "qualify",
            Use::Gates => "gates",
            Use::Generator => "generator",
            Use::Asr => "asr",
            Use::Tts => "tts",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Use::ALL.into_iter().find(|u| u.as_str() == s)
    }
    pub fn label(self) -> &'static str {
        match self {
            Use::Qualify => "Turn qualification",
            Use::Gates => "Laya gates",
            Use::Generator => "Generator",
            Use::Asr => "Speech to text",
            Use::Tts => "Text to speech",
        }
    }
    /// The provider kinds that may serve this use.
    pub fn kinds(self) -> &'static [Kind] {
        match self {
            Use::Qualify | Use::Generator => &[Kind::Openai],
            Use::Gates => &[Kind::Laya],
            Use::Asr | Use::Tts => &[Kind::W2v, Kind::Openai],
        }
    }
    /// A provider of `kind` may serve this use.
    pub fn accepts(self, kind: Kind) -> bool {
        self.kinds().contains(&kind)
    }
    /// A speech use (`asr`, `tts`): served by crate::voice, never by the LLM runtime.
    pub fn is_speech(self) -> bool {
        matches!(self, Use::Asr | Use::Tts)
    }
    /// The internal pgx vLLM (D70) serves this use for the platform-owned tenant.
    pub fn internal(self) -> bool {
        matches!(self, Use::Qualify | Use::Generator)
    }
    /// Why a provider of `kind` cannot serve this use (fixture `llmUseProblem`), as `(code, message)`, or `None`.
    /// Crossing between speech and LLM (a w2v provider on an LLM use, a Laya provider on a speech use) is 422
    /// `kind_not_valid_for_use` (B3 T4d); a wrong LLM dialect is 422 `provider_kind_mismatch` (T3).
    pub fn refusal(self, kind: Kind) -> Option<(&'static str, String)> {
        if self.accepts(kind) {
            return None;
        }
        if self.is_speech() {
            return Some((
                "kind_not_valid_for_use",
                format!(
                    "{} needs a w2v or OpenAI-compatible speech provider; this one is {}.",
                    self.label(),
                    kind.label()
                ),
            ));
        }
        if kind == Kind::W2v {
            return Some((
                "kind_not_valid_for_use",
                format!(
                    "A w2v provider serves speech (asr, tts) only; {} needs an LLM provider.",
                    self.label()
                ),
            ));
        }
        let why = match self {
            Use::Gates => format!(
                "Only a Laya provider can serve gates; this one is {}.",
                kind.label()
            ),
            _ => format!(
                "{} needs an {} provider; this one is {}.",
                self.label(),
                Kind::Openai.label(),
                kind.label()
            ),
        };
        Some(("provider_kind_mismatch", why))
    }
}

/// A provider key to send. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct LlmKey(String);

impl LlmKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for LlmKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LlmKey([redacted])")
    }
}

/// What serves one use (plan §3 `ResolvedLlm`).
#[derive(Clone, Debug)]
pub struct ResolvedLlm {
    /// `None` = the internal pgx vLLM (config, no row).
    pub provider_id: Option<Uuid>,
    pub base_url: String,
    pub model: String,
    pub kind: Kind,
    /// `local_gpu` (internal) or `tenant_byo`.
    pub funding: &'static str,
    pub key: Option<LlmKey>,
    /// A `tts` binding's voice (B3 T4d); `None` = the gateway's default.
    pub voice: Option<String>,
    /// The tenant a platform-funded call is billed to, sent as `X-Effective-Tenant-Id` (the inference-router
    /// attributes the cost to it; vLLM ignores it). `Some` only for the internal OpenAI-compatible model; `None` for
    /// Laya and for every tenant provider: a tenant's own (BYO, third-party) provider never sees our tenant id.
    pub effective_tenant: Option<Uuid>,
    /// The actor a platform-funded call is attributed to (`X-Actor-Id`, [`router_identity_headers`]): agent's own
    /// service account (`SERVICE_ACTOR_ID`) until a call site with a human `sub` in scope replaces it
    /// ([`caller_actor`]). Like [`Self::effective_tenant`], `None` for Laya and every tenant provider.
    pub actor: Option<Uuid>,
    /// `Some` only when `key` is the TENANT's own minted bundle token for that app ([`router_resolved`]): an upstream
    /// 401 then drops the cached token, mints a fresh one and retries once (crate::turn::qualify::extract). `None` for
    /// the platform key, Laya and every BYO provider key.
    pub tenant_licence: Option<App>,
}

impl ResolvedLlm {
    /// The platform call made for the person `sub` (when it is a non-nil uuid): its `X-Actor-Id` is that person,
    /// not agent's service account ([`caller_actor`]). A tenant provider's call gets no actor either way.
    pub fn acting_for(mut self, sub: Option<&str>) -> Self {
        if self.effective_tenant.is_some() {
            self.actor = caller_actor(sub, self.actor);
        }
        self
    }
}

/// The header naming the tenant a platform-funded LLM call is billed to ([`ResolvedLlm::effective_tenant`]).
pub const EFFECTIVE_TENANT_HEADER: &str = "X-Effective-Tenant-Id";
/// The calling app (its licences-registry key) on every inference-router call.
pub const APP_ID_HEADER: &str = "X-App-Id";
/// The tenant-level project of the end tenant ([`tenant_level_project_id`]): agent has no projects.
pub const PROJECT_ID_HEADER: &str = "X-Project-Id";
/// The acting person's Keycloak `sub`, else agent's service account; never a nil or invented id.
pub const ACTOR_ID_HEADER: &str = "X-Actor-Id";
/// agent's key in the licences registry: the `X-App-Id` value.
pub const APP_ID: &str = "agent";

/// The tenant-level project id of a project-less app's tenant: the 32 hex digits of the tenant uuid reversed, as a
/// uuid (the platform convention, strata_rag `metering.rs` `tenant_level_project_id`). Its own inverse.
pub fn tenant_level_project_id(tenant: Uuid) -> Uuid {
    let b = tenant.as_u128().swap_bytes();
    let lo = 0x0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_u128;
    Uuid::from_u128(((b & lo) << 4) | ((b >> 4) & lo))
}

/// The actor of a platform call: the human `sub` when it is a non-nil uuid, else `service` (`SERVICE_ACTOR_ID`),
/// else none (the header is omitted). A nil id never comes out.
pub fn caller_actor(human_sub: Option<&str>, service: Option<Uuid>) -> Option<Uuid> {
    human_sub
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|u| !u.is_nil())
        .or(service.filter(|u| !u.is_nil()))
}

/// The caller-identity headers of one platform (inference-router) call billed to `tenant`: `X-Effective-Tenant-Id`,
/// `X-App-Id: agent`, `X-Project-Id` ([`tenant_level_project_id`]) and, when there is one, `X-Actor-Id`. Only for a
/// call whose [`ResolvedLlm::effective_tenant`] is set: a third-party provider gets none of them.
pub fn router_identity_headers(tenant: Uuid, actor: Option<Uuid>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        (EFFECTIVE_TENANT_HEADER, tenant.to_string()),
        (APP_ID_HEADER, APP_ID.to_string()),
        (
            PROJECT_ID_HEADER,
            tenant_level_project_id(tenant).to_string(),
        ),
    ];
    if let Some(a) = actor.filter(|a| !a.is_nil()) {
        h.push((ACTOR_ID_HEADER, a.to_string()));
    }
    h
}

/// The client of the platform's own model (config URL, not tenant input, so not through netguard): connect 5 s,
/// `timeout` per call, and, when the call is billed to a tenant, the [`router_identity_headers`] on every request.
pub fn platform_client(
    timeout: Duration,
    tenant: Option<Uuid>,
    actor: Option<Uuid>,
) -> anyhow::Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(t) = tenant {
        for (k, v) in router_identity_headers(t, actor) {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes())?,
                reqwest::header::HeaderValue::from_str(&v)?,
            );
        }
    }
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .default_headers(headers)
        .build()?)
}

/// One stored provider. Not `Debug`: it carries the sealed key.
#[derive(Clone, sqlx::FromRow)]
pub struct ProviderRow {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub region: Option<String>,
    pub secret_sealed: Option<String>,
    pub secret_fpr: Option<String>,
    pub secret_updated_at: Option<DateTime<Utc>>,
    pub last_test: Option<Value>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
    /// Soft-deleted (0050): kept so a snapshot naming it resolves to `provider_deleted`.
    pub deleted_at: Option<DateTime<Utc>>,
}

impl ProviderRow {
    /// The stored kind, or a 500 (a kind 0050's CHECK admits but this build cannot parse).
    pub fn kind(&self) -> ApiResult<Kind> {
        Kind::parse(&self.kind).ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!(
                "llm_provider {} has kind {} this build does not serve",
                self.id,
                self.kind
            ))
        })
    }
}

const PROVIDER_COLUMNS_ONE: &str = "SELECT id, name, kind, base_url, models, region, secret_sealed, secret_fpr, \
    secret_updated_at, last_test, updated_at, updated_by, deleted_at FROM llm_provider \
    WHERE id = $1 AND deleted_at IS NULL";
const PROVIDER_FOR_UPDATE: &str = "SELECT id, name, kind, base_url, models, region, secret_sealed, secret_fpr, \
    secret_updated_at, last_test, updated_at, updated_by, deleted_at FROM llm_provider \
    WHERE id = $1 AND deleted_at IS NULL FOR UPDATE";
const PROVIDER_KEY_SHARE: &str = "SELECT id, name, kind, base_url, models, region, secret_sealed, secret_fpr, \
    secret_updated_at, last_test, updated_at, updated_by, deleted_at FROM llm_provider \
    WHERE id = $1 AND deleted_at IS NULL FOR KEY SHARE";
const PROVIDERS_ALL: &str = "SELECT id, name, kind, base_url, models, region, secret_sealed, secret_fpr, \
    secret_updated_at, last_test, updated_at, updated_by, deleted_at FROM llm_provider \
    WHERE deleted_at IS NULL ORDER BY name, id";
/// Deleted rows too: what a design value names ([`check_binding`]).
const PROVIDER_ANY: &str = "SELECT id, name, kind, base_url, models, region, secret_sealed, secret_fpr, \
    secret_updated_at, last_test, updated_at, updated_by, deleted_at FROM llm_provider WHERE id = $1";

/// How a provider row is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    None,
    /// Delete and update: blocks a concurrent bind of it.
    Update,
    /// Bind: blocks a concurrent delete of it.
    KeyShare,
}

/// One live (not deleted) provider of the transaction's tenant (RLS).
pub async fn provider(
    c: &mut PgConnection,
    id: Uuid,
    lock: Lock,
) -> ApiResult<Option<ProviderRow>> {
    let sql = match lock {
        Lock::None => PROVIDER_COLUMNS_ONE,
        Lock::Update => PROVIDER_FOR_UPDATE,
        Lock::KeyShare => PROVIDER_KEY_SHARE,
    };
    Ok(sqlx::query_as::<_, ProviderRow>(sql)
        .bind(id)
        .fetch_optional(&mut *c)
        .await?)
}

/// Every live (not deleted) provider of the transaction's tenant, by name.
pub async fn providers(c: &mut PgConnection) -> ApiResult<Vec<ProviderRow>> {
    Ok(sqlx::query_as::<_, ProviderRow>(PROVIDERS_ALL)
        .fetch_all(&mut *c)
        .await?)
}

/// D70: the tenant is nonagentic's own (`tenants.platform_owned`, set at boot for `AGENT_PLATFORM_TENANT_ID`, or
/// that id itself).
pub async fn platform_owned(c: &mut PgConnection, st: &AppState, tenant: Uuid) -> ApiResult<bool> {
    platform_owned_in(c, st.cfg.platform_tenant_id, tenant).await
}

/// [`platform_owned`] with `AGENT_PLATFORM_TENANT_ID` given.
async fn platform_owned_in(
    c: &mut PgConnection,
    platform_tenant: Option<Uuid>,
    tenant: Uuid,
) -> ApiResult<bool> {
    if platform_tenant == Some(tenant) {
        return Ok(true);
    }
    let owned: Option<bool> =
        sqlx::query_scalar("SELECT platform_owned FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_optional(&mut *c)
            .await?;
    Ok(owned.unwrap_or(false))
}

/// The stored key of `p`, opened (503 without the keyring, 409 `secret_unreadable`).
pub fn open_key(kr: Option<&Keyring>, tenant: Uuid, p: &ProviderRow) -> ApiResult<Option<LlmKey>> {
    match &p.secret_sealed {
        None => Ok(None),
        Some(s) => {
            let t = secret::open(kr, tenant, SecretKind::LlmKey, p.id, s)?;
            Ok(Some(LlmKey(t.expose().to_string())))
        }
    }
}

/// The platform tenant's provider of `use_` (D70): the internal pgx vLLM for an OpenAI-compatible LLM use; for
/// `gates`, Laya through the public gateway (`INTERNAL_LAYA_*`, B5 T9) when `INTERNAL_LAYA_KEY` is set, else `None`;
/// `None` for a speech use (the platform tenant's speech is the platform gateway, D102). The vLLM call is billed to
/// `tenant` ([`ResolvedLlm::effective_tenant`]); Laya is not behind the inference-router, so it is not.
fn internal_resolved(ip: &InternalProvidersConfig, tenant: Uuid, use_: Use) -> Option<ResolvedLlm> {
    if use_ == Use::Gates {
        return ip.laya_key.clone().map(|key| ResolvedLlm {
            provider_id: None,
            base_url: ip.laya_url.clone(),
            model: ip.laya_model.clone(),
            kind: Kind::Laya,
            funding: "local_gpu",
            key: Some(LlmKey(key)),
            voice: None,
            effective_tenant: None,
            actor: None,
            tenant_licence: None,
        });
    }
    if !use_.internal() {
        return None;
    }
    Some(ResolvedLlm {
        provider_id: None,
        base_url: ip.llm_url.clone(),
        model: ip.llm_model.clone(),
        kind: Kind::Openai,
        funding: "local_gpu",
        key: ip.llm_key.clone().map(LlmKey),
        voice: None,
        effective_tenant: Some(tenant),
        actor: ip.service_actor_id,
        tenant_licence: None,
    })
}

fn resolved(
    kr: Option<&Keyring>,
    tenant: Uuid,
    p: &ProviderRow,
    model: String,
    voice: Option<String>,
) -> ApiResult<ResolvedLlm> {
    Ok(ResolvedLlm {
        provider_id: Some(p.id),
        base_url: p.base_url.clone(),
        model,
        kind: p.kind()?,
        funding: "tenant_byo",
        key: open_key(kr, tenant, p)?,
        voice,
        effective_tenant: None,
        actor: None,
        tenant_licence: None,
    })
}

/// What [`resolve_in`] reads besides the transaction: `AGENT_PLATFORM_TENANT_ID`, the internal providers (D70), the
/// design registry and the provider keyring.
#[derive(Clone, Copy)]
pub struct ResolveCtx<'a> {
    pub platform_tenant: Option<Uuid>,
    pub internal: &'a InternalProvidersConfig,
    pub design: &'a Registry,
    pub keyring: Option<&'a Keyring>,
    /// The running API, when the caller holds one: an unbound tenant `qualify` then resolves to the inference-router
    /// under the tenant's own licence ([`tenant_router`]). `None` (the interaction backend, which is built before
    /// `AppState`): such a use stays unbound.
    pub st: Option<&'a AppState>,
}

impl<'a> ResolveCtx<'a> {
    /// The context of the running API.
    pub fn of(st: &'a AppState) -> Self {
        Self {
            platform_tenant: st.cfg.platform_tenant_id,
            internal: &st.cfg.internal_providers,
            design: st.design.as_ref(),
            keyring: st.provider_keyring.as_deref(),
            st: Some(st),
        }
    }
}

/// What serves `use_` for `job` of `tenant` in the design `of` (plan §3, for B4/B5). `None` = the use is unbound
/// (its fallback runs). A binding that cannot serve is an error, never `None`: 409 `provider_deleted` (its provider
/// was deleted, or has no row) and 409 `provider_kind_mismatch` / `kind_not_valid_for_use` (its provider's kind
/// cannot serve the use). An unbound `qualify` of any other tenant goes to the inference-router under THAT
/// tenant's own licence ([`tenant_router`]; 503 `llm_inference_not_provisioned` without one, never the platform key).
/// Run in a transaction scoped to `tenant`.
pub async fn resolve(
    c: &mut PgConnection,
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    use_: Use,
    of: &DesignOf,
) -> ApiResult<Option<ResolvedLlm>> {
    resolve_in(c, ResolveCtx::of(st), tenant, job, use_, of).await
}

/// [`resolve`] with its inputs spelled out (crate::interaction::ProviderBackend holds no `AppState`).
pub async fn resolve_in(
    c: &mut PgConnection,
    ctx: ResolveCtx<'_>,
    tenant: Uuid,
    job: &JobRef,
    use_: Use,
    of: &DesignOf,
) -> ApiResult<Option<ResolvedLlm>> {
    if platform_owned_in(&mut *c, ctx.platform_tenant, tenant).await? {
        return Ok(internal_resolved(ctx.internal, tenant, use_));
    }
    let v = design::value(
        &mut *c,
        ctx.design,
        job,
        of,
        "llm_use",
        &use_ref(use_.as_str()),
    )
    .await?;
    let Some(v) = v else {
        return tenant_router(&mut *c, ctx, tenant, job, use_).await;
    };
    match check_binding(&mut *c, use_, &v).await {
        Ok(None) => tenant_router(&mut *c, ctx, tenant, job, use_).await,
        Ok(Some(b)) => resolved(ctx.keyring, tenant, &b.provider, b.model, b.voice).map(Some),
        Err(e) => {
            if let ApiError::Conflict(code, _) = &e {
                metrics::counter!(RESOLVE_REFUSED_TOTAL, "code" => *code).increment(1);
                tracing::warn!(
                    tenant = %tenant,
                    job = %job.key,
                    use_ = use_.as_str(),
                    code = *code,
                    "llm use binding cannot serve; refusing instead of falling back"
                );
            }
            Err(e)
        }
    }
}

/// 503 `llm_inference_not_provisioned`: the tenant has no inference-router licence yet. Fail closed: the platform's
/// own key is never a substitute.
pub fn inference_not_provisioned() -> ApiError {
    ApiError::Unavailable(
        INFERENCE_NOT_PROVISIONED_CODE,
        "Language model access is still being set up for this tenant.".to_string(),
        None,
    )
}

/// The code of [`inference_not_provisioned`].
pub const INFERENCE_NOT_PROVISIONED_CODE: &str = "llm_inference_not_provisioned";

/// An unbound `qualify` of a tenant that is not platform-owned (the design binds no BYO provider; a binding always
/// wins): the internal inference-router (`INTERNAL_LLM_URL`), called with the TENANT's own inference-router licence
/// ([`crate::tenants::app_licence::token`]) as the bearer, platform-funded (`local_gpu`) and billed to the tenant.
/// No licence: 503 [`inference_not_provisioned`], never `INTERNAL_LLM_KEY`, which is the platform's own service
/// licence and serves the platform-owned tenant only ([`internal_resolved`]). Every other use stays unbound; so does
/// a caller without `AppState` (`ctx.st` is `None`).
async fn tenant_router(
    c: &mut PgConnection,
    ctx: ResolveCtx<'_>,
    tenant: Uuid,
    job: &JobRef,
    use_: Use,
) -> ApiResult<Option<ResolvedLlm>> {
    if use_ != Use::Qualify {
        return Ok(None);
    }
    let Some(st) = ctx.st else {
        return Ok(None);
    };
    let Some(token) = app_licence::token(st, c, tenant, App::InferenceRouter).await? else {
        metrics::counter!(RESOLVE_REFUSED_TOTAL, "code" => INFERENCE_NOT_PROVISIONED_CODE)
            .increment(1);
        tracing::warn!(tenant = %tenant, job = %job.key, "llm_inference_not_provisioned");
        return Err(inference_not_provisioned());
    };
    tracing::info!(tenant = %tenant, job = %job.key, use_ = use_.as_str(), "llm_tenant_router");
    Ok(Some(router_resolved(ctx.internal, tenant, &token)))
}

/// The inference-router lane of `tenant` under its own licence `token`.
fn router_resolved(ip: &InternalProvidersConfig, tenant: Uuid, token: &AppToken) -> ResolvedLlm {
    ResolvedLlm {
        provider_id: None,
        base_url: ip.llm_url.clone(),
        model: ip.llm_model.clone(),
        kind: Kind::Openai,
        funding: "local_gpu",
        key: Some(LlmKey(token.expose().to_string())),
        voice: None,
        effective_tenant: Some(tenant),
        actor: ip.service_actor_id,
        tenant_licence: Some(App::InferenceRouter),
    }
}

/// Counter of [`resolve`] refusals, by `code`.
pub const RESOLVE_REFUSED_TOTAL: &str = "agent_llm_resolve_refused_total";

/// A bound use's provider, checked.
struct Bound {
    provider: ProviderRow,
    model: String,
    voice: Option<String>,
}

/// The provider a design value of `use_` binds, checked: `None` = not bound (no provider id or no model); 409
/// `provider_deleted` when the provider is soft-deleted or has no row; 409 [`Use::refusal`]'s code when its kind
/// cannot serve the use.
async fn check_binding(c: &mut PgConnection, use_: Use, v: &Value) -> ApiResult<Option<Bound>> {
    let Some(pid) = v
        .get("provider_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return Ok(None);
    };
    let Some(model) = v.get("model").and_then(Value::as_str).map(str::to_string) else {
        return Ok(None);
    };
    let voice = v.get("voice").and_then(Value::as_str).map(str::to_string);
    let row = sqlx::query_as::<_, ProviderRow>(PROVIDER_ANY)
        .bind(pid)
        .fetch_optional(&mut *c)
        .await?;
    let provider = match row {
        Some(p) if p.deleted_at.is_none() => p,
        gone => {
            let name = gone.map_or_else(|| pid.to_string(), |p| p.name);
            return Err(ApiError::Conflict(
                "provider_deleted",
                format!(
                    "{} is bound to {name}, which was deleted: bind another provider and publish.",
                    use_.label()
                ),
            ));
        }
    };
    if let Some((code, why)) = use_.refusal(provider.kind()?) {
        return Err(ApiError::Conflict(
            code,
            format!("{} is bound to {}: {why}", use_.label(), provider.name),
        ));
    }
    Ok(Some(Bound {
        provider,
        model,
        voice,
    }))
}

/// One live binding of a job that cannot serve (I1/I3): the providers view lists them, T12 readiness reads them.
#[derive(Clone, Debug, Serialize)]
pub struct LiveProblem {
    pub job: String,
    #[serde(rename = "use")]
    pub use_name: &'static str,
    pub code: &'static str,
    pub message: String,
}

/// The live bindings of `job` that [`resolve`] refuses (`provider_deleted`, a kind that cannot serve the use).
pub async fn live_problems(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Vec<LiveProblem>> {
    problems(c, reg, job, &DesignOf::Live).await
}

/// The bindings of `job`'s design `of` that [`resolve`] would refuse (B3 T12 readiness reads the design it checks).
pub async fn problems(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<LiveProblem>> {
    let mut out = Vec::new();
    for u in Use::ALL {
        let v = design::value(&mut *c, reg, job, of, "llm_use", &use_ref(u.as_str())).await?;
        let Some(v) = v else {
            continue;
        };
        match check_binding(&mut *c, u, &v).await {
            Ok(_) => {}
            Err(ApiError::Conflict(code, message)) => out.push(LiveProblem {
                job: job.key.clone(),
                use_name: u.as_str(),
                code,
                message,
            }),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// One provider record by id, with its first model (plan §3, B8 generator: a provider of the platform-owned tenant,
/// read in a transaction scoped to `tenant`).
pub async fn resolve_by_id(
    c: &mut PgConnection,
    kr: Option<&Keyring>,
    tenant: Uuid,
    provider_id: Uuid,
) -> ApiResult<Option<ResolvedLlm>> {
    let Some(p) = provider(&mut *c, provider_id, Lock::None).await? else {
        return Ok(None);
    };
    let Some(model) = p.models.first().cloned() else {
        return Ok(None);
    };
    resolved(kr, tenant, &p, model, None).map(Some)
}

#[derive(sqlx::FromRow)]
struct JobKey {
    id: Uuid,
    key: String,
}

/// Every job of the transaction's tenant, by key.
pub async fn jobs(c: &mut PgConnection) -> ApiResult<Vec<JobRef>> {
    let jobs: Vec<JobKey> = sqlx::query_as("SELECT id, key FROM jobs ORDER BY key")
        .fetch_all(&mut *c)
        .await?;
    Ok(jobs
        .into_iter()
        .map(|j| JobRef {
            id: j.id,
            key: j.key,
        })
        .collect())
}

/// Lock every job of the transaction's tenant in `id` order (the one lock order: tenants → users → jobs): `FOR
/// SHARE` holds off a concurrent apply (`release::apply` locks its job `FOR UPDATE`) while a guard reads the
/// snapshots; `exclusive` takes `FOR UPDATE` (a handler that then runs `design::track_tenant`, which does too).
pub async fn lock_jobs(c: &mut PgConnection, exclusive: bool) -> ApiResult<()> {
    let sql = if exclusive {
        "SELECT id FROM jobs ORDER BY id FOR UPDATE"
    } else {
        "SELECT id FROM jobs ORDER BY id FOR SHARE"
    };
    sqlx::query(sql).fetch_all(&mut *c).await?;
    Ok(())
}

fn names(v: Option<&Value>, provider_id: &str) -> bool {
    v.and_then(|x| x.get("provider_id")).and_then(Value::as_str) == Some(provider_id)
}

/// The keys of the jobs whose LIVE design (`DesignOf::Live`: the active snapshot, or the baseline of a job never
/// released) binds any use to `provider_id`, by key. Run in a transaction scoped to the tenant.
pub async fn live_users(
    c: &mut PgConnection,
    reg: &Registry,
    provider_id: Uuid,
) -> ApiResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for h in holders(c, reg, provider_id).await? {
        if h.live && !out.contains(&h.job) {
            out.push(h.job);
        }
    }
    Ok(out)
}

/// One job + use binding a provider ([`holders`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub job: String,
    pub use_name: String,
    /// In the live design; `false` = only in an approved, unapplied cut (applying it would make it live).
    pub live: bool,
}

#[derive(sqlx::FromRow)]
struct ApprovedCut {
    job_id: Uuid,
    release_id: String,
}

/// Every job + use that binds `provider_id` in its LIVE design or in an approved, unapplied cut (`job_release.state
/// = 'approved'`: its snapshot and the drafts it bundled), in job key order. A guard (delete, kind change) locks
/// the jobs first ([`lock_jobs`]) so no apply runs between this read and its commit. Run in a transaction scoped to
/// the tenant.
pub async fn holders(
    c: &mut PgConnection,
    reg: &Registry,
    provider_id: Uuid,
) -> ApiResult<Vec<Holder>> {
    let want = provider_id.to_string();
    let jobs = jobs(&mut *c).await?;
    let cuts: Vec<ApprovedCut> = sqlx::query_as(
        "SELECT job_id, release_id FROM job_release WHERE state = 'approved' ORDER BY job_id, release_id",
    )
    .fetch_all(&mut *c)
    .await?;
    // the drafts an approved cut bundled (stored `{"v": value}`)
    let bundled: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT d.job_id, COALESCE(d.ref->>'use', '') FROM release_bundled_draft d \
           JOIN job_release r ON r.tenant_id = d.tenant_id AND r.job_id = d.job_id AND r.release_id = d.release_id \
          WHERE r.state = 'approved' AND d.kind = 'llm_use' AND d.after->'v'->>'provider_id' = $1",
    )
    .bind(&want)
    .fetch_all(&mut *c)
    .await?;
    let mut out: Vec<Holder> = Vec::new();
    let mut push = |job: &str, use_name: &str, live: bool| match out
        .iter_mut()
        .find(|h| h.job == job && h.use_name == use_name)
    {
        Some(h) => h.live |= live,
        None => out.push(Holder {
            job: job.to_string(),
            use_name: use_name.to_string(),
            live,
        }),
    };
    for jr in &jobs {
        for u in STORED_USES {
            let r = use_ref(u);
            let v = design::value(&mut *c, reg, jr, &DesignOf::Live, "llm_use", &r).await?;
            if names(v.as_ref(), &want) {
                push(jr.key.as_str(), u, true);
            }
            for cut in cuts.iter().filter(|x| x.job_id == jr.id) {
                let of = DesignOf::Release(cut.release_id.clone());
                let v = design::value(&mut *c, reg, jr, &of, "llm_use", &r).await?;
                if names(v.as_ref(), &want) {
                    push(jr.key.as_str(), u, false);
                }
            }
        }
        for (_, u) in bundled.iter().filter(|(j, _)| *j == jr.id) {
            push(jr.key.as_str(), u.as_str(), false);
        }
    }
    Ok(out)
}

/// A provider test's answer (`testLlmProvider` drops `models`).
#[derive(Clone, Debug, Serialize)]
pub struct ProbeResult {
    pub ok: bool,
    pub at: DateTime<Utc>,
    pub detail: String,
    pub models: Vec<String>,
}

impl ProbeResult {
    /// A failure without a call.
    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            at: Utc::now(),
            detail: detail.into(),
            models: Vec::new(),
        }
    }
}

/// Model names from an OpenAI-style `{"data":[{"id":…}]}` or a `{"models":[…]}` list of names or objects.
pub fn model_names(v: &Value) -> Vec<String> {
    let items = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(Value::as_array)
        .or_else(|| v.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|m| match m {
            Value::String(s) => Some(s.clone()),
            o => o
                .get("id")
                .or_else(|| o.get("name"))
                .and_then(Value::as_str)
                .map(String::from),
        })
        .filter(|n| !n.is_empty() && n.chars().count() <= 200)
        .take(MAX_MODELS)
        .collect()
}

/// `GET url` with `key` as bearer, at most [`TEST_TIMEOUT`]; `note` says which token was sent.
async fn probe(client: &reqwest::Client, url: Url, key: Option<&str>, note: &str) -> ProbeResult {
    let started = Instant::now();
    let mut req = client.get(url).timeout(TEST_TIMEOUT);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let sent =
        crate::observability::upstream_http("llm_provider", "models".to_string(), req.send()).await;
    let mut resp = match sent {
        Ok(r) => r,
        Err(e) => {
            let why = if e.is_timeout() {
                format!("no answer within {} s", TEST_TIMEOUT.as_secs())
            } else {
                "the provider could not be reached".to_string()
            };
            return ProbeResult::failed(format!("{why} ({note})"));
        }
    };
    let status = resp.status();
    let mut models = Vec::new();
    if status.is_success() {
        let mut body: Vec<u8> = Vec::new();
        let mut whole = true;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > netguard::MAX_BODY_BYTES {
                        whole = false;
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    whole = false;
                    break;
                }
            }
        }
        if whole {
            models = serde_json::from_slice::<Value>(&body)
                .map(|v| model_names(&v))
                .unwrap_or_default();
        }
    }
    let ms = started.elapsed().as_millis();
    let mut detail = format!(
        "HTTP {} · {} models in {ms} ms ({note})",
        status.as_u16(),
        models.len()
    );
    if status.as_u16() == 401 || status.as_u16() == 403 {
        detail.push_str(": the key was rejected");
    }
    ProbeResult {
        ok: status.is_success(),
        at: Utc::now(),
        detail,
        models,
    }
}

/// Probe a tenant-entered provider: the models URL passes `providers::check_url` (422 `invalid_base_url` /
/// `invalid_target`) and is called through `netguard::pinned_client` (one lookup, every answer public, pinned, no
/// redirects). A name resolving to a non-public address is 422 `invalid_target`; one that does not resolve is
/// `ok: false`.
pub async fn probe_tenant(
    policy: &Policy,
    tenant: Uuid,
    kind: Kind,
    base_url: &str,
    key: Option<&str>,
    note: &str,
) -> ApiResult<ProbeResult> {
    let url = check_url(
        &kind.models_url(base_url),
        policy,
        tenant,
        UrlField::BaseUrl,
    )?;
    let client = match netguard::pinned_client(&url, policy, tenant, TEST_TIMEOUT).await {
        Ok(c) => c,
        Err(e) if e.code == "invalid_target" => {
            return Err(ApiError::Unprocessable("invalid_target", e.detail));
        }
        Err(e) => return Ok(ProbeResult::failed(format!("{} ({note})", e.detail))),
    };
    Ok(probe(&client, url, key, note).await)
}

/// Probe the internal pgx vLLM (D70) with the platform service key `INTERNAL_LLM_KEY` (config URL, not netguard), on
/// a client that follows no redirect (the service key goes to that URL only).
pub async fn probe_internal(st: &AppState) -> ProbeResult {
    let ip = &st.cfg.internal_providers;
    let Some(key) = ip.llm_key.as_deref() else {
        return ProbeResult::failed(
            "422 key_required: the platform service key (INTERNAL_LLM_KEY) is not configured.",
        );
    };
    let Ok(url) = Url::parse(&Kind::Openai.models_url(&ip.llm_url)) else {
        return ProbeResult::failed("the internal LLM URL (INTERNAL_LLM_URL) is not a URL");
    };
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TEST_TIMEOUT)
        .build()
    else {
        return ProbeResult::failed("the internal LLM client could not be built");
    };
    probe(&client, url, Some(key), "token: platform service key").await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// agent's service account (`SERVICE_ACTOR_ID`) in these tests.
    const SERVICE_ACTOR: Uuid = Uuid::from_u128(0x0195_f48b_d000_7000_8000_0000_0000_00aa);

    fn ip(laya_key: Option<&str>) -> InternalProvidersConfig {
        InternalProvidersConfig {
            pgx_url: None,
            llm_url: "http://vllm.example.com:8001/v1".into(),
            llm_model: "local-llm".into(),
            llm_key: Some("svc-key".into()),
            mlx_text_url: None,
            mlx_media_url: None,
            laya_url: "https://api.nonagentic.ai/laya".into(),
            laya_model: "jev-latest".into(),
            laya_key: laya_key.map(String::from),
            service_actor_id: Some(SERVICE_ACTOR),
        }
    }

    #[test]
    fn the_internal_model_is_billed_to_the_tenant() {
        let tenant = Uuid::new_v4();
        for u in [Use::Qualify, Use::Generator] {
            let r = internal_resolved(&ip(None), tenant, u).unwrap();
            assert_eq!(r.funding, "local_gpu");
            assert_eq!(r.effective_tenant, Some(tenant), "{u:?}");
            assert_eq!(r.actor, Some(SERVICE_ACTOR), "{u:?}");
        }
    }

    #[test]
    fn laya_gates_carry_no_effective_tenant() {
        let r = internal_resolved(&ip(Some("laya-tok")), Uuid::new_v4(), Use::Gates).unwrap();
        assert_eq!(r.kind, Kind::Laya);
        assert_eq!(r.effective_tenant, None);
        assert_eq!(r.actor, None);
    }

    #[test]
    fn a_tenant_provider_never_gets_our_tenant_id() {
        let tenant = Uuid::new_v4();
        let p = ProviderRow {
            id: Uuid::new_v4(),
            name: "byo".into(),
            kind: "openai".into(),
            base_url: "https://llm.example.com/v1".into(),
            models: vec!["m".into()],
            region: None,
            secret_sealed: None,
            secret_fpr: None,
            secret_updated_at: None,
            last_test: None,
            updated_at: Utc::now(),
            updated_by: "t".into(),
            deleted_at: None,
        };
        let r = resolved(None, tenant, &p, "m".into(), None).unwrap();
        assert_eq!(r.funding, "tenant_byo");
        assert_eq!(r.effective_tenant, None);
        assert_eq!(r.actor, None);
    }

    #[test]
    fn a_person_replaces_the_service_actor_on_the_platform_lane_only() {
        let person = Uuid::new_v4();
        let p = person.to_string();
        let platform = internal_resolved(&ip(None), Uuid::new_v4(), Use::Qualify).unwrap();
        assert_eq!(
            platform.clone().acting_for(Some(p.as_str())).actor,
            Some(person)
        );
        assert_eq!(
            platform.clone().acting_for(Some("u-super")).actor,
            Some(SERVICE_ACTOR),
            "a sub that is no uuid keeps the service account"
        );
        assert_eq!(platform.acting_for(None).actor, Some(SERVICE_ACTOR));
        let laya = internal_resolved(&ip(Some("laya-tok")), Uuid::new_v4(), Use::Gates).unwrap();
        assert_eq!(
            laya.acting_for(Some(p.as_str())).actor,
            None,
            "not a router call"
        );
    }

    #[test]
    fn the_tenant_level_project_id_reverses_the_hex_digits() {
        let tenant = Uuid::parse_str("0195f48b-d000-7000-8000-000000000100").unwrap();
        let project = tenant_level_project_id(tenant);
        assert_eq!(project.to_string(), "00100000-0000-0008-0007-000db84f5910");
        let hex = tenant.simple().to_string();
        assert_eq!(
            project.simple().to_string(),
            hex.chars().rev().collect::<String>()
        );
        for t in [tenant, Uuid::new_v4(), Uuid::now_v7(), Uuid::max()] {
            assert_eq!(
                tenant_level_project_id(tenant_level_project_id(t)),
                t,
                "{t}"
            );
        }
    }

    #[test]
    fn the_actor_is_the_person_then_the_service_account_then_none() {
        let person = Uuid::new_v4();
        let p = person.to_string();
        assert_eq!(
            caller_actor(Some(p.as_str()), Some(SERVICE_ACTOR)),
            Some(person)
        );
        assert_eq!(caller_actor(Some(p.as_str()), None), Some(person));
        // a sub that is not a uuid (a service-account name), a nil one, or none: the service account
        for sub in [
            Some("service-account-agent"),
            Some(""),
            Some("00000000-0000-0000-0000-000000000000"),
            None,
        ] {
            assert_eq!(
                caller_actor(sub, Some(SERVICE_ACTOR)),
                Some(SERVICE_ACTOR),
                "{sub:?}"
            );
            assert_eq!(caller_actor(sub, None), None, "{sub:?}");
        }
        assert_eq!(
            caller_actor(None, Some(Uuid::nil())),
            None,
            "never a nil actor"
        );
    }

    #[test]
    fn the_router_identity_is_exactly_four_headers() {
        let tenant = Uuid::parse_str("0195f48b-d000-7000-8000-000000000100").unwrap();
        assert_eq!(
            router_identity_headers(tenant, Some(SERVICE_ACTOR)),
            vec![
                ("X-Effective-Tenant-Id", tenant.to_string()),
                ("X-App-Id", "agent".to_string()),
                (
                    "X-Project-Id",
                    "00100000-0000-0008-0007-000db84f5910".to_string()
                ),
                ("X-Actor-Id", SERVICE_ACTOR.to_string()),
            ]
        );
        let without = router_identity_headers(tenant, None);
        assert_eq!(without.len(), 3);
        assert!(without.iter().all(|(k, _)| *k != ACTOR_ID_HEADER));
        assert_eq!(router_identity_headers(tenant, Some(Uuid::nil())), without);
    }

    /// What one request to a fake upstream carried: the four identity headers, `None` when absent.
    type Identity = [Option<String>; 4];

    async fn identity_upstream() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Identity>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/x",
            axum::routing::get(move |h: axum::http::HeaderMap| {
                let seen = s2.clone();
                async move {
                    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(String::from);
                    seen.lock().unwrap().push([
                        get("x-effective-tenant-id"),
                        get("x-app-id"),
                        get("x-project-id"),
                        get("x-actor-id"),
                    ]);
                    "ok"
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}/x"), seen)
    }

    // Guards `platform_client`: the identity rides on every request of a tenant-billed client, and a client with no
    // tenant (never built for a third party, but the same function) sends none of it.
    #[tokio::test]
    async fn the_platform_client_carries_the_identity_only_when_billed_to_a_tenant() {
        let (url, seen) = identity_upstream().await;
        let tenant = Uuid::new_v4();
        let billed =
            platform_client(Duration::from_secs(5), Some(tenant), Some(SERVICE_ACTOR)).unwrap();
        billed.get(&url).send().await.unwrap();
        let bare = platform_client(Duration::from_secs(5), None, Some(SERVICE_ACTOR)).unwrap();
        bare.get(&url).send().await.unwrap();
        let g = seen.lock().unwrap();
        assert_eq!(
            g[0],
            [
                Some(tenant.to_string()),
                Some("agent".to_string()),
                Some(tenant_level_project_id(tenant).to_string()),
                Some(SERVICE_ACTOR.to_string()),
            ]
        );
        assert_eq!(g[1], [None, None, None, None]);
    }
}
