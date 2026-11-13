//! Telegram webhook health (B7 T15b, owner decisions 2026-10-22 / 2026-10-23; 0153, 0155). Two sources:
//!
//! - What the edge OBSERVES: updates arriving and sends succeeding or failing. [`WebhookHealth`] keeps per-binding
//!   counters in memory, per replica: the last update a binding's webhook received, the newest successful send, the
//!   sends that failed in a row (a success resets it), and the last send error as a short code with its time. The edge
//!   pipeline (crate::channels::edge) records an authenticated Telegram update and the outcome of every Telegram send
//!   (an answer, a refusal text, a staff reply / notice / CSAT through `/internal/deliver`); typing indicators are not
//!   counted.
//! - What Telegram SAYS: the read-only webhook-info call (crate::channels::telegram::TelegramConnector::webhook_info),
//!   made by [`report_once`] for every Telegram binding this edge serves, active or not, so a bot nobody writes to is
//!   still checked. It claims nothing (C1, send-only, holds: the claiming methods stay forbidden crate-wide by the
//!   signup scan, which exempts only crate::channels::telegram for this one read). The registered URL stays on the
//!   edge; only `url_set` / `url_matches` and the pending count and delivery error (as a code) are reported.
//!
//! [`spawn_reporter`] posts one snapshot per binding every [`REPORT_EVERY`] to core's
//! `POST /api/internal/channel/webhook-status` ([`CoreLink::webhook_status`]); a binding that left the binding store
//! (deleted, disabled) is forgotten. agent-api merges the snapshots of every replica into `channel_webhook_status`:
//! times merge by newest, and [`degraded_since`] keeps a `degraded_at` that only a successful send NEWER than it
//! clears, so a replica that saw no failures cannot flip a degraded binding back to `ok`. [`status_of`] is the status
//! rule (precedence `down` > `degraded` > `quiet` > `ok`):
//!
//! - `down` ([`down_reason`], only with this report's webhook info): no URL registered (`url_unset`), a URL that is
//!   not this binding's route (`url_mismatch`), or a delivery error within [`WEBHOOK_ERROR_WINDOW`] newer than the last
//!   update (`webhook_error`);
//! - `degraded`: no send succeeded after `degraded_at`, or more than [`PENDING_DEGRADED_AFTER`] updates wait;
//! - `quiet` (informational, not an error): the last update is older than [`STALE_AFTER`];
//! - else `ok`. A report without webhook info (an older edge, or the call failed) keeps the row's stored reason and
//!   pending count while they are younger than [`WEBHOOK_INFO_FRESH`] (so replicas do not flap); after that `down`
//!   is not derivable and the rule is the observed one alone.
//!
//! Nothing here holds or logs message text, a token or the webhook URL: an error is reduced to a fixed code by
//! [`error_code`] / crate::channels::telegram::webhook_error_code.
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use uuid::Uuid;

use super::bindings::Binding;
use super::connector::{ChannelConnector, ChannelError, ChannelKind, Side};
use super::core::{CoreError, CoreLink, WebhookInfoReport, WebhookStatusReq};
use super::edge::EdgeState;
use super::telegram;

/// How often the edge reports.
pub const REPORT_EVERY: Duration = Duration::from_secs(60);
/// Consecutive send failures that make a binding `degraded`.
pub const DEGRADED_AFTER: u32 = 3;
/// No update for this long makes a binding `quiet` (nobody wrote to the bot; not an error by itself).
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);
/// The longest error code kept (the column allows 120).
pub const MAX_ERROR_LEN: usize = 120;
/// A webhook delivery error Telegram reports makes a binding `down` while it is this recent (and newer than the last
/// update the edge received).
pub const WEBHOOK_ERROR_WINDOW: Duration = Duration::from_secs(15 * 60);
/// More undelivered updates than this make a binding `degraded`.
pub const PENDING_DEGRADED_AFTER: u32 = 100;
/// Stored webhook info stands in for a report without any (an older edge, a failed call) this long: five reporting
/// passes ([`REPORT_EVERY`]).
pub const WEBHOOK_INFO_FRESH: Duration = Duration::from_secs(5 * 60);

/// `channel_webhook_status.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookStatus {
    Ok,
    Degraded,
    Quiet,
    Down,
}

impl WebhookStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookStatus::Ok => "ok",
            WebhookStatus::Degraded => "degraded",
            WebhookStatus::Quiet => "quiet",
            WebhookStatus::Down => "down",
        }
    }
}

/// `channel_webhook_status.webhook_down_reason` (set exactly when the status is `down`, 0155).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownReason {
    UrlUnset,
    UrlMismatch,
    WebhookError,
}

impl DownReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DownReason::UrlUnset => "url_unset",
            DownReason::UrlMismatch => "url_mismatch",
            DownReason::WebhookError => "webhook_error",
        }
    }

    /// The stored text back (`None` for anything else).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "url_unset" => Some(DownReason::UrlUnset),
            "url_mismatch" => Some(DownReason::UrlMismatch),
            "webhook_error" => Some(DownReason::WebhookError),
            _ => None,
        }
    }
}

/// The next `degraded_at` (epoch ms) of a binding, merged across replicas. `prev` is the stored one,
/// `last_send_ok_at_ms` the newest successful send known for the binding (already merged with the report), and
/// `consecutive_send_failures` / `last_error_at_ms` the reporting replica's failure run and its newest failure.
///
/// A run of [`DEGRADED_AFTER`] failures degrades the binding as of its newest failure, but only when that failure is
/// newer than every known successful send (a stale run on one replica does not outvote a later success on another).
/// Otherwise the stored value stands: a report with fewer failures never clears it; only [`status_of`] reads it
/// against a newer successful send.
pub fn degraded_since(
    prev: Option<i64>,
    last_send_ok_at_ms: Option<i64>,
    consecutive_send_failures: u32,
    last_error_at_ms: Option<i64>,
) -> Option<i64> {
    match last_error_at_ms {
        Some(failed)
            if consecutive_send_failures >= DEGRADED_AFTER
                && last_send_ok_at_ms.is_none_or(|ok| failed > ok) =>
        {
            Some(prev.map_or(failed, |p| p.max(failed)))
        }
        _ => prev,
    }
}

/// Why a binding is `down`, from this report's webhook info (`None` without it: `down` is not derivable then). In
/// order: no URL registered; a URL that is not this binding's route; a delivery error at most [`WEBHOOK_ERROR_WINDOW`]
/// old and newer than `last_update_at_ms` (an update that arrived after the error shows delivery recovered).
pub fn down_reason(
    now_ms: i64,
    last_update_at_ms: Option<i64>,
    info: Option<&WebhookInfoReport>,
) -> Option<DownReason> {
    let info = info?;
    let window_ms = i64::try_from(WEBHOOK_ERROR_WINDOW.as_millis()).unwrap_or(i64::MAX);
    if !info.url_set {
        Some(DownReason::UrlUnset)
    } else if info.url_matches == Some(false) {
        Some(DownReason::UrlMismatch)
    } else if info.last_error_at_ms.is_some_and(|e| {
        now_ms.saturating_sub(e) <= window_ms && last_update_at_ms.is_none_or(|u| e > u)
    }) {
        Some(DownReason::WebhookError)
    } else {
        None
    }
}

/// The webhook info a row holds from an earlier report (0155 `webhook_info_at`, `webhook_down_reason`,
/// `pending_update_count`). It stands in for a report that carries none (an older edge, a replica whose call failed)
/// while it is younger than [`WEBHOOK_INFO_FRESH`], so replicas without info do not flap a `down` binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredInfo {
    /// When a report last carried webhook info (epoch ms).
    pub at_ms: i64,
    pub down_reason: Option<DownReason>,
    pub pending_update_count: Option<u32>,
}

/// The status rule with the reason it is `down`, if it is (precedence `down` > `degraded` > `quiet` > `ok`). The
/// webhook view is this report's `info` when it has one ([`down_reason`] over it, its pending count), else `stored`
/// while it is fresh ([`WEBHOOK_INFO_FRESH`]: its reason and count are kept as they are), else none. Then: `down`
/// with a reason; `degraded` while `degraded_at_ms` is set and no send succeeded after it, or when more than
/// [`PENDING_DEGRADED_AFTER`] updates wait; `quiet` when the newest update is older than `stale` (`None` = never seen
/// one, which is `ok`: a bot nobody has written to yet is not broken); else `ok`. Without either view this is the
/// observed rule alone.
pub fn health_of(
    now_ms: i64,
    last_update_at_ms: Option<i64>,
    degraded_at_ms: Option<i64>,
    last_send_ok_at_ms: Option<i64>,
    info: Option<&WebhookInfoReport>,
    stored: Option<&StoredInfo>,
    stale: Duration,
) -> (WebhookStatus, Option<DownReason>) {
    let stale_ms = i64::try_from(stale.as_millis()).unwrap_or(i64::MAX);
    let fresh_ms = i64::try_from(WEBHOOK_INFO_FRESH.as_millis()).unwrap_or(i64::MAX);
    let (down, pending) = match (info, stored) {
        (Some(i), _) => (
            down_reason(now_ms, last_update_at_ms, Some(i)),
            Some(i.pending_update_count),
        ),
        (None, Some(s)) if now_ms.saturating_sub(s.at_ms) < fresh_ms => {
            (s.down_reason, s.pending_update_count)
        }
        _ => (None, None),
    };
    let status = if down.is_some() {
        WebhookStatus::Down
    } else if degraded_at_ms.is_some_and(|d| last_send_ok_at_ms.is_none_or(|ok| ok <= d))
        || pending.is_some_and(|p| p > PENDING_DEGRADED_AFTER)
    {
        WebhookStatus::Degraded
    } else if last_update_at_ms.is_some_and(|t| now_ms.saturating_sub(t) > stale_ms) {
        WebhookStatus::Quiet
    } else {
        WebhookStatus::Ok
    };
    (status, down)
}

/// [`health_of`] without the reason.
pub fn status_of(
    now_ms: i64,
    last_update_at_ms: Option<i64>,
    degraded_at_ms: Option<i64>,
    last_send_ok_at_ms: Option<i64>,
    info: Option<&WebhookInfoReport>,
    stored: Option<&StoredInfo>,
    stale: Duration,
) -> WebhookStatus {
    health_of(
        now_ms,
        last_update_at_ms,
        degraded_at_ms,
        last_send_ok_at_ms,
        info,
        stored,
        stale,
    )
    .0
}

/// True when `s` is a storable error code: 1 to [`MAX_ERROR_LEN`] of `[A-Za-z0-9_.:-]`.
pub fn is_error_code(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ERROR_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// A send error as a fixed code: the provider's status / error code (crate::channels::telegram formats only those),
/// never a description, a transport error or a URL (which would carry the bot token).
pub fn error_code(e: &ChannelError) -> String {
    let code = match e {
        ChannelError::Upstream(c) => c.as_str(),
        ChannelError::RateLimited { .. } => "rate_limited",
        ChannelError::Unavailable(_) => "unavailable",
        ChannelError::Invalid(c)
        | ChannelError::Unauthenticated(c)
        | ChannelError::Forbidden(c) => *c,
        ChannelError::Unsupported => "unsupported",
    };
    if is_error_code(code) {
        code.to_string()
    } else {
        "send_failed".to_string()
    }
}

/// What the edge saw of one binding since it started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    pub last_update_at_ms: Option<i64>,
    /// The newest send through the binding's bot that succeeded.
    pub last_send_ok_at_ms: Option<i64>,
    pub consecutive_send_failures: u32,
    pub last_error: Option<String>,
    pub last_error_at_ms: Option<i64>,
}

/// Per-binding counters (only bindings with activity have an entry).
#[derive(Debug, Default)]
pub struct WebhookHealth {
    by_binding: Mutex<HashMap<Uuid, Observed>>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl WebhookHealth {
    fn with(&self, id: Uuid, f: impl FnOnce(&mut Observed)) {
        let mut m = self
            .by_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f(m.entry(id).or_default());
    }

    /// The binding's webhook received an (authenticated) update at `at_ms`.
    pub fn update_received_at(&self, id: Uuid, at_ms: i64) {
        self.with(id, |o| {
            o.last_update_at_ms = Some(o.last_update_at_ms.map_or(at_ms, |t| t.max(at_ms)));
        });
    }

    pub fn update_received(&self, id: Uuid) {
        self.update_received_at(id, now_ms());
    }

    /// A send through the binding's bot succeeded at `at_ms`: the failure run ends (the last error stays as history).
    pub fn send_ok_at(&self, id: Uuid, at_ms: i64) {
        self.with(id, |o| {
            o.consecutive_send_failures = 0;
            o.last_send_ok_at_ms = Some(o.last_send_ok_at_ms.map_or(at_ms, |t| t.max(at_ms)));
        });
    }

    pub fn send_ok(&self, id: Uuid) {
        self.send_ok_at(id, now_ms());
    }

    pub fn send_failed_at(&self, id: Uuid, e: &ChannelError, at_ms: i64) {
        let code = error_code(e);
        self.with(id, |o| {
            o.consecutive_send_failures = o.consecutive_send_failures.saturating_add(1);
            o.last_error = Some(code);
            o.last_error_at_ms = Some(at_ms);
        });
    }

    pub fn send_failed(&self, id: Uuid, e: &ChannelError) {
        self.send_failed_at(id, e, now_ms());
    }

    /// Record one send outcome.
    pub fn sent<T>(&self, id: Uuid, r: &Result<T, ChannelError>) {
        match r {
            Ok(_) => self.send_ok(id),
            Err(e) => self.send_failed(id, e),
        }
    }

    /// Every binding with activity, in id order.
    pub fn snapshot(&self) -> Vec<(Uuid, Observed)> {
        let m = self
            .by_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut v: Vec<(Uuid, Observed)> = m.iter().map(|(k, o)| (*k, o.clone())).collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    pub fn get(&self, id: Uuid) -> Option<Observed> {
        self.by_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    pub fn forget(&self, id: Uuid) {
        self.by_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
}

/// A served Telegram customer core binding (others are not reported).
fn served(st: &EdgeState, id: Uuid) -> Option<Binding> {
    st.bindings
        .by_id(id)
        .filter(|b| b.kind == ChannelKind::Telegram && b.side == Side::Customer)
}

/// The binding's webhook registration as Telegram reports it, reduced for the wire (no URL, codes only). `None` when
/// the call fails (Telegram unreachable, …): logged by code, and the report goes without it, so a failed call never
/// changes the status by itself. A binding whose credential does not name its own bot
/// ([`telegram::binding_has_bot`]) is not asked at all (no call, no warning, no failure metric): the registry would
/// fall back to the registered connector, which has no bot, so the call could only fail, every pass, on every
/// replica. Its report goes without webhook info, as after a failed call.
async fn webhook_info_of(
    st: &EdgeState,
    base: &Arc<dyn ChannelConnector>,
    b: &Binding,
    id: Uuid,
) -> Option<WebhookInfoReport> {
    if !telegram::binding_has_bot(b) {
        tracing::debug!(binding_id = %id, "channel_webhook_info_skipped_no_bot");
        return None;
    }
    let c = st.registry.for_binding(base.clone(), Some(b));
    match c.read_webhook_info().await {
        Ok(w) => {
            let url_set = !w.url.is_empty();
            Some(WebhookInfoReport {
                url_set,
                url_matches: url_set.then(|| telegram::url_points_at(&w.url, id)),
                pending_update_count: w.pending_update_count,
                last_error_code: w.last_error_code,
                last_error_at_ms: w.last_error_at_ms,
            })
        }
        Err(e) => {
            tracing::warn!(binding_id = %id, error = %error_code(&e), "channel_webhook_info_failed");
            metrics::counter!("agent_edge_webhook_info_failed_total").increment(1);
            None
        }
    }
}

/// One report: a `webhook-status` call for every Telegram customer core binding this edge serves (from the binding
/// store, so a bot without any traffic is covered too) and every binding with activity that is still one (others are
/// forgotten), each with the webhook info read this pass. Returns the number of snapshots core stored.
///
/// Sequential on purpose: one webhook-info call and one report per binding per pass, each bounded by
/// crate::channels::telegram::API_TIMEOUT / the core call timeout; a pass that outlasts [`REPORT_EVERY`] delays the
/// next one (the ticker skips missed ticks) rather than piling up calls. Parallelise when bindings number in the
/// hundreds.
pub async fn report_once(st: &EdgeState, core: &CoreLink) -> usize {
    let mut ids: BTreeSet<Uuid> = st
        .bindings
        .of_kind(ChannelKind::Telegram, Side::Customer)
        .into_iter()
        .filter_map(|b| b.id)
        .collect();
    ids.extend(st.health.snapshot().into_iter().map(|(id, _)| id));
    st.registry.evict_stale(&st.bindings);
    let base = st.registry.get(ChannelKind::Telegram);
    let mut stored = 0;
    for id in ids {
        let Some(b) = served(st, id) else {
            st.health.forget(id);
            continue;
        };
        let o = st.health.get(id).unwrap_or_default();
        let webhook_info = match &base {
            Some(c) => webhook_info_of(st, c, &b, id).await,
            None => None,
        };
        let req = WebhookStatusReq {
            binding_id: id,
            kind: ChannelKind::Telegram.as_str().into(),
            tenant: b.scope.tenant,
            job_id: b.scope.job_id,
            last_update_at_ms: o.last_update_at_ms,
            last_send_ok_at_ms: o.last_send_ok_at_ms,
            consecutive_send_failures: o.consecutive_send_failures,
            last_error: o.last_error,
            last_error_at_ms: o.last_error_at_ms,
            webhook_info,
        };
        match core.webhook_status(&req).await {
            Ok(_) => stored += 1,
            Err(CoreError::Code(c)) if c == "binding_not_found" => st.health.forget(id),
            Err(e) => {
                tracing::warn!(binding_id = %id, error = %e, "channel_webhook_status_report_failed");
                metrics::counter!("agent_edge_webhook_status_report_failed_total").increment(1);
            }
        }
    }
    stored
}

/// Report every [`REPORT_EVERY`] (the first report one interval after start). `None` without core.
pub fn spawn_reporter(st: EdgeState) -> Option<tokio::task::JoinHandle<()>> {
    let core = st.core.clone()?;
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(REPORT_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            report_once(&st, &core).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    #[test]
    fn status_rule() {
        let stale = STALE_AFTER;
        let now = 100 * H;
        let st = |upd, deg, ok| status_of(now, upd, deg, ok, None, None, stale);
        assert_eq!(st(None, None, None), WebhookStatus::Ok);
        assert_eq!(st(Some(now - H), None, None), WebhookStatus::Ok);
        // degraded until a send succeeds after degraded_at
        assert_eq!(
            st(Some(now - H), Some(now - 10), None),
            WebhookStatus::Degraded
        );
        assert_eq!(
            st(Some(now - H), Some(now - 10), Some(now - 20)),
            WebhookStatus::Degraded
        );
        assert_eq!(
            st(Some(now - H), Some(now - 10), Some(now - 10)),
            WebhookStatus::Degraded
        );
        assert_eq!(
            st(Some(now - H), Some(now - 10), Some(now - 5)),
            WebhookStatus::Ok
        );
        // 24 h without an update is quiet (not down, not an error); exactly 24 h is still ok
        assert_eq!(st(Some(now - 24 * H), None, None), WebhookStatus::Ok);
        assert_eq!(st(Some(now - 24 * H - 1), None, None), WebhookStatus::Quiet);
        // degraded wins over quiet
        assert_eq!(
            st(Some(now - 30 * H), Some(now - H), None),
            WebhookStatus::Degraded
        );
        assert_eq!(
            st(Some(now - 30 * H), Some(now - H), Some(now)),
            WebhookStatus::Quiet
        );
        assert_eq!(
            [
                WebhookStatus::Ok,
                WebhookStatus::Degraded,
                WebhookStatus::Quiet,
                WebhookStatus::Down
            ]
            .map(WebhookStatus::as_str),
            ["ok", "degraded", "quiet", "down"]
        );
        assert_eq!(
            [
                DownReason::UrlUnset,
                DownReason::UrlMismatch,
                DownReason::WebhookError
            ]
            .map(DownReason::as_str),
            ["url_unset", "url_mismatch", "webhook_error"]
        );
    }

    fn info(url_set: bool, url_matches: Option<bool>) -> WebhookInfoReport {
        WebhookInfoReport {
            url_set,
            url_matches,
            ..WebhookInfoReport::default()
        }
    }

    #[test]
    fn down_needs_webhook_info_and_has_a_reason() {
        let now = 100 * H;
        let upd = Some(now - H);
        let good = info(true, Some(true));
        // no webhook info (an older edge, a failed call): down is not derivable
        assert_eq!(down_reason(now, upd, None), None);
        assert_eq!(down_reason(now, upd, Some(&good)), None);
        assert_eq!(down_reason(now, None, Some(&info(true, None))), None);
        assert_eq!(
            down_reason(now, upd, Some(&info(false, None))),
            Some(DownReason::UrlUnset)
        );
        assert_eq!(
            down_reason(now, upd, Some(&info(true, Some(false)))),
            Some(DownReason::UrlMismatch)
        );
        // url_unset wins over a mismatch flag (an edge never sends both, but the order is fixed)
        assert_eq!(
            down_reason(now, upd, Some(&info(false, Some(false)))),
            Some(DownReason::UrlUnset)
        );
        let err_at = |at: i64| WebhookInfoReport {
            last_error_code: Some("webhook_http_502".into()),
            last_error_at_ms: Some(at),
            ..good.clone()
        };
        let window = 15 * 60_000;
        // a delivery error newer than the last update, inside the window: down
        assert_eq!(
            down_reason(now, upd, Some(&err_at(now - 60_000))),
            Some(DownReason::WebhookError)
        );
        assert_eq!(
            down_reason(now, None, Some(&err_at(now - window))),
            Some(DownReason::WebhookError)
        );
        // just outside the window, or an update arrived after it (delivery recovered): not down
        assert_eq!(
            down_reason(now, None, Some(&err_at(now - window - 1))),
            None
        );
        assert_eq!(
            down_reason(now, Some(now - 30_000), Some(&err_at(now - 60_000))),
            None
        );
        assert_eq!(
            down_reason(now, Some(now - 60_000), Some(&err_at(now - 60_000))),
            None
        );
        // the URL reasons come before the error
        assert_eq!(
            down_reason(
                now,
                upd,
                Some(&WebhookInfoReport {
                    url_set: false,
                    url_matches: None,
                    ..err_at(now - 60_000)
                })
            ),
            Some(DownReason::UrlUnset)
        );
    }

    #[test]
    fn status_precedence_down_degraded_quiet_ok() {
        let now = 100 * H;
        let st = |upd, deg, ok, i: Option<&WebhookInfoReport>| {
            status_of(now, upd, deg, ok, i, None, STALE_AFTER)
        };
        let good = info(true, Some(true));
        let pending = |n| WebhookInfoReport {
            pending_update_count: n,
            ..good.clone()
        };
        let old = Some(now - 30 * H);
        // with healthy webhook info nothing changes
        assert_eq!(
            st(Some(now - H), None, None, Some(&good)),
            WebhookStatus::Ok
        );
        assert_eq!(st(old, None, None, Some(&good)), WebhookStatus::Quiet);
        // pending: degraded above PENDING_DEGRADED_AFTER, not at it; wins over quiet
        assert_eq!(
            st(Some(now - H), None, None, Some(&pending(100))),
            WebhookStatus::Ok
        );
        assert_eq!(
            st(Some(now - H), None, None, Some(&pending(150))),
            WebhookStatus::Degraded
        );
        assert_eq!(
            st(old, None, None, Some(&pending(150))),
            WebhookStatus::Degraded
        );
        // the degraded_at rule still applies with webhook info
        assert_eq!(
            st(Some(now - H), Some(now - 10), None, Some(&good)),
            WebhookStatus::Degraded
        );
        // down wins over degraded (both rules) and quiet
        let unset = WebhookInfoReport {
            url_set: false,
            url_matches: None,
            pending_update_count: 500,
            ..WebhookInfoReport::default()
        };
        assert_eq!(
            st(old, Some(now - 10), None, Some(&unset)),
            WebhookStatus::Down
        );
        assert_eq!(
            st(Some(now - H), None, None, Some(&info(true, Some(false)))),
            WebhookStatus::Down
        );
        // without webhook info: the observed rule alone (pending is unknown, never down)
        assert_eq!(st(old, None, None, None), WebhookStatus::Quiet);
        assert_eq!(
            st(Some(now - H), Some(now - 10), None, None),
            WebhookStatus::Degraded
        );
    }

    #[test]
    fn degraded_since_needs_a_fresh_failure_run() {
        // fewer than DEGRADED_AFTER failures: nothing changes
        assert_eq!(degraded_since(None, None, 2, Some(50)), None);
        assert_eq!(degraded_since(Some(40), None, 0, None), Some(40));
        // a run of 3: degraded as of its newest failure
        assert_eq!(degraded_since(None, None, 3, Some(50)), Some(50));
        assert_eq!(degraded_since(None, Some(10), 3, Some(50)), Some(50));
        // a run older than a known successful send does not degrade
        assert_eq!(degraded_since(None, Some(60), 5, Some(50)), None);
        // an ongoing run keeps the stored value moving forward, never back
        assert_eq!(degraded_since(Some(50), None, 4, Some(70)), Some(70));
        assert_eq!(degraded_since(Some(70), None, 4, Some(50)), Some(70));
        // a run without its time (an old edge) cannot be ordered: keep what is stored
        assert_eq!(degraded_since(Some(70), None, 9, None), Some(70));
    }

    /// The flap (two replicas, one stored row): A sees 3 failures, B sees none and reports a second later. Before 0155
    /// the last reporter's counters set the status, so B flipped it to ok. Now B cannot clear it; only a send that
    /// succeeded after the failures does, and then A's stale run cannot bring degraded back.
    #[test]
    fn degraded_does_not_flap_between_replicas() {
        let now = 100 * H;
        let upd = Some(now - 60_000);
        // the stored row, merged as agent-api does (times by newest)
        let mut degraded_at = None;
        let mut ok_at: Option<i64> = Some(now - 10 * 60_000);
        let mut report = |ok: Option<i64>, failures: u32, failed: Option<i64>| {
            ok_at = ok_at.max(ok);
            degraded_at = degraded_since(degraded_at, ok_at, failures, failed);
            status_of(now, upd, degraded_at, ok_at, None, None, STALE_AFTER)
        };
        // replica A: three failed sends after the last good one
        let a_failed = Some(now - 30_000);
        assert_eq!(report(None, 3, a_failed), WebhookStatus::Degraded);
        // replica B: no failures, no newer successful send (its last ok is older than A's failures)
        assert_eq!(
            report(Some(now - 5 * 60_000), 0, None),
            WebhookStatus::Degraded
        );
        // B again, an edge restart with empty counters
        assert_eq!(report(None, 0, None), WebhookStatus::Degraded);
        // A again, same run
        assert_eq!(report(None, 3, a_failed), WebhookStatus::Degraded);
        // B: a send succeeded after A's failures: ok
        assert_eq!(report(Some(now - 10_000), 0, None), WebhookStatus::Ok);
        // A still reports its stale run (its counters did not see B's success): stays ok
        assert_eq!(report(None, 3, a_failed), WebhookStatus::Ok);
        // A: a fresh run of failures after B's success: degraded again
        assert_eq!(report(None, 6, Some(now - 1_000)), WebhookStatus::Degraded);
    }

    #[test]
    fn counters_track_updates_and_failure_runs() {
        let h = WebhookHealth::default();
        let id = Uuid::new_v4();
        assert!(h.get(id).is_none());
        h.update_received_at(id, 2_000);
        h.update_received_at(id, 1_000);
        assert_eq!(h.get(id).unwrap().last_update_at_ms, Some(2_000));
        for at in [10, 20, 30] {
            h.send_failed_at(id, &ChannelError::Upstream("telegram_403_403".into()), at);
        }
        let o = h.get(id).unwrap();
        assert_eq!(o.consecutive_send_failures, 3);
        assert_eq!(
            (o.last_error.as_deref(), o.last_error_at_ms),
            (Some("telegram_403_403"), Some(30))
        );
        h.send_ok_at(id, 40);
        h.send_ok_at(id, 35);
        let o = h.get(id).unwrap();
        assert_eq!(o.consecutive_send_failures, 0);
        assert_eq!(o.last_send_ok_at_ms, Some(40));
        assert_eq!(o.last_error.as_deref(), Some("telegram_403_403"));
        assert_eq!(h.snapshot().len(), 1);
        h.forget(id);
        assert!(h.snapshot().is_empty());
    }

    #[test]
    fn errors_are_codes_only() {
        assert_eq!(
            error_code(&ChannelError::RateLimited { retry_after_s: 3 }),
            "rate_limited"
        );
        assert_eq!(
            error_code(&ChannelError::Unavailable(
                "connect to https://x/bot123:abc".into()
            )),
            "unavailable"
        );
        assert_eq!(
            error_code(&ChannelError::Invalid("missing_route")),
            "missing_route"
        );
        assert_eq!(
            error_code(&ChannelError::Upstream("a description with spaces".into())),
            "send_failed"
        );
        assert!(is_error_code("telegram_400_400"));
        assert!(!is_error_code(""));
        assert!(!is_error_code("bad text"));
        assert!(!is_error_code(&"x".repeat(MAX_ERROR_LEN + 1)));
    }

    /// Replica A reads the webhook info (no URL: down); replica B, whose call fails, reports a minute later without
    /// it: still down, from the stored info. Once the stored info is older than WEBHOOK_INFO_FRESH: the observed rule.
    #[test]
    fn stored_info_keeps_down_across_replicas_while_fresh() {
        let now = 100 * H;
        let upd = Some(now - 60_000);
        let unset = info(false, None);
        let a = health_of(now, upd, None, None, Some(&unset), None, STALE_AFTER);
        assert_eq!(a, (WebhookStatus::Down, Some(DownReason::UrlUnset)));
        let stored = |age_ms: i64, reason, pending| StoredInfo {
            at_ms: now - age_ms,
            down_reason: reason,
            pending_update_count: pending,
        };
        let b = |s: &StoredInfo| health_of(now, upd, None, None, None, Some(s), STALE_AFTER);
        assert_eq!(
            b(&stored(60_000, Some(DownReason::UrlUnset), Some(0))),
            (WebhookStatus::Down, Some(DownReason::UrlUnset))
        );
        // a stored backlog keeps degraded the same way
        assert_eq!(
            b(&stored(60_000, None, Some(150))),
            (WebhookStatus::Degraded, None)
        );
        // stale stored info (5 min and older): the observed rule alone
        let fresh_ms = 5 * 60_000;
        assert_eq!(
            b(&stored(fresh_ms, Some(DownReason::UrlUnset), Some(150))),
            (WebhookStatus::Ok, None)
        );
        assert_eq!(
            b(&stored(fresh_ms - 1, Some(DownReason::UrlUnset), None)).0,
            WebhookStatus::Down
        );
        // this report's info always wins over the stored one
        assert_eq!(
            health_of(
                now,
                upd,
                None,
                None,
                Some(&info(true, Some(true))),
                Some(&stored(1_000, Some(DownReason::UrlUnset), Some(500))),
                STALE_AFTER
            ),
            (WebhookStatus::Ok, None)
        );
        for r in [
            DownReason::UrlUnset,
            DownReason::UrlMismatch,
            DownReason::WebhookError,
        ] {
            assert_eq!(DownReason::parse(r.as_str()), Some(r));
        }
        assert_eq!(DownReason::parse("down"), None);
    }
}
