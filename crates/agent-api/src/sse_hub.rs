//! The Redis side of the SSE feeds (B4 T10, Q-B4-3: bridge, not replace). B12's in-process
//! [`LocalHub`](crate::conversations::hub::LocalHub) stays the fan-out every feed reads; this module adds the frames
//! other processes publish (the runtime daemons, other agent-api replicas) by republishing a job's Redis Pub/Sub
//! channels into it.
//!
//! - One Pub/Sub connection per process ([`Bridge`]), opened on the first feed and shared by every feed after it.
//!   SUBSCRIBE is ref-counted per channel: the first feed of a job subscribes the job's `console` and `chat`
//!   channels (prod and test scope), the last one to close unsubscribes them.
//! - `console` frames (crate::runtime::bus mirror) become `bus` (a `BusEvent`, stream name without the scope
//!   prefix, SSE id `{env}/{stream}/{entry_id}`) and `ack` frames on the job feed; `chat` frames
//!   (`{conversation_id, event, data}`, [`publish_chat`]) reach the job feed and that conversation's feed.
//! - A client that lags behind the broadcast buffer (1024) is closed; it reconnects with `Last-Event-ID` and the job
//!   feed replays what it missed from `console:recent` (both envs, oldest first). When the Pub/Sub link drops every
//!   bridged stream is closed and the link is re-opened no earlier than 5 s later.
//! - Opening a feed never waits on a Redis connect: the link is opened by a background task (started by the first
//!   feed that finds it closed, retrying every 5 s until Redis answers). A feed waits at most [`FIRST_LINK_WAIT`]
//!   for it, without holding the link lock, then serves the local hub alone (degraded) — so an unreachable Redis
//!   costs each feed open at most that, concurrently, never a 10 s queue.
//! - The whole bridge step of a feed open (the link lock, the link wait and a SUBSCRIBE) is bounded by
//!   [`ACQUIRE_BUDGET`]: a feed that cannot take the lock in time (another feed's SUBSCRIBE is waiting on a Redis
//!   that stopped answering) is served degraded, and a SUBSCRIBE Redis does not answer drops the link, so the feeds
//!   after it go degraded at once instead of queueing one SUBSCRIBE timeout each. A degraded feed skips the
//!   `console:recent` replay (Redis is not answering); a bridged one reads both envs in one [`CMD_TIMEOUT`].
//! - The conversation feed (F2) carries an SSE id on every `message` frame, `msg/{message_id}`, and replays the
//!   conversation's stored messages after `Last-Event-ID` from Postgres (oldest first, at most [`MAX_REPLAY`]);
//!   an id it does not hold replays nothing (the client re-reads the transcript on every open). Bus ids stay on the
//!   job feed only, so a chat frame there never moves the job feed's `Last-Event-ID`.
//! - `GET /api/jobs/{job}/events?after_ms=` (or `Accept: application/json`) is the JSON `busBacklog`: the
//!   `console:recent` entries published after `after_ms`, oldest first, `acked` folded in from the ack frames.
//! - Gauge `agent_sse_open`: SSE streams open in this process.
//!
//! Without `REDIS_URL` there is no bridge: the feeds serve the local hub exactly as B12 did, and the JSON backlog is
//! 503 `unavailable`. Frames carry ids and codes for the console (K50, `runtime::bus::console_view`); nothing here
//! logs a payload.
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use futures_util::{Stream, StreamExt};
use redis::aio::{PubSubSink, PubSubStream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;
use uuid::Uuid;

use crate::conversations::hub::{self, EventHub, Frame};
use crate::conversations::{store, webchat_stream};
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::runtime::{Bus, Env, Scope, Streams};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember};

/// How long a dropped Pub/Sub link waits before it is re-opened.
pub const RECONNECT: Duration = Duration::from_secs(5);
/// A feed opened while the bridge is unreachable serves local frames for this long, then closes so the client
/// reconnects and tries the bridge again.
pub const DEGRADED_FOR: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a feed that finds the link closed waits for the background task to open it (a healthy Redis answers in
/// milliseconds), before it is served degraded.
pub const FIRST_LINK_WAIT: Duration = Duration::from_secs(1);
/// The most one bus command (SUBSCRIBE, UNSUBSCRIBE, the `console:recent` read, a chat PUBLISH) waits on Redis.
pub const CMD_TIMEOUT: Duration = Duration::from_secs(3);
/// The most a feed open waits on the bridge in total: the link lock, the link wait and a SUBSCRIBE.
pub const ACQUIRE_BUDGET: Duration = Duration::from_secs(2);
/// The most stored messages one conversation feed open replays after `Last-Event-ID`.
pub const MAX_REPLAY: i64 = 200;
/// The SSE keep-alive interval.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// The SSE event names a `chat` frame may carry (B12's feed events plus the runtime's `chat`, the staff workspace's
/// `presence` (SW T7, crate::support::presence), and the web chat customer stream's `webchat`,
/// crate::conversations::webchat_stream); anything else is dropped by the bridge.
pub const CHAT_EVENTS: [&str; 10] = [
    "message",
    "case",
    "handover",
    "owner",
    "csat",
    "delivery_failed",
    "chat",
    "presence",
    webchat_stream::EVENT,
    webchat_stream::SIGN_IN_EVENT,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Console,
    Chat,
}

/// Where a subscribed channel's frames go.
#[derive(Clone, Debug)]
struct Route {
    tenant: Uuid,
    job_id: Uuid,
    env: Env,
    kind: Kind,
    /// The scope's key prefix, stripped from stream names (one `BusEvent` shape across REST and SSE).
    prefix: String,
}

#[derive(Default)]
struct Link {
    sink: Option<PubSubSink>,
    refs: HashMap<String, usize>,
    /// Bumped on every new connection; a release from an older connection is a no-op.
    generation: u64,
    /// Not before this instant after a drop.
    retry_at: Option<Instant>,
    /// The background task opening the link is running.
    connecting: bool,
}

/// One Redis Pub/Sub connection per process, republishing a job's channels into the local hub while a feed of that
/// job is open.
pub struct Bridge {
    client: redis::Client,
    root: String,
    hub: Arc<dyn EventHub>,
    link: tokio::sync::Mutex<Link>,
    routes: Mutex<HashMap<String, Route>>,
    /// The generation whose link dropped: every bridged stream closes on a change.
    reset: watch::Sender<u64>,
    /// The generation of the link last opened: a feed waiting for the link watches it.
    up: watch::Sender<u64>,
    /// The generation of the open link, 0 while none is: a reader of an older (dropped) link stops.
    live: AtomicU64,
    opened: AtomicU64,
}

/// `Debug` never prints the client (its URL may hold a password).
impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Bridge {
    /// A bridge on `bus`'s Redis and environment prefix into `hub`. Connects on the first [`Bridge::acquire`].
    pub fn new(bus: &Bus, hub: Arc<dyn EventHub>) -> Arc<Bridge> {
        Arc::new(Bridge {
            client: bus.client().clone(),
            root: bus.root_prefix().to_string(),
            hub,
            link: tokio::sync::Mutex::new(Link::default()),
            routes: Mutex::new(HashMap::new()),
            reset: watch::channel(0).0,
            up: watch::channel(0).0,
            live: AtomicU64::new(0),
            opened: AtomicU64::new(0),
        })
    }

    /// How many Pub/Sub connections this bridge has opened (one per link drop, plus the first).
    pub fn connections_opened(&self) -> u64 {
        self.opened.load(Ordering::Relaxed)
    }

    fn routes(&self) -> MutexGuard<'_, HashMap<String, Route>> {
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Subscribe the `console` and `chat` channels of (`tenant`, `job_key`) in both envs, unless already
    /// subscribed; the [`Subscription`] releases them when dropped. When the link is closed this starts its
    /// background open and waits at most [`FIRST_LINK_WAIT`] for it (holding no lock), else fails fast. Everything
    /// here is bounded by [`ACQUIRE_BUDGET`]; a SUBSCRIBE that fails or times out drops the link (see the module
    /// doc).
    pub async fn acquire(
        self: &Arc<Self>,
        tenant: Uuid,
        job_id: Uuid,
        job_key: &str,
    ) -> Result<Subscription> {
        let mut wanted = Vec::with_capacity(4);
        for env in Env::ALL {
            let scope = Scope::new(tenant, job_key, env)?;
            let s = Streams::scoped(&self.root, &scope);
            let prefix = s.prefix().to_string();
            let route = |kind| Route {
                tenant,
                job_id,
                env,
                kind,
                prefix: prefix.clone(),
            };
            wanted.push((s.console(), route(Kind::Console)));
            wanted.push((s.chat(), route(Kind::Chat)));
        }
        let deadline = Instant::now() + ACQUIRE_BUDGET;
        let mut link = self.lock_by(deadline).await?;
        if link.sink.is_none() {
            let mut up = self.up.subscribe();
            if !link.connecting {
                link.connecting = true;
                tokio::spawn(self.clone().open_link(link.retry_at.take()));
            }
            drop(link);
            // subscribed under the lock: the open (which installs the link under it) cannot be missed
            let wait = deadline.min(Instant::now() + FIRST_LINK_WAIT);
            let _ = tokio::time::timeout_at(wait, up.changed()).await;
            link = self.lock_by(deadline).await?;
        }
        let Some(mut sink) = link.sink.clone() else {
            bail!("the Redis Pub/Sub link is not open (re-opening in the background)");
        };
        let mut channels = Vec::with_capacity(wanted.len());
        let mut fresh = Vec::new();
        for (ch, route) in wanted {
            let n = link.refs.entry(ch.clone()).or_insert(0);
            *n += 1;
            if *n == 1 {
                self.routes().insert(ch.clone(), route);
                fresh.push(ch.clone());
            }
            channels.push(ch);
        }
        if !fresh.is_empty() {
            let by = deadline.min(Instant::now() + CMD_TIMEOUT);
            let r = tokio::time::timeout_at(by, sink.subscribe(&fresh)).await;
            if !matches!(r, Ok(Ok(()))) {
                // Redis did not answer on the link: drop it (every bridged stream closes and reconnects; the link
                // re-opens in the background) so the feeds queued behind this lock do not each wait the same
                self.drop_link(&mut link);
                bail!(
                    "SUBSCRIBE on the Redis Pub/Sub link failed or timed out; the link is re-opened"
                );
            }
        }
        Ok(Subscription {
            bridge: self.clone(),
            channels,
            generation: link.generation,
            reset: self.reset.subscribe(),
        })
    }

    /// The link lock, waited for until `deadline` at most.
    async fn lock_by(&self, deadline: Instant) -> Result<tokio::sync::MutexGuard<'_, Link>> {
        match tokio::time::timeout_at(deadline, self.link.lock()).await {
            Ok(g) => Ok(g),
            Err(_) => bail!("the Redis Pub/Sub link is busy (a SUBSCRIBE is waiting on Redis)"),
        }
    }

    /// Drop the open link (under its lock): its refs and routes go, every bridged stream closes, the next open is
    /// not before [`RECONNECT`].
    fn drop_link(&self, link: &mut Link) {
        link.sink = None;
        link.refs.clear();
        link.retry_at = Some(Instant::now() + RECONNECT);
        self.live.store(0, Ordering::Relaxed);
        self.routes().clear();
        self.reset.send_replace(link.generation);
    }

    /// The background open of the one Pub/Sub connection: not before `not_before` (a drop's back-off), then every
    /// [`RECONNECT`] until Redis answers; installs the link, starts its reader and wakes the waiting feeds.
    async fn open_link(self: Arc<Self>, not_before: Option<Instant>) {
        if let Some(at) = not_before {
            tokio::time::sleep_until(at).await;
        }
        let ps = loop {
            match tokio::time::timeout(CONNECT_TIMEOUT, self.client.get_async_pubsub()).await {
                Ok(Ok(ps)) => break ps,
                Ok(Err(e)) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "sse: opening the Redis Pub/Sub connection failed; retrying in 5 s"
                ),
                Err(_) => tracing::warn!(
                    "sse: opening the Redis Pub/Sub connection timed out; retrying in 5 s"
                ),
            }
            tokio::time::sleep(RECONNECT).await;
        };
        let (sink, stream) = ps.split();
        let mut link = self.link.lock().await;
        link.generation += 1;
        link.sink = Some(sink);
        link.refs.clear();
        link.connecting = false;
        self.routes().clear();
        self.live.store(link.generation, Ordering::Relaxed);
        self.opened.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(self.clone().read(stream, link.generation));
        self.up.send_replace(link.generation);
    }

    /// Republish every message of the link into the hub until the link drops (or was dropped: a reader of an older
    /// link stops at its next message, closing that connection); then close every bridged stream.
    async fn read(self: Arc<Self>, stream: PubSubStream, generation: u64) {
        let mut stream = Box::pin(stream);
        while let Some(msg) = stream.next().await {
            if self.live.load(Ordering::Relaxed) != generation {
                break;
            }
            self.dispatch(&msg);
        }
        self.lost(generation).await;
    }

    fn dispatch(&self, msg: &redis::Msg) {
        let Some(route) = self.routes().get(msg.get_channel_name()).cloned() else {
            return;
        };
        let Ok(text) = msg.get_payload::<String>() else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let frame = match route.kind {
            Kind::Console => {
                console_frame(&route.prefix, route.env, &v).map(|(event, data)| Frame {
                    tenant: route.tenant,
                    job_id: route.job_id,
                    conversation_id: None,
                    event,
                    data,
                })
            }
            Kind::Chat => chat_frame(&v).map(|(conversation, event, data)| Frame {
                tenant: route.tenant,
                job_id: route.job_id,
                conversation_id: Some(conversation),
                event,
                data,
            }),
        };
        if let Some(f) = frame {
            self.hub.publish(f);
        }
    }

    async fn lost(&self, generation: u64) {
        let mut link = self.link.lock().await;
        // another link since, or this one was already dropped (a failed SUBSCRIBE)
        if link.generation != generation || link.sink.is_none() {
            return;
        }
        self.drop_link(&mut link);
        tracing::warn!(
            "sse: the Redis Pub/Sub link dropped; open streams closed, re-opening after 5 s"
        );
    }

    async fn release(&self, channels: Vec<String>, generation: u64) {
        let mut link = self.link.lock().await;
        if link.generation != generation {
            return;
        }
        let Some(mut sink) = link.sink.clone() else {
            return;
        };
        let mut gone = Vec::new();
        for ch in channels {
            if let Some(n) = link.refs.get_mut(&ch) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    link.refs.remove(&ch);
                    gone.push(ch);
                }
            }
        }
        if gone.is_empty() {
            return;
        }
        {
            let mut routes = self.routes();
            for ch in &gone {
                routes.remove(ch);
            }
        }
        if !matches!(
            tokio::time::timeout(CMD_TIMEOUT, sink.unsubscribe(&gone)).await,
            Ok(Ok(()))
        ) {
            tracing::warn!(
                channels = gone.len(),
                "sse: UNSUBSCRIBE failed; frames of those channels are dropped"
            );
        }
    }
}

/// A feed's hold on a job's channels; dropping it releases them.
pub struct Subscription {
    bridge: Arc<Bridge>,
    channels: Vec<String>,
    generation: u64,
    reset: watch::Receiver<u64>,
}

impl Subscription {
    /// Changes when the Pub/Sub link this subscription is on drops: a stream that holds it closes then (the tenant-wide
    /// support stream, crate::support::events, holds one per job and watches any one of them: they share the link).
    pub fn reset(&self) -> watch::Receiver<u64> {
        self.reset.clone()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let channels = std::mem::take(&mut self.channels);
        let bridge = self.bridge.clone();
        let generation = self.generation;
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { bridge.release(channels, generation).await });
        }
    }
}

/// What a feed reads besides the local hub.
pub enum Upstream {
    /// No bridge (`REDIS_URL` unset): local frames only, closed only by the client.
    Local,
    /// Bridged: closed when the Pub/Sub link drops.
    Bridged(Subscription),
    /// A bridge exists but could not be reached: local frames for [`DEGRADED_FOR`], then closed.
    Degraded,
}

enum Close {
    Never,
    Reset(watch::Receiver<u64>),
    At(Instant),
}

impl Close {
    async fn wait(&mut self) {
        match self {
            Close::Never => std::future::pending::<()>().await,
            Close::Reset(rx) => {
                let _ = rx.changed().await;
            }
            Close::At(t) => tokio::time::sleep_until(*t).await,
        }
    }
}

/// Counts an open SSE stream in `agent_sse_open` while alive.
struct Open;

impl Open {
    fn new() -> Self {
        metrics::gauge!("agent_sse_open").increment(1.0);
        Open
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        metrics::gauge!("agent_sse_open").decrement(1.0);
    }
}

/// The SSE id of a frame on one kind of feed.
type IdOf = fn(&str, &Value) -> Option<String>;

struct FeedState {
    rx: broadcast::Receiver<Frame>,
    close: Close,
    replay: VecDeque<Frame>,
    replayed: HashSet<String>,
    id_of: IdOf,
    tenant: Uuid,
    job_id: Uuid,
    conversation: Option<String>,
    _upstream: Upstream,
    _open: Open,
}

enum Step {
    Hub(std::result::Result<Frame, broadcast::error::RecvError>),
    Close,
}

/// The frames of one feed: `replay` first, then what `rx` (subscribed BEFORE the replay was read) receives for
/// (`tenant`, `job_id`, `conversation`), skipping frames already replayed (by their SSE id: [`event_id`] on the job
/// feed, [`message_event_id`] on a conversation feed). Ends when the client lags behind the broadcast buffer, when
/// the bridged link drops, or after [`DEGRADED_FOR`] on a degraded feed.
pub fn frames(
    rx: broadcast::Receiver<Frame>,
    upstream: Upstream,
    tenant: Uuid,
    job_id: Uuid,
    conversation: Option<String>,
    replay: Vec<Frame>,
) -> impl Stream<Item = Frame> + Send + 'static {
    let close = match &upstream {
        Upstream::Local => Close::Never,
        Upstream::Bridged(s) => Close::Reset(s.reset.clone()),
        Upstream::Degraded => Close::At(Instant::now() + DEGRADED_FOR),
    };
    let id_of: IdOf = if conversation.is_some() {
        message_event_id
    } else {
        event_id
    };
    let replayed = replay
        .iter()
        .filter_map(|f| id_of(f.event, &f.data))
        .collect();
    let state = FeedState {
        rx,
        close,
        replay: replay.into(),
        replayed,
        id_of,
        tenant,
        job_id,
        conversation,
        _upstream: upstream,
        _open: Open::new(),
    };
    futures_util::stream::unfold(state, |mut s| async move {
        if let Some(f) = s.replay.pop_front() {
            return Some((f, s));
        }
        loop {
            let step = tokio::select! {
                r = s.rx.recv() => Step::Hub(r),
                () = s.close.wait() => Step::Close,
            };
            match step {
                Step::Hub(Ok(f)) => {
                    let wanted = hub::matches(&f, s.tenant, s.job_id, s.conversation.as_deref());
                    let again =
                        (s.id_of)(f.event, &f.data).is_some_and(|id| s.replayed.contains(&id));
                    if wanted && !again {
                        return Some((f, s));
                    }
                }
                // A lagging client is closed (it reconnects with Last-Event-ID); so is every stream of a dropped link.
                Step::Hub(Err(_)) | Step::Close => return None,
            }
        }
    })
}

/// The SSE form of `frames` (15 s keep-alive comments), opened by an `open` comment (`: open`) so the response headers leave at
/// once: production (Cloudflare → istio) holds them until the first body byte, which without it is the first
/// keep-alive, so the web feed sat on "connecting…" for 15 s after every (re)connect.
/// The job feed's SSE ids are [`event_id`]s.
pub fn feed(
    frames: impl Stream<Item = Frame> + Send + 'static,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>> + Send + 'static> {
    sse(frames, event_id)
}

/// [`feed`] for a conversation feed: its SSE ids are [`message_event_id`]s.
pub fn conversation_feed(
    frames: impl Stream<Item = Frame> + Send + 'static,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>> + Send + 'static> {
    sse(frames, message_event_id)
}

fn sse(
    frames: impl Stream<Item = Frame> + Send + 'static,
    id_of: IdOf,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>> + Send + 'static> {
    let open = futures_util::stream::once(std::future::ready(Event::default().comment("open")));
    Sse::new(
        open.chain(frames.map(move |f| to_event(&f, id_of)))
            .map(Ok::<_, Infallible>),
    )
    .keep_alive(KeepAlive::new().interval(KEEPALIVE))
}

fn to_event(f: &Frame, id_of: IdOf) -> Event {
    let ev = Event::default().event(f.event).data(f.data.to_string());
    match id_of(f.event, &f.data) {
        Some(id) => ev.id(id),
        None => ev,
    }
}

/// The SSE id of a `bus` frame: `{env}/{stream}/{entry_id}` (entry ids repeat across streams and envs). Other
/// frames have none, so they never move a client's `Last-Event-ID`.
pub fn event_id(event: &str, data: &Value) -> Option<String> {
    if event != "bus" {
        return None;
    }
    let env = data["envelope"]["env"].as_str().unwrap_or("prod");
    let stream = data["stream"].as_str()?;
    let entry = data["entry_id"].as_str()?;
    Some(format!("{env}/{stream}/{entry}"))
}

/// The SSE id of a conversation feed's `message` (or legacy `chat`) frame: `msg/{message_id}`. Other frames have
/// none.
pub fn message_event_id(event: &str, data: &Value) -> Option<String> {
    if event != "message" && event != "chat" {
        return None;
    }
    let id = data["message_id"].as_str().filter(|s| !s.is_empty())?;
    Some(format!("msg/{id}"))
}

/// A `console` channel / `console:recent` frame as the feed sends it: the publish mirror becomes `bus` (a
/// `BusEvent`), the recipient's XACK becomes `ack`. `prefix` (the scope's key prefix) is stripped from the stream.
fn console_frame(prefix: &str, env: Env, v: &Value) -> Option<(&'static str, Value)> {
    let full = v["stream"].as_str()?;
    let stream = full.strip_prefix(prefix).unwrap_or(full);
    let entry_id = v["entry_id"].as_str()?;
    if v["kind"] == "ack" {
        return Some((
            "ack",
            json!({
                "stream": stream,
                "entry_id": entry_id,
                "consumer_group": v["consumer_group"],
                "correlation_id": v["correlation_id"],
                "env": env.as_str(),
            }),
        ));
    }
    Some((
        "bus",
        json!({
            "stream": stream,
            "entry_id": entry_id,
            "envelope": v["envelope"],
            "consumer_group": v["consumer_group"],
            "acked": v["acked"],
        }),
    ))
}

/// A `chat` channel frame: `{conversation_id, event, data}` with `event` in [`CHAT_EVENTS`].
fn chat_frame(v: &Value) -> Option<(String, &'static str, Value)> {
    let conversation = v["conversation_id"].as_str()?;
    let name = v["event"].as_str()?;
    let event = CHAT_EVENTS.into_iter().find(|e| *e == name)?;
    Some((
        conversation.to_string(),
        event,
        v.get("data").cloned().unwrap_or(Value::Null),
    ))
}

/// PUBLISH a chat frame on `bus`'s scope (runtime processes; agent-api uses [`publish`]). Lossy like every Pub/Sub
/// frame: the write it reports is already committed. A Redis that does not answer within [`CMD_TIMEOUT`] is an
/// error (the callers await it on handover and inbound responses).
pub async fn publish_chat(
    bus: &Bus,
    conversation_id: &str,
    event: &'static str,
    data: &Value,
) -> Result<()> {
    if bus.scope().is_none() {
        bail!("bus is not scoped: refusing PUBLISH chat");
    }
    let text =
        json!({"conversation_id": conversation_id, "event": event, "data": data}).to_string();
    let mut c = bus.conn();
    let mut cmd = redis::cmd("PUBLISH");
    cmd.arg(bus.streams.chat()).arg(text);
    tokio::time::timeout(CMD_TIMEOUT, cmd.query_async::<i64>(&mut c))
        .await
        .context("PUBLISH chat timed out")?
        .context("PUBLISH chat")?;
    Ok(())
}

/// Publish a conversation frame from agent-api: through Redis when the runtime bus is configured (the bridge brings
/// it back to every replica's feeds, this one included), else straight into the local hub. A PUBLISH that fails or
/// times out ([`CMD_TIMEOUT`]) is warned and delivered to the local hub, so a stalled Redis never stalls the caller.
pub async fn publish(
    st: &AppState,
    scope: &Scope,
    job_id: Uuid,
    conversation_id: &str,
    event: &'static str,
    data: Value,
) {
    if let Some(root) = &st.redis {
        match publish_chat(&root.scoped(scope), conversation_id, event, &data).await {
            Ok(()) => return,
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "sse: chat PUBLISH failed; frame delivered to this replica only"
            ),
        }
    }
    st.events.publish(Frame {
        tenant: scope.tenant,
        job_id,
        conversation_id: Some(conversation_id.to_string()),
        event,
        data,
    });
}

/// One retained console frame of a job (`console:recent`).
struct Recent {
    /// The entry id's millisecond part: the merge order of the two envs' lists.
    ms: u64,
    event: &'static str,
    data: Value,
}

fn entry_ms(id: &str) -> Option<u64> {
    id.split_once('-')?.0.parse().ok()
}

/// The retained console frames of (`tenant`, `job_key`) in both envs, oldest first. Both lists are read at
/// once, within one [`CMD_TIMEOUT`].
async fn recent(root: &Bus, tenant: Uuid, job_key: &str) -> Result<Vec<Recent>> {
    let mut buses = Vec::with_capacity(2);
    for env in Env::ALL {
        buses.push((env, root.scoped(&Scope::new(tenant, job_key, env)?)));
    }
    let reads = buses.iter().map(|(_, bus)| {
        let mut c = bus.conn();
        let key = bus.streams.console_recent();
        async move {
            redis::cmd("LRANGE")
                .arg(key)
                .arg(0)
                .arg(-1)
                .query_async::<Vec<String>>(&mut c)
                .await
        }
    });
    let lists = tokio::time::timeout(CMD_TIMEOUT, futures_util::future::try_join_all(reads))
        .await
        .context("reading console:recent timed out")?
        .context("LRANGE console:recent")?;
    let mut out = Vec::new();
    for ((env, bus), texts) in buses.iter().zip(lists) {
        let env = *env;
        // the list is newest first
        for t in texts.iter().rev() {
            let Ok(v) = serde_json::from_str::<Value>(t) else {
                continue;
            };
            if let Some((event, data)) = console_frame(bus.streams.prefix(), env, &v) {
                let ms = data["entry_id"].as_str().and_then(entry_ms).unwrap_or(0);
                out.push(Recent { ms, event, data });
            }
        }
    }
    out.sort_by_key(|r| r.ms);
    Ok(out)
}

fn ack_key(env: &str, stream: &str, entry: &str, group: &str) -> String {
    format!("{env}/{stream}/{entry}/{group}")
}

/// `BusBacklog{items, agents}`: the `bus` frames published after `after_ms`, oldest first, each `acked` true when
/// its recipient group's ack is retained too. `agents` is the runtime area's (`busAgents`, T14); empty here.
fn backlog(recent: Vec<Recent>, after_ms: u64) -> Value {
    let acked: HashSet<String> = recent
        .iter()
        .filter(|r| r.event == "ack")
        .map(|r| {
            let s = |k: &str| r.data[k].as_str().unwrap_or("").to_string();
            ack_key(
                &s("env"),
                &s("stream"),
                &s("entry_id"),
                &s("consumer_group"),
            )
        })
        .collect();
    let mut items: Vec<Value> = recent
        .into_iter()
        .filter(|r| r.event == "bus")
        .map(|r| r.data)
        .filter(|d| d["envelope"]["published_at_ms"].as_u64().unwrap_or(0) > after_ms)
        .map(|mut d| {
            if let Some(group) = d["consumer_group"].as_str() {
                let env = d["envelope"]["env"].as_str().unwrap_or("prod");
                let key = ack_key(
                    env,
                    d["stream"].as_str().unwrap_or(""),
                    d["entry_id"].as_str().unwrap_or(""),
                    group,
                );
                if acked.contains(&key) {
                    d["acked"] = Value::Bool(true);
                }
            }
            d
        })
        .collect();
    items.sort_by_key(|d| d["envelope"]["published_at_ms"].as_u64().unwrap_or(0));
    json!({ "items": items, "agents": [] })
}

/// The frames after the `bus` frame `last` (a `Last-Event-ID`); every retained frame when `last` is no longer held.
fn replay(recent: Vec<Recent>, last: &str, tenant: Uuid, job_id: Uuid) -> Vec<Frame> {
    let start = recent
        .iter()
        .rposition(|r| event_id(r.event, &r.data).as_deref() == Some(last))
        .map_or(0, |i| i + 1);
    recent
        .into_iter()
        .skip(start)
        .map(|r| Frame {
            tenant,
            job_id,
            conversation_id: None,
            event: r.event,
            data: r.data,
        })
        .collect()
}

/// The feed's upstream: bridged when the bridge answers, degraded when it does not, local without a bridge.
async fn upstream(st: &AppState, j: &JobCtx) -> Upstream {
    upstream_for(st, j.t.tenant, j.job_id, &j.job_key).await
}

/// [`upstream`] for a job named by the caller (the edge's web chat stream, crate::conversations::webchat_stream).
pub async fn upstream_for(st: &AppState, tenant: Uuid, job_id: Uuid, job_key: &str) -> Upstream {
    let Some(bridge) = st.sse.as_ref() else {
        return Upstream::Local;
    };
    match bridge.acquire(tenant, job_id, job_key).await {
        Ok(s) => Upstream::Bridged(s),
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "sse: Redis bridge unavailable; serving in-process frames only"
            );
            Upstream::Degraded
        }
    }
}

/// JSON unless the client asks for `text/event-stream`: `after_ms` or `Accept: application/json` selects it.
fn wants_json(h: &HeaderMap, has_after: bool) -> bool {
    let accept = h
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    !accept.contains("text/event-stream") && (has_after || accept.contains("application/json"))
}

fn bus_unavailable() -> ApiError {
    ApiError::Unavailable(
        "unavailable",
        "The runtime bus is unavailable.".into(),
        Some(5),
    )
}

#[derive(Deserialize)]
pub struct EventsQuery {
    after_ms: Option<u64>,
}

/// `GET /api/jobs/{job}/events`: the job's SSE feed (bus, ack and conversation frames; `Last-Event-ID` replays from
/// `console:recent`), or the JSON `busBacklog` (`?after_ms=` / `Accept: application/json`).
pub async fn job_events(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    headers: HeaderMap,
    ApiQuery(q): ApiQuery<EventsQuery>,
) -> ApiResult<Response> {
    if wants_json(&headers, q.after_ms.is_some()) {
        let Some(root) = st.redis.as_ref() else {
            return Err(bus_unavailable());
        };
        let r = recent(root, j.t.tenant, &j.job_key).await.map_err(|e| {
            tracing::warn!(error = %format!("{e:#}"), "busBacklog: console:recent unreadable");
            bus_unavailable()
        })?;
        return Ok(Json(backlog(r, q.after_ms.unwrap_or(0))).into_response());
    }
    // the hub first, then the bridge, then the replay: nothing published meanwhile is missed (replayed bus frames
    // are skipped when they also arrive live)
    let rx = st.events.subscribe();
    let up = upstream(&st, &j).await;
    let last = last_event_id(&headers);
    // a degraded feed does not ask Redis again: it just failed to answer the bridge
    let bridged = matches!(up, Upstream::Bridged(_));
    let replayed = match (last, st.redis.as_ref()) {
        (Some(last), Some(root)) if bridged => match recent(root, j.t.tenant, &j.job_key).await {
            Ok(r) => replay(r, &last, j.t.tenant, j.job_id),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "sse: Last-Event-ID replay skipped");
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    let s = frames(rx, up, j.t.tenant, j.job_id, None, replayed).filter(staff_frame);
    Ok(feed(s).into_response())
}

/// Not a web chat customer frame: those are the edge's ([`webchat_stream`]), never a staff feed's.
fn staff_frame(f: &Frame) -> std::future::Ready<bool> {
    std::future::ready(f.event != webchat_stream::EVENT && f.event != webchat_stream::SIGN_IN_EVENT)
}

/// The request's `Last-Event-ID`, when it names one.
fn last_event_id(h: &HeaderMap) -> Option<String> {
    h.get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// One stored message as the conversation feed replays it.
#[derive(sqlx::FromRow)]
struct Replayed {
    message_id: Uuid,
    role: String,
    author: Option<String>,
    text: String,
    case_id: Option<String>,
    at_ms: i64,
    refused: Option<bool>,
    client_msg_id: Option<Uuid>,
}

/// The conversation's stored messages after the one `last` (a [`message_event_id`]) names, oldest first, at most
/// [`MAX_REPLAY`], as `message` frames shaped like the live ones (the test-env answer trace stays on the transcript
/// read, which the client repeats on every open). Nothing when `last` names no message of it.
async fn messages_since(
    c: &mut sqlx::PgConnection,
    tenant: Uuid,
    job_id: Uuid,
    conversation_id: &str,
    last: &str,
) -> ApiResult<Vec<Frame>> {
    let Some(after) = last
        .strip_prefix("msg/")
        .and_then(|id| Uuid::parse_str(id).ok())
    else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query_as::<_, Replayed>(
        "SELECT m.message_id, m.role, m.author, m.text, m.case_id, m.at_ms, m.refused, m.client_msg_id \
           FROM conversation_message m \
           JOIN conversation_message l \
             ON l.tenant_id = m.tenant_id AND l.job_id = m.job_id AND l.conversation_id = m.conversation_id \
            AND l.message_id = $2 \
          WHERE m.job_id = $4 AND m.conversation_id = $1 AND (m.at_ms, m.message_id) > (l.at_ms, l.message_id) \
          ORDER BY m.at_ms, m.message_id \
          LIMIT $3",
    )
    .bind(conversation_id.to_string())
    .bind(after)
    .bind(MAX_REPLAY)
    .bind(job_id)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut data = json!({
                "conversation_id": conversation_id, "message_id": r.message_id.to_string(), "role": r.role,
                "author": r.author.unwrap_or_default(), "text": r.text, "case_id": r.case_id, "at_ms": r.at_ms,
                "client_msg_id": r.client_msg_id,
            });
            if r.refused == Some(true) {
                data["refused"] = Value::Bool(true);
            }
            Frame {
                tenant,
                job_id,
                conversation_id: Some(conversation_id.to_string()),
                event: "message",
                data,
            }
        })
        .collect())
}

/// `GET /api/jobs/{job}/conversations/{id}/events`: that conversation's frames only (404 for an unknown one). Each
/// `message` frame carries the SSE id `msg/{message_id}`; `Last-Event-ID` replays the stored messages after it
/// (F2), read after the hub subscription so nothing in between is lost (a replayed message that also arrives live
/// is sent once).
pub async fn conversation_events(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    headers: HeaderMap,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Response> {
    let mut tx = j.begin(&st.db).await?;
    store::conversation_or_404(&mut tx, &id).await?;
    tx.commit().await?;
    let rx = st.events.subscribe();
    let up = upstream(&st, &j).await;
    let replayed = match last_event_id(&headers) {
        Some(last) => {
            let mut tx = j.begin(&st.db).await?;
            let r = messages_since(&mut tx, j.t.tenant, j.job_id, &id, &last).await?;
            tx.commit().await?;
            r
        }
        None => Vec::new(),
    };
    let s = frames(rx, up, j.t.tenant, j.job_id, Some(id), replayed).filter(staff_frame);
    Ok(conversation_feed(s).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(event: &'static str, data: Value) -> Recent {
        let ms = data["entry_id"].as_str().and_then(entry_ms).unwrap_or(0);
        Recent { ms, event, data }
    }

    fn bus_data(env: &str, entry: &str, at: u64) -> Value {
        json!({"stream": "bus:op", "entry_id": entry, "consumer_group": "runner", "acked": false,
               "envelope": {"env": env, "published_at_ms": at}})
    }

    #[test]
    fn console_frames_strip_the_scope_prefix_and_split_acks() {
        let pre = "p:s:t:j:test:";
        let (ev, d) = console_frame(
            pre,
            Env::Test,
            &json!({"stream": "p:s:t:j:test:bus:op", "entry_id": "5-0", "envelope": {}, "consumer_group": "runner", "acked": false}),
        )
        .unwrap();
        assert_eq!((ev, d["stream"].as_str()), ("bus", Some("bus:op")));
        let (ev, d) = console_frame(
            pre,
            Env::Test,
            &json!({"kind": "ack", "stream": "p:s:t:j:test:bus:op", "entry_id": "5-0", "consumer_group": "runner"}),
        )
        .unwrap();
        assert_eq!((ev, d["env"].as_str()), ("ack", Some("test")));
        assert!(console_frame(pre, Env::Prod, &json!({"entry_id": "1-0"})).is_none());
    }

    #[test]
    fn chat_frames_carry_only_known_events() {
        let ok =
            chat_frame(&json!({"conversation_id": "c1", "event": "message", "data": {"x": 1}}))
                .unwrap();
        assert_eq!((ok.0.as_str(), ok.1), ("c1", "message"));
        assert!(
            chat_frame(&json!({"conversation_id": "c1", "event": "evil", "data": {}})).is_none()
        );
        assert!(chat_frame(&json!({"event": "message"})).is_none());
    }

    #[test]
    fn bus_ids_are_unique_across_envs_and_streams() {
        let a = event_id("bus", &bus_data("prod", "1-0", 1)).unwrap();
        let b = event_id("bus", &bus_data("test", "1-0", 1)).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, "prod/bus:op/1-0");
        assert_eq!(event_id("ack", &bus_data("prod", "1-0", 1)), None);
    }

    #[test]
    fn backlog_is_oldest_first_after_after_ms_with_acks_folded_in() {
        let r = vec![
            rec("bus", bus_data("prod", "3-0", 3000)),
            rec("bus", bus_data("test", "1-0", 1000)),
            rec("bus", bus_data("test", "2-0", 2000)),
            rec(
                "ack",
                json!({"stream": "bus:op", "entry_id": "3-0", "consumer_group": "runner", "env": "prod"}),
            ),
        ];
        let b = backlog(r, 1500);
        let at: Vec<u64> = b["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["envelope"]["published_at_ms"].as_u64().unwrap())
            .collect();
        assert_eq!(at, vec![2000, 3000]);
        assert_eq!(b["items"][0]["acked"], json!(false));
        assert_eq!(b["items"][1]["acked"], json!(true));
    }

    #[test]
    fn replay_starts_after_the_last_event_id() {
        let mk = || {
            vec![
                rec("bus", bus_data("prod", "1-0", 1)),
                rec("bus", bus_data("prod", "2-0", 2)),
                rec("bus", bus_data("prod", "3-0", 3)),
            ]
        };
        let (t, j) = (Uuid::new_v4(), Uuid::new_v4());
        let got: Vec<String> = replay(mk(), "prod/bus:op/1-0", t, j)
            .iter()
            .filter_map(|f| event_id(f.event, &f.data))
            .collect();
        assert_eq!(got, vec!["prod/bus:op/2-0", "prod/bus:op/3-0"]);
        assert_eq!(replay(mk(), "gone", t, j).len(), 3);
    }

    #[test]
    fn message_ids_name_the_message_and_only_message_frames() {
        let m = json!({"message_id": "0199-a", "text": "hi"});
        assert_eq!(
            message_event_id("message", &m).as_deref(),
            Some("msg/0199-a")
        );
        assert_eq!(message_event_id("chat", &m).as_deref(), Some("msg/0199-a"));
        assert_eq!(message_event_id("csat", &m), None);
        assert_eq!(message_event_id("message", &json!({"text": "no id"})), None);
        assert_eq!(
            message_event_id("message", &json!({"message_id": ""})),
            None
        );
        // the job feed keeps bus ids only: a chat frame never moves its Last-Event-ID
        assert_eq!(event_id("message", &m), None);
    }

    #[test]
    fn last_event_id_is_read_trimmed_and_blank_is_none() {
        let mut h = HeaderMap::new();
        assert_eq!(last_event_id(&h), None);
        h.insert("last-event-id", " msg/x ".parse().unwrap());
        assert_eq!(last_event_id(&h).as_deref(), Some("msg/x"));
        h.insert("last-event-id", "  ".parse().unwrap());
        assert_eq!(last_event_id(&h), None);
    }

    #[tokio::test]
    async fn a_conversation_feed_sends_a_replayed_message_once() {
        let hub = crate::conversations::hub::LocalHub::default();
        let (t, j) = (Uuid::new_v4(), Uuid::new_v4());
        let msg = |id: &str| Frame {
            tenant: t,
            job_id: j,
            conversation_id: Some("c1".into()),
            event: "message",
            data: json!({"message_id": id}),
        };
        let rx = hub.subscribe();
        let mut s = Box::pin(frames(
            rx,
            Upstream::Local,
            t,
            j,
            Some("c1".into()),
            vec![msg("m2")],
        ));
        // m2 arrives live too (published between the subscription and the replay read), then m3
        hub.publish(msg("m2"));
        hub.publish(msg("m3"));
        let first = s.next().await.unwrap().data["message_id"].clone();
        let second = s.next().await.unwrap().data["message_id"].clone();
        assert_eq!((first, second), (json!("m2"), json!("m3")));
    }

    #[test]
    fn json_is_chosen_by_after_ms_or_accept_unless_sse_is_asked() {
        let mut h = HeaderMap::new();
        assert!(!wants_json(&h, false));
        assert!(wants_json(&h, true));
        h.insert(header::ACCEPT, "application/json".parse().unwrap());
        assert!(wants_json(&h, false));
        h.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        assert!(!wants_json(&h, true));
    }
}
