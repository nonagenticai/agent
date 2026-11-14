//! Environment. Fail loud: a required variable that is missing, or any variable that is set but malformed,
//! stops the process at boot with its NAME (never its value).
//!
//! The names mirror strata_configurator's api (gitops-production `clusters/strato/services/agent/api.yaml`, PR #4783).
//! Always required: `DATABASE_URL` (runtime role `agent_app`, under RLS), `MIGRATE_DATABASE_URL` (the owner,
//! migrations only), `KEYCLOAK_ISSUER`, `KEYCLOAK_AUDIENCE`. Optional: `PORT` (default 8098; `AGENT_API_BIND`
//! overrides the whole address), `KEYCLOAK_JWKS_URL` (default `<issuer>/protocol/openid-connect/certs`),
//! `SUPER_ADMIN_ROLE` (default `super_admin`), `KEYCLOAK_CLIENT_ID` (the `azp` a person token must carry;
//! default `KEYCLOAK_AUDIENCE`), `METRICS_BIND` (default `0.0.0.0:9098`, the Prometheus listener),
//! `LOG_FORMAT` (`json`, the default, or `pretty`).
//!
//! Realm per app (runbook `realm-per-app-agent-runbook.md` A1; after commonpractice #480). Every default reproduces
//! the single-issuer behaviour: `KEYCLOAK_TRUSTED_ISSUERS` (CSV of full issuer URLs whose tokens verify, each against
//! its own JWKS with `iss` pinned to that exact value; default `[KEYCLOAK_ISSUER]`, which must be in it),
//! `KEYCLOAK_SERVICE_ONLY_REALMS` (default `secure-apps`: a token from these realms is refused on every PERSON route
//! and verifies only on the service-account doors), `SIGNUP_INTAKE_ISSUER` / `SIGNUP_DECIDER_ISSUER` (the ONE issuer
//! each signup door accepts its client from; default `KEYCLOAK_ISSUER`; boot refuses a pin outside the trusted list).
//! ` `REDIS_URL` + `AGENT_PREFIX`: the B4 runtime bus (optional). `LICENSE_GATE_2_ENABLED` (default `false`):
//! licence gate 2 at tenant-context resolution (spec 04 §4b, crate::licensing::gate::check_request) and
//! `enforced` in `GET /api/license/status`; with it on, agent-api refuses to boot without `LICENSE_INTROSPECT_URL`
//! (no verdict would ever be written, so every tenant would be refused). `SOURCE_SCHEDULE_ENABLED=true` is refused
//! (`source_schedule_disabled_by_policy`, D82: scheduled source re-sync is built and switched off).
//!
//! Identity (B1 T4, [`IdentityConfig`]) is ALWAYS required, signup on or off: tenant admin (create, invite,
//! add-by-sub) and the licence storage of D77 use the same Keycloak admin and licences clients as the signup
//! approval. `KEYCLOAK_ADMIN_{TOKEN_URL,CLIENT_ID,CLIENT_SECRET,API_URL}`, `LICENSES_ADMIN_{TOKEN_URL,CLIENT_ID,
//! CLIENT_SECRET,URL}`, the licence-token keys (`LICENSE_TOKEN_ENC_KEYS` + `LICENSE_TOKEN_ENC_KEY_CURRENT`, or
//! transitionally the legacy `LICENSE_TOKEN_ENC_KEY` alone: crate::crypto::licence), `AGENT_PUBLIC_URL`; optional `SIGNUP_FRONTEND_CLIENT_ID`
//! (default `agent`), `INVITE_EMAIL_LIFESPAN_SECONDS` (default 7 days), `INVITE_RESEND_COOLDOWN_SECONDS` (default 600,
//! at least 60), `LICENCES_REVOKE_TIMEOUT_MS` (default 3000, 100 to 8000), `LICENSE_APP_KEY`, `LICENSE_PLAN_CODE`,
//! `LICENSE_SUBJECT_REALM`. The B4 runtime daemons read none of it (B4 F1): orchestrator and interaction load
//! [`RuntimeConfig`], the runner [`RunnerConfig`] (this config with an inert identity and no signup or owner login).
//!
//! Early-access signup is DARK unless `EARLY_ACCESS_SIGNUP_ENABLED=true`: every `/api/signup-approvals` route is
//! then the unknown-route 404, so the app deploys before intake is live. Once it is on, its intake/decider
//! settings apply (see [`SignupConfig`]); Telegram is optional but all-or-nothing.
//!
//! Credential custody (B7 T8) is OPTIONAL: `TENANT_DATA_KEYS` (JSON `{kid: base64 32 bytes}`) +
//! `TENANT_DATA_KEY_CURRENT` (crate::crypto::keyring). Both unset → `tenant_data_keys: None` and custody answers 503
//! `tenant_data_key_unconfigured`; one without the other, or any malformed part, fails the boot.
//!
//! Connector outbound policy (B7 T6/T9) is OPTIONAL: `CONNECTOR_PRIVATE_TARGETS` (JSON array of
//! `{host, port, scheme, tenants}`, crate::netguard::Policy) admits exact operator-set private targets; unset or empty
//! = none. Malformed fails the boot naming the variable.
//!
//! Connector template sandboxes (B7 T7, T2 runs) are OPTIONAL: `CONNECTOR_SANDBOXES` (JSON object
//! `{"<template key>": {base_url, env, probes}}`, crate::connectors::t2) — `env` names the variable that holds the
//! sandbox credential, read at run time. Unset or empty = none (a T2 run answers 409 `sandbox_not_configured`);
//! malformed fails the boot naming the variable.
//!
//! The interaction API's interim backend (crate::interaction, [`InteractionLlmConfig`]) is optional and
//! all-or-nothing like Telegram: on only when `INTERACTION_LLM_URL` and `INTERACTION_LLM_TOKEN` are both set (else
//! every interaction is 409 `provider_not_configured` and the missing names are logged at boot), and then it serves
//! ONLY the tenants in `INTERACTION_LLM_TENANTS` (comma-separated uuids; empty or unset: nobody), tried first; every
//! other tenant is served by its job's own `qualify` provider (B3 T4d, crate::interaction::ProviderBackend), else 409
//! `provider_not_configured`.
//! `INTERACTION_LLM_MODEL` (default `local-llm`), `INTERACTION_LLM_TIMEOUT_S` (default 120, at least 1). A malformed
//! URL, timeout or tenant uuid fails the boot by name. The list gates the passthrough ONLY: the runtime's chat
//! turns no longer have a tenant gate (an unbound `qualify` of any tenant uses the inference-router under that
//! tenant's own licence; `INTERNAL_LLM_KEY` is the platform tenant's alone).
//!
//! The voice connector's interim platform speech gateway (crate::voice, [`VoiceConfig`]) is optional and
//! all-or-nothing too: on only when `VOICE_ASR_URL`, `VOICE_TTS_URL` and `VOICE_TOKEN` are all set (else the missing
//! names are logged at boot), and then it serves ONLY the tenants in `VOICE_PLATFORM_TENANTS` (comma-separated
//! uuids), tried first; every other tenant's voice turn uses its job's own `asr` + `tts` providers (B3 T4d,
//! crate::voice::ProviderSpeech), else 409 `provider_not_configured`. `VOICE_PROVIDER_KIND` `w2v` (default) or `openai_compatible`; `VOICE_ASR_MODEL` (default
//! `whisper-1`) and `VOICE_TTS_MODEL` (default `tts-1`, both openai_compatible only); `VOICE_TTS_VOICE` (default
//! `af_heart` for w2v, `alloy` for openai_compatible); `VOICE_TIMEOUT_S` (default 60, at least 1). A malformed kind,
//! URL, timeout or tenant uuid fails the boot by name.
//!
//! Async interactions with signed webhook callbacks (crate::interaction::webhook, [`InteractionWebhookConfig`]) are on
//! only when `INTERACTION_WEBHOOK_KEY` (the platform signing master key, at least 32 characters) is set; unset, a
//! `callback_url` is 409 `webhooks_not_configured`. A set but shorter key fails the boot by name.
//!
//! The internal seam to agent-edge (crate::conversations, B12 §3, [`EdgeConfig`]): `EDGE_INTERNAL_KEY` (at least 32
//! characters; unset, every `/api/internal/channel/*` route is 503 `edge_not_configured`) and `EDGE_DELIVER_URL` (the
//! edge's base URL; unset, staff replies are stored but not delivered). Channel-binding credentials are sealed under
//! the tenant data key (crate::connectors::custody, `TENANT_DATA_KEYS`). `WEBCHAT_TURNSTILE_SITE_KEY` +
//! `WEBCHAT_TURNSTILE_SECRET` (both or neither; one alone fails the boot): the platform Turnstile widget of web chat
//! sessions (B14 §2).
//! Knowledge + providers (B3, [`StrataConfig`], [`InternalProvidersConfig`]) are OPTIONAL. Provider secrets:
//! `PROVIDER_SECRET_KEYS` + `PROVIDER_SECRET_KEY_CURRENT` (crate::crypto::keyring `Purpose::ProviderSecret`; unset →
//! 503 `provider_secret_key_unconfigured`, malformed fails the boot by name). strata: `STRATA_RAG_URL` /
//! `STRATA_INGEST_URL` with their per-tenant static tokens `STRATA_RAG_TOKENS` / `STRATA_INGEST_TOKENS`
//! (`<tenant-uuid>:<token>,…`, no any-tenant token); a URL without its tokens fails the boot, a malformed entry fails it
//! naming the variable and the entry's position only. Internal providers (D70): `INTERNAL_LLM_URL` (default
//! `http://localhost:8001/v1`), `INTERNAL_LLM_MODEL` (default `local-llm`), `INTERNAL_LLM_KEY` (optional),
//! `INTERNAL_MLX_TEXT_URL` / `INTERNAL_MLX_MEDIA_URL` / `INTERNAL_PGX_VLLM_URL` (optional, display only),
//! `INTERNAL_LAYA_URL` (default
//! `https://api.nonagentic.ai/laya`), `INTERNAL_LAYA_MODEL` (default `jev-latest`), `INTERNAL_LAYA_KEY` (optional; the
//! platform tenant's gates, B5 T9). `SERVICE_ACTOR_ID` (optional, a non-nil uuid, else the boot fails by name): agent's
//! service-account `sub`, the `X-Actor-Id` of inference-router and strata_rag calls no person acts in.
//! `AGENT_PLATFORM_TENANT_ID` (optional
//! uuid, not nil): the D70 platform-owned tenant.
//! Licence verdicts (B7 T2, crate::licensing::verdict): `LICENSE_INTROSPECT_URL` (optional; the FULL licences
//! endpoint, `<licences>/license/introspect`). Unset: no verdict is written and the refresher idles (logged at boot);
//! not an http(s) URL fails the boot by name.
use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::connectors::t2::{self, Sandboxes};
use crate::crypto::keyring::{KeySet, Purpose};
use crate::crypto::licence::{LEGACY_KEY_VAR, LicenceKeys};
use crate::http_base::BaseUrl;
use crate::netguard::Policy;
use crate::observability::{DEFAULT_METRICS_BIND, LogFormat, ObsConfig};

pub const DEFAULT_PORT: u16 = 8098;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    /// `DATABASE_URL`: the runtime role (`agent_app`: NOSUPERUSER, NOBYPASSRLS, owns no table), so row-level
    /// security applies to every query the API serves. Startup refuses a role that bypasses RLS.
    pub database_url: String,
    /// `MIGRATE_DATABASE_URL`: the database owner. Used only to run migrations at startup, then closed.
    pub migrate_database_url: String,
    pub keycloak_issuer: String,
    pub keycloak_audience: String,
    /// `KEYCLOAK_CLIENT_ID`: the web client a PERSON token must have been issued to (`azp`). Default
    /// `KEYCLOAK_AUDIENCE`. Without it, another portal app's token (fullScopeAllowed) would be accepted.
    pub keycloak_client_id: String,
    pub keycloak_jwks_url: String,
    /// `KEYCLOAK_TRUSTED_ISSUERS`: every issuer whose tokens verify (exact strings, no trailing `/`). Default
    /// `[KEYCLOAK_ISSUER]`; always contains `keycloak_issuer`. `keycloak_jwks_url` is the JWKS of `keycloak_issuer`;
    /// every other issuer's is `<issuer>/protocol/openid-connect/certs`.
    pub keycloak_trusted_issuers: Vec<String>,
    /// `KEYCLOAK_SERVICE_ONLY_REALMS` (default `secure-apps`): realms whose tokens verify only on the service-account
    /// doors. ⛔ secure-apps holds human accounts too, so its person tokens must never open a person route.
    pub keycloak_service_only_realms: Vec<String>,
    /// `SUPER_ADMIN_ROLE`: the client role (on `KEYCLOAK_AUDIENCE`) that makes a person super_admin.
    pub super_admin_role: String,
    /// Keycloak admin, licences admin, the token key and the app URL (always required).
    pub identity: IdentityConfig,
    /// `None` = signup is dark.
    pub signup: Option<SignupConfig>,
    /// `TENANT_OFFBOARD_SA_{ISSUER,AZP,ROLE}`: the ONE service account that may call `POST /api/tenants/{id}/offboard`
    /// besides a human super_admin. `None` (all three unset) = closed: only a person super_admin offboards.
    pub tenant_offboard_service: Option<OffboardServiceConfig>,
    /// The signup launch prerequisites by NAME and whether each is set (never a value), read whether signup is on
    /// or not (`GET /api/platform/signup-launch`).
    pub signup_launch: Vec<(&'static str, bool)>,
    /// `LOG_FORMAT`, `METRICS_BIND`.
    pub obs: ObsConfig,
    /// `TENANT_DATA_KEYS` + `TENANT_DATA_KEY_CURRENT`: the KEKs of the per-tenant data keys (B7 T8). `None` = unset.
    pub tenant_data_keys: Option<KeySet>,
    /// `CONNECTOR_PRIVATE_TARGETS`: the connector outbound policy (empty = public https hosts only).
    pub connector_policy: Policy,
    /// `CONNECTOR_SANDBOXES`: the vendor sandboxes T2 runs against, by template key (empty = none).
    pub connector_sandboxes: Sandboxes,
    /// `None` (interactions answer 409 `provider_not_configured`) unless `INTERACTION_LLM_URL` and `INTERACTION_LLM_TOKEN` are both set.
    pub interaction_llm: Option<InteractionLlmConfig>,
    /// The interaction LLM variables that are missing (names only), for the boot log.
    pub interaction_llm_missing: Vec<&'static str>,
    /// `None` (no tenant has a speech provider: 409) unless `VOICE_ASR_URL`, `VOICE_TTS_URL` and `VOICE_TOKEN` are all
    /// set.
    pub voice: Option<VoiceConfig>,
    /// The voice variables that are missing (names only), for the boot log.
    pub voice_missing: Vec<&'static str>,
    /// `None` (a `callback_url` is 409 `webhooks_not_configured`) unless `INTERACTION_WEBHOOK_KEY` is set.
    pub interaction_webhook: Option<InteractionWebhookConfig>,
    /// The agent-edge seam (crate::conversations::internal, crate::conversations::edge_link).
    pub edge: EdgeConfig,
    /// `ATTACHMENT_QUOTA_BYTES` (SW-9): the live attachment bytes one tenant may hold (default 1 GiB,
    /// crate::attachments::store::DEFAULT_QUOTA_BYTES); over it an upload is 413 `attachment_quota`.
    pub attachment_quota_bytes: i64,
    /// `ARTIFACTS_URL` / `ARTIFACTS_TIMEOUT_SECS` (SW-9): the platform artifact_service that holds attachment bytes
    /// (crate::attachments::artifacts).
    pub artifacts: crate::attachments::artifacts::ArtifactsConfig,
    /// `SOURCE_SCHEDULE_ENABLED` (B3 T8b, D82): scheduled source re-sync. Default `false`; boot refuses `true`
    /// (`source_schedule_disabled_by_policy`) until the runbook `docs/runbooks/source-schedule-activation.md` lifts it.
    pub source_schedule_enabled: bool,
    /// `LICENSE_GATE_2_ENABLED` (spec 04 §4b, C1): licence gate 2 at tenant-context resolution on the stored verdict
    /// (crate::licensing::gate::check_request). Default `false` = unchanged behaviour (the per-turn gate still runs).
    pub license_gate_2_enabled: bool,
    /// `PROVIDER_SECRET_KEYS` + `PROVIDER_SECRET_KEY_CURRENT`: the keys provider/source secrets are sealed under (B3).
    /// `None` = unset.
    pub provider_secret_keys: Option<KeySet>,
    /// strata_rag / strata_ingest base URLs and per-tenant static tokens (B3; interim until W5 A4).
    pub strata: StrataConfig,
    /// The bundle-token minter (crate::tenants::app_licence): `None` = unset, so a tenant's inference-router /
    /// strata call fails closed. Read by EVERY binary that calls those upstreams for a tenant (api, orchestrator,
    /// runner, interaction).
    pub bundle_minter: Option<BundleMinterConfig>,
    /// The platform's internal providers (D70).
    pub internal_providers: InternalProvidersConfig,
    /// `AGENT_PLATFORM_TENANT_ID`: the D70 platform-owned tenant (`None` = unset).
    pub platform_tenant_id: Option<uuid::Uuid>,
    /// `LICENSE_INTROSPECT_URL`: licences' `POST /license/introspect` (B7 T2). `None` = verdicts are not refreshed.
    pub license_introspect_url: Option<String>,
    /// `MCP_URL` (ends in `/mcp`) + `MCP_TASK_TOOLS`: the nonagenticai MCP server (crate::mcp_client). `None` while
    /// `MCP_URL` is unset: MCP-transport connectors answer `connector_misconfigured` and the MCP routes 503.
    pub mcp: Option<McpConfig>,
    /// `REDIS_URL` + `AGENT_PREFIX`: the runtime bus (B4, crate::runtime). `None` while `REDIS_URL` is unset: the
    /// runtime is dark and agent-api still boots.
    pub redis: Option<RedisConfig>,
    /// `METERING_EMIT_ENABLED` + `METERING_REDIS_URL` (B4 T11, crate::usage): the outbox and its drainer to billing.
    /// Default off (K52): the ledger is written regardless.
    pub metering: crate::usage::MeteringConfig,
    /// `GENERATOR_PROVIDER_ID` + `GENERATOR_MODEL` + `GENERATOR_DRAFT_TIMEOUT_SECS` (B8, crate::generator): the
    /// platform journey generator's provider. Unset provider: platform drafting answers 503 `unavailable`.
    pub generator: crate::generator::GeneratorConfig,
}

/// The runtime bus (B4). `Debug` never prints the URL (it may carry a password).
#[derive(Clone)]
pub struct RedisConfig {
    /// `REDIS_URL`: `redis://[user:password@]host:port[/db]`.
    pub url: String,
    /// `AGENT_PREFIX`: the environment prefix of every key (default empty). No whitespace.
    pub prefix: String,
}

impl std::fmt::Debug for RedisConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConfig")
            .field("url", &"[redacted]")
            .field("prefix", &self.prefix)
            .finish()
    }
}

/// `REDIS_URL` (optional) and `AGENT_PREFIX`. A malformed value fails the boot by name, never echoing the URL.
fn redis_from(trimmed: &impl Fn(&str) -> Option<String>) -> Result<Option<RedisConfig>> {
    let prefix = trimmed("AGENT_PREFIX").unwrap_or_default();
    if prefix.chars().any(char::is_whitespace) {
        anyhow::bail!("AGENT_PREFIX may not contain whitespace");
    }
    let Some(url) = trimmed("REDIS_URL") else {
        return Ok(None);
    };
    let ok = reqwest::Url::parse(&url)
        .is_ok_and(|u| u.scheme() == "redis" && u.host_str().is_some_and(|h| !h.is_empty()));
    if !ok {
        anyhow::bail!("REDIS_URL must be a redis://host:port URL");
    }
    Ok(Some(RedisConfig { url, prefix }))
}

/// A per-tenant static token map (`<tenant-uuid>:<token>,…`). `Debug` lists the tenants, never a token.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TenantTokens(BTreeMap<uuid::Uuid, String>);

impl std::fmt::Debug for TenantTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantTokens")
            .field("tenants", &self.0.keys().collect::<Vec<_>>())
            .field("tokens", &"[redacted]")
            .finish()
    }
}

impl TenantTokens {
    /// Parses `var`'s value: comma-separated `<tenant-uuid>:<token>` entries (blanks dropped). A malformed entry, a
    /// nil tenant, a tenant listed twice or a token shared by two entries is an error naming `var` and the entry's
    /// 1-based position, never its value.
    pub fn parse(var: &str, raw: &str) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        let mut seen = std::collections::BTreeSet::new();
        for (i, entry) in split_list(raw).iter().enumerate() {
            let n = i + 1;
            let (tenant, token) = entry
                .split_once(':')
                .ok_or_else(|| format!("{var}: entry {n} is not <tenant-uuid>:<token>"))?;
            let tenant = uuid::Uuid::parse_str(tenant.trim())
                .map_err(|_| format!("{var}: entry {n} does not start with a tenant uuid"))?;
            if tenant.is_nil() {
                return Err(format!("{var}: entry {n} names the nil tenant"));
            }
            let token = token.trim();
            if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(format!(
                    "{var}: entry {n} has an empty or non-printable token"
                ));
            }
            if !seen.insert(token.to_string()) {
                return Err(format!(
                    "{var}: entry {n} repeats the token of an earlier entry"
                ));
            }
            if map.insert(tenant, token.to_string()).is_some() {
                return Err(format!(
                    "{var}: entry {n} repeats the tenant of an earlier entry"
                ));
            }
        }
        Ok(Self(map))
    }

    /// The token pinned to `tenant`.
    pub fn get(&self, tenant: uuid::Uuid) -> Option<&str> {
        self.0.get(&tenant).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// strata_rag and strata_ingest (B3): in-cluster base URLs (no trailing slash) and the per-tenant static tokens
/// (spec 06 §2, interim until W5 A4). A tenant without a token pair is 409 `provision_pending` on the data plane.
#[derive(Clone, Debug, Default)]
pub struct StrataConfig {
    /// `STRATA_RAG_URL`.
    pub rag_url: Option<String>,
    /// `STRATA_INGEST_URL`.
    pub ingest_url: Option<String>,
    /// `STRATA_RAG_TOKENS`.
    pub rag_tokens: TenantTokens,
    /// `STRATA_INGEST_TOKENS`.
    pub ingest_tokens: TenantTokens,
    /// `SERVICE_ACTOR_ID` ([`service_actor_from`]): strata_rag's `X-Actor-Id` when no person's `sub` is in scope.
    pub service_actor_id: Option<uuid::Uuid>,
}

/// The bundle-token minter (mirrors nonagenticai_mcp_server `licence_minter_*`): the Keycloak client-credentials
/// client `agent-bundle-minter` and the licences base URL it mints short-lived tenant bundle tokens from.
/// All-or-nothing: `BUNDLE_MINTER_LICENSES_URL`, `BUNDLE_MINTER_TOKEN_URL` and `BUNDLE_MINTER_CLIENT_SECRET` are set
/// together or not at all (a partial set fails the boot by name).
#[derive(Clone)]
pub struct BundleMinterConfig {
    /// `BUNDLE_MINTER_LICENSES_URL`, no trailing slash.
    pub licenses_url: String,
    /// `BUNDLE_MINTER_TOKEN_URL`: the Keycloak token endpoint.
    pub token_url: String,
    /// `BUNDLE_MINTER_CLIENT_ID` (default `agent-bundle-minter`).
    pub client_id: String,
    /// `BUNDLE_MINTER_CLIENT_SECRET`.
    pub client_secret: String,
    /// `LICENSE_BUNDLE_KEY` (default `agent`): the bundle the tokens are minted in.
    pub bundle: String,
    /// `BUNDLE_TOKEN_TTL_MINUTES` (default 15, 1 to 60).
    pub ttl_minutes: u32,
}

impl std::fmt::Debug for BundleMinterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundleMinterConfig")
            .field("licenses_url", &self.licenses_url)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("bundle", &self.bundle)
            .field("ttl_minutes", &self.ttl_minutes)
            .finish()
    }
}

/// The default `INTERNAL_LLM_URL` when the environment does not set one.
pub const DEFAULT_INTERNAL_LLM_URL: &str = "http://localhost:8001/v1";

/// The default `INTERNAL_LAYA_URL`: Laya behind the public gateway (license-authz binds `/laya/` to app `laya`); no pod
/// can dial the pgx bridge directly (B5 "Laya — how it is called").
pub const DEFAULT_INTERNAL_LAYA_URL: &str = "https://api.nonagentic.ai/laya";

/// The platform's internal providers (D70): config only, never from a request, never through netguard.
#[derive(Clone)]
pub struct InternalProvidersConfig {
    /// `INTERNAL_LLM_URL` (default [`DEFAULT_INTERNAL_LLM_URL`]), no trailing slash.
    pub llm_url: String,
    /// `INTERNAL_LLM_MODEL` (default `local-llm`).
    pub llm_model: String,
    /// `INTERNAL_LLM_URL` is the inference-router. `INTERNAL_LLM_KEY`: the PLATFORM's own service licence for it
    /// (optional; the platform-owned tenant's calls, platform background work and the internal probe). Any other
    /// tenant calls the router with its own licence (crate::tenants::app_licence), never this key.
    pub llm_key: Option<String>,
    /// `INTERNAL_MLX_TEXT_URL` (display only).
    pub mlx_text_url: Option<String>,
    /// `INTERNAL_MLX_MEDIA_URL` (display only).
    pub mlx_media_url: Option<String>,
    /// `INTERNAL_PGX_VLLM_URL` (display only): the platform's own vLLM behind the `pgx_vllm` kind.
    pub pgx_url: Option<String>,
    /// `INTERNAL_LAYA_URL` (default [`DEFAULT_INTERNAL_LAYA_URL`]), no trailing slash: the platform tenant's gates.
    pub laya_url: String,
    /// `INTERNAL_LAYA_MODEL` (default `jev-latest`).
    pub laya_model: String,
    /// `INTERNAL_LAYA_KEY`: the platform tenant's `laya` licence token (Secret `agent-laya`, B5 T13a), sent as
    /// `Authorization: Bearer`. Unset: the platform tenant's gates are unbound (every question `skipped`).
    pub laya_key: Option<String>,
    /// `SERVICE_ACTOR_ID` ([`service_actor_from`]): the inference-router `X-Actor-Id` of a platform call with no
    /// person's `sub` in scope (crate::providers::llm::caller_actor). Unset: the header is omitted on those calls.
    pub service_actor_id: Option<uuid::Uuid>,
}
impl std::fmt::Debug for InternalProvidersConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalProvidersConfig")
            .field("llm_url", &self.llm_url)
            .field("llm_model", &self.llm_model)
            .field("llm_key", &self.llm_key.as_ref().map(|_| "[redacted]"))
            .field("mlx_text_url", &self.mlx_text_url)
            .field("mlx_media_url", &self.mlx_media_url)
            .field("pgx_url", &self.pgx_url)
            .field("laya_url", &self.laya_url)
            .field("laya_model", &self.laya_model)
            .field("laya_key", &self.laya_key.as_ref().map(|_| "[redacted]"))
            .field("service_actor_id", &self.service_actor_id)
            .finish()
    }
}

/// `EDGE_INTERNAL_KEY` / `EDGE_DELIVER_URL`. `Debug` shows no key.
#[derive(Clone, Default)]
pub struct EdgeConfig {
    /// `EDGE_INTERNAL_KEY`: the bearer both directions carry (at least [`MIN_EDGE_KEY_CHARS`]).
    pub internal_key: Option<String>,
    /// `EDGE_DELIVER_URL`, without a trailing slash (`http://agent-edge:8099`).
    pub deliver_url: Option<String>,
    /// `WEBCHAT_PUBLIC_BASE` (B13 §4), without a trailing slash: the edge's public base a web chat binding's `snippet`
    /// loads `/webchat/widget.js` from. Unset → `https://chat.nonagentic.ai`
    /// (crate::conversations::bindings::DEFAULT_WEBCHAT_PUBLIC_BASE).
    pub webchat_public_base: Option<String>,
    /// Web chat session Turnstile (B14 §2).
    pub webchat_turnstile: WebchatTurnstileConfig,
}

/// Web chat session Turnstile (B14 §2): tokens are verified at `TURNSTILE_VERIFY_URL` within `TURNSTILE_TIMEOUT_MS`
/// (shared with the signup intake), for the platform widget and a binding's own alike.
#[derive(Clone, Debug, Default)]
pub struct WebchatTurnstileConfig {
    /// `WEBCHAT_TURNSTILE_SITE_KEY` + `WEBCHAT_TURNSTILE_SECRET` (both or neither): the platform widget a binding with
    /// `settings.turnstile.mode = platform` challenges with. `None` = unset (such a binding's sessions answer 503
    /// `turnstile_unconfigured`).
    pub platform: Option<TurnstileKeys>,
    pub verify_url: String,
    pub timeout_ms: u64,
}

/// A Turnstile widget's site key and secret. `Debug` shows no secret.
#[derive(Clone)]
pub struct TurnstileKeys {
    pub site_key: String,
    pub secret: String,
}
impl std::fmt::Debug for TurnstileKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnstileKeys")
            .field("site_key", &self.site_key)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for EdgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeConfig")
            .field(
                "internal_key",
                &self.internal_key.as_ref().map(|_| "[redacted]"),
            )
            .field("deliver_url", &self.deliver_url)
            .field("webchat_public_base", &self.webchat_public_base)
            .field("webchat_turnstile", &self.webchat_turnstile)
            .finish()
    }
}

/// The shortest `EDGE_INTERNAL_KEY` accepted (characters).
pub const MIN_EDGE_KEY_CHARS: usize = 32;

/// The async interaction callbacks (crate::interaction::webhook): the master key every job's signing secret is derived
/// from.
#[derive(Clone)]
pub struct InteractionWebhookConfig {
    /// `INTERACTION_WEBHOOK_KEY`: at least 32 characters.
    pub key: String,
}
impl std::fmt::Debug for InteractionWebhookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractionWebhookConfig")
            .field("key", &"[redacted]")
            .finish()
    }
}

/// The shortest `INTERACTION_WEBHOOK_KEY` accepted (characters).
pub const MIN_WEBHOOK_KEY_CHARS: usize = 32;

/// Which wire contract a speech gateway speaks (crate::voice::asr, crate::voice::tts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechProviderKind {
    /// The w2v services: `POST {asr}/api/transcribe` (multipart), `POST {tts}/api/tts/synthesize` (JSON, kokoro).
    W2v,
    /// OpenAI-compatible: `POST {asr}/audio/transcriptions` (multipart), `POST {tts}/audio/speech` (JSON).
    OpenaiCompatible,
}

/// The interim platform speech gateway (crate::voice): speech-to-text and text-to-speech, one token for both, for the
/// allow-listed tenants only.
#[derive(Clone)]
pub struct VoiceConfig {
    /// `VOICE_PROVIDER_KIND`: `w2v` (default) or `openai_compatible`.
    pub kind: SpeechProviderKind,
    /// `VOICE_ASR_URL`, without a trailing slash.
    pub asr_url: String,
    /// `VOICE_TTS_URL`, without a trailing slash.
    pub tts_url: String,
    /// `VOICE_TOKEN`: sent as `Authorization: Bearer` to both.
    pub token: String,
    /// `VOICE_ASR_MODEL` (default `whisper-1`; openai_compatible only).
    pub asr_model: String,
    /// `VOICE_TTS_MODEL` (default `tts-1`; openai_compatible only).
    pub tts_model: String,
    /// `VOICE_TTS_VOICE` (default `af_heart` for w2v, `alloy` for openai_compatible): the voice when a turn names none.
    pub tts_voice: String,
    /// `VOICE_TIMEOUT_S` (default 60): the bound on one whole upstream call.
    pub timeout_s: u64,
    /// `VOICE_PLATFORM_TENANTS`: the only tenants this gateway serves.
    pub platform_tenants: Vec<uuid::Uuid>,
}
impl std::fmt::Debug for VoiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceConfig")
            .field("asr_url", &self.asr_url)
            .field("tts_url", &self.tts_url)
            .field("token", &"[redacted]")
            .field("kind", &self.kind)
            .field("asr_model", &self.asr_model)
            .field("tts_model", &self.tts_model)
            .field("tts_voice", &self.tts_voice)
            .field("timeout_s", &self.timeout_s)
            .field("platform_tenants", &self.platform_tenants)
            .finish()
    }
}

/// The interim interaction backend (crate::interaction::llm): an OpenAI-compatible `POST {url}/chat/completions`
/// (in-cluster `inference-router`) until B4's runtime implements crate::interaction::InteractionBackend.
#[derive(Clone)]
pub struct InteractionLlmConfig {
    /// `INTERACTION_LLM_URL`, without a trailing slash.
    pub url: String,
    /// `INTERACTION_LLM_TOKEN`: sent as `Authorization: Bearer`.
    pub token: String,
    /// `INTERACTION_LLM_MODEL` (default `local-llm`).
    pub model: String,
    /// `INTERACTION_LLM_TIMEOUT_S` (default 120): the bound on one whole call, streamed body included.
    pub timeout_s: u64,
    /// `INTERACTION_LLM_TENANTS`: the only tenants this backend serves.
    pub tenants: Vec<uuid::Uuid>,
}
impl std::fmt::Debug for InteractionLlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractionLlmConfig")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("model", &self.model)
            .field("timeout_s", &self.timeout_s)
            .field("tenants", &self.tenants)
            .finish()
    }
}

/// Client-credentials settings of one Keycloak service account.
#[derive(Clone)]
pub struct ClientCredentials {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
}
impl std::fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .finish()
    }
}

/// `KEYCLOAK_ADMIN_*`: the portal-realm admin service account (client `agent-admin`).
#[derive(Clone, Debug)]
pub struct KeycloakAdminConfig {
    pub creds: ClientCredentials,
    /// `KEYCLOAK_ADMIN_API_URL`: `.../admin/realms/portal`, parsed at boot (http(s), no query or fragment).
    pub api_url: BaseUrl,
    /// `AGENT_KC_CLIENT_UUID` (required): the internal id of the `agent` client in the portal realm. The super_admin
    /// check reads the user's role mappings on it directly, so the admin service account needs `view-users` only,
    /// never `view-clients` (which would expose every client's secret).
    pub agent_client_uuid: String,
}

/// `LICENSES_ADMIN_*`: the secure-apps service account (client `agent-licenses-admin`).
#[derive(Clone, Debug)]
pub struct LicensesAdminConfig {
    pub creds: ClientCredentials,
    /// `LICENSES_ADMIN_URL`: the licences admin API base, parsed at boot (http(s), no query or fragment).
    pub url: BaseUrl,
    /// `LICENSE_APP_KEY` (default `agent`).
    pub app_key: String,
    /// `LICENSE_PLAN_CODE` (default `byo`, as commonpractice issues early-access licences; owner 2026-10-20).
    pub plan_code: String,
    /// `LICENSE_SUBJECT_REALM` (default `portal`): the realm the holder lives in.
    pub subject_realm: String,
}

/// What tenant admin and the signup approval share (B1 T4): the portal-realm admin SA, the licences admin SA,
/// the licence token key, the app's public URL and the action-email settings.
#[derive(Clone, Debug)]
pub struct IdentityConfig {
    pub keycloak_admin: KeycloakAdminConfig,
    pub licenses_admin: LicensesAdminConfig,
    /// `LICENSE_TOKEN_ENC_KEYS` + `LICENSE_TOKEN_ENC_KEY_CURRENT` (crate::crypto::keyring `Purpose::LicenceToken`), or
    /// the set derived from the legacy `LICENSE_TOKEN_ENC_KEY` (crate::crypto::licence): licence tokens are sealed
    /// under it.
    pub licence_keys: LicenceKeys,
    /// `AGENT_PUBLIC_URL` (required), no trailing slash: action-email redirects and deep links.
    pub public_url: String,
    /// `SIGNUP_FRONTEND_CLIENT_ID` (default `agent`): the execute-actions-email `client_id`.
    pub frontend_client_id: String,
    /// `INVITE_EMAIL_LIFESPAN_SECONDS` (default 14 days, as fr, CP and w2v): the set-password link's lifetime.
    pub invite_email_lifespan_seconds: u64,
    /// `INVITE_RESEND_COOLDOWN_SECONDS` (default 600, at least 60; B1b F-01): how soon a member invite can be
    /// sent again.
    pub invite_resend_cooldown_seconds: u64,
    /// `LICENCES_REVOKE_TIMEOUT_MS` (default 3000, 100 to 8000: under the HTTP client's 10 s; B1b T8): the bound on
    /// one licences revoke call made under the licence lock (crate::identity::Identity::revoke_licence).
    pub licences_revoke_timeout_ms: u64,
}

#[derive(Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
    pub thread_id: i64,
    /// `TELEGRAM_API_BASE`, without a trailing slash (overridable for tests only).
    pub api_base: String,
}
impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &"[redacted]")
            .field("chat_id", &self.chat_id)
            .field("thread_id", &self.thread_id)
            .field("api_base", &self.api_base)
            .finish()
    }
}

/// The tenant-offboard service door (2026-10-25): automation offboards an early-access tenant with a secure-apps
/// client-credentials token instead of a human password. All three pins are required together; each is EXACT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffboardServiceConfig {
    /// `TENANT_OFFBOARD_SA_ISSUER`: the issuer the token must come from. In `KEYCLOAK_TRUSTED_ISSUERS` and in a
    /// `KEYCLOAK_SERVICE_ONLY_REALMS` realm (boot refuses otherwise), so no person token can ever reach the door.
    pub issuer: String,
    /// `TENANT_OFFBOARD_SA_AZP`: the ONE client id (`azp`) allowed; `aud` must contain it too.
    pub azp: String,
    /// `TENANT_OFFBOARD_SA_ROLE`: the REALM role the token must carry (never `SUPER_ADMIN_ROLE`).
    pub role: String,
}

/// Early-access signup (owner 2026-10-19, D73), after strata_configurator `SignupConfig` and commonpractice v2.
#[derive(Clone)]
pub struct SignupConfig {
    /// `SIGNUP_INTAKE_AZP` (default `agent-signup-intake`): client ids allowed to call the intake.
    pub intake_azp: Vec<String>,
    /// `SIGNUP_INTAKE_ROLE` (default `agent-signup-intake`).
    pub intake_role: String,
    /// `SIGNUP_DECIDER_AZP` (default `chat-bridge-signup-decider`): client ids allowed to decide (C3).
    pub decider_azp: Vec<String>,
    /// `SIGNUP_DECIDER_ROLE` (default `agent-signup-decide`).
    pub decider_role: String,
    /// `SIGNUP_INTAKE_ISSUER` (default `KEYCLOAK_ISSUER`): the ONE issuer the intake token must come from. Always in
    /// `KEYCLOAK_TRUSTED_ISSUERS` (boot refuses otherwise).
    pub intake_issuer: String,
    /// `SIGNUP_DECIDER_ISSUER` (default `KEYCLOAK_ISSUER`): the ONE issuer the decider token must come from.
    pub decider_issuer: String,
    /// `SIGNUP_QUEUE_MAX_PENDING` (default 500).
    pub max_pending: i64,
    /// `TURNSTILE_SECRET`: when set, a relayed Turnstile token is verified.
    pub turnstile_secret: Option<String>,
    pub turnstile_verify_url: String,
    pub turnstile_timeout_ms: u64,
    /// `None` (Telegram off, never a crash) unless `TELEGRAM_BOT_TOKEN`, `TELEGRAM_SIGNUP_CHAT_ID` and
    /// `TELEGRAM_SIGNUP_THREAD_ID` are all set.
    pub telegram: Option<TelegramConfig>,
    /// The Telegram variables that are missing (names only), for the boot log.
    pub telegram_missing: Vec<&'static str>,
    /// `SUPER_ADMIN_ROLE`, refused on the service-account doors.
    pub super_admin_role: String,
    /// [`IdentityConfig::public_url`], copied: the Telegram deep link and the queue's notes.
    pub public_url: String,
    /// `SIGNUP_APP_KEY` (default `agent`): the `ea1:<app>` callback prefix and the `signup_app` attribute.
    pub app_key: String,
    /// `SIGNUP_TICK_SECONDS` (default 60): how often unfinished provisioning steps are retried.
    pub tick_seconds: u64,
    /// `SIGNUP_TELEGRAM_RESIDENCY` (`eu|us`, D66): the residency of a workspace a Telegram tap approves (a tap cannot
    /// choose). Required when Telegram is configured.
    pub telegram_residency: Option<&'static str>,
}
impl std::fmt::Debug for SignupConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignupConfig")
            .field("intake_azp", &self.intake_azp)
            .field("intake_role", &self.intake_role)
            .field("decider_azp", &self.decider_azp)
            .field("decider_role", &self.decider_role)
            .field("intake_issuer", &self.intake_issuer)
            .field("decider_issuer", &self.decider_issuer)
            .field("max_pending", &self.max_pending)
            .field("turnstile", &self.turnstile_secret.is_some())
            .field("telegram", &self.telegram)
            .field("public_url", &self.public_url)
            .field("app_key", &self.app_key)
            .field("tick_seconds", &self.tick_seconds)
            .field("telegram_residency", &self.telegram_residency)
            .finish()
    }
}

/// Comma-separated list; surrounding whitespace trimmed, blanks dropped.
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// `from_env` over any lookup, so tests never touch the process environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Self::parse(get, Mode::Api)
    }

    /// B5 T12: the optional settings a B4 daemon that boots on [`RuntimeConfig`] (orchestrator, interaction) runs its
    /// gate hooks and the assistant with, read as the runner reads them (no identity, signup, token verifier or owner
    /// variable; `MIGRATE_DATABASE_URL` is not read, so not refused). The daemon's boot never depends on it: the
    /// caller treats an error as "run without" (crate::state::optional_runtime).
    pub fn for_runtime_daemon(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Self::parse(get, Mode::Runner)
    }

    /// agent-api's config, or the runner's (B4 F1, [`RunnerConfig`]): the runner reads no identity, signup, token
    /// verifier or owner variable; those slots get inert placeholders nothing connects to.
    fn parse(get: impl Fn(&str) -> Option<String>, mode: Mode) -> Result<Self> {
        let trimmed = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let req = |k: &str| trimmed(k).with_context(|| format!("{k} is not set"));
        let license_gate_2_enabled = flag(&trimmed, "LICENSE_GATE_2_ENABLED")?;
        if license_gate_2_enabled
            && mode == Mode::Api
            && trimmed("LICENSE_INTROSPECT_URL").is_none()
        {
            anyhow::bail!(
                "LICENSE_GATE_2_ENABLED=true needs LICENSE_INTROSPECT_URL (gate 2 reads the verdicts it refreshes)"
            );
        }
        // D82: no timer may trigger GPU calls, and a scheduled re-sync re-embeds.
        let source_schedule_enabled = flag(&trimmed, "SOURCE_SCHEDULE_ENABLED")?;
        if source_schedule_enabled {
            anyhow::bail!(
                "source_schedule_disabled_by_policy: SOURCE_SCHEDULE_ENABLED=true is refused (no scheduled job may \
                 trigger GPU calls; see docs/runbooks/source-schedule-activation.md)"
            );
        }
        let super_admin_role = trimmed("SUPER_ADMIN_ROLE").unwrap_or_else(|| "super_admin".into());
        let identity = match mode {
            Mode::Api => identity_from(&trimmed)?,
            Mode::Runner => inert_identity(),
        };
        let bind = match trimmed("AGENT_API_BIND") {
            Some(b) => b,
            None => format!("0.0.0.0:{}", number(&trimmed, "PORT", DEFAULT_PORT)?),
        };
        let log_format = match trimmed("LOG_FORMAT")
            .map(|v| v.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("json") => LogFormat::Json,
            Some("pretty") => LogFormat::Pretty,
            Some(_) => anyhow::bail!("LOG_FORMAT must be json or pretty"),
        };
        let obs = ObsConfig {
            log_format,
            metrics_bind: trimmed("METRICS_BIND").unwrap_or_else(|| DEFAULT_METRICS_BIND.into()),
        };
        let (keycloak_issuer, keycloak_audience) = match mode {
            Mode::Api => (
                req("KEYCLOAK_ISSUER")?.trim_end_matches('/').to_string(),
                req("KEYCLOAK_AUDIENCE")?,
            ),
            Mode::Runner => (RUNNER_NO_ISSUER.to_string(), RUNNER_NO_AUDIENCE.to_string()),
        };
        let keycloak_client_id = match mode {
            Mode::Api => trimmed("KEYCLOAK_CLIENT_ID").unwrap_or_else(|| keycloak_audience.clone()),
            Mode::Runner => keycloak_audience.clone(),
        };
        let issuers = match mode {
            Mode::Api => issuers_from(&trimmed, &keycloak_issuer)?,
            Mode::Runner => Issuers {
                trusted: vec![keycloak_issuer.clone()],
                service_only_realms: split_list(DEFAULT_SERVICE_ONLY_REALMS),
                intake: keycloak_issuer.clone(),
                decider: keycloak_issuer.clone(),
            },
        };
        let (migrate_database_url, keycloak_jwks_url) = match mode {
            Mode::Api => (
                req("MIGRATE_DATABASE_URL")?,
                trimmed("KEYCLOAK_JWKS_URL")
                    .unwrap_or_else(|| format!("{keycloak_issuer}/protocol/openid-connect/certs")),
            ),
            Mode::Runner => (
                RUNNER_NO_OWNER_LOGIN.to_string(),
                format!("{keycloak_issuer}/protocol/openid-connect/certs"),
            ),
        };
        let (interaction_llm, interaction_llm_missing) = interaction_llm_from(&trimmed)?;
        let (voice, voice_missing) = voice_from(&trimmed)?;
        let interaction_webhook = match trimmed("INTERACTION_WEBHOOK_KEY") {
            Some(key) if key.chars().count() >= MIN_WEBHOOK_KEY_CHARS => {
                Some(InteractionWebhookConfig { key })
            }
            Some(_) => anyhow::bail!(
                "INTERACTION_WEBHOOK_KEY must be at least {MIN_WEBHOOK_KEY_CHARS} characters"
            ),
            None => None,
        };
        let edge = EdgeConfig {
            internal_key: match trimmed("EDGE_INTERNAL_KEY") {
                Some(k) if k.chars().count() >= MIN_EDGE_KEY_CHARS => Some(k),
                Some(_) => anyhow::bail!(
                    "EDGE_INTERNAL_KEY must be at least {MIN_EDGE_KEY_CHARS} characters"
                ),
                None => None,
            },
            deliver_url: match trimmed("EDGE_DELIVER_URL") {
                Some(u) => {
                    let ok = reqwest::Url::parse(&u).is_ok_and(|p| {
                        matches!(p.scheme(), "http" | "https") && p.host_str().is_some()
                    });
                    if !ok {
                        anyhow::bail!("EDGE_DELIVER_URL must be an http(s) URL");
                    }
                    Some(u.trim_end_matches('/').to_string())
                }
                None => None,
            },
            webchat_public_base: match trimmed("WEBCHAT_PUBLIC_BASE") {
                Some(u) => {
                    let ok = reqwest::Url::parse(&u).is_ok_and(|p| {
                        matches!(p.scheme(), "http" | "https") && p.host_str().is_some()
                    });
                    if !ok {
                        anyhow::bail!("WEBCHAT_PUBLIC_BASE must be an http(s) URL");
                    }
                    Some(u.trim_end_matches('/').to_string())
                }
                None => None,
            },
            webchat_turnstile: WebchatTurnstileConfig {
                platform: match (
                    trimmed("WEBCHAT_TURNSTILE_SITE_KEY"),
                    trimmed("WEBCHAT_TURNSTILE_SECRET"),
                ) {
                    (Some(site_key), Some(secret)) => Some(TurnstileKeys { site_key, secret }),
                    (None, None) => None,
                    _ => anyhow::bail!(
                        "WEBCHAT_TURNSTILE_SITE_KEY and WEBCHAT_TURNSTILE_SECRET are set together or not at all"
                    ),
                },
                verify_url: trimmed("TURNSTILE_VERIFY_URL")
                    .unwrap_or_else(|| crate::turnstile::DEFAULT_VERIFY_URL.into()),
                timeout_ms: number(&trimmed, "TURNSTILE_TIMEOUT_MS", 5000)?,
            },
        };
        let signup = if mode == Mode::Api && flag(&trimmed, "EARLY_ACCESS_SIGNUP_ENABLED")? {
            Some(signup_from(
                &trimmed,
                &super_admin_role,
                &identity.public_url,
                &issuers,
            )?)
        } else {
            None
        };
        let tenant_offboard_service = match mode {
            Mode::Api => offboard_service_from(&trimmed, &issuers, &super_admin_role)?,
            Mode::Runner => None,
        };
        let signup_launch = SIGNUP_LAUNCH_VARS
            .iter()
            .map(|k| (*k, trimmed(k).is_some()))
            .collect();
        let tenant_data_keys = KeySet::parse(
            Purpose::TenantData,
            trimmed(Purpose::TenantData.keys_var()).as_deref(),
            trimmed(Purpose::TenantData.current_var()).as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        let connector_policy = Policy::from_env_json(
            trimmed("CONNECTOR_PRIVATE_TARGETS")
                .as_deref()
                .unwrap_or(""),
        )
        .map_err(anyhow::Error::msg)?;
        let connector_sandboxes =
            t2::parse(trimmed("CONNECTOR_SANDBOXES").as_deref().unwrap_or(""))
                .map_err(anyhow::Error::msg)?;
        let provider_secret_keys = KeySet::parse(
            Purpose::ProviderSecret,
            trimmed(Purpose::ProviderSecret.keys_var()).as_deref(),
            trimmed(Purpose::ProviderSecret.current_var()).as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        let strata = strata_from(&trimmed)?;
        let bundle_minter = bundle_minter_from(&trimmed)?;
        let internal_providers = internal_providers_from(&trimmed)?;
        let redis = redis_from(&trimmed)?;
        let platform_tenant_id = match trimmed("AGENT_PLATFORM_TENANT_ID") {
            None => None,
            Some(v) => match uuid::Uuid::parse_str(&v) {
                Ok(id) if !id.is_nil() => Some(id),
                _ => anyhow::bail!("AGENT_PLATFORM_TENANT_ID must be a non-nil uuid"),
            },
        };
        let license_introspect_url = trimmed("LICENSE_INTROSPECT_URL")
            .map(|v| base_url("LICENSE_INTROSPECT_URL", v))
            .transpose()?;
        let mcp = mcp_from(&trimmed)?;
        Ok(Self {
            bind,
            database_url: req("DATABASE_URL")?,
            migrate_database_url,
            keycloak_jwks_url,
            keycloak_issuer,
            keycloak_audience,
            keycloak_client_id,
            keycloak_trusted_issuers: issuers.trusted,
            keycloak_service_only_realms: issuers.service_only_realms,
            super_admin_role,
            identity,
            signup,
            tenant_offboard_service,
            signup_launch,
            obs,
            tenant_data_keys,
            connector_policy,
            connector_sandboxes,
            interaction_llm,
            interaction_llm_missing,
            voice,
            voice_missing,
            interaction_webhook,
            provider_secret_keys,
            strata,
            bundle_minter,
            internal_providers,
            platform_tenant_id,
            edge,
            attachment_quota_bytes: {
                let q = number(
                    &trimmed,
                    "ATTACHMENT_QUOTA_BYTES",
                    crate::attachments::store::DEFAULT_QUOTA_BYTES,
                )?;
                if q < 1 {
                    anyhow::bail!("ATTACHMENT_QUOTA_BYTES must be at least 1");
                }
                q
            },
            artifacts: crate::attachments::artifacts::ArtifactsConfig::from_lookup(&trimmed)?,
            source_schedule_enabled,
            license_gate_2_enabled,
            license_introspect_url,
            mcp,
            redis,
            metering: crate::usage::MeteringConfig::from_lookup(&trimmed)?,
            generator: crate::generator::GeneratorConfig::from_lookup(&trimmed)?,
        })
    }
}

/// Which binary a [`Config`] is parsed for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// agent-api: everything, identity and the owner login required.
    Api,
    /// The runner (B4 F1): no identity, signup, token verifier or owner variable is read.
    Runner,
}

/// The runner's owner-URL slot: `state::build_runtime` never connects to it.
const RUNNER_NO_OWNER_LOGIN: &str = "postgres://runner-never-migrates.invalid/none";
/// The runner's token-verifier slots: it serves no HTTP and verifies no token.
const RUNNER_NO_ISSUER: &str = "https://runner-verifies-no-token.invalid";
const RUNNER_NO_AUDIENCE: &str = "runner-verifies-no-token";

/// A constant `.invalid` base URL for the runner's inert identity; the literal is http(s) by construction.
fn inert_base(url: &str) -> BaseUrl {
    BaseUrl::parse(url).expect("the inert identity URL is a valid http(s) URL")
}

/// The runner's identity slot (B4 F1): no Keycloak-admin or licences-admin credential and no licence-token key.
/// Every URL is `.invalid` (never resolves) and the licence keyring is empty (a seal fails, an open finds no key), so
/// a call through it fails instead of reaching a real service. No runner path uses `AppState::identity`.
fn inert_identity() -> IdentityConfig {
    let none = |token_url: &str| ClientCredentials {
        token_url: token_url.to_string(),
        client_id: String::new(),
        client_secret: String::new(),
    };
    IdentityConfig {
        keycloak_admin: KeycloakAdminConfig {
            creds: none("https://runner-has-no-keycloak-admin.invalid/token"),
            api_url: inert_base("https://runner-has-no-keycloak-admin.invalid"),
            agent_client_uuid: String::new(),
        },
        licenses_admin: LicensesAdminConfig {
            creds: none("https://runner-has-no-licences-admin.invalid/token"),
            url: inert_base("https://runner-has-no-licences-admin.invalid"),
            app_key: "agent".into(),
            plan_code: "byo".into(),
            subject_realm: "portal".into(),
        },
        licence_keys: LicenceKeys {
            set: KeySet {
                current: String::new(),
                keys: BTreeMap::new(),
            },
            derived: false,
        },
        public_url: "https://runner-has-no-public-url.invalid".into(),
        frontend_client_id: "agent".into(),
        invite_email_lifespan_seconds: 7 * 24 * 3600,
        invite_resend_cooldown_seconds: 600,
        licences_revoke_timeout_ms: 3000,
    }
}

/// What every B4 runtime daemon (orchestrator, runner, interaction) reads (B4 F1). Fail loud by name, never echoing
/// a URL: `DATABASE_URL` (required: the `agent_runtime` login) and `REDIS_URL` (required) — every missing one is named
/// in one error — `AGENT_PREFIX` (optional, no whitespace), `LOG_FORMAT` (`json` default | `pretty`). No identity,
/// licence or Keycloak variable: the daemons hold no admin credential. `Debug` prints neither URL.
#[derive(Clone)]
pub struct RuntimeConfig {
    pub log_format: LogFormat,
    pub database_url: String,
    pub redis: RedisConfig,
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("log_format", &self.log_format)
            .field("database_url", &"[redacted]")
            .field("redis", &self.redis)
            .finish()
    }
}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// `from_env` over any lookup. Checked in this order: `LOG_FORMAT`, `AGENT_PREFIX`, then the required pair.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let trimmed = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let log_format = match trimmed("LOG_FORMAT")
            .map(|v| v.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("json") => LogFormat::Json,
            Some("pretty") => LogFormat::Pretty,
            Some(_) => anyhow::bail!("LOG_FORMAT must be json or pretty"),
        };
        let prefix = trimmed("AGENT_PREFIX").unwrap_or_default();
        if prefix.chars().any(char::is_whitespace) {
            anyhow::bail!("AGENT_PREFIX may not contain whitespace");
        }
        let missing: Vec<String> = ["DATABASE_URL", "REDIS_URL"]
            .into_iter()
            .filter(|k| trimmed(k).is_none())
            .map(|k| format!("{k} is required"))
            .collect();
        if !missing.is_empty() {
            anyhow::bail!("{}", missing.join(", "));
        }
        let redis = redis_from(&trimmed)?.context("REDIS_URL is required")?;
        Ok(Self {
            log_format,
            database_url: req_var(&trimmed, "DATABASE_URL")?,
            redis,
        })
    }
}

/// Identity, signup and token-verifier variables the runner does NOT read (B4 F1): set or not, they never reach it.
/// [`RunnerConfig::ignored`] names the ones still set, so a deploy that carries them shows up in the boot log.
pub const RUNNER_UNREAD_VARS: [&str; 28] = [
    "KEYCLOAK_ADMIN_TOKEN_URL",
    "KEYCLOAK_ADMIN_CLIENT_ID",
    "KEYCLOAK_ADMIN_CLIENT_SECRET",
    "KEYCLOAK_ADMIN_API_URL",
    "AGENT_KC_CLIENT_UUID",
    "LICENSES_ADMIN_TOKEN_URL",
    "LICENSES_ADMIN_CLIENT_ID",
    "LICENSES_ADMIN_CLIENT_SECRET",
    "LICENSES_ADMIN_URL",
    "LICENSE_TOKEN_ENC_KEYS",
    "LICENSE_TOKEN_ENC_KEY_CURRENT",
    "LICENSE_TOKEN_ENC_KEY",
    "LICENSE_APP_KEY",
    "LICENSE_PLAN_CODE",
    "LICENSE_SUBJECT_REALM",
    "AGENT_PUBLIC_URL",
    "SIGNUP_FRONTEND_CLIENT_ID",
    "INVITE_EMAIL_LIFESPAN_SECONDS",
    "INVITE_RESEND_COOLDOWN_SECONDS",
    "LICENCES_REVOKE_TIMEOUT_MS",
    "KEYCLOAK_ISSUER",
    "KEYCLOAK_AUDIENCE",
    "KEYCLOAK_CLIENT_ID",
    "KEYCLOAK_JWKS_URL",
    "KEYCLOAK_TRUSTED_ISSUERS",
    "KEYCLOAK_SERVICE_ONLY_REALMS",
    "SIGNUP_INTAKE_ISSUER",
    "SIGNUP_DECIDER_ISSUER",
];

/// The runner's environment (B4 F1): the daemon variables ([`RuntimeConfig`]) plus agent-api's connector, custody,
/// strata and provider settings ([`Config`], parsed the same way, so a malformed value fails the boot by name) —
/// WITHOUT identity, signup, the token verifier or the owner login: `app.identity` is inert (no Keycloak-admin or
/// licences-admin credential, no licence-token key), `app.signup` is `None`. `MIGRATE_DATABASE_URL` is refused (the
/// runner calls live systems and must never hold the owner login); `EARLY_ACCESS_SIGNUP_ENABLED` is not read.
#[derive(Debug)]
pub struct RunnerConfig {
    pub runtime: RuntimeConfig,
    pub app: Config,
    /// The [`RUNNER_UNREAD_VARS`] that are set anyway (names only).
    pub ignored: Vec<&'static str>,
}

impl RunnerConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let set = |k: &str| get(k).is_some_and(|v| !v.trim().is_empty());
        if set("MIGRATE_DATABASE_URL") {
            anyhow::bail!(
                "MIGRATE_DATABASE_URL must not be set for the runner (it never migrates; owner login refused)"
            );
        }
        let ignored: Vec<&'static str> =
            RUNNER_UNREAD_VARS.into_iter().filter(|k| set(k)).collect();
        let runtime = RuntimeConfig::from_lookup(&get)?;
        let app = Config::parse(get, Mode::Runner)?;
        Ok(Self {
            runtime,
            app,
            ignored,
        })
    }
}

/// A required variable, trimmed; missing or blank fails by name.
fn req_var(trimmed: &impl Fn(&str) -> Option<String>, k: &str) -> Result<String> {
    trimmed(k).with_context(|| format!("{k} is not set"))
}

/// An http(s) base URL from config, without its trailing slash; anything else fails the boot by name.
pub(crate) fn base_url(k: &str, v: String) -> Result<String> {
    let v = v.trim_end_matches('/').to_string();
    if !(v.starts_with("https://") || v.starts_with("http://")) {
        anyhow::bail!("{k} must be an http(s) URL");
    }
    Ok(v)
}

/// An http(s) base URL from config, parsed and checked at boot (crate::http_base); anything else fails the boot by
/// name, so a client never sends a request to an unvalidated string.
fn parsed_base(k: &str, v: String) -> Result<BaseUrl> {
    BaseUrl::parse(&v).map_err(|e| anyhow::anyhow!("{k}: {e}"))
}

/// The bundle-token minter: unset = `None`; a partial set fails the boot by name; ttl 1 to 60 minutes.
fn bundle_minter_from(
    trimmed: &impl Fn(&str) -> Option<String>,
) -> Result<Option<BundleMinterConfig>> {
    let url = trimmed("BUNDLE_MINTER_LICENSES_URL");
    let token_url = trimmed("BUNDLE_MINTER_TOKEN_URL");
    let secret = trimmed("BUNDLE_MINTER_CLIENT_SECRET");
    let (Some(url), Some(token_url), Some(client_secret)) =
        (url.clone(), token_url.clone(), secret.clone())
    else {
        if url.is_some() || token_url.is_some() || secret.is_some() {
            anyhow::bail!(
                "BUNDLE_MINTER_LICENSES_URL, BUNDLE_MINTER_TOKEN_URL and BUNDLE_MINTER_CLIENT_SECRET are set together or not at all"
            );
        }
        return Ok(None);
    };
    let ttl_minutes: u32 = number(trimmed, "BUNDLE_TOKEN_TTL_MINUTES", 15)?;
    if !(1..=60).contains(&ttl_minutes) {
        anyhow::bail!("BUNDLE_TOKEN_TTL_MINUTES must be between 1 and 60");
    }
    Ok(Some(BundleMinterConfig {
        licenses_url: base_url("BUNDLE_MINTER_LICENSES_URL", url)?,
        token_url: base_url("BUNDLE_MINTER_TOKEN_URL", token_url)?,
        client_id: trimmed("BUNDLE_MINTER_CLIENT_ID")
            .unwrap_or_else(|| "agent-bundle-minter".into()),
        client_secret,
        bundle: trimmed("LICENSE_BUNDLE_KEY").unwrap_or_else(|| "agent".into()),
        ttl_minutes,
    }))
}

/// strata_rag / strata_ingest: each URL needs its token map; tokens alone are parsed (and unused).
fn strata_from(trimmed: &impl Fn(&str) -> Option<String>) -> Result<StrataConfig> {
    let side = |url_var: &str, tokens_var: &str| -> Result<(Option<String>, TenantTokens)> {
        let tokens = TenantTokens::parse(tokens_var, &trimmed(tokens_var).unwrap_or_default())
            .map_err(anyhow::Error::msg)?;
        let url = match trimmed(url_var) {
            None => None,
            Some(_) if tokens.is_empty() => {
                anyhow::bail!("{url_var} is set but {tokens_var} is not")
            }
            Some(v) => Some(base_url(url_var, v)?),
        };
        Ok((url, tokens))
    };
    let (rag_url, rag_tokens) = side("STRATA_RAG_URL", "STRATA_RAG_TOKENS")?;
    let (ingest_url, ingest_tokens) = side("STRATA_INGEST_URL", "STRATA_INGEST_TOKENS")?;
    Ok(StrataConfig {
        rag_url,
        ingest_url,
        rag_tokens,
        ingest_tokens,
        service_actor_id: service_actor_from(trimmed)?,
    })
}

/// `SERVICE_ACTOR_ID` (optional): the Keycloak `sub` of agent's own service account, the `X-Actor-Id` of a call no
/// person acts in (inference-router, strata_rag). A non-nil uuid; anything else fails the boot by name.
pub(crate) fn service_actor_from(
    trimmed: &impl Fn(&str) -> Option<String>,
) -> Result<Option<uuid::Uuid>> {
    match trimmed("SERVICE_ACTOR_ID") {
        None => Ok(None),
        Some(v) => match uuid::Uuid::parse_str(&v) {
            Ok(id) if !id.is_nil() => Ok(Some(id)),
            _ => anyhow::bail!("SERVICE_ACTOR_ID must be a non-nil uuid"),
        },
    }
}

/// The nonagenticai MCP server a tenant's MCP-transport connectors call (crate::mcp_client).
#[derive(Clone, Debug)]
pub struct McpConfig {
    /// `MCP_URL`: the streamable-HTTP endpoint, ending in `/mcp`.
    pub url: String,
    /// `MCP_TASK_TOOLS`: the long-running tools started as durable MCP tasks (comma-separated; configuration decides,
    /// never the model).
    pub task_tools: Vec<String>,
    /// `MCP_TASK_ANSWER_TIMEOUT_SECS` (default a day, at least a minute): a task waiting this long for an answer goes
    /// to the staff Tasks inbox (crate::mcp_client::store::TaskPoller).
    pub answer_timeout: std::time::Duration,
}

/// The default of `MCP_TASK_ANSWER_TIMEOUT_SECS`.
pub const MCP_TASK_ANSWER_TIMEOUT_DEFAULT_SECS: u64 = 86_400;

/// `MCP_URL` unset → `None`; set, it must be an http(s) URL ending in `/mcp`.
fn mcp_from(trimmed: &impl Fn(&str) -> Option<String>) -> Result<Option<McpConfig>> {
    let Some(url) = trimmed("MCP_URL") else {
        return Ok(None);
    };
    let url = base_url("MCP_URL", url)?;
    if !url.ends_with("/mcp") {
        anyhow::bail!("MCP_URL must end in /mcp");
    }
    let answer_timeout = match trimmed("MCP_TASK_ANSWER_TIMEOUT_SECS") {
        None => MCP_TASK_ANSWER_TIMEOUT_DEFAULT_SECS,
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n >= 60 => n,
            _ => anyhow::bail!(
                "MCP_TASK_ANSWER_TIMEOUT_SECS must be a whole number of seconds, at least 60"
            ),
        },
    };
    Ok(Some(McpConfig {
        url,
        task_tools: crate::mcp_client::tasks::task_tools(trimmed("MCP_TASK_TOOLS").as_deref()),
        answer_timeout: std::time::Duration::from_secs(answer_timeout),
    }))
}

/// The D70 internal providers: defaults for the vLLM URL and model; a set but malformed URL fails the boot by name.
fn internal_providers_from(
    trimmed: &impl Fn(&str) -> Option<String>,
) -> Result<InternalProvidersConfig> {
    let opt_url = |k: &str| trimmed(k).map(|v| base_url(k, v)).transpose();
    Ok(InternalProvidersConfig {
        llm_url: base_url(
            "INTERNAL_LLM_URL",
            trimmed("INTERNAL_LLM_URL").unwrap_or_else(|| DEFAULT_INTERNAL_LLM_URL.into()),
        )?,
        llm_model: trimmed("INTERNAL_LLM_MODEL").unwrap_or_else(|| "local-llm".into()),
        llm_key: trimmed("INTERNAL_LLM_KEY"),
        mlx_text_url: opt_url("INTERNAL_MLX_TEXT_URL")?,
        mlx_media_url: opt_url("INTERNAL_MLX_MEDIA_URL")?,
        pgx_url: opt_url("INTERNAL_PGX_VLLM_URL")?,
        laya_url: base_url(
            "INTERNAL_LAYA_URL",
            trimmed("INTERNAL_LAYA_URL").unwrap_or_else(|| DEFAULT_INTERNAL_LAYA_URL.into()),
        )?,
        laya_model: trimmed("INTERNAL_LAYA_MODEL").unwrap_or_else(|| "jev-latest".into()),
        laya_key: trimmed("INTERNAL_LAYA_KEY"),
        service_actor_id: service_actor_from(trimmed)?,
    })
}

/// The interim interaction backend: on only when the URL and the token are set; either missing → off, logged by name
/// at boot, never a crash. A set but malformed URL, timeout or tenant list still fails the boot.
pub(crate) fn interaction_llm_from(
    trimmed: &impl Fn(&str) -> Option<String>,
) -> Result<(Option<InteractionLlmConfig>, Vec<&'static str>)> {
    let timeout_s: u64 = number(trimmed, "INTERACTION_LLM_TIMEOUT_S", 120)?;
    if timeout_s < 1 {
        anyhow::bail!("INTERACTION_LLM_TIMEOUT_S must be at least 1");
    }
    let tenants = uuid_list(trimmed, "INTERACTION_LLM_TENANTS")?;
    let missing: Vec<&'static str> = ["INTERACTION_LLM_URL", "INTERACTION_LLM_TOKEN"]
        .into_iter()
        .filter(|k| trimmed(k).is_none())
        .collect();
    let cfg = match (
        trimmed("INTERACTION_LLM_URL"),
        trimmed("INTERACTION_LLM_TOKEN"),
    ) {
        (Some(url), Some(token)) => {
            let url = url.trim_end_matches('/').to_string();
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                anyhow::bail!("INTERACTION_LLM_URL must be an http(s) URL");
            }
            Some(InteractionLlmConfig {
                url,
                token,
                model: trimmed("INTERACTION_LLM_MODEL").unwrap_or_else(|| "local-llm".into()),
                timeout_s,
                tenants,
            })
        }
        _ => None,
    };
    Ok((cfg, missing))
}

/// A comma-separated uuid list (blanks dropped; unset or empty is none); any other entry fails the boot by name.
fn uuid_list(trimmed: &impl Fn(&str) -> Option<String>, k: &str) -> Result<Vec<uuid::Uuid>> {
    split_list(&trimmed(k).unwrap_or_default())
        .iter()
        .map(|t| uuid::Uuid::parse_str(t))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| anyhow::anyhow!("{k} must be comma-separated uuids"))
}

/// The platform speech gateway: on only when both URLs and the token are set; any missing → off, logged by name at
/// boot, never a crash. A set but malformed kind, URL, timeout or tenant list still fails the boot.
fn voice_from(
    trimmed: &impl Fn(&str) -> Option<String>,
) -> Result<(Option<VoiceConfig>, Vec<&'static str>)> {
    let timeout_s: u64 = number(trimmed, "VOICE_TIMEOUT_S", 60)?;
    if timeout_s < 1 {
        anyhow::bail!("VOICE_TIMEOUT_S must be at least 1");
    }
    let kind = match trimmed("VOICE_PROVIDER_KIND").as_deref() {
        None | Some("w2v") => SpeechProviderKind::W2v,
        Some("openai_compatible") => SpeechProviderKind::OpenaiCompatible,
        Some(_) => anyhow::bail!("VOICE_PROVIDER_KIND must be w2v or openai_compatible"),
    };
    let platform_tenants = uuid_list(trimmed, "VOICE_PLATFORM_TENANTS")?;
    let missing: Vec<&'static str> = ["VOICE_ASR_URL", "VOICE_TTS_URL", "VOICE_TOKEN"]
        .into_iter()
        .filter(|k| trimmed(k).is_none())
        .collect();
    let url = |k: &str, v: String| -> Result<String> {
        let v = v.trim_end_matches('/').to_string();
        if !(v.starts_with("https://") || v.starts_with("http://")) {
            anyhow::bail!("{k} must be an http(s) URL");
        }
        Ok(v)
    };
    let default_voice = match kind {
        SpeechProviderKind::W2v => "af_heart",
        SpeechProviderKind::OpenaiCompatible => "alloy",
    };
    let cfg = match (
        trimmed("VOICE_ASR_URL"),
        trimmed("VOICE_TTS_URL"),
        trimmed("VOICE_TOKEN"),
    ) {
        (Some(asr), Some(tts), Some(token)) => Some(VoiceConfig {
            kind,
            asr_url: url("VOICE_ASR_URL", asr)?,
            tts_url: url("VOICE_TTS_URL", tts)?,
            token,
            asr_model: trimmed("VOICE_ASR_MODEL").unwrap_or_else(|| "whisper-1".into()),
            tts_model: trimmed("VOICE_TTS_MODEL").unwrap_or_else(|| "tts-1".into()),
            tts_voice: trimmed("VOICE_TTS_VOICE").unwrap_or_else(|| default_voice.into()),
            timeout_s,
            platform_tenants,
        }),
        _ => None,
    };
    Ok((cfg, missing))
}

/// The environment the signup launch page lists by name (P3 dark-launch switch, K21).
pub const SIGNUP_LAUNCH_VARS: [&str; 8] = [
    "EARLY_ACCESS_SIGNUP_ENABLED",
    "SIGNUP_INTAKE_AZP",
    "SIGNUP_DECIDER_AZP",
    "TURNSTILE_SECRET",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_SIGNUP_CHAT_ID",
    "TELEGRAM_SIGNUP_THREAD_ID",
    "SIGNUP_TELEGRAM_RESIDENCY",
];

/// `true`/`1`/`yes`/`on` or `false`/`0`/`no`/`off` (any case); unset is false; anything else fails.
pub(crate) fn flag(trimmed: &impl Fn(&str) -> Option<String>, k: &str) -> Result<bool> {
    match trimmed(k).map(|v| v.to_ascii_lowercase()).as_deref() {
        None | Some("false" | "0" | "no" | "off") => Ok(false),
        Some("true" | "1" | "yes" | "on") => Ok(true),
        Some(_) => anyhow::bail!("{k} must be true or false"),
    }
}

pub(crate) fn number<T: std::str::FromStr>(
    trimmed: &impl Fn(&str) -> Option<String>,
    k: &str,
    default: T,
) -> Result<T> {
    match trimmed(k) {
        Some(v) => v
            .parse::<T>()
            .map_err(|_| anyhow::anyhow!("{k} is not a valid number")),
        None => Ok(default),
    }
}

/// Identity, required whether or not signup is on.
fn identity_from(trimmed: &impl Fn(&str) -> Option<String>) -> Result<IdentityConfig> {
    let req = |k: &str| trimmed(k).with_context(|| format!("{k} is not set"));
    let or = |k: &str, d: &str| trimmed(k).unwrap_or_else(|| d.to_string());
    let licence_keys = LicenceKeys::from_env(
        trimmed(Purpose::LicenceToken.keys_var()).as_deref(),
        trimmed(Purpose::LicenceToken.current_var()).as_deref(),
        trimmed(LEGACY_KEY_VAR).as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    let public_url = req("AGENT_PUBLIC_URL")?.trim_end_matches('/').to_string();
    if !(public_url.starts_with("https://") || public_url.starts_with("http://")) {
        anyhow::bail!("AGENT_PUBLIC_URL must be an http(s) URL");
    }
    let invite_resend_cooldown_seconds: u64 =
        number(trimmed, "INVITE_RESEND_COOLDOWN_SECONDS", 600)?;
    if invite_resend_cooldown_seconds < 60 {
        anyhow::bail!("INVITE_RESEND_COOLDOWN_SECONDS must be at least 60");
    }
    let licences_revoke_timeout_ms: u64 = number(trimmed, "LICENCES_REVOKE_TIMEOUT_MS", 3000)?;
    // Below the HTTP client's 10 s timeout (crate::state), or the bound would bound nothing.
    if !(100..=8_000).contains(&licences_revoke_timeout_ms) {
        anyhow::bail!("LICENCES_REVOKE_TIMEOUT_MS must be 100 to 8000");
    }
    Ok(IdentityConfig {
        keycloak_admin: KeycloakAdminConfig {
            creds: ClientCredentials {
                token_url: req("KEYCLOAK_ADMIN_TOKEN_URL")?,
                client_id: req("KEYCLOAK_ADMIN_CLIENT_ID")?,
                client_secret: req("KEYCLOAK_ADMIN_CLIENT_SECRET")?,
            },
            api_url: parsed_base("KEYCLOAK_ADMIN_API_URL", req("KEYCLOAK_ADMIN_API_URL")?)?,
            agent_client_uuid: req("AGENT_KC_CLIENT_UUID")?,
        },
        licenses_admin: LicensesAdminConfig {
            creds: ClientCredentials {
                token_url: req("LICENSES_ADMIN_TOKEN_URL")?,
                client_id: req("LICENSES_ADMIN_CLIENT_ID")?,
                client_secret: req("LICENSES_ADMIN_CLIENT_SECRET")?,
            },
            url: parsed_base("LICENSES_ADMIN_URL", req("LICENSES_ADMIN_URL")?)?,
            app_key: or("LICENSE_APP_KEY", "agent"),
            plan_code: or("LICENSE_PLAN_CODE", "byo"),
            subject_realm: or("LICENSE_SUBJECT_REALM", "portal"),
        },
        licence_keys,
        public_url,
        frontend_client_id: or("SIGNUP_FRONTEND_CLIENT_ID", "agent"),
        invite_email_lifespan_seconds: number(
            trimmed,
            "INVITE_EMAIL_LIFESPAN_SECONDS",
            14 * 24 * 3600,
        )?,
        invite_resend_cooldown_seconds,
        licences_revoke_timeout_ms,
    })
}

/// `KEYCLOAK_SERVICE_ONLY_REALMS` when unset.
pub const DEFAULT_SERVICE_ONLY_REALMS: &str = "secure-apps";

/// The trusted issuers and the per-door issuer pins (realm per app, A1).
struct Issuers {
    trusted: Vec<String>,
    service_only_realms: Vec<String>,
    intake: String,
    decider: String,
}

/// The realm named by a Keycloak issuer URL (`https://host/realms/<realm>` → `<realm>`), or `None` when the URL has
/// no `/realms/<name>` suffix.
pub fn realm_of_issuer(issuer: &str) -> Option<&str> {
    let (_, realm) = issuer.trim_end_matches('/').rsplit_once("/realms/")?;
    (!realm.is_empty() && !realm.contains('/')).then_some(realm)
}

/// `KEYCLOAK_TRUSTED_ISSUERS`, `KEYCLOAK_SERVICE_ONLY_REALMS`, `SIGNUP_INTAKE_ISSUER`, `SIGNUP_DECIDER_ISSUER`. Every
/// default reproduces the single-issuer behaviour. Refused at boot: an empty or non-http(s) list, a list without
/// `KEYCLOAK_ISSUER` (its JWKS override and the person login would point nowhere), `KEYCLOAK_ISSUER` in a service-only
/// realm (no person could ever sign in), and a door pin outside the list (that door would 401 forever).
fn issuers_from(trimmed: &impl Fn(&str) -> Option<String>, default: &str) -> Result<Issuers> {
    let mut trusted: Vec<String> = Vec::new();
    match trimmed("KEYCLOAK_TRUSTED_ISSUERS") {
        None => trusted.push(default.to_string()),
        Some(raw) => {
            for i in split_list(&raw) {
                let i = i.trim_end_matches('/').to_string();
                let ok = reqwest::Url::parse(&i).is_ok_and(|p| {
                    matches!(p.scheme(), "http" | "https") && p.host_str().is_some()
                });
                if !ok {
                    anyhow::bail!("KEYCLOAK_TRUSTED_ISSUERS must list http(s) issuer URLs");
                }
                if !trusted.contains(&i) {
                    trusted.push(i);
                }
            }
        }
    }
    if trusted.is_empty() {
        anyhow::bail!("KEYCLOAK_TRUSTED_ISSUERS names no issuer (no token could ever verify)");
    }
    if !trusted.iter().any(|t| t == default) {
        anyhow::bail!("KEYCLOAK_TRUSTED_ISSUERS must contain KEYCLOAK_ISSUER");
    }
    let service_only_realms = split_list(
        &trimmed("KEYCLOAK_SERVICE_ONLY_REALMS")
            .unwrap_or_else(|| DEFAULT_SERVICE_ONLY_REALMS.into()),
    );
    if realm_of_issuer(default).is_some_and(|r| service_only_realms.iter().any(|s| s == r)) {
        anyhow::bail!(
            "KEYCLOAK_ISSUER is in a KEYCLOAK_SERVICE_ONLY_REALMS realm: no person could ever sign in"
        );
    }
    let pin = |k: &str| -> Result<String> {
        let v = trimmed(k)
            .map(|v| v.trim_end_matches('/').to_string())
            .unwrap_or_else(|| default.to_string());
        if !trusted.contains(&v) {
            anyhow::bail!("{k} is not in KEYCLOAK_TRUSTED_ISSUERS (that door could never verify)");
        }
        Ok(v)
    };
    let intake = pin("SIGNUP_INTAKE_ISSUER")?;
    let decider = pin("SIGNUP_DECIDER_ISSUER")?;
    Ok(Issuers {
        intake,
        decider,
        trusted,
        service_only_realms,
    })
}

/// `TENANT_OFFBOARD_SA_ISSUER` / `_AZP` / `_ROLE`: all unset = the door is closed (`None`); some set = boot refused
/// (a half-configured door is a typo, not a choice). The issuer must be trusted AND in a service-only realm: a person
/// token can then never be read as this door's caller, and the person super_admin path is untouched. The role must
/// not be the super_admin role, and `azp` is ONE client (no list).
fn offboard_service_from(
    trimmed: &impl Fn(&str) -> Option<String>,
    issuers: &Issuers,
    super_admin_role: &str,
) -> Result<Option<OffboardServiceConfig>> {
    const VARS: [&str; 3] = [
        "TENANT_OFFBOARD_SA_ISSUER",
        "TENANT_OFFBOARD_SA_AZP",
        "TENANT_OFFBOARD_SA_ROLE",
    ];
    let set: Vec<Option<String>> = VARS.iter().map(|k| trimmed(k)).collect();
    let (issuer, azp, role) = match (set[0].clone(), set[1].clone(), set[2].clone()) {
        (None, None, None) => return Ok(None),
        (Some(i), Some(a), Some(r)) => (i.trim_end_matches('/').to_string(), a, r),
        _ => {
            let missing: Vec<&str> = VARS
                .iter()
                .zip(&set)
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| *k)
                .collect();
            anyhow::bail!(
                "TENANT_OFFBOARD_SA_ISSUER, TENANT_OFFBOARD_SA_AZP and TENANT_OFFBOARD_SA_ROLE are set together or not \
                 at all (missing: {})",
                missing.join(", ")
            );
        }
    };
    if !issuers.trusted.contains(&issuer) {
        anyhow::bail!(
            "TENANT_OFFBOARD_SA_ISSUER is not in KEYCLOAK_TRUSTED_ISSUERS (that door could never verify)"
        );
    }
    if !realm_of_issuer(&issuer).is_some_and(|r| issuers.service_only_realms.iter().any(|s| s == r))
    {
        anyhow::bail!(
            "TENANT_OFFBOARD_SA_ISSUER must be in a KEYCLOAK_SERVICE_ONLY_REALMS realm (a person realm's tokens \
             would be read as the service door's)"
        );
    }
    if azp.contains(',') || azp.chars().any(char::is_whitespace) {
        anyhow::bail!("TENANT_OFFBOARD_SA_AZP names exactly one client id");
    }
    if role == super_admin_role || role == crate::auth::SUPER_ADMIN {
        anyhow::bail!("TENANT_OFFBOARD_SA_ROLE must not be the super_admin role");
    }
    Ok(Some(OffboardServiceConfig { issuer, azp, role }))
}

/// The intake / decider / queue / tick settings, once signup is on.
fn signup_from(
    trimmed: &impl Fn(&str) -> Option<String>,
    super_admin_role: &str,
    public_url: &str,
    issuers: &Issuers,
) -> Result<SignupConfig> {
    let or = |k: &str, d: &str| trimmed(k).unwrap_or_else(|| d.to_string());
    let list = |k: &str, d: &str| {
        let v = split_list(&or(k, d));
        if v.is_empty() {
            anyhow::bail!("{k} names no client");
        }
        Ok(v)
    };
    let max_pending: i64 = number(trimmed, "SIGNUP_QUEUE_MAX_PENDING", 500)?;
    if max_pending < 1 {
        anyhow::bail!("SIGNUP_QUEUE_MAX_PENDING must be a whole number of at least 1");
    }
    let tick_seconds: u64 = number(trimmed, "SIGNUP_TICK_SECONDS", 60)?;
    if tick_seconds < 1 {
        anyhow::bail!("SIGNUP_TICK_SECONDS must be at least 1");
    }
    // Telegram (commonpractice / strata_configurator names): on only when the token, chat and forum topic
    // are all set; any missing → off, logged by name at boot, never a crash (the topic id arrives later).
    let names = [
        "TELEGRAM_BOT_TOKEN",
        "TELEGRAM_SIGNUP_CHAT_ID",
        "TELEGRAM_SIGNUP_THREAD_ID",
    ];
    let telegram_missing: Vec<&'static str> =
        names.into_iter().filter(|k| trimmed(k).is_none()).collect();
    let telegram = match (
        trimmed("TELEGRAM_BOT_TOKEN"),
        trimmed("TELEGRAM_SIGNUP_CHAT_ID"),
        trimmed("TELEGRAM_SIGNUP_THREAD_ID"),
    ) {
        (Some(bot_token), Some(chat_id), Some(thread_id)) => {
            let thread_id: i64 = thread_id.parse().map_err(|_| {
                anyhow::anyhow!("TELEGRAM_SIGNUP_THREAD_ID is not a message_thread_id")
            })?;
            // Topic 167 is reserved for external-probe alarms (early-access guide, C1).
            if thread_id == 167 {
                anyhow::bail!(
                    "TELEGRAM_SIGNUP_THREAD_ID must not be 167 (reserved for probe alarms)"
                );
            }
            Some(TelegramConfig {
                bot_token,
                chat_id,
                thread_id,
                api_base: or("TELEGRAM_API_BASE", "https://api.telegram.org")
                    .trim_end_matches('/')
                    .to_string(),
            })
        }
        _ => None,
    };
    let telegram_residency = match trimmed("SIGNUP_TELEGRAM_RESIDENCY")
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        None => None,
        Some("eu") => Some("eu"),
        Some("us") => Some("us"),
        Some(_) => anyhow::bail!("SIGNUP_TELEGRAM_RESIDENCY must be eu or us"),
    };
    if telegram.is_some() && telegram_residency.is_none() {
        anyhow::bail!(
            "SIGNUP_TELEGRAM_RESIDENCY is required when Telegram is configured (eu or us): a Telegram approval \
             cannot choose the residency"
        );
    }
    Ok(SignupConfig {
        intake_azp: list("SIGNUP_INTAKE_AZP", "agent-signup-intake")?,
        intake_role: or("SIGNUP_INTAKE_ROLE", "agent-signup-intake"),
        decider_azp: list("SIGNUP_DECIDER_AZP", "chat-bridge-signup-decider")?,
        decider_role: or("SIGNUP_DECIDER_ROLE", "agent-signup-decide"),
        intake_issuer: issuers.intake.clone(),
        decider_issuer: issuers.decider.clone(),
        max_pending,
        turnstile_secret: trimmed("TURNSTILE_SECRET"),
        turnstile_verify_url: or("TURNSTILE_VERIFY_URL", crate::turnstile::DEFAULT_VERIFY_URL),
        turnstile_timeout_ms: number(trimmed, "TURNSTILE_TIMEOUT_MS", 5000)?,
        telegram,
        telegram_missing,
        super_admin_role: super_admin_role.to_string(),
        public_url: public_url.to_string(),
        app_key: or("SIGNUP_APP_KEY", "agent"),
        tick_seconds,
        telegram_residency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built at runtime (bytes 0..32), so no key literal is ever committed.
    fn key() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode((0u8..32).collect::<Vec<u8>>())
    }

    /// The identity variables (always required).
    const IDENTITY_VARS: [&str; 11] = [
        "AGENT_PUBLIC_URL",
        "LICENSE_TOKEN_ENC_KEY",
        "KEYCLOAK_ADMIN_TOKEN_URL",
        "KEYCLOAK_ADMIN_CLIENT_ID",
        "KEYCLOAK_ADMIN_CLIENT_SECRET",
        "KEYCLOAK_ADMIN_API_URL",
        "AGENT_KC_CLIENT_UUID",
        "LICENSES_ADMIN_TOKEN_URL",
        "LICENSES_ADMIN_CLIENT_ID",
        "LICENSES_ADMIN_CLIENT_SECRET",
        "LICENSES_ADMIN_URL",
    ];

    fn base(k: &str) -> Option<String> {
        match k {
            "DATABASE_URL" => Some("postgres://agent_app@h/agent".into()),
            "MIGRATE_DATABASE_URL" => Some("postgres://agent@h/agent".into()),
            "KEYCLOAK_ISSUER" => Some("https://issuer.test/realms/portal/".into()),
            "KEYCLOAK_AUDIENCE" => Some("agent".into()),
            "AGENT_PUBLIC_URL" => Some("https://agent.test/".into()),
            "LICENSE_TOKEN_ENC_KEY" => Some(key()),
            "KEYCLOAK_ADMIN_TOKEN_URL" => Some("http://kc.test/token".into()),
            "KEYCLOAK_ADMIN_CLIENT_ID" => Some("agent-admin".into()),
            "KEYCLOAK_ADMIN_CLIENT_SECRET" => Some("kc-secret".into()),
            "KEYCLOAK_ADMIN_API_URL" => Some("http://kc.test/admin/realms/portal".into()),
            "AGENT_KC_CLIENT_UUID" => Some("cid-agent".into()),
            "LICENSES_ADMIN_TOKEN_URL" => Some("http://kc.test/token2".into()),
            "LICENSES_ADMIN_CLIENT_ID" => Some("agent-licenses-admin".into()),
            "LICENSES_ADMIN_CLIENT_SECRET" => Some("lic-secret".into()),
            "LICENSES_ADMIN_URL" => Some("http://licenses.test".into()),
            _ => None,
        }
    }

    fn signup_env(k: &str) -> Option<String> {
        match k {
            "EARLY_ACCESS_SIGNUP_ENABLED" => Some("true".into()),
            _ => base(k),
        }
    }

    #[test]
    fn service_actor_id_is_optional_and_a_non_nil_uuid() {
        let cfg = Config::from_lookup(base).unwrap();
        assert_eq!(cfg.internal_providers.service_actor_id, None);
        assert_eq!(cfg.strata.service_actor_id, None);
        let with = |v: &'static str| {
            move |k: &str| match k {
                "SERVICE_ACTOR_ID" => Some(v.to_string()),
                _ => base(k),
            }
        };
        let id = uuid::Uuid::parse_str("0195f48b-d000-7000-8000-0000000000aa").unwrap();
        let cfg = Config::from_lookup(with(" 0195f48b-d000-7000-8000-0000000000aa ")).unwrap();
        assert_eq!(cfg.internal_providers.service_actor_id, Some(id));
        assert_eq!(cfg.strata.service_actor_id, Some(id));
        assert_eq!(
            Config::from_lookup(with(""))
                .unwrap()
                .internal_providers
                .service_actor_id,
            None,
            "blank is unset"
        );
        for bad in [
            "svc-agent",
            "00000000-0000-0000-0000-000000000000",
            "0195f48b-d000",
        ] {
            let e = Config::from_lookup(with(bad)).unwrap_err().to_string();
            assert!(e.contains("SERVICE_ACTOR_ID"), "{bad}: {e}");
        }
    }

    #[test]
    fn redis_is_optional_validated_and_never_printed() {
        let cfg = Config::from_lookup(base).unwrap();
        assert!(cfg.redis.is_none(), "no REDIS_URL: the runtime is dark");
        let with = |url: &'static str, prefix: &'static str| {
            move |k: &str| match k {
                "REDIS_URL" => Some(url.to_string()),
                "AGENT_PREFIX" => Some(prefix.to_string()),
                _ => base(k),
            }
        };
        let cfg =
            Config::from_lookup(with("redis://:s3cr3t-pw@redis.agent:6379/0", "agent:")).unwrap();
        let r = cfg.redis.as_ref().unwrap();
        assert_eq!(r.prefix, "agent:");
        assert_eq!(r.url, "redis://:s3cr3t-pw@redis.agent:6379/0");
        assert!(!format!("{cfg:?}").contains("s3cr3t-pw"));
        for bad in ["http://redis:6379", "redis://", "not a url"] {
            let e = Config::from_lookup(with(bad, "")).unwrap_err().to_string();
            assert!(e.contains("REDIS_URL"), "{bad}: {e}");
        }
        let e = Config::from_lookup(with("redis://r:6379", "a b"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("AGENT_PREFIX"), "{e}");
    }

    #[test]
    fn connector_private_targets_are_optional_and_fail_loud_when_malformed() {
        let cfg = Config::from_lookup(base).unwrap();
        assert!(cfg.connector_policy.private_targets.is_empty());
        assert!(!cfg.connector_policy.allow_loopback);
        let with = |v: &'static str| {
            move |k: &str| match k {
                "CONNECTOR_PRIVATE_TARGETS" => Some(v.to_string()),
                _ => base(k),
            }
        };
        let ok = Config::from_lookup(with(
            r#"[{"host":"frappe.example.com","port":8000,"scheme":"http","tenants":["00000000-0000-0000-0000-0000000000a1"]}]"#,
        ))
        .unwrap();
        assert_eq!(ok.connector_policy.private_targets.len(), 1);
        for bad in [
            "{",
            r#"[{"host":"x","port":0,"scheme":"http","tenants":[]}]"#,
        ] {
            let e = Config::from_lookup(with(bad)).unwrap_err().to_string();
            assert!(e.contains("CONNECTOR_PRIVATE_TARGETS"), "{e}");
        }
    }

    #[test]
    fn connector_sandboxes_are_optional_and_fail_loud_when_malformed() {
        assert!(
            Config::from_lookup(base)
                .unwrap()
                .connector_sandboxes
                .is_empty()
        );
        let with = |v: &'static str| {
            move |k: &str| match k {
                "CONNECTOR_SANDBOXES" => Some(v.to_string()),
                _ => base(k),
            }
        };
        let ok = Config::from_lookup(with(
            r#"{"zendesk_support":{"base_url":"https://acme.zendesk.com","env":"ZD_SANDBOX"}}"#,
        ))
        .unwrap();
        assert_eq!(ok.connector_sandboxes["zendesk_support"].env, "ZD_SANDBOX");
        for bad in ["{", r#"{"k":{"base_url":"","env":"X"}}"#] {
            let e = Config::from_lookup(with(bad)).unwrap_err().to_string();
            assert!(e.contains("CONNECTOR_SANDBOXES"), "{e}");
        }
    }

    #[test]
    fn licence_keys_come_from_the_keyring_vars_or_the_legacy_key() {
        // transitional: the legacy key alone derives a one-key set, kid `v1`
        let cfg = Config::from_lookup(base).unwrap();
        let k = &cfg.identity.licence_keys;
        assert!(k.derived);
        assert_eq!(k.set.current, "v1");
        assert!(
            !format!("{cfg:?}").contains(&key()),
            "the key is never printed"
        );
        // the keyring variables win
        let with_set = |k: &str| match k {
            "LICENSE_TOKEN_ENC_KEYS" => Some(format!(r#"{{"k1":"{}"}}"#, key())),
            "LICENSE_TOKEN_ENC_KEY_CURRENT" => Some("k1".into()),
            _ => base(k),
        };
        let cfg = Config::from_lookup(with_set).unwrap();
        assert!(!cfg.identity.licence_keys.derived);
        assert_eq!(cfg.identity.licence_keys.set.current, "k1");
        // without the legacy key too
        let only_set = |k: &str| match k {
            "LICENSE_TOKEN_ENC_KEY" => None,
            _ => with_set(k),
        };
        let cfg = Config::from_lookup(only_set).unwrap();
        assert!(!cfg.identity.licence_keys.derived);
        assert_eq!(cfg.identity.licence_keys.set.current, "k1");
        // neither: the boot error names the new variable
        let e = Config::from_lookup(|k| match k {
            "LICENSE_TOKEN_ENC_KEY" => None,
            _ => base(k),
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("LICENSE_TOKEN_ENC_KEYS"), "{e}");
        // half a set fails loud, never falling back to the legacy key
        let e = Config::from_lookup(|k| match k {
            "LICENSE_TOKEN_ENC_KEY_CURRENT" => Some("k1".into()),
            _ => base(k),
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("LICENSE_TOKEN_ENC_KEYS"), "{e}");
    }

    #[test]
    fn tenant_data_keys_are_optional_and_fail_loud_when_malformed() {
        assert!(
            Config::from_lookup(base)
                .unwrap()
                .tenant_data_keys
                .is_none()
        );
        let with = |keys: &'static str, current: &'static str| {
            move |k: &str| match k {
                "TENANT_DATA_KEYS" => Some(keys.replace("KEY", &key())),
                "TENANT_DATA_KEY_CURRENT" => Some(current.to_string()),
                _ => base(k),
            }
        };
        let cfg = Config::from_lookup(with(r#"{"k1":"KEY"}"#, "k1")).unwrap();
        let set = cfg.tenant_data_keys.as_ref().expect("configured");
        assert_eq!(set.current, "k1");
        assert!(
            !format!("{cfg:?}").contains(&key()),
            "the key is never printed"
        );
        for (keys, current) in [
            (r#"{"k1":"KEY"}"#, "k2"),
            (r#"{"k1":"short"}"#, "k1"),
            ("[]", "k1"),
        ] {
            let e = Config::from_lookup(with(keys, current))
                .unwrap_err()
                .to_string();
            assert!(e.contains("TENANT_DATA_KEY"), "{e}");
            assert!(!e.contains(&key()), "{e}");
        }
        let only_keys = |k: &str| match k {
            "TENANT_DATA_KEYS" => Some(format!(r#"{{"k1":"{}"}}"#, key())),
            _ => base(k),
        };
        let e = Config::from_lookup(only_keys).unwrap_err().to_string();
        assert!(e.contains("TENANT_DATA_KEY_CURRENT"), "{e}");
    }

    #[test]
    fn split_list_trims_and_drops_blanks() {
        assert_eq!(split_list(" a, ,b ,,c"), vec!["a", "b", "c"]);
        assert!(split_list(" , ").is_empty());
    }

    #[test]
    fn required_env_fails_loud_by_name() {
        for k in [
            "DATABASE_URL",
            "MIGRATE_DATABASE_URL",
            "KEYCLOAK_ISSUER",
            "KEYCLOAK_AUDIENCE",
        ]
        .into_iter()
        .chain(IDENTITY_VARS)
        {
            let err = Config::from_lookup(|x| if x == k { None } else { base(x) })
                .unwrap_err()
                .to_string();
            assert!(err.contains(k), "{k}: {err}");
        }
        let c = Config::from_lookup(base).unwrap();
        assert_eq!(c.bind, "0.0.0.0:8098");
        assert_eq!(c.obs.metrics_bind, "0.0.0.0:9098");
        assert_eq!(c.obs.log_format, LogFormat::Json);
        let c = Config::from_lookup(|k| match k {
            "LOG_FORMAT" => Some("Pretty".into()),
            "METRICS_BIND" => Some("127.0.0.1:9100".into()),
            _ => base(k),
        })
        .unwrap();
        assert_eq!(c.obs.log_format, LogFormat::Pretty);
        assert_eq!(c.obs.metrics_bind, "127.0.0.1:9100");
        let err = Config::from_lookup(|k| match k {
            "LOG_FORMAT" => Some("xml".into()),
            _ => base(k),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("LOG_FORMAT"), "{err}");
        assert_eq!(c.keycloak_issuer, "https://issuer.test/realms/portal");
        assert_eq!(
            c.keycloak_jwks_url,
            "https://issuer.test/realms/portal/protocol/openid-connect/certs"
        );
        assert_eq!(c.super_admin_role, "super_admin");
        assert_eq!(c.keycloak_client_id, "agent", "defaults to the audience");
        let c = Config::from_lookup(|k| match k {
            "KEYCLOAK_CLIENT_ID" => Some(" agent-web ".into()),
            _ => base(k),
        })
        .unwrap();
        assert_eq!(c.keycloak_client_id, "agent-web");
        assert_eq!(c.keycloak_audience, "agent");
        let c = Config::from_lookup(|k| match k {
            "PORT" => Some("9001".into()),
            "REDIS_URL" => Some("redis://ignored".into()),
            _ => base(k),
        })
        .unwrap();
        assert_eq!(c.bind, "0.0.0.0:9001");
        assert!(!c.license_gate_2_enabled, "gate 2 defaults off");
        let gate = Config::from_lookup(|k| match k {
            "LICENSE_GATE_2_ENABLED" => Some("true".into()),
            _ => base(k),
        });
        assert!(
            gate.is_err(),
            "gate 2 without LICENSE_INTROSPECT_URL is refused: no verdict would be written"
        );
        let gate = Config::from_lookup(|k| match k {
            "LICENSE_GATE_2_ENABLED" => Some("true".into()),
            "LICENSE_INTROSPECT_URL" => Some("http://licences.test/license/introspect".into()),
            _ => base(k),
        })
        .unwrap();
        assert!(gate.license_gate_2_enabled);
    }

    #[test]
    fn invite_resend_cooldown_defaults_to_600_and_refuses_under_60() {
        assert_eq!(
            Config::from_lookup(base)
                .unwrap()
                .identity
                .invite_resend_cooldown_seconds,
            600
        );
        let c = Config::from_lookup(|k| match k {
            "INVITE_RESEND_COOLDOWN_SECONDS" => Some("60".into()),
            _ => base(k),
        })
        .unwrap();
        assert_eq!(c.identity.invite_resend_cooldown_seconds, 60);
        for bad in ["59", "0", "soon"] {
            let err = Config::from_lookup(|k| match k {
                "INVITE_RESEND_COOLDOWN_SECONDS" => Some(bad.into()),
                _ => base(k),
            })
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("INVITE_RESEND_COOLDOWN_SECONDS"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn licences_revoke_timeout_is_bounded() {
        for (v, ok) in [
            ("100", true),
            ("8000", true),
            ("99", false),
            ("8001", false),
            ("10000", false),
            ("x", false),
        ] {
            let r = Config::from_lookup(|k| match k {
                "LICENCES_REVOKE_TIMEOUT_MS" => Some(v.into()),
                _ => base(k),
            });
            assert_eq!(r.is_ok(), ok, "{v}");
            if let Err(e) = r {
                assert!(e.to_string().contains("LICENCES_REVOKE_TIMEOUT_MS"), "{e}");
            }
        }
    }

    const PORTAL: &str = "https://issuer.test/realms/portal";
    const AGENT: &str = "https://issuer.test/realms/agent";
    const SECURE_APPS: &str = "https://issuer.test/realms/secure-apps";

    fn with_env(
        extra: Vec<(&'static str, &'static str)>,
        inner: fn(&str) -> Option<String>,
    ) -> impl Fn(&str) -> Option<String> {
        move |k| {
            extra
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
                .or_else(|| inner(k))
        }
    }

    /// Realm per app A1: with none of the new variables set, the config is exactly the single-issuer one.
    #[test]
    fn issuers_default_to_keycloak_issuer_alone() {
        let c = Config::from_lookup(signup_env).unwrap();
        assert_eq!(c.keycloak_issuer, PORTAL);
        assert_eq!(c.keycloak_trusted_issuers, [PORTAL]);
        assert_eq!(c.keycloak_service_only_realms, ["secure-apps"]);
        assert_eq!(
            c.keycloak_jwks_url,
            format!("{PORTAL}/protocol/openid-connect/certs")
        );
        let s = c.signup.unwrap();
        assert_eq!(s.intake_issuer, PORTAL);
        assert_eq!(s.decider_issuer, PORTAL);
        assert_eq!(realm_of_issuer(PORTAL), Some("portal"));
        assert_eq!(realm_of_issuer("https://x.test/"), None);
    }

    #[test]
    fn issuers_parse_the_list_and_the_door_pins() {
        let c = Config::from_lookup(with_env(
            vec![
                (
                    "KEYCLOAK_TRUSTED_ISSUERS",
                    " https://issuer.test/realms/portal/ , https://issuer.test/realms/agent,https://issuer.test/realms/secure-apps,https://issuer.test/realms/agent ",
                ),
                ("KEYCLOAK_SERVICE_ONLY_REALMS", "secure-apps, other"),
                ("SIGNUP_INTAKE_ISSUER", "https://issuer.test/realms/secure-apps/"),
            ],
            signup_env,
        ))
        .unwrap();
        assert_eq!(c.keycloak_trusted_issuers, [PORTAL, AGENT, SECURE_APPS]);
        assert_eq!(c.keycloak_service_only_realms, ["secure-apps", "other"]);
        let s = c.signup.unwrap();
        assert_eq!(s.intake_issuer, SECURE_APPS);
        assert_eq!(s.decider_issuer, PORTAL);
        // the door pin reaches the guard: intake accepts its client from secure-apps only
        let sa = |iss: &str| crate::auth::ServiceClaims {
            iss: iss.into(),
            sub: "sa".into(),
            azp: Some("agent-signup-intake".into()),
            aud: vec!["agent-signup-intake".into()],
            roles: vec!["agent-signup-intake".into()],
            client_roles: vec![],
        };
        assert!(crate::signup::require_intake(&s, &sa(SECURE_APPS)).is_ok());
        let e = crate::signup::require_intake(&s, &sa(PORTAL)).unwrap_err();
        assert_eq!(e.code(), "signup_intake_forbidden");
    }

    /// The tenant-offboard service door: closed unless all three pins are set; the issuer must be a trusted,
    /// service-only realm; the role is never super_admin; azp is one client.
    #[test]
    fn tenant_offboard_service_door_is_closed_unless_fully_pinned() {
        assert_eq!(
            Config::from_lookup(base).unwrap().tenant_offboard_service,
            None
        );
        const TRUSTED: (&str, &str) = (
            "KEYCLOAK_TRUSTED_ISSUERS",
            "https://issuer.test/realms/portal,https://issuer.test/realms/secure-apps",
        );
        let ok = Config::from_lookup(with_env(
            vec![
                TRUSTED,
                (
                    "TENANT_OFFBOARD_SA_ISSUER",
                    "https://issuer.test/realms/secure-apps/",
                ),
                ("TENANT_OFFBOARD_SA_AZP", " agent-tenant-offboarder "),
                ("TENANT_OFFBOARD_SA_ROLE", "agent-tenant-offboard"),
            ],
            base,
        ))
        .unwrap();
        assert_eq!(
            ok.tenant_offboard_service,
            Some(OffboardServiceConfig {
                issuer: SECURE_APPS.into(),
                azp: "agent-tenant-offboarder".into(),
                role: "agent-tenant-offboard".into(),
            })
        );
        let full = |issuer: &'static str, azp: &'static str, role: &'static str| {
            vec![
                TRUSTED,
                ("TENANT_OFFBOARD_SA_ISSUER", issuer),
                ("TENANT_OFFBOARD_SA_AZP", azp),
                ("TENANT_OFFBOARD_SA_ROLE", role),
            ]
        };
        for (extra, want) in [
            // half-configured
            (
                vec![
                    TRUSTED,
                    ("TENANT_OFFBOARD_SA_AZP", "agent-tenant-offboarder"),
                ],
                "missing: TENANT_OFFBOARD_SA_ISSUER, TENANT_OFFBOARD_SA_ROLE",
            ),
            // not trusted
            (
                full("https://evil.test/realms/secure-apps", "a", "r"),
                "not in KEYCLOAK_TRUSTED_ISSUERS",
            ),
            // a person realm
            (full(PORTAL, "a", "r"), "KEYCLOAK_SERVICE_ONLY_REALMS"),
            // super_admin is never the door role
            (full(SECURE_APPS, "a", "super_admin"), "super_admin"),
            // one client, not a list
            (full(SECURE_APPS, "a,b", "r"), "exactly one client"),
        ] {
            let err = Config::from_lookup(with_env(extra, base))
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{err}");
        }
    }

    /// Boot refuses a pin outside the trusted list (that door would 401 forever), a list without KEYCLOAK_ISSUER,
    /// a non-URL entry, and KEYCLOAK_ISSUER in a service-only realm. Checked whether signup is on or not.
    #[test]
    fn boot_refuses_a_bad_issuer_configuration() {
        for (extra, name) in [
            (
                &[("SIGNUP_INTAKE_ISSUER", SECURE_APPS)][..],
                "SIGNUP_INTAKE_ISSUER",
            ),
            (
                &[("SIGNUP_DECIDER_ISSUER", "https://evil.test/realms/portal")][..],
                "SIGNUP_DECIDER_ISSUER",
            ),
            (
                &[
                    (
                        "KEYCLOAK_TRUSTED_ISSUERS",
                        "https://issuer.test/realms/portal",
                    ),
                    ("SIGNUP_DECIDER_ISSUER", AGENT),
                ][..],
                "SIGNUP_DECIDER_ISSUER",
            ),
            (
                &[(
                    "KEYCLOAK_TRUSTED_ISSUERS",
                    "https://issuer.test/realms/agent",
                )][..],
                "KEYCLOAK_TRUSTED_ISSUERS",
            ),
            (
                &[("KEYCLOAK_TRUSTED_ISSUERS", " , ")][..],
                "KEYCLOAK_TRUSTED_ISSUERS",
            ),
            (
                &[(
                    "KEYCLOAK_TRUSTED_ISSUERS",
                    "https://issuer.test/realms/portal,not-a-url",
                )][..],
                "KEYCLOAK_TRUSTED_ISSUERS",
            ),
            (
                &[("KEYCLOAK_SERVICE_ONLY_REALMS", "portal")][..],
                "KEYCLOAK_SERVICE_ONLY_REALMS",
            ),
        ] {
            for env in [signup_env as fn(&str) -> Option<String>, base] {
                let e = Config::from_lookup(with_env(extra.to_vec(), env))
                    .unwrap_err()
                    .to_string();
                assert!(e.contains(name), "{extra:?}: {e}");
            }
        }
        // control: the same pin is accepted once its issuer is trusted
        assert!(
            Config::from_lookup(with_env(
                vec![
                    (
                        "KEYCLOAK_TRUSTED_ISSUERS",
                        "https://issuer.test/realms/portal,https://issuer.test/realms/secure-apps"
                    ),
                    ("SIGNUP_INTAKE_ISSUER", SECURE_APPS),
                ],
                signup_env,
            ))
            .is_ok()
        );
    }

    #[test]
    fn signup_is_dark_by_default() {
        assert!(Config::from_lookup(base).unwrap().signup.is_none());
        let off = Config::from_lookup(|k| {
            if k == "EARLY_ACCESS_SIGNUP_ENABLED" {
                Some("false".into())
            } else {
                signup_env(k)
            }
        })
        .unwrap();
        assert!(off.signup.is_none());
        let bad = Config::from_lookup(|k| {
            if k == "EARLY_ACCESS_SIGNUP_ENABLED" {
                Some("maybe".into())
            } else {
                base(k)
            }
        });
        assert!(bad.is_err());
    }

    #[test]
    fn signup_on_requires_its_env_and_defaults_the_rest() {
        let c = Config::from_lookup(signup_env).unwrap();
        assert_eq!(c.identity.public_url, "https://agent.test");
        assert_eq!(c.identity.frontend_client_id, "agent");
        assert_eq!(c.identity.licenses_admin.app_key, "agent");
        assert_eq!(c.identity.invite_email_lifespan_seconds, 14 * 24 * 3600);
        assert_eq!(c.identity.invite_resend_cooldown_seconds, 600);
        assert_eq!(c.identity.licences_revoke_timeout_ms, 3000);
        let s = c.signup.unwrap();
        assert_eq!(s.public_url, "https://agent.test");
        assert_eq!(s.intake_azp, ["agent-signup-intake"]);
        assert_eq!(s.intake_role, "agent-signup-intake");
        assert_eq!(s.decider_azp, ["chat-bridge-signup-decider"]);
        assert_eq!(s.decider_role, "agent-signup-decide");
        assert_eq!(s.max_pending, 500);
        assert!(s.telegram.is_none());
        assert_eq!(s.telegram_missing.len(), 3);
        for k in IDENTITY_VARS {
            let err = Config::from_lookup(|x| if x == k { None } else { signup_env(x) })
                .unwrap_err()
                .to_string();
            assert!(err.contains(k), "{k}: {err}");
        }
    }

    #[test]
    fn interaction_llm_is_off_until_url_and_token_and_redacts_the_token() {
        let c = Config::from_lookup(base).unwrap();
        assert!(c.interaction_llm.is_none());
        assert_eq!(
            c.interaction_llm_missing,
            ["INTERACTION_LLM_URL", "INTERACTION_LLM_TOKEN"]
        );
        fn with(extra: &[(&str, &str)]) -> Result<Config> {
            Config::from_lookup(|k| {
                extra
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
                    .or_else(|| base(k))
            })
        }
        let half = with(&[("INTERACTION_LLM_URL", "http://router.test/v1")]).unwrap();
        assert!(half.interaction_llm.is_none());
        assert_eq!(half.interaction_llm_missing, ["INTERACTION_LLM_TOKEN"]);
        let c = with(&[
            ("INTERACTION_LLM_URL", " http://router.test/v1/ "),
            ("INTERACTION_LLM_TOKEN", "llm-secret"),
        ])
        .unwrap();
        let l = c.interaction_llm.as_ref().unwrap();
        assert_eq!(l.url, "http://router.test/v1");
        assert_eq!(l.model, "local-llm");
        assert_eq!(l.timeout_s, 120);
        assert!(l.tenants.is_empty(), "unset: nobody is served");
        assert!(c.interaction_llm_missing.is_empty());
        assert!(
            !format!("{c:?}").contains("llm-secret"),
            "Debug redacts the token"
        );
        let c = with(&[
            ("INTERACTION_LLM_URL", "http://router.test"),
            ("INTERACTION_LLM_TOKEN", "t"),
            ("INTERACTION_LLM_MODEL", "qwen"),
            ("INTERACTION_LLM_TIMEOUT_S", "30"),
        ])
        .unwrap();
        let l = c.interaction_llm.unwrap();
        assert_eq!((l.model.as_str(), l.timeout_s), ("qwen", 30));
        let t1 = uuid::Uuid::new_v4();
        let t2 = uuid::Uuid::new_v4();
        let tenants = format!(" {t1}, {t2} ,");
        let c = with(&[
            ("INTERACTION_LLM_URL", "http://router.test"),
            ("INTERACTION_LLM_TOKEN", "t"),
            ("INTERACTION_LLM_TENANTS", tenants.as_str()),
        ])
        .unwrap();
        assert_eq!(c.interaction_llm.unwrap().tenants, [t1, t2]);
        let bads: [&[(&str, &str)]; 4] = [
            &[("INTERACTION_LLM_TENANTS", "not-a-uuid")],
            &[("INTERACTION_LLM_TIMEOUT_S", "0")],
            &[("INTERACTION_LLM_TIMEOUT_S", "soon")],
            &[
                ("INTERACTION_LLM_URL", "router.test"),
                ("INTERACTION_LLM_TOKEN", "t"),
            ],
        ];
        for bad in bads {
            let err = with(bad).unwrap_err().to_string();
            assert!(err.contains("INTERACTION_LLM_"), "{err}");
        }
    }

    #[test]
    fn interaction_webhook_key_is_optional_bounded_and_redacted() {
        let c = Config::from_lookup(base).unwrap();
        assert!(c.interaction_webhook.is_none());
        fn with(v: &str) -> Result<Config> {
            let v = v.to_string();
            Config::from_lookup(move |k| match k {
                "INTERACTION_WEBHOOK_KEY" => Some(v.clone()),
                _ => base(k),
            })
        }
        let short = "s".repeat(31);
        let err = with(&short).unwrap_err().to_string();
        assert!(err.contains("INTERACTION_WEBHOOK_KEY"), "{err}");
        assert!(!err.contains(&short), "the error never carries the value");
        let long = "w".repeat(40);
        let c = with(&format!(" {long} ")).unwrap();
        assert_eq!(c.interaction_webhook.as_ref().unwrap().key, long);
        assert!(!format!("{c:?}").contains(&long), "Debug redacts the key");
    }

    #[test]
    fn edge_seam_is_optional_bounded_and_redacted() {
        let c = Config::from_lookup(base).unwrap();
        assert!(c.edge.internal_key.is_none() && c.edge.deliver_url.is_none());
        fn with(k: &'static str, v: &str) -> Result<Config> {
            let v = v.to_string();
            Config::from_lookup(move |x| if x == k { Some(v.clone()) } else { base(x) })
        }
        let short = "e".repeat(31);
        let err = with("EDGE_INTERNAL_KEY", &short).unwrap_err().to_string();
        assert!(
            err.contains("EDGE_INTERNAL_KEY") && !err.contains(&short),
            "{err}"
        );
        let long = "k".repeat(40);
        let c = with("EDGE_INTERNAL_KEY", &long).unwrap();
        assert_eq!(c.edge.internal_key.as_deref(), Some(long.as_str()));
        assert!(!format!("{c:?}").contains(&long), "Debug redacts the key");
        let c = with("EDGE_DELIVER_URL", "http://edge.svc:8099/").unwrap();
        assert_eq!(c.edge.deliver_url.as_deref(), Some("http://edge.svc:8099"));
        assert!(with("EDGE_DELIVER_URL", "nope").is_err());
    }

    #[test]
    fn voice_is_off_until_both_urls_and_the_token_and_redacts_the_token() {
        let c = Config::from_lookup(base).unwrap();
        assert!(c.voice.is_none());
        assert_eq!(
            c.voice_missing,
            ["VOICE_ASR_URL", "VOICE_TTS_URL", "VOICE_TOKEN"]
        );
        fn with(extra: &[(&str, &str)]) -> Result<Config> {
            Config::from_lookup(|k| {
                extra
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
                    .or_else(|| base(k))
            })
        }
        let half = with(&[("VOICE_ASR_URL", "http://asr.test"), ("VOICE_TOKEN", "t")]).unwrap();
        assert!(half.voice.is_none());
        assert_eq!(half.voice_missing, ["VOICE_TTS_URL"]);
        let c = with(&[
            ("VOICE_ASR_URL", " http://asr.test/ "),
            ("VOICE_TTS_URL", "https://tts.test"),
            ("VOICE_TOKEN", "voice-secret"),
        ])
        .unwrap();
        let v = c.voice.as_ref().unwrap();
        assert_eq!(v.asr_url, "http://asr.test");
        assert_eq!(v.tts_url, "https://tts.test");
        assert_eq!(v.tts_voice, "af_heart");
        assert_eq!(v.timeout_s, 60);
        assert_eq!(v.kind, SpeechProviderKind::W2v);
        assert!(v.platform_tenants.is_empty());
        assert!(c.voice_missing.is_empty());
        let t1 = uuid::Uuid::new_v4();
        let t2 = uuid::Uuid::new_v4();
        let tenants = format!(" {t1}, {t2} ,");
        let c = with(&[
            ("VOICE_ASR_URL", "http://gw.test/v1"),
            ("VOICE_TTS_URL", "http://gw.test/v1"),
            ("VOICE_TOKEN", "voice-secret"),
            ("VOICE_PROVIDER_KIND", "openai_compatible"),
            ("VOICE_PLATFORM_TENANTS", tenants.as_str()),
        ])
        .unwrap();
        let v = c.voice.as_ref().unwrap();
        assert_eq!(v.kind, SpeechProviderKind::OpenaiCompatible);
        assert_eq!(
            (
                v.asr_model.as_str(),
                v.tts_model.as_str(),
                v.tts_voice.as_str()
            ),
            ("whisper-1", "tts-1", "alloy")
        );
        assert_eq!(v.platform_tenants, [t1, t2]);
        assert!(
            !format!("{c:?}").contains("voice-secret"),
            "Debug redacts the token"
        );
        let bads: [&[(&str, &str)]; 5] = [
            &[("VOICE_PROVIDER_KIND", "azure")],
            &[("VOICE_PLATFORM_TENANTS", "not-a-uuid")],
            &[("VOICE_TIMEOUT_S", "0")],
            &[("VOICE_TIMEOUT_S", "soon")],
            &[
                ("VOICE_ASR_URL", "asr.test"),
                ("VOICE_TTS_URL", "http://tts.test"),
                ("VOICE_TOKEN", "t"),
            ],
        ];
        for bad in bads {
            let err = with(bad).unwrap_err().to_string();
            assert!(err.contains("VOICE_"), "{err}");
        }
    }

    #[test]
    fn telegram_is_off_until_all_three_are_set_and_never_topic_167() {
        fn with(extra: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
            move |k: &str| {
                extra
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
                    .or_else(|| signup_env(k))
            }
        }
        let full = vec![
            ("TELEGRAM_BOT_TOKEN", "bot-secret"),
            ("TELEGRAM_SIGNUP_CHAT_ID", "-1003934173351"),
            ("TELEGRAM_SIGNUP_THREAD_ID", "4242"),
            ("SIGNUP_TELEGRAM_RESIDENCY", "eu"),
        ];
        let c = Config::from_lookup(with(full.clone())).unwrap();
        assert_eq!(c.signup.as_ref().unwrap().telegram_residency, Some("eu"));
        // Telegram on without the residency a tap approves into: refused at boot, by name
        let err = Config::from_lookup(with(full[..3].to_vec()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("SIGNUP_TELEGRAM_RESIDENCY"), "{err}");
        let mut bad = full.clone();
        bad[3] = ("SIGNUP_TELEGRAM_RESIDENCY", "mars");
        assert!(Config::from_lookup(with(bad)).is_err());
        let t = c.signup.as_ref().unwrap().telegram.as_ref().unwrap();
        assert_eq!(t.thread_id, 4242);
        assert_eq!(t.api_base, "https://api.telegram.org");
        let shown = format!("{c:?}");
        assert!(!shown.contains("bot-secret"), "Debug redacts the bot token");
        assert!(!shown.contains("kc-secret") && !shown.contains("lic-secret"));
        // the topic id not created yet (gitops ships without it): off, not a crash
        let partial = Config::from_lookup(with(full[..2].to_vec())).unwrap();
        let s = partial.signup.unwrap();
        assert!(s.telegram.is_none());
        assert_eq!(s.telegram_missing, ["TELEGRAM_SIGNUP_THREAD_ID"]);
        let probe = vec![
            ("TELEGRAM_BOT_TOKEN", "b"),
            ("TELEGRAM_SIGNUP_CHAT_ID", "c"),
            ("TELEGRAM_SIGNUP_THREAD_ID", "167"),
        ];
        assert!(Config::from_lookup(with(probe)).is_err());
    }

    /// The bundle minter is all-or-nothing, defaults client id / bundle / ttl, bounds the ttl, and is read by the
    /// runner and the daemons too (they call the router and strata for a tenant).
    #[test]
    fn the_bundle_minter_is_all_or_nothing_with_defaults() {
        let with = |extra: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                extra
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(bundle_minter_from(&with(&[])).unwrap().is_none());
        let partial = bundle_minter_from(&with(&[("BUNDLE_MINTER_LICENSES_URL", "http://l")]));
        assert!(
            partial
                .unwrap_err()
                .to_string()
                .contains("together or not at all")
        );
        let full = &[
            ("BUNDLE_MINTER_LICENSES_URL", "http://licences.example.com/"),
            ("BUNDLE_MINTER_TOKEN_URL", "https://auth/token"),
            ("BUNDLE_MINTER_CLIENT_SECRET", "s3cret"),
        ];
        let c = bundle_minter_from(&with(full)).unwrap().unwrap();
        assert_eq!(
            (
                c.licenses_url.as_str(),
                c.client_id.as_str(),
                c.bundle.as_str(),
                c.ttl_minutes
            ),
            (
                "http://licences.example.com",
                "agent-bundle-minter",
                "agent",
                15
            )
        );
        assert!(!format!("{c:?}").contains("s3cret"));
        let too_long = &[
            ("BUNDLE_MINTER_LICENSES_URL", "http://l"),
            ("BUNDLE_MINTER_TOKEN_URL", "http://t"),
            ("BUNDLE_MINTER_CLIENT_SECRET", "x"),
            ("BUNDLE_TOKEN_TTL_MINUTES", "61"),
        ];
        assert!(bundle_minter_from(&with(too_long)).is_err());
        // the runner and daemons parse the same variables, and no guard lists them as unread
        for k in [
            "BUNDLE_MINTER_LICENSES_URL",
            "BUNDLE_MINTER_TOKEN_URL",
            "BUNDLE_MINTER_CLIENT_ID",
            "BUNDLE_MINTER_CLIENT_SECRET",
            "LICENSE_BUNDLE_KEY",
            "BUNDLE_TOKEN_TTL_MINUTES",
        ] {
            assert!(!RUNNER_UNREAD_VARS.contains(&k), "{k}");
        }
        let rc = RunnerConfig::from_lookup(|k| match k {
            "BUNDLE_MINTER_LICENSES_URL" => Some("http://l".into()),
            "BUNDLE_MINTER_TOKEN_URL" => Some("http://t".into()),
            "BUNDLE_MINTER_CLIENT_SECRET" => Some("x".into()),
            other => runner_env(other),
        })
        .unwrap();
        assert!(rc.app.bundle_minter.is_some());
    }

    /// The runner's whole environment (B4 F1): the daemon pair, nothing of identity.
    fn runner_env(k: &str) -> Option<String> {
        match k {
            "DATABASE_URL" => Some("postgres://agent_runtime:rt-pw@h/agent".into()),
            "REDIS_URL" => Some("redis://:redis-pw@redis.agent:6379/0".into()),
            "AGENT_PREFIX" => Some("agent:".into()),
            _ => None,
        }
    }

    #[test]
    fn the_runner_loads_with_every_admin_secret_unset_and_holds_none() {
        for k in IDENTITY_VARS.iter().chain(&[
            "KEYCLOAK_ISSUER",
            "KEYCLOAK_AUDIENCE",
            "MIGRATE_DATABASE_URL",
        ]) {
            assert!(runner_env(k).is_none(), "{k}");
            assert!(
                *k == "MIGRATE_DATABASE_URL" || RUNNER_UNREAD_VARS.contains(k),
                "{k} is read by agent-api and must be listed as unread by the runner"
            );
        }
        for k in [
            Purpose::LicenceToken.keys_var(),
            Purpose::LicenceToken.current_var(),
            LEGACY_KEY_VAR,
        ] {
            assert!(RUNNER_UNREAD_VARS.contains(&k), "{k}");
        }
        let rc = RunnerConfig::from_lookup(runner_env).unwrap();
        assert_eq!(
            rc.runtime.database_url,
            "postgres://agent_runtime:rt-pw@h/agent"
        );
        assert_eq!(rc.app.database_url, rc.runtime.database_url);
        assert_eq!(rc.runtime.redis.prefix, "agent:");
        assert!(rc.app.signup.is_none());
        assert!(rc.ignored.is_empty());
        let shown = format!("{:?}", rc.runtime);
        assert!(
            !shown.contains("rt-pw") && !shown.contains("redis-pw"),
            "{shown}"
        );

        // agent-api's environment verbatim (admin secrets included, owner login aside): none of it is held
        let rc = RunnerConfig::from_lookup(|k| match k {
            "MIGRATE_DATABASE_URL" => None,
            _ => runner_env(k).or_else(|| base(k)),
        })
        .unwrap();
        let id = &rc.app.identity;
        assert!(id.keycloak_admin.creds.client_secret.is_empty());
        assert!(id.licenses_admin.creds.client_secret.is_empty());
        assert!(id.keycloak_admin.creds.client_id.is_empty());
        assert!(id.licence_keys.set.keys.is_empty());
        for url in [
            id.keycloak_admin.creds.token_url.as_str(),
            id.keycloak_admin.api_url.as_str(),
            id.licenses_admin.creds.token_url.as_str(),
            id.licenses_admin.url.as_str(),
            id.public_url.as_str(),
            rc.app.migrate_database_url.as_str(),
            rc.app.keycloak_issuer.as_str(),
            rc.app.keycloak_jwks_url.as_str(),
        ] {
            assert!(url.contains(".invalid"), "{url}");
        }
        for k in [
            "KEYCLOAK_ADMIN_CLIENT_SECRET",
            "LICENSES_ADMIN_CLIENT_SECRET",
            "LICENSE_TOKEN_ENC_KEY",
            "KEYCLOAK_ISSUER",
        ] {
            assert!(rc.ignored.contains(&k), "{k} is set and must be reported");
        }
        // agent-api itself still requires every one of them
        for k in IDENTITY_VARS {
            let e = Config::from_lookup(|v| if v == k { None } else { base(v) })
                .unwrap_err()
                .to_string();
            assert!(e.contains(k), "{k}: {e}");
        }
    }

    #[test]
    fn the_daemons_fail_loud_by_name_on_a_missing_or_malformed_variable() {
        let without = |skip: &'static str| {
            move |k: &str| {
                if k == skip {
                    return None;
                }
                runner_env(k)
            }
        };
        for skip in ["DATABASE_URL", "REDIS_URL"] {
            let e = RuntimeConfig::from_lookup(without(skip))
                .unwrap_err()
                .to_string();
            assert_eq!(e, format!("{skip} is required"));
            let e = RunnerConfig::from_lookup(without(skip))
                .unwrap_err()
                .to_string();
            assert_eq!(e, format!("{skip} is required"));
        }
        let e = RuntimeConfig::from_lookup(|_| None)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "DATABASE_URL is required, REDIS_URL is required");
        let with = |key: &'static str, val: &'static str| {
            move |k: &str| {
                if k == key {
                    return Some(val.to_string());
                }
                runner_env(k)
            }
        };
        for (k, v) in [
            ("REDIS_URL", "http://:redis-pw@redis.agent:6379"),
            ("AGENT_PREFIX", "a b"),
            ("LOG_FORMAT", "xml"),
        ] {
            let e = RuntimeConfig::from_lookup(with(k, v))
                .unwrap_err()
                .to_string();
            assert!(e.contains(k) && !e.contains("redis-pw"), "{k}: {e}");
        }
        // the runner: the owner login is refused outright; agent-api's own settings still fail the boot by name
        let e = RunnerConfig::from_lookup(with("MIGRATE_DATABASE_URL", "postgres://agent@h/agent"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("MIGRATE_DATABASE_URL must not be set"), "{e}");
        let e = RunnerConfig::from_lookup(with("STRATA_RAG_URL", "https://rag.test"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("STRATA_RAG_URL"), "{e}");
    }
}
