//! One job's attention list (console redesign T7): what needs a person on this job, as `AttentionItem`s
//! (`{id, kind, severity, title, detail, href, action, job, at, owner_role, due_at}`), most urgent first.
//!
//! Three readers share it, so they cannot disagree: the job dashboard (`GET /api/jobs/{job}/overview`, which already
//! probed the bus and the dependencies and hands those facts to [`build`]), the agent status
//! ([`super::agent_status`], whose `urgent` / `review` are the `blocking` / `action` counts) and the inbox
//! (`GET /api/inbox`). Severity: `blocking` = customers are refused or a call fails now, `action` = waits on a
//! decision, `info` = worth knowing. Titles and details are plain language without customer data (K50: no customer text, name, case title or payload);
//! the raw technical text (keys, ids, error codes) goes in `code`, which the console shows only on request.
//!
//! The owner of each source (`owner_role`, a [`crate::auth::Role`] key or `automatic`):
//! - handover past its SLA: blocking, `support_agent`, `due_at` = the SLA deadline that went by
//! - handover not claimed: action, `support_manager`
//! - operation failing at least 10 %: action, `integrations_manager`
//! - dead letters, an unreadable agent list, a dependency down, an unchecked unknown outcome: blocking, `operator`
//! - an agent behind on its streams: action, `operator`
//! - release selection error, approved cut waiting, newer base, readiness items: action, `release_manager`
//! - a release is current but the bus does not answer: blocking, `operator`, "Agent isn't answering customers"
use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::bus_admin;
use crate::design::{self, JobRef};
use crate::error::ApiResult;
use crate::readmodel::wording;
use crate::release::compile::Compiled;
use crate::release::summary;
use crate::routes::overview::{OpRow, STEP, dead_letters, failing_ops};
use crate::state::AppState;
use crate::tenancy::JobCtx;

/// The most unknown-outcome cases listed one by one.
const ALERTS_LISTED: i64 = 10;

/// What the database says about the job's attention (read in the job-scoped transaction).
pub struct DbFacts {
    /// `release::summary::status` (base_changed, approved_waiting, selection_error, missing_slots, newer_pins …).
    pub status: Value,
    /// The draft compile `status` made (`None`: no selection, or the template refused it), for the draft's readiness.
    pub draft: Option<Compiled>,
    /// The job has a current (applied) release.
    pub has_release: bool,
    /// Handed-over cases nobody claimed.
    pub unclaimed: i64,
    /// Open handovers past a due time they carry.
    pub breached: i64,
    /// The earliest due time that went by among them.
    pub breach_due: Option<DateTime<Utc>>,
    /// Cases with an unchecked unknown outcome, newest first (at most [`ALERTS_LISTED`]).
    pub unknown_cases: Vec<String>,
    pub unknown_total: i64,
    /// 30-day Production calls and errors per `(operation, instance)`.
    pub ops: Vec<OpRow>,
}

/// T13: an unknown outcome stays on the list until an operator checked it (`operation_call_check`).
const UNCHECKED: &str = "o.job_id = $1 AND o.state = 'unknown_outcome' AND o.case_id IS NOT NULL \
     AND NOT EXISTS (SELECT 1 FROM operation_call_check k \
                      WHERE k.tenant_id = o.tenant_id AND k.job_id = o.job_id AND k.call_id = o.call_id)";

/// Read the [`DbFacts`] of `job` in the caller's job-scoped transaction.
pub async fn db_facts(c: &mut PgConnection, st: &AppState, job: &JobRef) -> ApiResult<DbFacts> {
    let (status, draft) = summary::status_with_draft(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        &st.readiness,
        job,
    )
    .await?;
    let has_release = summary::current_ref(&mut *c, job).await?.is_some();
    let unclaimed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_handover_items_owned \
          WHERE job_id = $1 AND state = 'queued' AND assignee IS NULL",
    )
    .bind(job.id)
    .fetch_one(&mut *c)
    .await?;
    // a handover is past its SLA when a due time it carries went by while it is still open (T16 sets the due times
    // from the handover policy; until it lands no item carries one and none is breached)
    let (breached, breach_due): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT count(*), \
                min(CASE WHEN first_response_at IS NULL AND first_response_due < now() \
                         THEN first_response_due ELSE resolve_due END) \
           FROM agent_handover_items \
          WHERE job_id = $1 AND state IN ('queued', 'assigned', 'waiting_customer') \
            AND ((first_response_at IS NULL AND first_response_due < now()) OR resolve_due < now())",
    )
    .bind(job.id)
    .fetch_one(&mut *c)
    .await?;
    let unknown_cases: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT o.case_id FROM operation_call o WHERE {UNCHECKED} \
          GROUP BY o.case_id ORDER BY max(o.started_at) DESC, o.case_id LIMIT $2"
    )))
    .bind(job.id)
    .bind(ALERTS_LISTED)
    .fetch_all(&mut *c)
    .await?;
    let unknown_total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(DISTINCT o.case_id) FROM operation_call o WHERE {UNCHECKED}"
    )))
    .bind(job.id)
    .fetch_one(&mut *c)
    .await?;
    let ops = sqlx::query_as::<_, OpRow>(
        "SELECT operation, instance_id, count(*) AS calls, \
                count(*) FILTER (WHERE state IN ('error', 'unknown_outcome')) AS errors \
           FROM operation_call \
          WHERE job_id = $1 AND env = 'prod' AND started_at >= now() - interval '30 days' \
          GROUP BY operation, instance_id",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    Ok(DbFacts {
        status,
        draft,
        has_release,
        unclaimed,
        breached,
        breach_due,
        unknown_cases,
        unknown_total,
        ops,
    })
}

/// What the bus said, probed with no transaction held.
pub struct BusFacts {
    /// Redis answered a PING.
    pub up: bool,
    /// Redis is configured but did not answer (error or timeout); `false` when it is not configured at all.
    pub reported_down: bool,
    /// The bus agents (`None` when the bus is down or they could not be read).
    pub agents: Option<Vec<Value>>,
    /// Dead letters of both environments (`None` when not counted).
    pub dead: Option<i64>,
}

/// The bus for `j` given the PING the caller already made (`up`, from [`super::agent_status::bus_up`], the one PING): the agents and the
/// dead-letter streams when it answered. Every step is bounded by [`STEP`].
pub async fn bus_facts(st: &AppState, j: &JobCtx, up: bool) -> BusFacts {
    let Some(root) = st.redis.as_ref() else {
        return BusFacts {
            up: false,
            reported_down: false,
            agents: None,
            dead: None,
        };
    };
    if !up {
        return BusFacts {
            up: false,
            reported_down: true,
            agents: None,
            dead: None,
        };
    }
    let agents = match tokio::time::timeout(STEP, bus_admin::agents(st, j)).await {
        Ok(Ok(v)) => Some(v),
        _ => None,
    };
    let dead = dead_letters(root, j).await;
    BusFacts {
        up: true,
        reported_down: false,
        agents,
        dead,
    }
}

/// Everything [`build`] reads.
pub struct Facts<'a> {
    pub job: &'a str,
    pub db: &'a DbFacts,
    /// Redis answered a PING.
    pub bus_up: bool,
    /// Redis is configured but did not answer.
    pub bus_down_reported: bool,
    /// Names of the other dependencies that are down (strata_rag, the LLM provider, `agents` when unreadable).
    pub down: &'a [String],
    pub agents: Option<&'a [Value]>,
    pub dead: Option<i64>,
}

/// A JSON version number (or string) as text.
fn num(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// One attention item before it is rendered as JSON. `page` is job-relative (`/release`).
struct Row {
    kind: &'static str,
    key: Option<String>,
    severity: &'static str,
    owner: &'static str,
    title: String,
    detail: String,
    page: String,
    action: &'static str,
    due_at: Option<DateTime<Utc>>,
    code: Option<String>,
}

fn row(
    kind: &'static str,
    severity: &'static str,
    owner: &'static str,
    title: String,
    page: &str,
    action: &'static str,
) -> Row {
    Row {
        kind,
        key: None,
        severity,
        owner,
        title,
        detail: String::new(),
        page: page.to_string(),
        action,
        due_at: None,
        code: None,
    }
}

impl Row {
    fn key(mut self, key: &str) -> Self {
        self.key = Some(key.to_string());
        self
    }

    fn detail(mut self, detail: &str) -> Self {
        self.detail = detail.to_string();
        self
    }

    fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    fn due(mut self, due: Option<DateTime<Utc>>) -> Self {
        self.due_at = due;
        self
    }

    /// The `AttentionItem`: `href` is the absolute console route of the job page.
    fn into_item(self, job: &str) -> Value {
        let id = match &self.key {
            Some(k) => format!("{}:{job}:{k}", self.kind),
            None => format!("{}:{job}", self.kind),
        };
        let detail = if self.detail.is_empty() {
            self.title.clone()
        } else {
            self.detail.clone()
        };
        json!({
            "id": id,
            "kind": self.kind,
            "severity": self.severity,
            "title": self.title,
            "detail": detail,
            "href": format!("/admin/jobs/{job}{}", self.page),
            "action": self.action,
            "job": job,
            "at": null,
            "owner_role": self.owner,
            "due_at": self.due_at,
            "code": self.code,
        })
    }
}

/// The attention items of `f`, most urgent first (see the module doc for the owners).
pub fn build(f: &Facts<'_>) -> Vec<Value> {
    let db = f.db;
    let mut rows: Vec<Row> = Vec::new();
    for name in f.down {
        rows.push(
            row(
                "dependency_down",
                "blocking",
                "operator",
                format!("{} is down", wording::human_key(name)),
                "/agents",
                "Open agents",
            )
            .key(name)
            .detail("It did not answer its health check.")
            .code(name.as_str()),
        );
    }
    if db.has_release && !f.bus_up {
        rows.push(
            row(
                "bus_unreachable",
                "blocking",
                "operator",
                "Agent isn't answering customers".to_string(),
                "/agents",
                "Check the message bus",
            )
            .detail("A release is live, but the message bus does not answer, so no customer message reaches the agent.")
            .code("bus_unreachable"),
        );
    } else if f.bus_down_reported {
        rows.push(
            row(
                "bus_unreachable",
                "blocking",
                "operator",
                "The message bus is down".to_string(),
                "/agents",
                "Check the message bus",
            )
            .detail("The message bus did not answer.")
            .code("redis (bus) did not answer."),
        );
    }
    if let Some(e) = db.status["selection_error"].as_str() {
        rows.push(
            row(
                "release_selection",
                "action",
                "release_manager",
                "The agent’s template selection can’t be built".to_string(),
                "/release",
                "Fix the selection",
            )
            .detail(&wording::plain(e))
            .code(e),
        );
    }
    for case in &db.unknown_cases {
        rows.push(
            row(
                "unknown_outcome",
                "blocking",
                "operator",
                "An action on your systems has an unknown outcome".to_string(),
                &format!("/cases/{case}"),
                "Check the call",
            )
            .key(case)
            .detail(&format!(
                "Case {case}: check what happened before anything is retried."
            ))
            .code(format!("{case}: unknown_outcome")),
        );
    }
    let more = db.unknown_total - db.unknown_cases.len() as i64;
    if more > 0 {
        rows.push(
            row(
                "unknown_outcome",
                "blocking",
                "operator",
                format!(
                    "{} more with an unknown outcome",
                    plural(more, "case", "cases")
                ),
                "/calls?outcome=unknown_outcome",
                "Check the calls",
            )
            .code("unknown_outcome"),
        );
    }
    match f.dead {
        Some(0) => {}
        Some(n) => rows.push(
            row(
                "dead_letters",
                "blocking",
                "operator",
                format!("{} failed to deliver", plural(n, "message", "messages")),
                "/buses?tab=dead",
                "Open failed messages",
            )
            .detail("Failed messages wait for someone to replay or discard them.")
            .code(format!("dead_letters: {n}")),
        ),
        None if f.bus_up => rows.push(
            row(
                "dead_letters",
                "action",
                "operator",
                "Failed messages could not be counted".to_string(),
                "/buses?tab=dead",
                "Open failed messages",
            )
            .code("dead_letters"),
        ),
        None => {}
    }
    if db.breached > 0 {
        rows.push(
            row(
                "handover_sla",
                "blocking",
                "support_agent",
                format!(
                    "{} past its SLA",
                    plural(db.breached, "handover", "handovers")
                ),
                "/handover",
                "Open handovers",
            )
            .due(db.breach_due)
            .code("handover_sla"),
        );
    }
    if db.unclaimed > 0 {
        rows.push(
            row(
                "handover_unclaimed",
                "action",
                "support_manager",
                format!(
                    "{} not claimed yet",
                    plural(db.unclaimed, "handed-over case", "handed-over cases")
                ),
                "/cases?status=handover",
                "Assign the cases",
            )
            .code("handover_unclaimed"),
        );
    }
    for fo in failing_ops(&db.ops) {
        let rate = fo["rate"].as_f64().unwrap_or(0.0) * 100.0;
        let op = fo["op"].as_str().unwrap_or_default();
        let op_name = wording::operation_name(op);
        rows.push(
            row(
                "operation_failing",
                "action",
                "integrations_manager",
                format!(
                    "{op_name} fails {rate:.0} % of its calls ({} of {} in 30 days)",
                    fo["errors"], fo["calls"]
                ),
                "/calls",
                "Open calls",
            )
            .key(op)
            .code(op),
        );
    }
    if let Some(id) = db.status["approved_waiting"].as_str() {
        rows.push(
            row(
                "release_waiting",
                "action",
                "release_manager",
                format!(
                    "The {} is approved and waiting to go live",
                    wording::release_name(id)
                ),
                "/release",
                "Go live",
            )
            .key(id)
            .code(id),
        );
    }
    for b in db.status["newer_pins"].as_array().into_iter().flatten() {
        let layer = b["layer"].as_str().unwrap_or_default();
        let (latest, pinned) = (num(&b["latest"]), num(&b["pinned"]));
        rows.push(
            row(
                "release_base_changed",
                "action",
                "release_manager",
                format!(
                    "{} is out — you run v{pinned}",
                    wording::layer_version(layer, &latest)
                ),
                "/release",
                "Review the change",
            )
            .key(layer)
            .code(format!("{layer}@v{latest} is out — you run @v{pinned}")),
        );
    }
    let missing = db.status["missing_slots"].as_array().map_or(0, Vec::len) as i64;
    if missing > 0 {
        rows.push(
            row(
                "readiness",
                "action",
                "release_manager",
                if missing == 1 {
                    "1 required setting is missing before going live".to_string()
                } else {
                    format!("{missing} required settings are missing before going live")
                },
                "/readiness",
                "Open the go-live checklist",
            )
            .code(format!(
                "{} block the next apply",
                plural(missing, "readiness item", "readiness items")
            )),
        );
    }
    if let Some(list) = f.agents {
        for a in list.iter().filter(|a| a["status"] == "warn") {
            let name = a["name"].as_str().unwrap_or_default();
            rows.push(
                row(
                    "agent_behind",
                    "action",
                    "operator",
                    format!("{} is falling behind", wording::human_key(name)),
                    "/agents",
                    "Open agents",
                )
                .key(name)
                .detail(&format!("It is {} messages behind.", a["lag"]))
                .code(format!("{name} is behind (lag {})", a["lag"])),
            );
        }
    }
    let mut items: Vec<Value> = rows.into_iter().map(|r| r.into_item(f.job)).collect();
    rank_sort(&mut items);
    items
}

fn severity_rank(v: &Value) -> u8 {
    match v["severity"].as_str() {
        Some("blocking") => 0,
        Some("action") => 1,
        _ => 2,
    }
}

fn instant(v: &Value, field: &str) -> Option<DateTime<Utc>> {
    v[field].as_str().and_then(|s| s.parse().ok())
}

/// The inbox order: severity (blocking, action, info), then `due_at` ascending with no due time last, then `at`
/// descending (an item with no time last). Stable: equal items keep their order.
pub fn rank_sort(items: &mut [Value]) {
    items.sort_by(|a, b| {
        severity_rank(a)
            .cmp(&severity_rank(b))
            .then_with(|| match (instant(a, "due_at"), instant(b, "due_at")) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then_with(|| instant(b, "at").cmp(&instant(a, "at")))
    });
}

/// `{blocking, action, info}`: how many items carry each severity.
pub fn counts(items: &[Value]) -> Value {
    let n = |s: &str| items.iter().filter(|i| i["severity"] == s).count();
    json!({"blocking": n("blocking"), "action": n("action"), "info": n("info")})
}

/// The attention of one job and the bus fact it was built on.
pub struct Collected {
    pub items: Vec<Value>,
    pub bus_up: bool,
    /// The draft compile made on the way (see [`DbFacts::draft`]), for a caller that also needs the draft's readiness.
    pub draft: Option<Compiled>,
}

/// Read the job's attention: the database facts in one job-scoped transaction, then the bus (no transaction held).
/// `bus_up` is the request's one PING ([`super::agent_status::bus_up`]); `ensure` runs the lazy `design::ensure_job` first (404 for an
/// unknown job) — pass `false` when the caller just listed the job.
pub async fn collect(
    st: &AppState,
    j: &JobCtx,
    job: &JobRef,
    bus_up: bool,
    ensure: bool,
) -> ApiResult<Collected> {
    let mut tx = j.begin(&st.db).await?;
    if ensure {
        design::ensure_job(&mut tx, &st.design, job).await?;
    }
    let db = db_facts(&mut tx, st, job).await?;
    tx.commit().await?;
    let bus = bus_facts(st, j, bus_up).await;
    let mut down: Vec<String> = Vec::new();
    if bus.up && bus.agents.is_none() {
        down.push("agents".to_string());
    }
    let items = build(&Facts {
        job: &job.key,
        db: &db,
        bus_up: bus.up,
        bus_down_reported: bus.reported_down,
        down: &down,
        agents: bus.agents.as_deref(),
        dead: bus.dead,
    });
    Ok(Collected {
        items,
        bus_up: bus.up,
        draft: db.draft,
    })
}

/// `job_attention`: the attention items of `job`, most urgent first. `bus_up` is the request's one PING; the caller
/// listed the job, so no `ensure_job` runs.
pub async fn job_attention(
    st: &AppState,
    j: &JobCtx,
    job: &JobRef,
    bus_up: bool,
) -> ApiResult<Vec<Value>> {
    Ok(collect(st, j, job, bus_up, false).await?.items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_plain_and_codes_keep_the_raw_text() {
        let db = DbFacts {
            status: json!({
                "approved_waiting": "cfg-2026-10-18.1",
                "newer_pins": [{"layer": "retail", "latest": 2, "pinned": 1}],
                "missing_slots": [
                    {"dependency": "returns", "slot": "window", "detail": "a", "href": "/admin/jobs/j/readiness"},
                    {"dependency": "returns", "slot": "carrier", "detail": "b", "href": "/admin/jobs/j/readiness"},
                ],
                "selection_error": "slot return_window missing in retail@v2",
            }),
            draft: None,
            has_release: false,
            unclaimed: 0,
            breached: 0,
            breach_due: None,
            unknown_cases: vec!["case-1".to_string()],
            unknown_total: 1,
            ops: vec![],
        };
        let items = build(&Facts {
            job: "j",
            db: &db,
            bus_up: true,
            bus_down_reported: false,
            down: &["strata_rag".to_string()],
            agents: None,
            dead: Some(3),
        });
        let by = |k: &str| items.iter().find(|i| i["kind"] == k).unwrap();
        let title = |k: &str| by(k)["title"].as_str().unwrap().to_string();
        assert_eq!(
            title("release_waiting"),
            "The 18 Oct release is approved and waiting to go live"
        );
        assert_eq!(by("release_waiting")["code"], "cfg-2026-10-18.1");
        assert_eq!(
            title("release_base_changed"),
            "Retail v2 is out — you run v1"
        );
        assert_eq!(
            by("release_base_changed")["code"],
            "retail@v2 is out — you run @v1"
        );
        assert_eq!(
            title("readiness"),
            "2 required settings are missing before going live"
        );
        assert_eq!(title("dependency_down"), "Knowledge search is down");
        assert_eq!(title("dead_letters"), "3 messages failed to deliver");
        assert!(
            by("unknown_outcome")["code"]
                .as_str()
                .unwrap()
                .contains("unknown_outcome")
        );
        for i in &items {
            for f in ["title", "detail", "action"] {
                let t = i[f].as_str().unwrap();
                assert!(
                    !t.contains("cfg-") && !t.contains("unknown_outcome") && !t.contains('@'),
                    "{t}"
                );
            }
            assert!(i.get("code").is_some(), "{i}");
        }
        assert_eq!(
            by("release_selection")["detail"],
            "slot return window missing in Retail v2"
        );
    }
}
