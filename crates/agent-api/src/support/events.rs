//! `GET /api/support/events` (SW T7): the tenant-wide live stream of the staff workspace, read by the web as SSE over a
//! fetch with the bearer (an `EventSource` cannot send it).
//!
//! - It reuses the per-job Redis bridge (crate::sse_hub::Bridge::acquire, the same `chat` channels the job feeds read)
//!   for every job of the tenant, and reads the process hub for frames of those jobs. No new channel scheme. The
//!   tenant's job list is re-read every [`JOBS_REFRESH`]: a new job is subscribed, a removed one released.
//! - Frames: `message` (a `MessageView` with its `client_msg_id`, plus the B12 fields), `owner`
//!   (`{conversation_id, owner, name, since, item_id}`), `handover`, `delivery_failed`
//!   (`{conversation_id, message_id, code}`) and `presence` (`{conversation_id, job, sub, name, state}`). Every frame
//!   names its `job` (the job key) next to its `conversation_id`: the web routes frames by both. After each `message`,
//!   `owner` and `handover` frame comes an `inbox_row` frame: the conversation's row as the inbox reads it
//!   (crate::support::inbox::one, `unread` for the viewer).
//! - A `message` frame that already carries the `MessageView` fields (`kind`, `at`: the staff Send's frames) is
//!   forwarded as is; one that does not (the inbound customer frame, the runtime's reply) gets them from the stored
//!   message. No per-process cache (a pre-prod limit): each open stream reads for itself.
//! - A conversation the inbox does not list (the staff test chat, channel `staff`) has no row: its `message`, `owner`
//!   and `handover` frames are not sent at all.
//! - A `typing` presence frame carries no name (crate::support::presence skips the lookup): the stream names it from
//!   the sender's earlier `viewing` frame, else reads the member's name once and keeps it for the stream.
//! - Without Redis (`REDIS_URL` unset) there is no bridge: 503 `stream_unavailable` (the web polls the inbox every
//!   10 s). A Redis that does not answer while the stream opens is 503 too; the stream closes when the Pub/Sub link
//!   drops, when a refresh cannot subscribe a new job, or when the client lags behind the hub's buffer (the web
//!   reconnects).
//! - Presence: the open stream marks its member seen on the `support` surface (crate::members::seen), now and on every
//!   jobs refresh, which is what support routing reads as online (crate::support::routing).
//! - Who: crate::support::require_worker (403 `forbidden`, `missing_role: support_agent`).
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use sqlx::Row;
use tokio::sync::{broadcast, watch};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use uuid::Uuid;

use crate::auth::Caller;
use crate::conversations::hub::Frame;
use crate::conversations::store;
use crate::error::{ApiError, ApiResult};
use crate::members::seen::{self, Surface};
use crate::sse_hub::{self, Bridge, Subscription};
use crate::state::AppState;
use crate::tenancy::{Member, begin_scoped};

/// The frames of the job feeds the tenant stream forwards (each with `job` added).
pub const SUPPORT_EVENTS: [&str; 5] = [
    "message",
    "owner",
    "handover",
    "delivery_failed",
    "presence",
];

/// How often an open stream re-reads the tenant's jobs.
pub const JOBS_REFRESH: Duration = Duration::from_secs(60);

/// 503 `stream_unavailable`.
fn stream_unavailable() -> ApiError {
    ApiError::Unavailable(
        "stream_unavailable",
        "The live stream is unavailable; the inbox refreshes every 10 seconds.".into(),
        Some(10),
    )
}

/// One open tenant stream.
struct Tail {
    rx: broadcast::Receiver<Frame>,
    /// Changes when the Pub/Sub link drops (`None` while no job is bridged).
    reset: Option<watch::Receiver<u64>>,
    st: AppState,
    bridge: Arc<Bridge>,
    tenant: Uuid,
    sub: String,
    /// D125: the member only sees the jobs whose team holds them (or is empty).
    restricted: bool,
    /// The member on the stream: marked online on the support surface while it is open.
    caller: Caller,
    /// The tenant's jobs: id → (key, the bridge hold, released when dropped).
    jobs: HashMap<Uuid, (String, Subscription)>,
    refresh: Interval,
    /// Staff names seen on this stream (sub → name), for `typing` frames.
    names: HashMap<String, String>,
    pending: VecDeque<Frame>,
}

/// Resolves when the link drops (never without a link).
async fn dropped(reset: &mut Option<watch::Receiver<u64>>) {
    match reset {
        Some(rx) => {
            let _ = rx.changed().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// The tenant's jobs: id → key.
/// D125: a `restricted` viewer (support_agent without support_manager) gets only the jobs whose staff team is empty
/// or holds `sub`; the set is re-read every [`JOBS_REFRESH`], so a team change takes effect within it.
async fn tenant_jobs(
    st: &AppState,
    tenant: Uuid,
    sub: &str,
    restricted: bool,
) -> ApiResult<HashMap<Uuid, String>> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let sql = format!(
        "SELECT j.id, j.key FROM jobs j WHERE (NOT $2::boolean OR {})",
        super::job_staff::team_open_sql("j.tenant_id", "j.id", "$1")
    );
    let jobs = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(sub)
        .bind(restricted)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|r| (r.get::<Uuid, _>("id"), r.get::<String, _>("key")))
        .collect();
    tx.commit().await?;
    Ok(jobs)
}

/// Re-read the tenant's jobs: subscribe the new ones, release the removed ones. `false` when a new job could not be
/// subscribed (the stream closes; the client reconnects).
async fn refresh_jobs(t: &mut Tail) -> bool {
    let now = match tenant_jobs(&t.st, t.tenant, &t.sub, t.restricted).await {
        Ok(j) => j,
        Err(e) => {
            // a failed read keeps the jobs this stream has
            tracing::warn!(code = e.code(), "support_events_jobs_unreadable");
            return true;
        }
    };
    t.jobs.retain(|id, _| now.contains_key(id));
    for (id, key) in now {
        if t.jobs.contains_key(&id) {
            continue;
        }
        match t.bridge.acquire(t.tenant, id, &key).await {
            Ok(s) => {
                if t.reset.is_none() {
                    t.reset = Some(s.reset());
                }
                t.jobs.insert(id, (key, s));
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "support_events: a new job could not be subscribed"
                );
                return false;
            }
        }
    }
    true
}

/// The [`store::MessageView`] fields of the message a `message` frame names, read as stored (`None` when it names
/// none or the read fails).
async fn message_fields(
    st: &AppState,
    tenant: Uuid,
    job_id: Uuid,
    data: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let id = data
        .get("message_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())?;
    let mut tx = store::begin_job(&st.db, tenant, job_id).await.ok()?;
    let view = store::message_view(&mut tx, id).await.ok()?;
    tx.commit().await.ok()?;
    match serde_json::to_value(view) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// A `message` frame that already carries the `MessageView` fields.
fn has_view(data: &Map<String, Value>) -> bool {
    data.get("kind").is_some_and(Value::is_string) && data.get("at").is_some_and(Value::is_string)
}

/// The `inbox_row` of conversation `conv` of job `job_id` for viewer `sub` (`None`: the inbox does not list it).
async fn inbox_row(
    st: &AppState,
    tenant: Uuid,
    job_id: Uuid,
    sub: &str,
    conv: &str,
) -> ApiResult<Option<Value>> {
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    let row = super::inbox::one(&mut tx, job_id, conv).await?;
    tx.commit().await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut rows = [row];
    super::inbox::mark_unread(st, sub, &mut rows).await;
    let [row] = rows;
    Ok(Some(json!(row)))
}

/// The name of staff member `sub` on this stream: one seen on it, else read once (the sub when unreadable).
async fn staff_name(t: &mut Tail, job_id: Uuid, sub: &str) -> String {
    if let Some(n) = t.names.get(sub) {
        return n.clone();
    }
    let read = async {
        let mut tx = store::begin_job(&t.st.db, t.tenant, job_id).await?;
        let n = store::member_name(&mut tx, sub).await?;
        tx.commit().await?;
        Ok::<String, ApiError>(n)
    };
    let name = read.await.unwrap_or_else(|_| sub.to_string());
    t.names.insert(sub.to_string(), name.clone());
    name
}

/// What the tenant stream sends for job frame `f` of job `job_key`: the frame with `job` (a `message` with its
/// `MessageView` fields, a `presence` with its name), then the conversation's `inbox_row` after a `message`, `owner`
/// or `handover`. Nothing for a `message`, `owner` or `handover` of a conversation the inbox does not list.
async fn project(t: &mut Tail, job_key: &str, f: Frame) -> Vec<Frame> {
    let Some(conv) = f.conversation_id.clone() else {
        return Vec::new();
    };
    let Value::Object(mut data) = f.data else {
        return Vec::new();
    };
    if f.event == "message"
        && !has_view(&data)
        && let Some(view) = message_fields(&t.st, t.tenant, f.job_id, &data).await
    {
        data.extend(view);
    }
    if f.event == "presence"
        && let Some(sub) = data.get("sub").and_then(Value::as_str).map(str::to_string)
    {
        match data.get("name").and_then(Value::as_str) {
            Some(n) => {
                t.names.insert(sub, n.to_string());
            }
            None => {
                let n = staff_name(t, f.job_id, &sub).await;
                data.insert("name".into(), json!(n));
            }
        }
    }
    data.insert("job".into(), json!(job_key));
    data.insert("conversation_id".into(), json!(conv));
    let frame = Frame {
        tenant: t.tenant,
        job_id: f.job_id,
        conversation_id: Some(conv.clone()),
        event: f.event,
        data: Value::Object(data),
    };
    if !matches!(f.event, "message" | "owner" | "handover") {
        return vec![frame];
    }
    match inbox_row(&t.st, t.tenant, f.job_id, &t.sub, &conv).await {
        Ok(Some(row)) => vec![
            frame,
            Frame {
                tenant: t.tenant,
                job_id: f.job_id,
                conversation_id: Some(conv),
                event: "inbox_row",
                data: row,
            },
        ],
        // not an inbox conversation (the staff test chat)
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!(
                conversation = %conv,
                code = e.code(),
                "support_inbox_row_not_projected"
            );
            vec![frame]
        }
    }
}

/// `GET /api/support/events` (see the module doc).
pub async fn support_events(Member(t): Member, State(st): State<AppState>) -> ApiResult<Response> {
    super::require_worker(&t)?;
    let Some(bridge) = st.sse.clone() else {
        return Err(stream_unavailable());
    };
    let restricted = super::job_staff::restricted(t.effective);
    let keys = tenant_jobs(&st, t.tenant, &t.caller.sub, restricted).await?;
    // the hub first, then the bridge: nothing published meanwhile is missed
    let rx = st.events.subscribe();
    let mut jobs = HashMap::with_capacity(keys.len());
    for (id, key) in keys {
        match bridge.acquire(t.tenant, id, &key).await {
            Ok(s) => {
                jobs.insert(id, (key, s));
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "support_events: Redis bridge unavailable"
                );
                return Err(stream_unavailable());
            }
        }
    }
    // open: online now (support routing, crate::support::routing)
    seen::record(&st, &t.caller, t.tenant, Surface::Support).await;
    let mut refresh = tokio::time::interval_at(Instant::now() + JOBS_REFRESH, JOBS_REFRESH);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let tail = Tail {
        rx,
        reset: jobs.values().next().map(|(_, s)| s.reset()),
        st,
        bridge,
        tenant: t.tenant,
        sub: t.caller.sub.clone(),
        restricted,
        caller: t.caller.clone(),
        jobs,
        refresh,
        names: HashMap::new(),
        pending: VecDeque::new(),
    };
    let frames = futures_util::stream::unfold(tail, |mut s| async move {
        loop {
            if let Some(f) = s.pending.pop_front() {
                return Some((f, s));
            }
            let got = tokio::select! {
                r = s.rx.recv() => Some(Some(r)),
                _ = s.refresh.tick() => Some(None),
                () = dropped(&mut s.reset) => None,
            };
            let f = match got {
                // the link dropped: every stream of it closes
                None => return None,
                Some(None) => {
                    // still here: keeps the member online for support routing (the write is throttled to one per
                    // 5 minutes per session, inside the 10-minute online window)
                    seen::record(&s.st, &s.caller, s.tenant, Surface::Support).await;
                    if refresh_jobs(&mut s).await {
                        continue;
                    }
                    return None;
                }
                Some(Some(Ok(f))) => f,
                // a lagging client is closed (it reconnects)
                Some(Some(Err(_))) => return None,
            };
            if f.tenant != s.tenant || !SUPPORT_EVENTS.contains(&f.event) {
                continue;
            }
            let Some(job_key) = s.jobs.get(&f.job_id).map(|(k, _)| k.clone()) else {
                continue;
            };
            let out = project(&mut s, &job_key, f).await;
            s.pending.extend(out);
        }
    });
    Ok(sse_hub::feed(frames).into_response())
}
