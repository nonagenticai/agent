//! Async interactions with signed webhook callbacks: `POST /api/jobs/{job}/interactions` with a `callback_url` is 202
//! `queued`; the answer (the native object, `completed` or `failed`) is POSTed to the URL, signed per Standard
//! Webhooks (standardwebhooks.com).
//!
//! - Each job has its own signing secret: HMAC-SHA256(`INTERACTION_WEBHOOK_KEY`,
//!   `agent-interaction-webhook:v1:{tenant}:{job_id}`), shown as `whsec_<base64>` by
//!   `GET /api/jobs/{job}/interactions/webhook-secret` (integrations_manager).
//! - Every delivery carries `webhook-id: int_<id>` (the same on every retry), `webhook-timestamp` (unix seconds) and
//!   `webhook-signature: v1,<base64(HMAC-SHA256(secret, "{id}.{timestamp}.{body}"))>`.
//! - Retries follow [`Webhooks::retry`] (default 0 s, 2 s, 10 s, 30 s): a 2xx is delivered; a network error, timeout,
//!   5xx or 429 is retried; any other status gives up. 10 s per attempt, no redirects, no proxy.
//! - SSRF: [`check_callback_url`] runs at submit AND before every attempt (https only; every resolved address
//!   public), and the connection is pinned to the addresses it checked, so DNS rebinding cannot redirect it.
//! - At most [`MAX_PENDING`] deliveries are pending (not delivered or failed) per tenant (else 429
//!   `too_many_pending_interactions`).
//!
//! Durable (B13 §1 row 6, migration 0114): the route stores one `interaction_deliveries` row (the translated request,
//! the caller, the URL) in its request transaction and kicks the worker ([`spawn_worker`], started at boot). The worker
//! claims due rows of active tenants (`agent_interaction_deliveries_due`, SKIP LOCKED, at most 8 leased per tenant,
//! a [`LEASE`] renewed every [`HEARTBEAT`]), answers (`answering`; the callback body is stored), then delivers
//! (`delivering`; `attempts` and `next_attempt_at` are written after every attempt) until `delivered` or `failed`
//! (`last_error`). A row whose worker died is claimed again once its lease expires; a stored answer is never
//! recomputed, the attempt count carries over, and a row claimed more than [`MAX_CLAIMS`] times without an answer is
//! failed. A suspended or archived tenant's row is not claimed (and failed if the tenant changed after the claim). Final rows older than 7 days are deleted by the tick. `GET /api/jobs/{job}/interactions/deliveries/{id}` reads
//! the state.
//! Customer text (the body, the callback URL) is never logged; the log line and the metric carry ids and outcomes.
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use base64::Engine;
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use ring::hmac;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::{Notify, Semaphore};
use uuid::Uuid;

use super::backend::collect;
use super::model::{BackendError, ConversationRef, InteractionRequest, Message, Part, Party, Role};
use super::shapes::Encoder;
use super::shapes::native::NativeEncoder;
use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// Deliveries pending (queued, answering or delivering) per tenant; one more is 429 `too_many_pending_interactions`.
pub const MAX_PENDING: i64 = 64;
/// Rows one process works on at once (each holds a [`Webhooks::workers`] permit until it ends).
pub const MAX_WORKERS: usize = 64;
/// The bound on one delivery attempt.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest callback URL accepted (characters).
pub const MAX_URL: usize = 2048;
/// How long an async interaction may take to answer before its callback reports a failure.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(300);
/// A claim's lease; the worker renews it every [`HEARTBEAT`] while it works the row, so a row whose worker died is
/// claimed again within a minute (the claim function's 60 s; at most 8 rows per tenant are leased at once).
pub const LEASE: Duration = Duration::from_secs(60);
/// How often a worked row's lease is renewed.
pub const HEARTBEAT: Duration = Duration::from_secs(20);
/// A row claimed more often than this without a stored answer is failed `too_many_claims`.
pub const MAX_CLAIMS: i32 = 5;
/// The worker's tick (it is also kicked by every submit and every finished row).
pub const TICK: Duration = Duration::from_secs(5);
/// The worker prunes final rows older than 7 days every this many ticks (10 min), and on its first tick.
pub const PRUNE_EVERY: u64 = 120;
/// `last_error` is cut to this many characters (the column's bound).
pub const MAX_ERROR: usize = 500;
pub const SCHEME: &str = "standard-webhooks-v1";
pub const ID_HEADER: &str = "webhook-id";
pub const TIMESTAMP_HEADER: &str = "webhook-timestamp";
pub const SIGNATURE_HEADER: &str = "webhook-signature";
pub const AGENT: &str = "agent-interaction-webhooks/1";
/// The metric every delivery counts once, by `outcome` ([`Outcome::as_str`]).
pub const DELIVERIES_METRIC: &str = "agent_interaction_webhook_deliveries_total";

/// The delay before each attempt (the first is usually zero).
pub fn default_retry() -> Vec<Duration> {
    [0, 2, 10, 30].map(Duration::from_secs).to_vec()
}

/// The callback sender: the master key, the worker's concurrency and kick, and the retry policy.
pub struct Webhooks {
    key: hmac::Key,
    /// [`MAX_WORKERS`] permits; a claimed row holds one until it is delivered, failed or handed back.
    pub workers: Arc<Semaphore>,
    /// Wakes the worker before its tick (a submit, a finished row).
    pub kick: Notify,
    /// The delay before each attempt; its length is the number of attempts.
    pub retry: Vec<Duration>,
    /// Private, loopback and plain-http callbacks (tests only; false in production).
    pub allow_private: bool,
}

/// `Some` when `INTERACTION_WEBHOOK_KEY` is set (logged by state, never the key).
pub fn from_config(cfg: &Config) -> Option<Arc<Webhooks>> {
    let w = cfg
        .interaction_webhook
        .as_ref()
        .map(|c| Arc::new(Webhooks::new(&c.key)));
    tracing::info!(enabled = w.is_some(), "interaction_webhooks");
    w
}

/// How a delivery ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A 2xx.
    Delivered,
    /// A 4xx other than 429 (or a redirect): the receiver refused it; not retried.
    Rejected,
    /// Every attempt failed with a network error, a timeout, a 5xx or a 429.
    Exhausted,
    /// The URL no longer passes [`check_callback_url`] (it now resolves to a private address); a host that does not
    /// resolve is retried instead.
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Delivered => "delivered",
            Outcome::Rejected => "rejected",
            Outcome::Exhausted => "exhausted",
            Outcome::Refused => "refused",
        }
    }
}

/// One attempt's result; the strings are `last_error` (a status or a class of failure, never the URL or the body).
enum Attempt {
    Done,
    Retry(String),
    GiveUp(String),
}

impl Webhooks {
    /// `key` is the master key (`INTERACTION_WEBHOOK_KEY`); production policy (public https only, default retries).
    pub fn new(key: &str) -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes()),
            workers: Arc::new(Semaphore::new(MAX_WORKERS)),
            kick: Notify::new(),
            retry: default_retry(),
            allow_private: false,
        }
    }

    /// The 32-byte signing secret of one job: HMAC-SHA256(master key, `agent-interaction-webhook:v1:{tenant}:{job}`).
    pub fn job_secret(&self, tenant: Uuid, job_id: Uuid) -> Vec<u8> {
        let label = format!("agent-interaction-webhook:v1:{tenant}:{job_id}");
        let tag = hmac::sign(&self.key, label.as_bytes());
        let bytes: &[u8] = tag.as_ref();
        bytes.to_vec()
    }
}

// ---- the stored request -------------------------------------------------------------------------------------------

/// The translated request as stored in `interaction_deliveries.request` (customer text: never logged). The ids
/// (interaction, tenant, job) are the row's own columns.
pub fn encode_request(r: &InteractionRequest, created_at: Option<&str>) -> Value {
    let input: Vec<Value> = r
        .input
        .iter()
        .map(|m| {
            let parts: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => json!({"type": "text", "text": t}),
                    Part::Data(d) => json!({"type": "data", "data": d}),
                })
                .collect();
            json!({"role": m.role, "parts": parts})
        })
        .collect();
    json!({
        "v": 1,
        "job_key": r.job_key,
        "channel": r.channel,
        "party": r.party.as_ref().map(|p| json!({"id": p.id, "display_name": p.display_name, "role": p.role})),
        "conversation": r.conversation,
        "instructions": r.instructions,
        "input": input,
        "metadata": r.metadata,
        "created_at": created_at,
    })
}

#[derive(Deserialize)]
struct StoredRequest {
    job_key: String,
    channel: String,
    party: Option<Party>,
    conversation: Option<ConversationRef>,
    instructions: Option<String>,
    input: Vec<StoredMessage>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct StoredMessage {
    role: Role,
    parts: Vec<StoredPart>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum StoredPart {
    Text { text: String },
    Data { data: Value },
}

/// The request back from its stored form, and the `created_at` the 202 answer carried; `None` when unreadable.
pub fn decode_request(
    id: Uuid,
    tenant: Uuid,
    job_id: Uuid,
    v: &Value,
) -> Option<(InteractionRequest, Option<String>)> {
    let s: StoredRequest = serde_json::from_value(v.clone()).ok()?;
    let input = s
        .input
        .into_iter()
        .map(|m| Message {
            role: m.role,
            parts: m
                .parts
                .into_iter()
                .map(|p| match p {
                    StoredPart::Text { text } => Part::Text(text),
                    StoredPart::Data { data } => Part::Data(data),
                })
                .collect(),
        })
        .collect();
    let req = InteractionRequest {
        id,
        tenant,
        job_id,
        job_key: s.job_key,
        channel: s.channel,
        party: s.party,
        conversation: s.conversation,
        instructions: s.instructions,
        input,
        metadata: s.metadata,
    };
    Some((req, s.created_at))
}

// ---- submit -------------------------------------------------------------------------------------------------------

/// 429 `too_many_pending_interactions`.
pub fn too_many_pending() -> ApiError {
    ApiError::detailed(
        StatusCode::TOO_MANY_REQUESTS,
        "too_many_pending_interactions",
        "too many interactions are waiting for delivery; try again shortly",
    )
}

/// Store the queued row of `req` (its id is the row id) in one tenant-scoped transaction, under the tenant's advisory
/// lock so the [`MAX_PENDING`] count cannot be raced past. `created_at` is the 202 object's, kept so the callback
/// carries the same one.
pub async fn enqueue(
    db: &PgPool,
    req: &InteractionRequest,
    caller: &Value,
    url: &str,
    created_at: Option<&str>,
) -> ApiResult<()> {
    let mut tx = begin_scoped(db, req.tenant).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("interaction-deliveries:{}", req.tenant))
        .execute(&mut *tx)
        .await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM interaction_deliveries WHERE state IN ('queued', 'answering', 'delivering')",
    )
    .fetch_one(&mut *tx)
    .await?;
    if pending >= MAX_PENDING {
        return Err(too_many_pending());
    }
    sqlx::query(
        "INSERT INTO interaction_deliveries (id, tenant_id, job_id, caller, request, callback_url) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(req.id)
    .bind(req.tenant)
    .bind(req.job_id)
    .bind(caller)
    .bind(encode_request(req, created_at))
    .bind(url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Deliver a message the platform produced later (an MCP task's question, result or escalation notice, posted by the
/// task relay) to the callback URL of the `api` conversation's latest async request (`conversation_state`
/// `callback_url`, written by the backend from an intake-validated delivery row; no other source). It is a
/// `delivering` row carrying its body, so the worker signs, retries and SSRF-checks it like any callback. Best
/// effort: a failure is logged, never an error of the relay. Nothing happens without webhooks, without a stored URL,
/// or for a conversation that is not an `api` one.
pub async fn notify_message(
    st: &AppState,
    scope: &super::Scope,
    conversation_id: &str,
    kind: &str,
    message_id: Option<Uuid>,
    text: &str,
) {
    let Some(w) = st.webhooks.clone() else {
        return;
    };
    let run = async {
        let mut tx =
            crate::conversations::store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
        let url = crate::conversations::state::load(&mut tx, conversation_id)
            .await?
            .get(crate::conversations::state::CALLBACK_URL)
            .and_then(|v| v.as_value().map(str::to_owned));
        let thread = crate::conversations::store::customer_ref(&mut tx, conversation_id)
            .await?
            .filter(|(c, _)| c.channel == super::model::DEFAULT_CHANNEL)
            .map(|(c, _)| c.id);
        let (Some(url), Some(thread)) = (url, thread) else {
            return ApiResult::Ok(None);
        };
        let id = Uuid::now_v7();
        let body = json!({
            "id": format!("int_{id}"),
            "object": "interaction.message",
            "kind": kind,
            "job": scope.job_key,
            "conversation": {"id": thread},
            "message": {"id": message_id.unwrap_or(id), "role": "assistant", "text": text},
            "created_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        });
        sqlx::query(
            "INSERT INTO interaction_deliveries (id, tenant_id, job_id, request, callback_url, state, answer) \
             VALUES ($1, app_current_tenant(), $2, $3, $4, 'delivering', $5)",
        )
        .bind(id)
        .bind(scope.job_id)
        .bind(json!({"v": 1, "kind": "message"}))
        .bind(&url)
        .bind(&body)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(id))
    };
    match run.await {
        Ok(Some(_)) => w.kick.notify_one(),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(conversation = conversation_id, error = %e, "task_message_callback_failed");
        }
    }
}

// ---- the worker ---------------------------------------------------------------------------------------------------

/// One claimed row (the columns the worker needs). `claims` (incremented by every claim) is the fence token: only
/// the worker holding the latest claim writes the row.
#[derive(Clone, Debug, sqlx::FromRow)]
struct DueRow {
    id: Uuid,
    tenant_id: Uuid,
    job_id: Uuid,
    request: Value,
    callback_url: String,
    answer: Option<Value>,
    attempts: i32,
    claims: i32,
}

const CLAIM: &str = "SELECT id, tenant_id, job_id, request, callback_url, answer, attempts, claims \
                       FROM agent_interaction_deliveries_due($1)";

/// The worker: [`run_due`] on every [`TICK`] and on every kick, [`prune`] every [`PRUNE_EVERY`] ticks; never stops.
/// `None` while webhooks are not configured (no row can be queued then).
pub fn spawn_worker(st: AppState) -> Option<tokio::task::JoinHandle<()>> {
    let w = st.webhooks.clone()?;
    Some(tokio::spawn(async move {
        let mut ticks: u64 = 0;
        loop {
            if ticks.is_multiple_of(PRUNE_EVERY) {
                prune(&st.db).await;
            }
            ticks = ticks.wrapping_add(1);
            run_due(&st, &w).await;
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = w.kick.notified() => {}
            }
        }
    }))
}

/// Delete final rows older than 7 days (`agent_interaction_deliveries_prune`); failures are logged.
pub async fn prune(db: &PgPool) {
    match sqlx::query_scalar::<_, i64>("SELECT agent_interaction_deliveries_prune()")
        .fetch_one(db)
        .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!(deleted = n, "interaction_deliveries_pruned"),
        Err(e) => tracing::warn!(error = %e, "interaction_deliveries_prune_failed"),
    }
}

/// One pass: claim as many due rows as there are free worker permits and work each in its own task. Returns how many
/// were claimed.
pub async fn run_due(st: &AppState, w: &Arc<Webhooks>) -> usize {
    let free = w.workers.available_permits();
    if free == 0 {
        return 0;
    }
    let rows: Vec<DueRow> = match sqlx::query_as(CLAIM)
        .bind(i32::try_from(free).unwrap_or(i32::MAX))
        .fetch_all(&st.db)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "interaction_deliveries_claim_failed");
            return 0;
        }
    };
    let n = rows.len();
    for row in rows {
        let Ok(permit) = w.workers.clone().acquire_owned().await else {
            break;
        };
        let (st, w) = (st.clone(), w.clone());
        tokio::spawn(async move {
            work(&st, &w, row).await;
            drop(permit);
            w.kick.notify_one();
        });
    }
    n
}

/// The writes a worker makes to a row it holds.
enum Write {
    /// `answering` (the backend is called next).
    Answering,
    /// `delivering` with the stored callback body (a resume never recomputes it).
    Answered(Value),
    /// `delivered` after this many attempts.
    Delivered(i32),
    /// `failed` after this many attempts, with `last_error`.
    Failed(i32, String),
    /// Still `delivering`: this many attempts, the last error, the next attempt in this many seconds.
    Retry(i32, String, f64),
}

/// A fenced write: only the holder of the row's latest claim (`claims` as claimed) changes it (false = the row was
/// claimed again by another worker, or the write failed; both are logged). A final state releases the lease.
async fn fenced(db: &PgPool, row: &DueRow, write: Write) -> bool {
    let run = async {
        let mut tx = begin_scoped(db, row.tenant_id).await?;
        let q = match write {
            Write::Answering => sqlx::query(
                "UPDATE interaction_deliveries SET state = 'answering', updated_at = now() \
                  WHERE id = $1 AND claims = $2",
            )
            .bind(row.id)
            .bind(row.claims),
            Write::Answered(body) => sqlx::query(
                "UPDATE interaction_deliveries SET state = 'delivering', answer = $3, updated_at = now() \
                  WHERE id = $1 AND claims = $2",
            )
            .bind(row.id)
            .bind(row.claims)
            .bind(body),
            Write::Delivered(n) => sqlx::query(
                "UPDATE interaction_deliveries SET state = 'delivered', attempts = $3, last_error = NULL, \
                        lease_until = NULL, updated_at = now() \
                  WHERE id = $1 AND claims = $2",
            )
            .bind(row.id)
            .bind(row.claims)
            .bind(n),
            Write::Failed(n, why) => sqlx::query(
                "UPDATE interaction_deliveries SET state = 'failed', attempts = $3, last_error = $4, \
                        lease_until = NULL, updated_at = now() \
                  WHERE id = $1 AND claims = $2",
            )
            .bind(row.id)
            .bind(row.claims)
            .bind(n)
            .bind(cut(&why)),
            Write::Retry(n, why, secs) => sqlx::query(
                "UPDATE interaction_deliveries SET attempts = $3, last_error = $4, \
                        next_attempt_at = now() + make_interval(secs => $5), updated_at = now() \
                  WHERE id = $1 AND claims = $2",
            )
            .bind(row.id)
            .bind(row.claims)
            .bind(n)
            .bind(cut(&why))
            .bind(secs),
        };
        let n = q.execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok::<_, ApiError>(n == 1)
    };
    match run.await {
        Ok(true) => true,
        Ok(false) => {
            tracing::info!(interaction = %row.id, "interaction_delivery_lease_lost");
            false
        }
        Err(e) => {
            tracing::warn!(interaction = %row.id, error = %e, "interaction_delivery_write_failed");
            false
        }
    }
}

/// `last_error` within its bound.
fn cut(why: &str) -> String {
    why.chars().take(MAX_ERROR).collect()
}

/// Renews the lease of a row every [`HEARTBEAT`] while it is worked; stops when dropped (the work ended, or panicked)
/// or when the row was claimed again elsewhere.
struct Heartbeat(tokio::task::JoinHandle<()>);

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn heartbeat(db: &PgPool, row: &DueRow) -> Heartbeat {
    let (db, id, tenant, claims) = (db.clone(), row.id, row.tenant_id, row.claims);
    Heartbeat(tokio::spawn(async move {
        loop {
            tokio::time::sleep(HEARTBEAT).await;
            if !renew(&db, id, tenant, claims).await {
                break;
            }
        }
    }))
}

/// `lease_until` = now + [`LEASE`], fenced by the claim; false once the row is final or claimed again (a failed write
/// is logged and keeps the heartbeat going).
async fn renew(db: &PgPool, id: Uuid, tenant: Uuid, claims: i32) -> bool {
    let run = async {
        let mut tx = begin_scoped(db, tenant).await?;
        let n = sqlx::query(
            "UPDATE interaction_deliveries SET lease_until = now() + make_interval(secs => $3) \
              WHERE id = $1 AND claims = $2 AND state IN ('queued', 'answering', 'delivering')",
        )
        .bind(id)
        .bind(claims)
        .bind(LEASE.as_secs_f64())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok::<_, ApiError>(n == 1)
    };
    match run.await {
        Ok(held) => held,
        Err(e) => {
            tracing::warn!(interaction = %id, error = %e, "interaction_delivery_heartbeat_failed");
            true
        }
    }
}

/// The tenant's status (`active`, `suspended`, `archived`).
async fn tenant_status(db: &PgPool, tenant: Uuid) -> ApiResult<String> {
    let mut tx = begin_scoped(db, tenant).await?;
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(status)
}

/// Answer the row (unless its answer is stored), then deliver it. A row of a tenant that is no longer active is
/// failed (`tenant_suspended`, `tenant_archived`); a row claimed more than [`MAX_CLAIMS`] times without an answer is
/// failed `too_many_claims` (a request that kills its worker is not re-answered forever).
async fn work(st: &AppState, w: &Webhooks, row: DueRow) {
    let _beat = heartbeat(&st.db, &row);
    match tenant_status(&st.db, row.tenant_id).await {
        Ok(s) if s == "active" => {}
        Ok(s) => {
            let why = format!("tenant_{s}");
            tracing::info!(interaction = %row.id, tenant = %row.tenant_id, reason = %why, "interaction_delivery_dropped");
            fenced(&st.db, &row, Write::Failed(row.attempts, why)).await;
            return;
        }
        Err(e) => {
            // the lease expires and the row is claimed again
            tracing::warn!(interaction = %row.id, error = %e, "interaction_delivery_tenant_read_failed");
            return;
        }
    }
    let answer = match &row.answer {
        Some(a) => a.clone(),
        None => {
            if row.claims > MAX_CLAIMS {
                let why = "too_many_claims".to_string();
                tracing::info!(interaction = %row.id, tenant = %row.tenant_id, claims = row.claims, "interaction_delivery_dropped");
                fenced(&st.db, &row, Write::Failed(row.attempts, why)).await;
                return;
            }
            if !fenced(&st.db, &row, Write::Answering).await {
                return;
            }
            let Some(body) = answer_of(st, &row).await else {
                let why = "the stored request is unreadable".to_string();
                fenced(&st.db, &row, Write::Failed(0, why)).await;
                return;
            };
            if !fenced(&st.db, &row, Write::Answered(body.clone())).await {
                return;
            }
            body
        }
    };
    deliver_row(&st.db, w, &row, &answer).await;
}

/// The callback body: the backend's answer within [`ANSWER_DEADLINE`] (`completed`), or `failed`. `None` when the
/// stored request cannot be read back.
async fn answer_of(st: &AppState, row: &DueRow) -> Option<Value> {
    let (req, created_at) = decode_request(row.id, row.tenant_id, row.job_id, &row.request)?;
    let enc = NativeEncoder::new(&req);
    let events = super::respond(st, req);
    let answer = tokio::time::timeout(ANSWER_DEADLINE, collect(events)).await;
    let mut body = match answer {
        Ok(Ok(r)) => enc.complete(&r),
        Ok(Err(e)) => enc.failed(&e),
        Err(_) => enc.failed(&BackendError::Upstream(
            "no answer within the async deadline".into(),
        )),
    };
    if let Some(c) = created_at {
        body["created_at"] = json!(c);
    }
    Some(body)
}

/// The attempts left of one row, from its stored attempt count: each is written back (`attempts`, `last_error`,
/// `next_attempt_at` = now + the next delay) before the next; the end is `delivered` or `failed`. A host that does not
/// resolve (a lookup error or timeout) is retried like a network error; one that resolves to a private address is
/// refused at once. Only an end this worker wrote is logged and counted.
async fn deliver_row(db: &PgPool, w: &Webhooks, row: &DueRow, answer: &Value) {
    let secret = w.job_secret(row.tenant_id, row.job_id);
    let msg_id = format!("int_{}", row.id);
    let body = answer.to_string();
    let url = row.callback_url.as_str();
    let mut done = usize::try_from(row.attempts).unwrap_or(0);
    // The delay before the first attempt (a resumed row already waited for `next_attempt_at`).
    let first = if done == 0 {
        w.retry.first().copied()
    } else {
        None
    };
    if let Some(d) = first.filter(|d| !d.is_zero()) {
        tokio::time::sleep(d).await;
    }
    let outcome = loop {
        if done >= w.retry.len() {
            let why = "every attempt failed".to_string();
            if !fenced(db, row, Write::Failed(attempts_i32(done), why)).await {
                return;
            }
            break Outcome::Exhausted;
        }
        let checked = resolve_callback_url(url, w.allow_private).await;
        done += 1;
        let n = attempts_i32(done);
        let result = match checked {
            Err(UrlError::Refused(why)) => {
                if !fenced(db, row, Write::Failed(n, format!("refused: {why}"))).await {
                    return;
                }
                break Outcome::Refused;
            }
            Err(UrlError::Unresolved(why)) => Attempt::Retry(why),
            Ok((host, addrs)) => attempt(url, &host, &addrs, &msg_id, &secret, &body).await,
        };
        let (write, end) = match result {
            Attempt::Done => (Write::Delivered(n), Outcome::Delivered),
            Attempt::GiveUp(why) => (Write::Failed(n, why), Outcome::Rejected),
            Attempt::Retry(why) => match w.retry.get(done).copied() {
                None => (Write::Failed(n, why), Outcome::Exhausted),
                Some(delay) => {
                    if !fenced(db, row, Write::Retry(n, why, delay.as_secs_f64())).await {
                        return;
                    }
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    continue;
                }
            },
        };
        // a lost claim: another worker owns the row and its outcome
        if !fenced(db, row, write).await {
            return;
        }
        break end;
    };
    tracing::info!(interaction = %row.id, tenant = %row.tenant_id, job_id = %row.job_id, attempts = done, outcome = outcome.as_str(), "interaction_webhook_delivery");
    metrics::counter!(DELIVERIES_METRIC, "outcome" => outcome.as_str()).increment(1);
}

fn attempts_i32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// One signed POST, connected only to `addrs` (the addresses [`check_callback_url`] just checked).
async fn attempt(
    url: &str,
    host: &str,
    addrs: &[SocketAddr],
    msg_id: &str,
    secret: &[u8],
    body: &str,
) -> Attempt {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(ATTEMPT_TIMEOUT)
        .no_proxy()
        .resolve_to_addrs(host, addrs)
        .build();
    let Ok(client) = client else {
        return Attempt::Retry("the HTTP client could not be built".into());
    };
    let ts = super::shapes::unix_now();
    let sent = client
        .post(url)
        .header(ID_HEADER, msg_id)
        .header(TIMESTAMP_HEADER, ts.to_string())
        .header(SIGNATURE_HEADER, signature(secret, msg_id, ts, body))
        .header(CONTENT_TYPE, "application/json")
        .header(USER_AGENT, AGENT)
        .body(body.to_owned())
        .send()
        .await;
    match sent {
        Err(e) if e.is_timeout() => Attempt::Retry("timeout".into()),
        Err(_) => Attempt::Retry("network error".into()),
        Ok(r) if r.status().is_success() => Attempt::Done,
        Ok(r) if r.status().is_server_error() || r.status().as_u16() == 429 => {
            Attempt::Retry(format!("status {}", r.status().as_u16()))
        }
        Ok(r) => Attempt::GiveUp(format!("status {}", r.status().as_u16())),
    }
}

/// `whsec_` + base64 of the secret (the Standard Webhooks presentation).
pub fn presented_secret(secret: &[u8]) -> String {
    format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode(secret)
    )
}

/// `v1,` + base64(HMAC-SHA256(secret, `{msg_id}.{ts}.{body}`)): the `webhook-signature` value.
pub fn signature(secret: &[u8], msg_id: &str, ts: i64, body: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let tag = hmac::sign(&key, format!("{msg_id}.{ts}.{body}").as_bytes());
    format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(tag)
    )
}

/// Why a callback URL cannot be used now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlError {
    /// Never usable: not https, credentials, too long, or a private / reserved address.
    Refused(String),
    /// The host lookup failed or timed out: possibly transient (a delivery retries it).
    Unresolved(String),
}

impl UrlError {
    /// The reason (never the URL).
    pub fn reason(self) -> String {
        match self {
            UrlError::Refused(why) | UrlError::Unresolved(why) => why,
        }
    }
}

/// The SSRF guard: `url` is https (http too when `allow_private`), at most [`MAX_URL`] characters, with a host and no
/// userinfo, and EVERY address it resolves to is public (unless `allow_private`). Returns the host and the checked
/// addresses (the connection is pinned to them). The error says why, never the URL. At submit, any failure (a host
/// that does not resolve included) is a 422; [`resolve_callback_url`] tells the two kinds apart.
pub async fn check_callback_url(
    url: &str,
    allow_private: bool,
) -> Result<(String, Vec<SocketAddr>), String> {
    resolve_callback_url(url, allow_private)
        .await
        .map_err(UrlError::reason)
}

/// [`check_callback_url`], with a lookup failure ([`UrlError::Unresolved`]) told apart from a refusal.
pub async fn resolve_callback_url(
    url: &str,
    allow_private: bool,
) -> Result<(String, Vec<SocketAddr>), UrlError> {
    let refused = |why: &str| UrlError::Refused(why.to_string());
    if url.chars().count() > MAX_URL {
        return Err(UrlError::Refused(format!(
            "callback_url is longer than {MAX_URL} characters"
        )));
    }
    let u = reqwest::Url::parse(url).map_err(|_| refused("callback_url is not a valid URL"))?;
    match u.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err(refused("callback_url must be https")),
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(refused("callback_url must not carry credentials"));
    }
    let host = u
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| refused("callback_url has no host"))?
        .to_string();
    let port = u
        .port_or_known_default()
        .ok_or_else(|| refused("callback_url has no port"))?;
    let literal = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => v6.parse::<Ipv6Addr>().ok().map(IpAddr::V6),
        None => host.parse::<Ipv4Addr>().ok().map(IpAddr::V4),
    };
    let addrs: Vec<SocketAddr> = match literal {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => {
            let found = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await;
            looked_up(found.ok().map(|r| r.map(|it| it.collect())))?
        }
    };
    if !allow_private && !addrs.iter().all(|a| public_ip(a.ip())) {
        return Err(refused(
            "callback_url resolves to a private or reserved address",
        ));
    }
    Ok((host, addrs))
}

/// A host lookup's result (`None` = it timed out): no address, an error or a timeout is [`UrlError::Unresolved`].
fn looked_up(found: Option<std::io::Result<Vec<SocketAddr>>>) -> Result<Vec<SocketAddr>, UrlError> {
    match found {
        None => Err(UrlError::Unresolved(
            "callback_url's host lookup timed out".into(),
        )),
        Some(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
        Some(_) => Err(UrlError::Unresolved(
            "callback_url's host does not resolve".into(),
        )),
    }
}

/// Whether `ip` is publicly routable (std's `is_global` is unstable, so the ranges are listed by hand).
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

/// Not 0/8, 10/8, 100.64/10, 127/8, 169.254/16, 172.16/12, 192.0.0/24, 192.168/16, 198.18/15, nor 224/4 (multicast)
/// and 240/4 (reserved, the broadcast address included).
fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let reserved = a == 0
        || a == 10
        || (a == 100 && (64..=127).contains(&b))
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 224;
    !reserved
}

/// Not ::, ::1, ff00::/8, fc00::/7 (ULA), fe80::/10, fec0::/10 (site-local), 100::/64 (discard), 2001::/32 (Teredo)
/// or 2001:db8::/32 (documentation); an IPv4-mapped (::ffff:a.b.c.d), IPv4-compatible (::/96), NAT64 (64:ff9b::/96)
/// or 6to4 (2002::/16) address is judged as the IPv4 address it carries.
fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let s = ip.segments();
    let o = ip.octets();
    if s[..6] == [0, 0, 0, 0, 0, 0] || s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return public_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    if s[0] == 0x2002 {
        return public_v4(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    let reserved = ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || s[..4] == [0x100, 0, 0, 0]
        || (s[0] == 0x2001 && (s[1] == 0 || s[1] == 0xdb8));
    !reserved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    #[test]
    fn job_secrets_are_deterministic_per_job() {
        let w = Webhooks::new("0123456789abcdef0123456789abcdef");
        let (t, j1, j2) = (Uuid::nil(), Uuid::from_u128(1), Uuid::from_u128(2));
        let s = w.job_secret(t, j1);
        assert_eq!(s.len(), 32);
        assert_eq!(s, w.job_secret(t, j1));
        assert_ne!(s, w.job_secret(t, j2));
        assert_ne!(s, w.job_secret(Uuid::from_u128(9), j1));
        assert_ne!(s, Webhooks::new(&"x".repeat(32)).job_secret(t, j1));
        // the derivation, computed independently (python hmac over the same label)
        assert_eq!(
            presented_secret(&s),
            "whsec_ktrMWKCiC3M8gUE/lhrUs3I/qc0WjZi+vvDRojAGLjQ=" // gitleaks:allow (test vector)
        );
        let k = hmac::Key::new(hmac::HMAC_SHA256, b"0123456789abcdef0123456789abcdef");
        let label = "agent-interaction-webhook:v1:00000000-0000-0000-0000-000000000000:00000000-0000-0000-0000-000000000001";
        let tag = hmac::sign(&k, label.as_bytes());
        let by_hand: &[u8] = tag.as_ref();
        assert_eq!(by_hand, s.as_slice());
    }

    #[test]
    fn signatures_match_the_standard_webhooks_vector() {
        // The Standard Webhooks / Svix published example.
        let presented = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"; // gitleaks:allow (public test vector)
        let raw = base64::engine::general_purpose::STANDARD
            .decode(presented.trim_start_matches("whsec_"))
            .unwrap();
        let body = r#"{"test": 2432232314}"#;
        assert_eq!(
            signature(&raw, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, body),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=" // gitleaks:allow (public test vector)
        );
        // and by hand with ring
        let k = hmac::Key::new(hmac::HMAC_SHA256, &raw);
        let tag = hmac::sign(&k, b"int_x.7.{}");
        assert_eq!(
            signature(&raw, "int_x", 7, "{}"),
            format!("v1,{}", b64(tag.as_ref()))
        );
        assert_eq!(presented_secret(&raw), presented);
    }

    #[tokio::test]
    async fn ssrf_guard_refuses_private_and_odd_urls() {
        for bad in [
            "https://127.0.0.1/x",
            "http://example.com/x",
            "https://10.1.2.3/x",
            "https://100.64.0.1/x",
            "https://169.254.169.254/latest/meta-data",
            "https://172.16.0.1/",
            "https://192.168.1.1/",
            "https://0.0.0.0/",
            "https://255.255.255.255/",
            "https://224.0.0.1/",
            "https://[::1]/x",
            "https://[fc00::1]/x",
            "https://[fe80::1]/x",
            "https://[::ffff:127.0.0.1]/x",
            "https://[64:ff9b::a00:1]/x",
            "https://user:pw@1.1.1.1/x",
            "https://user@1.1.1.1/x",
            "ftp://1.1.1.1/x",
            "not a url",
        ] {
            assert!(check_callback_url(bad, false).await.is_err(), "{bad}");
        }
        let long = format!("https://1.1.1.1/{}", "a".repeat(MAX_URL));
        assert!(check_callback_url(&long, false).await.is_err());
        let (host, addrs) = check_callback_url("https://1.1.1.1/x", false)
            .await
            .unwrap();
        assert_eq!(host, "1.1.1.1");
        assert_eq!(addrs, ["1.1.1.1:443".parse::<SocketAddr>().unwrap()]);
        let (_, addrs) = check_callback_url("https://[2606:4700::1111]:8443/x", false)
            .await
            .unwrap();
        assert_eq!(addrs[0].port(), 8443);
        // tests may reach a local receiver over http
        assert!(
            check_callback_url("http://127.0.0.1:9/x", true)
                .await
                .is_ok()
        );
        assert!(check_callback_url("ftp://127.0.0.1/x", true).await.is_err());
    }

    #[tokio::test]
    async fn a_failed_lookup_is_retried_a_private_address_is_refused() {
        let unresolved =
            |r: &Result<Vec<SocketAddr>, UrlError>| matches!(r, Err(UrlError::Unresolved(_)));
        assert!(unresolved(&looked_up(None)), "a timeout");
        assert!(unresolved(&looked_up(Some(Err(std::io::Error::other(
            "servfail"
        ))))));
        assert!(unresolved(&looked_up(Some(Ok(vec![])))), "no address");
        let one = vec!["1.1.1.1:443".parse::<SocketAddr>().unwrap()];
        assert_eq!(looked_up(Some(Ok(one.clone()))), Ok(one));
        // `.invalid` never resolves (RFC 6761); with no DNS at all it is a lookup error: unresolved either way
        let r = resolve_callback_url("https://callback.invalid/x", false).await;
        assert!(matches!(r, Err(UrlError::Unresolved(_))), "{r:?}");
        // a private address (and every other refusal) is final
        for bad in [
            "https://10.0.0.1/x",
            "https://[::1]/x",
            "ftp://1.1.1.1/x",
            "https://u@1.1.1.1/x",
        ] {
            let r = resolve_callback_url(bad, false).await;
            assert!(matches!(r, Err(UrlError::Refused(_))), "{bad}: {r:?}");
        }
        // submit (check_callback_url) still refuses both
        assert!(
            check_callback_url("https://callback.invalid/x", false)
                .await
                .is_err()
        );
    }

    #[test]
    fn address_ranges() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.0",
            "172.32.0.1",
            "198.20.0.1",
            "2606:4700::1111",
            "2002:808:808::1",
        ] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "100.127.255.255",
            "172.31.255.255",
            "198.19.0.1",
            "192.0.0.8",
            "240.0.0.1",
            "::",
            "ff02::1",
            "fd12::1",
            "febf::1",
            "::ffff:10.0.0.1",
            "::1",
            "::7f00:1",
            "2002:7f00:1::1",
            "2002:a00:1::",
            "fec0::1",
            "100::1",
            "2001:db8::1",
            "2001:0:4136:e378::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
