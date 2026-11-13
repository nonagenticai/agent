//! The interaction API: the one door through which a user's request enters the agent platform and its answer leaves
//! it. Web chat, Telegram, voice, email and plain API clients are connectors built against it, sync or streaming.
//!
//! - [`model`]: the channel-agnostic request ([`InteractionRequest`]) and what a backend streams back
//!   ([`BackendEvent`], [`BackendError`]); the input limits ([`InteractionRequest::validate`]).
//! - [`backend`]: the [`InteractionBackend`] trait (B4's runtime implements it; `answer` = the sync answer with the
//!   grounding the backend reports), [`collect`] (sync = drain the stream) and [`Unconfigured`] (409
//!   `provider_not_configured`).
//! - [`llm`]: [`LlmPassthrough`], the platform backend (an OpenAI-compatible chat-completions stream), serving only
//!   the owner test tenants in `INTERACTION_LLM_TENANTS` (D102).
//! - [`respond`] (D109, KH T6, A4): the ONE dispatch every caller uses. A request with a party of any role (an API
//!   key, a connector's customer, a body `party`) is a B4 runtime turn
//!   (crate::runtime::backend::runtime_stream: its conversation created first, then `turn::accept_message`;
//!   409 `job_not_live` without a live release that has a runtime design, 503 `unavailable` while no runtime process
//!   serves the scope; the caller's `instructions` are not applied, D108). Only a request without a party — the
//!   staff draft playground: a job member calling without a party — goes to `AppState.interaction`. No caller path
//!   reaches [`LlmPassthrough`].
//! - [`ProviderBackend`] (B3 T4d, D101/D102): `AppState.interaction`, the staff playground's backend. The platform
//!   passthrough is tried first for its allow-listed tenants; every other tenant is answered by the provider its
//!   job's LIVE design binds to the `qualify` use (the provider B4's runner uses; crate::providers::llm::resolve),
//!   streamed OpenAI-compatible through a client pinned to that provider's host (crate::netguard). Unbound, keyless
//!   or gone: 409 `provider_not_configured`.
//! - [`shapes`]: the wire shapes (native, OpenAI Responses, OpenAI chat-completions) as pure translations.
//! - [`routes`]: `POST /api/jobs/{job}/{interactions,responses,chat/completions}`.
//!
//! An in-process connector (voice) skips HTTP: build an [`InteractionRequest`] with [`InteractionRequest::new`] on a
//! [`Scope`], call [`InteractionRequest::validate`], then [`respond`]`(state, req)` and read the stream (or
//! [`collect`] it). Never `state.interaction` directly: that skips the runtime dispatch.
//!
//! Platform-hosted backends are allow-listed test-tenant stand-ins (D102); tenants bring their own providers (B3).
//!
//! Customer text is data: never in a log line, an audit detail or a metric label.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use sqlx::PgPool;
use uuid::Uuid;

pub mod apikeys;
pub mod backend;
pub mod llm;
pub mod mcp;
pub mod model;
pub mod routes;
pub mod shapes;
pub mod webhook;

pub use backend::{InteractionBackend, Unconfigured, collect};
pub use llm::LlmPassthrough;
pub use model::{
    BackendError, BackendEvent, ConversationRef, InteractionRequest, InteractionResult, Message,
    Part, Party, PartyRole, Role, Scope, Usage,
};

use crate::config::{Config, InternalProvidersConfig};
use crate::crypto::keyring::Keyring;
use crate::design::{DesignOf, JobRef, Registry};
use crate::error::ApiError;
use crate::netguard::{self, Policy};
use crate::providers::llm::{self as providers_llm, Kind, ResolveCtx, ResolvedLlm, Use};
use crate::tenancy::begin_scoped;

/// D109/A4: whether `req` is a runtime turn: it has a party of any role (an API key caller, an email sender, a
/// connector's customer, a body `party`). Only a request without a party (a job member: the staff draft playground) is
/// not.
pub fn is_runtime_caller(req: &InteractionRequest) -> bool {
    req.party.is_some()
}

/// The one answer path (D109, module doc): a request with a party is a runtime turn, anything else is
/// `AppState.interaction`'s (the staff playground).
pub fn respond(
    st: &crate::state::AppState,
    req: InteractionRequest,
) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
    if is_runtime_caller(&req) {
        crate::runtime::backend::runtime_stream(st.clone(), req)
    } else {
        st.interaction.respond(req)
    }
}

/// The bound on one whole provider call (streamed body included) when `INTERACTION_LLM_TIMEOUT_S` is not configured.
pub const PROVIDER_TIMEOUT_S: u64 = 120;

/// What [`ProviderBackend`] reads a job's provider with (the parts of `AppState` it needs; it lives inside it).
pub struct ProviderDeps {
    pub db: PgPool,
    pub design: Arc<Registry>,
    pub keyring: Option<Arc<Keyring>>,
    /// The outbound policy tenant hosts are checked against (`CONNECTOR_PRIVATE_TARGETS`, crate::netguard).
    pub policy: Policy,
}

/// The backend the config names: a [`ProviderBackend`], with the platform [`LlmPassthrough`] tried first when
/// `INTERACTION_LLM_URL` and `INTERACTION_LLM_TOKEN` are set (the missing names are logged, never a value).
pub fn from_config(
    cfg: &Config,
    deps: ProviderDeps,
) -> anyhow::Result<Arc<dyn InteractionBackend>> {
    let platform = match &cfg.interaction_llm {
        Some(c) => {
            tracing::info!(model = %c.model, timeout_s = c.timeout_s, tenants = c.tenants.len(), "interaction_backend_llm_passthrough");
            Some(LlmPassthrough::from_config(
                c,
                cfg.internal_providers.service_actor_id,
            )?)
        }
        None => {
            tracing::info!(missing = ?cfg.interaction_llm_missing, "interaction_backend_unconfigured");
            None
        }
    };
    Ok(Arc::new(ProviderBackend::new(cfg, deps, platform)))
}

/// B3 T4d (D101/D102): the platform passthrough for its allow-listed tenants, else the job's own `qualify` provider.
pub struct ProviderBackend {
    platform: Option<LlmPassthrough>,
    inner: Arc<Inner>,
}

struct Inner {
    deps: ProviderDeps,
    platform_tenant: Option<Uuid>,
    internal: InternalProvidersConfig,
    timeout: Duration,
}

impl ProviderBackend {
    /// `platform`: the passthrough tried first for the tenants on its list (`INTERACTION_LLM_TENANTS`).
    pub fn new(cfg: &Config, deps: ProviderDeps, platform: Option<LlmPassthrough>) -> Self {
        let timeout_s = cfg
            .interaction_llm
            .as_ref()
            .map_or(PROVIDER_TIMEOUT_S, |c| c.timeout_s);
        Self {
            platform,
            inner: Arc::new(Inner {
                deps,
                platform_tenant: cfg.platform_tenant_id,
                internal: cfg.internal_providers.clone(),
                timeout: Duration::from_secs(timeout_s),
            }),
        }
    }
}

impl InteractionBackend for ProviderBackend {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        if let Some(p) = &self.platform
            && p.tenants.contains(&req.tenant)
        {
            return p.respond(req);
        }
        let inner = self.inner.clone();
        stream::once(async move {
            let found = inner.passthrough(&req).await;
            match found {
                Ok(Some(p)) => p.respond(req),
                Ok(None) => stream::once(async {
                    Err(BackendError::NotConfigured(
                        "no provider serves this job's qualify use".into(),
                    ))
                })
                .boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            }
        })
        .flatten()
        .boxed()
    }
}

impl Inner {
    /// The job's LIVE `qualify` provider as a one-request passthrough (`None`: unbound, gone, keyless, not
    /// OpenAI-compatible). Only codes are logged: never the host, the key or the URL.
    async fn passthrough(
        &self,
        req: &InteractionRequest,
    ) -> Result<Option<LlmPassthrough>, BackendError> {
        let job = JobRef {
            id: req.job_id,
            key: req.job_key.clone(),
        };
        let ctx = ResolveCtx {
            platform_tenant: self.platform_tenant,
            internal: &self.internal,
            design: self.deps.design.as_ref(),
            keyring: self.deps.keyring.as_deref(),
            // built before AppState exists: an unbound tenant use stays unbound here (no tenant licence lookup)
            st: None,
        };
        let resolved = self.resolve(ctx, req.tenant, &job).await.map_err(|e| {
            let (status, code) = e.status_and_code();
            tracing::warn!(
                status = status.as_u16(),
                code,
                "interaction_provider_unresolved"
            );
            if status == axum::http::StatusCode::NOT_FOUND {
                BackendError::NotConfigured("the job has no provider".into())
            } else {
                BackendError::Unavailable("the job's provider could not be read".into())
            }
        })?;
        let Some(r) = resolved else {
            return Ok(None);
        };
        client_for(&self.deps.policy, self.timeout, req.tenant, r).await
    }

    async fn resolve(
        &self,
        ctx: ResolveCtx<'_>,
        tenant: Uuid,
        job: &JobRef,
    ) -> Result<Option<ResolvedLlm>, ApiError> {
        let mut tx = begin_scoped(&self.deps.db, tenant).await?;
        let r = providers_llm::resolve_in(&mut tx, ctx, tenant, job, Use::Qualify, &DesignOf::Live)
            .await?;
        tx.commit().await?;
        Ok(r)
    }
}

/// `r` as a one-request passthrough for `tenant` (`None`: not OpenAI-compatible, or a tenant provider without a key):
/// the internal model directly, a tenant provider only through the netguard under `policy`.
async fn client_for(
    policy: &Policy,
    timeout: Duration,
    tenant: Uuid,
    r: ResolvedLlm,
) -> Result<Option<LlmPassthrough>, BackendError> {
    if r.kind != Kind::Openai {
        return Ok(None);
    }
    let base = r.base_url.trim_end_matches('/').to_string();
    // only the internal model (no provider row) is billed through the router; a tenant provider never sees it
    let effective_tenant = r.provider_id.is_none() && r.effective_tenant.is_some();
    let (http, token) = match (r.provider_id, r.key) {
        // the internal pgx vLLM (D70, config URL): not tenant input, so not through netguard
        (None, key) => {
            let http = providers_llm::platform_client(timeout, None, None).map_err(|_| {
                BackendError::Unavailable("the model client could not be built".into())
            })?;
            (
                http,
                key.map(|k| k.expose().to_string()).unwrap_or_default(),
            )
        }
        (Some(_), None) => return Ok(None),
        (Some(_), Some(key)) => {
            let refused = |e: crate::connectors::wire::OpError| {
                tracing::warn!(code = e.code, "interaction_provider_refused");
                BackendError::Unavailable("the model endpoint is unreachable".into())
            };
            let url = netguard::check_url(&base, policy, tenant).map_err(refused)?;
            let http = netguard::pinned_client(&url, policy, tenant, timeout)
                .await
                .map_err(refused)?;
            (http, key.expose().to_string())
        }
    };
    Ok(Some(LlmPassthrough {
        http,
        url: base,
        token,
        model: r.model,
        default_instructions: llm::DEFAULT_INSTRUCTIONS.into(),
        tenants: HashSet::from([tenant]),
        effective_tenant,
        actor: if effective_tenant { r.actor } else { None },
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

    /// A fake chat-completions stream recording each call's X-App-Id, X-Project-Id and X-Actor-Id.
    async fn upstream() -> (String, Arc<Mutex<Vec<[Option<String>; 3]>>>) {
        let seen: Arc<Mutex<Vec<[Option<String>; 3]>>> = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |h: axum::http::HeaderMap| {
                let seen = s2.clone();
                async move {
                    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(String::from);
                    seen.lock().unwrap().push([
                        get("x-app-id"),
                        get("x-project-id"),
                        get("x-actor-id"),
                    ]);
                    ([("content-type", "text/event-stream")], SSE)
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}/v1"), seen)
    }

    // Guards `client_for`'s `actor: r.actor`: the job's internal `qualify` model is called with the router identity
    // and the resolved actor (agent's service account), not without one.
    #[tokio::test]
    async fn the_internal_qualify_passthrough_carries_the_resolved_actor() {
        let (url, seen) = upstream().await;
        let tenant = Uuid::new_v4();
        let actor = Uuid::new_v4();
        let r = ResolvedLlm {
            provider_id: None,
            base_url: url,
            model: "local-llm".into(),
            kind: Kind::Openai,
            funding: "local_gpu",
            key: None,
            voice: None,
            effective_tenant: Some(tenant),
            actor: Some(actor),
            tenant_licence: None,
        };
        let p = client_for(&Policy::default(), Duration::from_secs(5), tenant, r)
            .await
            .unwrap()
            .expect("the internal model serves");
        let req = InteractionRequest::new(
            &Scope {
                tenant,
                job_id: Uuid::new_v4(),
                job_key: "faq_agent".into(),
            },
            vec![Message::text(Role::User, "hi")],
        );
        assert_eq!(collect(p.respond(req)).await.unwrap().text, "ok");
        assert_eq!(
            *seen.lock().unwrap(),
            [[
                Some("agent".to_string()),
                Some(providers_llm::tenant_level_project_id(tenant).to_string()),
                Some(actor.to_string()),
            ]]
        );
    }
}
