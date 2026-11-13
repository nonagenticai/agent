use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;

use crate::auth::Verifier;
use crate::config::Config;
use crate::connectors;
use crate::conversations::edge_link::EdgeLink;
use crate::conversations::hub::{EventHub, LocalHub};
use crate::crypto::keyring::{Keyring, Purpose};
use crate::design::{self, Registry};
use crate::identity::Identity;
use crate::interaction::InteractionBackend;
use crate::interaction::webhook::Webhooks;
use crate::members::seen::Throttle;
use crate::platform::hooks::{GeneratorGuard, ObjectContextSources};
use crate::provision::Senders;
use crate::readiness::{self, Readiness};
use crate::release::compile::{LayerCompiler, PinsAdopted, TemplateCompiler};
use crate::release::conflicts::{self, ConflictDetectors};
use crate::release::hooks::ApprovedHooks;
use crate::runtime::{self, Bus};
use crate::signup::Signup;
use crate::voice::SpeechResolver;

#[derive(Clone)]
pub struct AppState {
    /// The runtime pool, connected as `agent_app` (`DATABASE_URL`): every query is under row-level security.
    pub db: PgPool,
    pub http: reqwest::Client,
    pub cfg: Arc<Config>,
    pub auth: Arc<Verifier>,
    /// Keycloak admin, licences admin and the licence-token cipher (B1 T4; B7 T1 keyring), shared by tenant admin and
    /// signup.
    pub identity: Arc<Identity>,
    /// Mints a tenant's short-lived bundle token for the inference-router / strata calls
    /// (crate::tenants::app_licence); unconfigured = those calls fail closed. In every binary's state.
    pub bundle_minter: Arc<crate::tenants::app_licence::BundleMinter>,
    /// The outbox senders by op (B1 registers none; B3 and B7 do). The drainer claims only these ops.
    pub senders: Arc<Senders>,
    /// Per-process throttles (member sessions, the `/api/me` licence mint).
    pub seen: Arc<Throttle>,
    /// `None` while early-access signup is dark (`EARLY_ACCESS_SIGNUP_ENABLED` unset).
    pub signup: Option<Arc<Signup>>,
    /// What blocks a template tag (B6 §4; B8 replaces the default [`crate::platform::hooks::NoHooks`]): passed to `tag_family`.
    pub generator_guard: Arc<dyn GeneratorGuard>,
    /// Where an object's systems and activities come from (B6 §4; B7/B8 replace the default [`crate::platform::hooks::NoHooks`]).
    pub object_context: Arc<dyn ObjectContextSources>,
    /// The design targets by ledger kind (B2 §2.2): every draft write, read and release goes through them.
    pub design: Arc<Registry>,
    /// The template compiler (B2 §5): [`LayerCompiler`] (B6a's pinned compile + the B2 seed sections). Approve and
    /// apply compile through it; tests may put another compiler here. The rule targets of `design` hold the
    /// compiler `build` made (they find rule slots through it): replacing this field does not reach them.
    pub compiler: Arc<dyn TemplateCompiler>,
    /// The release-readiness contributors (B2 §2.6, crate::readiness::registry): the boards and the publish gate
    /// (D67) run them; tests may replace it with a registry holding fakes.
    pub readiness: Arc<Readiness>,
    /// Run by approve when a cut adopts new template pins (B2 §2.7); B5 registers its base re-materialiser.
    pub pins_adopted: Vec<Arc<dyn PinsAdopted>>,
    /// Run after an approve commits (B2 §2.7 "release approved", F-06; crate::release::hooks); B8 registers the
    /// validation grid. A hook failure is logged and never blocks the approve or a later apply.
    pub release_approved: Arc<ApprovedHooks>,
    /// The release-conflict detectors (B2 §2.7, crate::release::conflicts::registry): apply recomputes the open
    /// conflicts through them and a cut that adopts pins raises its upgrade conflicts; B5 adds `base_shadowed`.
    pub conflict_detectors: Arc<ConflictDetectors>,
    /// The TenantData keyring (B7 T8, crate::connectors::custody); `None` while `TENANT_DATA_KEYS` is unset.
    pub tenant_keyring: Option<Arc<Keyring>>,
    /// Connector calls (B7 T6/T10): the netguard policy from `CONNECTOR_PRIVATE_TARGETS` + the OAuth token cache.
    pub connectors: connectors::transport::Transport,
    /// What answers the staff draft playground (crate::interaction::ProviderBackend, B3 T4d): the platform passthrough
    /// for `INTERACTION_LLM_TENANTS` (the passthrough's own list), else the job's own live `qualify` provider (none: 409 `provider_not_configured`).
    /// D109: never a customer party — crate::interaction::respond sends those to the B4 runtime. Tests set a fake.
    pub interaction: Arc<dyn InteractionBackend>,
    /// Which speech services serve a job's voice turns (crate::voice::ProviderSpeech, B3 T4d): the platform gateway
    /// for `VOICE_PLATFORM_TENANTS`, else the job's own live `asr` + `tts` providers (409 `provider_not_configured`
    /// without both); tests set a fake.
    pub speech: Arc<dyn SpeechResolver>,
    /// Async interactions' signed callbacks (crate::interaction::webhook): `None` while `INTERACTION_WEBHOOK_KEY` is
    /// unset (a `callback_url` is 409 `webhooks_not_configured`); tests replace it to reach a local receiver.
    pub webhooks: Option<Arc<Webhooks>>,
    /// The ProviderSecret keyring (B3, crate::providers::secret): provider keys, slot keys, source credentials;
    /// `None` while `PROVIDER_SECRET_KEYS` is unset (every secret read/write is 503
    /// `provider_secret_key_unconfigured`). The strata URLs and tokens are `cfg.strata`.
    pub provider_keyring: Option<Arc<Keyring>>,
    /// The frames behind the conversation / handover SSE feeds (crate::conversations::hub).
    pub events: Arc<dyn EventHub>,
    /// agent-api → agent-edge (`/internal/deliver`, `/internal/notify-workers`): `None` unless `EDGE_INTERNAL_KEY` and
    /// `EDGE_DELIVER_URL` are both set (replies are then stored, not delivered); tests point it at a local receiver.
    pub edge: Option<Arc<EdgeLink>>,
    /// The root bus of the B4 runtime (crate::runtime): scope registry, nudges, the console. `None` while
    /// `REDIS_URL` is unset — the runtime is dark and every runtime write path is a no-op; agent-api still boots.
    /// Connected lazily, so a Redis outage never stops the admin API.
    pub redis: Option<Bus>,
    /// D109 (KH T6): the Live runtime designs of the prod scopes, shared by the customer gate
    /// (crate::runtime::backend::job_live) and the turn's drain, so one request compiles the design at most once;
    /// dropped on a config nudge (followed while `REDIS_URL` is set) and after 30 s.
    pub designs: Arc<crate::runtime::design::DesignCache>,
    /// B4 T10 (Q-B4-3): the one Redis Pub/Sub connection that republishes the runtime's `console` / `chat` frames
    /// into `events` while a feed is open (crate::sse_hub). `None` while `REDIS_URL` is unset: the feeds serve
    /// `events` alone.
    pub sse: Option<Arc<crate::sse_hub::Bridge>>,
    /// The gate hooks (B4 T9b seams, B5 T12): P1 (the drain and the probe), R1 (the interaction daemon) and the route
    /// hinter. `assemble` installs B5's in-process hooks (crate::job_design::gates::hooks); tests may replace them.
    pub gates: crate::runtime::gates::Gates,
}

impl AppState {
    /// The task backends: the platform MCP server when configured, and the connector backend (the conveyor).
    pub fn task_clients(&self) -> crate::connectors::tasks::TaskClients {
        crate::connectors::tasks::TaskClients {
            platform: self.connectors.mcp().map(|m| m.client.clone()),
            connector: Arc::new(crate::connectors::tasks::ConnectorTasks::new(
                self.db.clone(),
                self.connectors.clone(),
                self.tenant_keyring.clone(),
                self.bundle_minter.clone(),
            )),
        }
    }
}

/// Migrate as the owner (one connection, closed before anything is served), then open the runtime pool, refuse a
/// runtime role that would bypass RLS.
pub async fn build(cfg: Config) -> Result<AppState> {
    let owner = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&cfg.migrate_database_url)
        .await
        .context("connect MIGRATE_DATABASE_URL (migration owner)")?;
    sqlx::migrate!("./migrations").run(&owner).await?;
    // E3: the platform tenant's identity row (no content), before the platform-owned flag is reconciled onto it.
    crate::platform::bootstrap::ensure_platform_tenant(
        &mut *owner.acquire().await?,
        cfg.platform_tenant_id,
    )
    .await
    .context("ensure the platform tenant (AGENT_PLATFORM_TENANT_ID)")?;
    // B3 T3 (D70): reconcile the platform-owned flag on every boot: exactly the `AGENT_PLATFORM_TENANT_ID` tenant
    // has it, and none does when the variable is unset (a NULL binding clears every flag).
    sqlx::query(
        "UPDATE tenants SET platform_owned = (id IS NOT DISTINCT FROM $1) \
          WHERE platform_owned IS DISTINCT FROM (id IS NOT DISTINCT FROM $1)",
    )
    .bind(cfg.platform_tenant_id)
    .execute(&owner)
    .await
    .context("reconcile the platform-owned tenant (AGENT_PLATFORM_TENANT_ID)")?;
    owner.close().await;
    let db = transaction_safe(PgPoolOptions::new())
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&cfg.database_url)
        .await
        .context("connect DATABASE_URL (runtime role)")?;
    refuse_rls_bypass(&db).await?;
    let redis = match &cfg.redis {
        Some(r) => Some(Bus::connect_lazy(&r.url, &r.prefix)?),
        None => {
            tracing::info!("runtime_dark: REDIS_URL is unset");
            None
        }
    };
    // B4 T1: rebuild the scope registry from the database before serving. Bounded and never fails the boot: Redis
    // may not accept yet, and agent-api's `scopes::spawn_reconciler` retries until it does.
    if let Some(root) = &redis {
        runtime::scopes::reconcile_once(&db, root).await;
    }
    let st = assemble(cfg, db, redis)?;
    // B3 T10 (review I4): jobs older than B3 get their template's knowledge kinds first, so the coverage backfill
    // below copies them into those jobs' older baselines and releases. Never fails the boot.
    crate::knowledge::kinds::backfill(&st.db).await;
    // B2 §2.5 late registration: copy newly registered kinds into older releases and baselines before serving.
    design::coverage::backfill(&st.db, &st.design)
        .await
        .map_err(|e| anyhow::anyhow!("backfill design coverage (B2 §2.5): {e}"))?;
    Ok(st)
}

/// A connection that goes back to the pool still inside a transaction is closed, never handed out again. sqlx
/// tracks only transactions it finished opening: a request cancelled after `BEGIN` reached the server but before
/// sqlx counted it (a browser aborting a fetch drops the handler) returns its connection mid-transaction, and the
/// next `begin()` on it joins that transaction ("already in progress") and fails on its first `SET TRANSACTION`.
/// The server knows: inside an explicit transaction the session's `xact_start` is older than the probe's own
/// `query_start`; outside one they are equal. A probe that fails (an aborted transaction) closes the connection too.
pub fn transaction_safe(opts: PgPoolOptions) -> PgPoolOptions {
    opts.after_release(|conn, _meta| {
        Box::pin(async move {
            // the simple query protocol: an implicit transaction starts with the statement itself (with the
            // extended protocol it starts at Parse, before the statement's own start)
            let row = sqlx::raw_sql(
                "SELECT xact_start < query_start FROM pg_stat_activity WHERE pid = pg_backend_pid()",
            )
            .fetch_one(&mut *conn)
            .await;
            let open: Option<Option<bool>> = row.ok().and_then(|r| r.try_get(0).ok());
            Ok(open == Some(Some(false)))
        })
    })
}

/// B4 T6: the state of a runtime process that calls connectors and built-ins (the `runner` binary): `cfg.database_url`
/// connects as `agent_runtime` (B4 0065). No migration, no seed, no backfill, no runtime bus (the process holds its
/// own): those are agent-api's. Refuses a role that bypasses RLS.
pub async fn build_runtime(cfg: Config) -> Result<AppState> {
    let db = transaction_safe(PgPoolOptions::new())
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&cfg.database_url)
        .await
        .context("connect DATABASE_URL (agent_runtime)")?;
    let bypass: bool = sqlx::query_scalar(
        "SELECT r.rolsuper OR r.rolbypassrls OR EXISTS (
                    SELECT 1 FROM pg_tables t WHERE t.schemaname = 'public' AND t.tableowner = current_user)
           FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(&db)
    .await
    .context("read the runtime role's attributes")?;
    if bypass {
        anyhow::bail!(
            "DATABASE_URL connects as a role that bypasses RLS (superuser, BYPASSRLS or table owner); \
             it must be agent_runtime"
        );
    }
    // the runner never serves `/ready`: nothing to report
    assemble(cfg, db, None)
}

/// B5 T12: the state a B4 daemon that boots on `RuntimeConfig` (orchestrator, interaction) runs the gate hooks and
/// the assistant with — on the daemon's own pool, from the env it already has ([`Config::for_runtime_daemon`]). Never
/// fails the boot and adds no requirement: `None` (logged) = the daemon runs without them. Without
/// `PROVIDER_SECRET_KEYS` a keyed gates binding resolves unbound (no Laya, no gate: fail open).
pub fn optional_runtime(db: PgPool) -> Option<AppState> {
    let cfg = match Config::for_runtime_daemon(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("runtime_state_unavailable: {e:#}");
            return None;
        }
    };
    match assemble(cfg, db, None) {
        Ok(st) => Some(st),
        Err(e) => {
            tracing::warn!("runtime_state_unavailable: {e:#}");
            None
        }
    }
}

/// Everything of [`AppState`] past the pools: keyrings, clients, the design registry, the interaction and speech
/// backends, senders and hubs.
fn assemble(cfg: Config, db: PgPool, redis: Option<Bus>) -> Result<AppState> {
    let tenant_keyring = cfg
        .tenant_data_keys
        .as_ref()
        .map(|set| Arc::new(Keyring::new(Purpose::TenantData, set)));
    let provider_keyring = cfg
        .provider_secret_keys
        .as_ref()
        .map(|set| Arc::new(Keyring::new(Purpose::ProviderSecret, set)));
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let auth = Arc::new(Verifier::jwks_many(
        &cfg.keycloak_trusted_issuers,
        &cfg.keycloak_service_only_realms,
        &cfg.keycloak_issuer,
        &cfg.keycloak_jwks_url,
        &cfg.keycloak_audience,
        &cfg.keycloak_client_id,
        http.clone(),
    ));
    let identity = Arc::new(Identity::new(cfg.identity.clone(), http.clone()));
    let bundle_minter = Arc::new(crate::tenants::app_licence::BundleMinter::new(
        cfg.bundle_minter.clone(),
        http.clone(),
    ));
    let signup = cfg
        .signup
        .clone()
        .map(|s| Arc::new(Signup::new(s, http.clone())));
    // MCP-transport connectors call the MCP server as each tenant (its mcp licence + binding, crate::mcp_client::creds)
    let connectors = match &cfg.mcp {
        None => connectors::transport::Transport::new(cfg.connector_policy.clone()),
        Some(m) => {
            let source = crate::mcp_client::creds::AgentMcpSource::new(
                db.clone(),
                tenant_keyring.clone(),
                bundle_minter.clone(),
                cfg.bundle_minter.clone(),
                http.clone(),
                m.url.trim_end_matches("/mcp"),
            );
            let client = Arc::new(crate::mcp_client::client::HttpMcp::new(
                m.url.clone(),
                crate::mcp_client::client::TenantCredentials::new(source),
            ));
            connectors::transport::Transport::with_mcp(
                cfg.connector_policy.clone(),
                connectors::transport::mcp::McpTransport::new(client, m.task_tools.clone()),
            )
        }
    };
    let compiler: Arc<dyn TemplateCompiler> = Arc::new(LayerCompiler::new());
    let speech = crate::voice::from_config(&cfg)?;
    let webhooks = crate::interaction::webhook::from_config(&cfg);
    let edge = EdgeLink::from_config(&cfg.edge, http.clone()).map(Arc::new);
    let registry = Arc::new(design::registry(compiler.clone()));
    // B3 T4d (D101/D102): the job's own qualify provider; the platform passthrough (INTERACTION_LLM_TENANTS) first
    let interaction = crate::interaction::from_config(
        &cfg,
        crate::interaction::ProviderDeps {
            db: db.clone(),
            design: registry.clone(),
            keyring: provider_keyring.clone(),
            policy: cfg.connector_policy.clone(),
        },
    )?;
    // B3 (§0 R5): one sender per op string
    let mut senders = Senders::default();
    senders.register(Arc::new(crate::provision::senders::stores::StoreEnsure));
    senders.register(Arc::new(
        crate::provision::senders::connections::ConnectionsPut,
    ));
    senders.register(Arc::new(
        crate::provision::senders::store_delete::StoreDelete,
    ));
    let events: Arc<dyn EventHub> = Arc::new(LocalHub::default());
    let sse = redis
        .as_ref()
        .map(|root| crate::sse_hub::Bridge::new(root, events.clone()));
    let designs = Arc::new(crate::runtime::design::DesignCache::default());
    if let Some(root) = &redis {
        // the handle is dropped: the follower lives as long as the process
        let _follow = designs.follow(root.clone());
    }
    Ok(AppState {
        db,
        http,
        auth,
        identity,
        bundle_minter,
        senders: Arc::new(senders),
        seen: Arc::new(Throttle::default()),
        signup,
        generator_guard: Arc::new(crate::generator::guard::B8Hooks),
        object_context: Arc::new(crate::generator::guard::B8Hooks),
        design: registry,
        compiler,
        readiness: Arc::new(readiness::registry()),
        // B3 T10 (§0 R9): adopted template pins re-materialise the job's knowledge kinds
        pins_adopted: vec![crate::knowledge::kinds::pins_adopted_hook()],
        release_approved: Arc::new(ApprovedHooks::default()),
        conflict_detectors: Arc::new(conflicts::registry()),
        tenant_keyring,
        connectors,
        interaction,
        speech,
        webhooks,
        provider_keyring,
        events,
        edge,
        redis,
        designs,
        sse,
        gates: crate::job_design::gates::hooks::gates(),
        cfg: Arc::new(cfg),
    })
}

/// RLS is silently off for a superuser, a BYPASSRLS role and the owner of a table (no FORCE ROW LEVEL
/// SECURITY), so the runtime pool must connect as none of them. It must also reach `agent_platform` the one
/// safe way (crate::platform_db): a member WITHOUT inheritance (else every platform grant would apply to
/// plain `agent_app` statements) and WITH the SET option (else the platform tables are unreachable).
pub async fn refuse_rls_bypass(db: &PgPool) -> Result<()> {
    let bypass: bool = sqlx::query_scalar(
        "SELECT r.rolsuper OR r.rolbypassrls OR EXISTS (
                    SELECT 1 FROM pg_tables t WHERE t.schemaname = 'public' AND t.tableowner = current_user)
           FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(db)
    .await
    .context("read the runtime role's attributes")?;
    if bypass {
        anyhow::bail!(
            "DATABASE_URL connects as a role that bypasses RLS (superuser, BYPASSRLS or table owner); \
             it must be agent_app"
        );
    }
    let (ok, inherit, no_set): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE NOT m.inherit_option AND m.set_option),
                count(*) FILTER (WHERE m.inherit_option),
                count(*) FILTER (WHERE NOT m.set_option)
           FROM pg_auth_members m
           JOIN pg_roles p ON p.oid = m.roleid AND p.rolname = 'agent_platform'
           JOIN pg_roles u ON u.oid = m.member AND u.rolname = current_user",
    )
    .fetch_one(db)
    .await
    .context("read the runtime role's agent_platform membership")?;
    if inherit > 0 {
        anyhow::bail!(
            "the runtime role inherits agent_platform (membership INHERIT TRUE): platform tables would be \
             open to every request; re-grant it WITH INHERIT FALSE, SET TRUE"
        );
    }
    if ok == 0 || no_set > 0 {
        anyhow::bail!(
            "the runtime role is not a member of agent_platform WITH INHERIT FALSE, SET TRUE: \
             GRANT agent_platform TO agent_app WITH INHERIT FALSE, SET TRUE"
        );
    }
    Ok(())
}

/// Same as `build`, but tokens are verified against a static HS256 key instead of Keycloak's JWKS.
/// `main` never calls this.
pub async fn build_for_tests(cfg: Config, secret: &[u8]) -> Result<AppState> {
    let mut st = build(cfg).await?;
    st.auth = Arc::new(Verifier::test_key_many(
        &st.cfg.keycloak_trusted_issuers,
        &st.cfg.keycloak_service_only_realms,
        &st.cfg.keycloak_audience,
        &st.cfg.keycloak_client_id,
        secret,
    ));
    Ok(st)
}
