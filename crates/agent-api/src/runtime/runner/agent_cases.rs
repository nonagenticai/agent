//! `agent_cases` (B4 T6): the fallback case store. `case.create{handover}` and `case.create{complaint}` queue an
//! `agent_handover_items` row through B12's `conversations::handover::create` (the one queue, D64; stamped with the
//! case's design). The runner also uses [`queue`] for step 10 (`unknown_outcome` → reason `unknown_outcome`).
//!
//! Due times come from the scope's handover policy (B4 T16, crate::handover::sla). Out of hours (B4 T16a, F-14) a
//! `case.create` still queues the item, `outside_hours`, its SLA clocks starting at the scope's next open time; the
//! call answers `ok` with outcome code `outside_hours` and the next open time, which the interaction daemon names
//! to the customer (crate::runtime::reply). Every other agent-raised item ([`queue`]: the runner's step 10 and
//! T6a hand-overs, the orchestrator's hand-over, out-of-scope and failed case; the drain and reply gates through
//! [`closed_at_now`]) is out-of-hours aware the same way (acceptance A15), and [`out_of_hours_note`] gives the
//! orchestrator the same `{handover_id, next_open, next_open_local, out_of_hours}` for the case's reply
//! (`Resolution::out_of_hours`).
//!
//! What the person who takes the item sees (B4 T6a, plan "agent_cases" table; staff-facing, so ids may appear):
//! `priority` is the step arg `priority` (`urgent` / `high` / `normal` / `low`, anything else `normal`, due times
//! stamped for it); `agent_did` is built here from the case's finished `operation_call` rows, never from model text;
//! `collected` is the caller's list ([`collected_of`]: the resolved slots and the verified identity, no secrets);
//! `suggested_next` is a template per reason ([`suggested_next`]). `proposal_hint` is [`proposal_hint_of`]: the plan's
//! rule over the case's activity (hidden in the design, or unknown) and its knowledge results (`grounded`, D109), read
//! by the runner at queue time ([`HintFacts`]); the orchestrator's and the runner's other raisers pass none.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::ops::{OpOutcome, OpState, failed, outcome};
use crate::conversations::{handover, store};
use crate::design::{CaseDesign, DesignRow, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::handover::hours::{self, Closed, OutOfHours};
use crate::handover::{policy, sla};
use crate::runtime::domain::OutOfHoursNote;

/// The `case.create` outcome code of an item queued out of hours (catalogue error state `outside_hours`).
pub const OUTSIDE_HOURS: &str = "outside_hours";

/// The eight handover reasons of 0063.
pub const REASONS: [&str; 8] = [
    "identity_failed",
    "gate_blocked",
    "customer_asked",
    "tool_failed",
    "unknown_outcome",
    "out_of_scope",
    "staff_decision",
    "gate_unavailable",
];

/// The priority a step arg names: `urgent` / `high` / `normal` / `low` (any case, migration 0211), anything else
/// (or none) `normal`.
pub fn priority_of(input: &Map<String, Value>) -> &'static str {
    match input
        .get("priority")
        .and_then(Value::as_str)
        .map(|p| p.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("urgent") => "urgent",
        Some("high") => "high",
        Some("low") => "low",
        _ => "normal",
    }
}

/// The first step suggested to the person, a fixed template per handover reason (never case data).
pub fn suggested_next(reason: &str) -> &'static str {
    match reason {
        "identity_failed" => "Check who the customer is before acting on the request.",
        "gate_blocked" => {
            "Review why the request was blocked and decide whether an exception applies."
        }
        "tool_failed" => {
            "A system step failed: see what the agent did, then finish the request by hand."
        }
        "unknown_outcome" => {
            "Check in the system whether the last action went through before doing it again."
        }
        "out_of_scope" => {
            "The agent does not handle this request: deal with it or pass it to the right team."
        }
        "staff_decision" => "A person has to decide this: decide, then reply to the customer.",
        "gate_unavailable" => "A required check was unavailable: run it by hand, then carry on.",
        _ => "The customer asked for a person: read the conversation and pick up from there.",
    }
}

/// Slot names that never reach the item (secrets a customer may have typed).
const SECRET_WORDS: [&str; 10] = [
    "password",
    "passcode",
    "pin",
    "secret",
    "token",
    "otp",
    "cvv",
    "cvc",
    "security_code",
    "card_number",
];

fn is_secret(slot: &str) -> bool {
    let s = slot.to_ascii_lowercase();
    // short words only as whole `_`-separated parts (`pin` is in `shipping`), longer ones anywhere
    SECRET_WORDS.iter().any(|w| {
        if w.len() > 3 {
            s.contains(w)
        } else {
            s.split('_').any(|p| p == *w)
        }
    })
}

/// `order_number` -> `Order number`.
fn label_of(slot: &str) -> String {
    let t = slot.replace('_', " ");
    let mut cs = t.trim().chars();
    cs.next()
        .map(|f| f.to_uppercase().collect::<String>() + cs.as_str())
        .unwrap_or_default()
}

/// What the case collected, as `{label, value, verified}` rows (web `HandoverCollected`): the customer's slots (as
/// they said them, `verified: false`), the slots the system resolved from the customer's own records (these win,
/// `verified: true`) and the verified identity. Empty values and secret-looking slot names are left out.
pub fn collected_of(
    slots: &BTreeMap<String, String>,
    resolved: &BTreeMap<String, String>,
    identity_level: u8,
    customer_name: Option<&str>,
) -> Vec<Value> {
    let mut rows = Vec::new();
    if identity_level > 0 {
        if let Some(n) = customer_name.map(str::trim).filter(|n| !n.is_empty()) {
            rows.push(json!({"label": "Customer", "value": n, "verified": true}));
        }
        rows.push(json!({
            "label": "Identity check", "value": format!("checked to level {identity_level}"), "verified": true
        }));
    }
    let mut all: BTreeMap<&str, (&str, bool)> = BTreeMap::new();
    for (k, v) in slots {
        all.insert(k, (v, false));
    }
    for (k, v) in resolved {
        all.insert(k, (v, true));
    }
    for (k, (v, verified)) in all {
        let v = v.trim();
        if v.is_empty() || is_secret(k) {
            continue;
        }
        let v: String = v.chars().take(200).collect();
        rows.push(json!({"label": label_of(k), "value": v, "verified": verified}));
    }
    rows
}

/// What the agent did on case `case_id`: its finished operation calls in order, as `{op, outcome, detail}` (web
/// `HandoverAgentStep`; `case.create` itself and calls still pending are left out). Read from the `operation_call`
/// ledger, never from model text.
pub async fn agent_did_of(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
) -> ApiResult<Vec<Value>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT operation, state, outcome_code FROM operation_call \
          WHERE job_id = $1 AND case_id = $2 AND state <> 'pending' AND operation NOT LIKE 'case.create%' \
          ORDER BY started_at, call_id",
    )
    .bind(job.id)
    .bind(case_id)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(op, state, code)| step_of(&op, &state, code.as_deref()))
        .collect())
}

/// One ledger row as an `agent_did` step.
fn step_of(op: &str, state: &str, code: Option<&str>) -> Value {
    let outcome = match (state, code) {
        ("ok", _) => "ok",
        ("denied", _) => "blocked",
        (_, Some("not_found")) => "not_found",
        _ => "failed",
    };
    let detail = code.map_or_else(|| state.replace('_', " "), |c| c.replace('_', " "));
    json!({"op": op, "outcome": outcome, "detail": detail})
}

/// The [`handover::ItemExtras`] of a new item: `priority` from `input`, what the case did (ledger) and `collected`.
async fn extras_for(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    reason: &str,
    input: Option<&Map<String, Value>>,
    collected: &[Value],
    hint: Option<Value>,
) -> ApiResult<handover::ItemExtras> {
    let did = agent_did_of(&mut *c, job, case_id).await?;
    Ok(handover::ItemExtras {
        priority: input.map(|i| priority_of(i).to_owned()),
        agent_did: Some(Value::Array(did)),
        collected: (!collected.is_empty()).then(|| Value::Array(collected.to_vec())),
        suggested_next: Some(suggested_next(reason).to_owned()),
        proposal_hint: hint,
    })
}

/// What the runner read about a case at queue time, for its item's `proposal_hint` ([`proposal_hint_of`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HintFacts {
    /// The case's activity (the qualified request's, else its context space's); `None` = not known.
    pub activity: Option<String>,
    /// The activity is hidden in the design (a tombstone in the `overlay` places).
    pub activity_hidden: bool,
    /// A knowledge step of the case found nothing to answer from and none of its knowledge steps was grounded.
    pub knowledge_empty: bool,
}

/// Whether `activity` is hidden among a design's `overlay` places (`{object: "activity", key}` ref, `tombstone` value).
pub fn activity_hidden_in(rows: &[DesignRow], activity: &str) -> bool {
    rows.iter().any(|r| {
        r.r#ref.get("object").and_then(Value::as_str) == Some("activity")
            && r.r#ref.get("key").and_then(Value::as_str) == Some(activity)
            && r.snap
                .as_ref()
                .and_then(|s| s.get("kind"))
                .and_then(Value::as_str)
                == Some("tombstone")
    })
}

/// Whether the case's step results (`CaseStore::results`) show the knowledge came up empty: some knowledge step
/// answered `grounded: false` (`knowledge.answer`) or returned no `passages` (`knowledge.search`), and none was
/// grounded. A case that ran no knowledge step is not empty (nothing was looked for).
pub fn knowledge_empty_in(results: &Map<String, Value>) -> bool {
    let mut empty = false;
    for v in results.values() {
        match (
            v.get("grounded").and_then(Value::as_bool),
            v.get("passages").and_then(Value::as_array),
        ) {
            (Some(true), _) => return false,
            (Some(false), _) => empty = true,
            (None, Some(p)) if p.is_empty() => empty = true,
            (None, Some(_)) => return false,
            _ => {}
        }
    }
    empty
}

/// The item's `proposal_hint` (web `HandoverProposal`), by the plan's order: activity hidden -> `enable_activity`;
/// activity unknown -> `tenant_extension`; knowledge empty -> `knowledge_gap`; else `None`.
pub fn proposal_hint_of(job_key: &str, f: &HintFacts) -> Option<Value> {
    let base = format!("/admin/jobs/{job_key}");
    let activity = f
        .activity
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    match activity {
        Some(a) if f.activity_hidden => Some(json!({
            "kind": "enable_activity", "activity": a, "stage": null,
            "label": format!("Enable the {a} activity"),
            "detail": "The request belongs to an activity this job has hidden, so the agent could not handle it.",
            "href": format!("{base}/journey"),
        })),
        None => Some(json!({
            "kind": "tenant_extension", "activity": null, "stage": null,
            "label": "Add an activity for this request",
            "detail": "The request matched no activity of this job's journey.",
            "href": format!("{base}/journey"),
        })),
        Some(a) if f.knowledge_empty => Some(json!({
            "kind": "knowledge_gap", "activity": a, "stage": null,
            "label": "Add knowledge the agent was missing",
            "detail": "The agent had no passage to answer from for this request.",
            "href": format!("{base}/knowledge-kinds"),
        })),
        Some(_) => None,
    }
}

/// What [`queue`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Queued {
    Created(String),
    /// The case already has an open item (one per case).
    Already,
    /// The case's conversation does not exist (nothing to hand over).
    NoConversation,
}

/// Queue a handover item for `case_id` on the open (tenant + job scoped) transaction; the case goes to `handover`.
/// Out of hours at the database's `now()` (B4 T16a, F-14: [`closed_at_now`]) the item is queued `outside_hours`, its
/// SLA clocks starting at the scope's next open time. The extra reads are policy / calendar SELECTs: no lock.
#[allow(clippy::too_many_arguments)]
pub async fn queue(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    conversation_id: &str,
    reason: &str,
    trigger: &str,
    subject: &str,
    design: &CaseDesign,
) -> ApiResult<Queued> {
    let closed = closed_at_now(&mut *c, job, design).await?;
    queue_with(
        c,
        job,
        case_id,
        conversation_id,
        reason,
        trigger,
        subject,
        design,
        closed.as_ref().map(|(_, o)| o),
        None,
        &[],
        None,
    )
    .await
}

/// The scope of `design` at the database's `now()` (live hours in prod, the draft in test: plan R8): `None` =
/// open, else when it opens and what an agent-raised item is queued with ([`handover::create_with`]). For the
/// callers that queue without [`queue`] (the drain and reply gates).
pub async fn closed_at_now(
    c: &mut PgConnection,
    job: &JobRef,
    design: &CaseDesign,
) -> ApiResult<Option<(Closed, handover::OutsideHours)>> {
    let now = db_now(&mut *c).await?;
    Ok(closed_at(&mut *c, job, design, now).await?.map(|x| {
        let o = outside_of(&x, now);
        (x, o)
    }))
}

/// The database's `now()` (the transaction's start: the instant the item's `queued_at` records).
async fn db_now(c: &mut PgConnection) -> ApiResult<DateTime<Utc>> {
    Ok(sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *c)
        .await?)
}

/// The out-of-hours stamp of an item queued at `now` while the scope is `x`: clocks from the next open time, a
/// call-back offered in `callback` mode.
fn outside_of(x: &Closed, now: DateTime<Utc>) -> handover::OutsideHours {
    handover::OutsideHours {
        clock_starts_at: x.next_open,
        callback: (x.out_of_hours == OutOfHours::Callback)
            .then(|| json!({"offered_at": now, "after": x.next_open})),
    }
}

/// The out-of-hours fields of item `handover_id` (the `case.create` output, read by crate::runtime::reply).
fn out_of_hours_fields(
    handover_id: String,
    next_open: DateTime<Utc>,
    next_open_local: String,
    mode: OutOfHours,
) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("handover_id".into(), Value::String(handover_id));
    out.insert("outside_hours".into(), Value::Bool(true));
    out.insert("next_open".into(), json!(next_open));
    out.insert("next_open_local".into(), Value::String(next_open_local));
    out.insert(
        "out_of_hours".into(),
        Value::String(mode.as_str().to_owned()),
    );
    out
}

/// The out-of-hours note of case `case_id`'s open item when it was queued `outside_hours` (by any agent path), in
/// the shape `case.create` answers, for the case's reply (`Resolution::out_of_hours`, crate::runtime::reply);
/// `None` = no such item. The next open time is the item's `clock_starts_at` on the wall clock of the scope's hours
/// (`design`; UTC when none are set any more). Plain SELECTs: no row lock.
pub async fn out_of_hours_note(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    design: &CaseDesign,
) -> ApiResult<Option<OutOfHoursNote>> {
    let row: Option<(String, Option<DateTime<Utc>>, bool)> = sqlx::query_as(
        "SELECT id, clock_starts_at, callback IS NOT NULL FROM agent_handover_items \
          WHERE job_id = $1 AND case_id = $2 AND outside_hours \
            AND state NOT IN ('resolved', 'returned_to_agent')",
    )
    .bind(job.id)
    .bind(case_id)
    .fetch_optional(&mut *c)
    .await?;
    let Some((handover_id, Some(next_open), callback)) = row else {
        return Ok(None);
    };
    let (h, _) = policy::hours_of(&mut *c, job, &sla::design_of(design)).await?;
    let tz = h.as_ref().map_or("UTC", |h| h.timezone.as_str());
    let local = hours::local_time(&mut *c, tz, next_open).await?;
    let mode = if callback {
        OutOfHours::Callback
    } else {
        OutOfHours::Queue
    };
    Ok(Some(OutOfHoursNote {
        handover_id,
        next_open,
        next_open_local: hours::local_text(local, tz),
        out_of_hours: mode.as_str().to_owned(),
    }))
}

/// [`queue`], out of hours when `outside` is set ([`handover::create_with`]).
#[allow(clippy::too_many_arguments)]
async fn queue_with(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    conversation_id: &str,
    reason: &str,
    trigger: &str,
    subject: &str,
    design: &CaseDesign,
    outside: Option<&handover::OutsideHours>,
    input: Option<&Map<String, Value>>,
    collected: &[Value],
    hint: Option<Value>,
) -> ApiResult<Queued> {
    let Some(conv) = store::conversation(&mut *c, conversation_id).await? else {
        return Ok(Queued::NoConversation);
    };
    let item = handover::NewItem {
        job_id: job.id,
        case_id,
        conv: &conv,
        reason,
        trigger,
        subject,
        actor: "agent",
        design,
    };
    let extras = extras_for(&mut *c, job, case_id, reason, input, collected, hint).await?;
    match handover::create_with_extras(&mut *c, item, outside, &extras).await {
        Ok(id) => Ok(Queued::Created(id)),
        Err(ApiError::Conflict("already_handed_over", _)) => Ok(Queued::Already),
        Err(e) => Err(e),
    }
}

/// Whether the scope of `design` (live hours in prod, the draft in test: plan R8) is closed at `now`.
pub async fn closed_at(
    c: &mut PgConnection,
    job: &JobRef,
    design: &CaseDesign,
    now: DateTime<Utc>,
) -> ApiResult<Option<Closed>> {
    let (h, cal) = policy::hours_of(&mut *c, job, &sla::design_of(design)).await?;
    hours::closed_now(&mut *c, h.as_ref(), cal.as_ref(), now).await
}

/// `case.create{handover|complaint}` at the database's `now()` ([`create_at`]).
pub async fn create(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    conversation_id: &str,
    variant: &str,
    input: &Map<String, Value>,
    design: &CaseDesign,
) -> ApiResult<OpOutcome> {
    create_collected(
        c,
        job,
        case_id,
        conversation_id,
        variant,
        input,
        design,
        &[],
        None,
    )
    .await
}

/// [`create`] with what the case `collected` ([`collected_of`], read by the runner from the case's slots) and its
/// `proposal_hint` ([`proposal_hint_of`]).
#[allow(clippy::too_many_arguments)]
pub async fn create_collected(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    conversation_id: &str,
    variant: &str,
    input: &Map<String, Value>,
    design: &CaseDesign,
    collected: &[Value],
    hint: Option<Value>,
) -> ApiResult<OpOutcome> {
    let now = db_now(&mut *c).await?;
    create_at_hinted(
        c,
        job,
        case_id,
        conversation_id,
        variant,
        input,
        design,
        now,
        collected,
        hint,
    )
    .await
}

/// `case.create{handover|complaint}` at instant `now` ([`create_at_collected`], nothing collected).
#[allow(clippy::too_many_arguments)]
pub async fn create_at(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    conversation_id: &str,
    variant: &str,
    input: &Map<String, Value>,
    design: &CaseDesign,
    now: DateTime<Utc>,
) -> ApiResult<OpOutcome> {
    create_at_collected(
        c,
        job,
        case_id,
        conversation_id,
        variant,
        input,
        design,
        now,
        &[],
    )
    .await
}

/// [`create_at_hinted`] without a `proposal_hint`.
#[allow(clippy::too_many_arguments)]
pub async fn create_at_collected(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    conversation_id: &str,
    variant: &str,
    input: &Map<String, Value>,
    design: &CaseDesign,
    now: DateTime<Utc>,
    collected: &[Value],
) -> ApiResult<OpOutcome> {
    create_at_hinted(
        c,
        job,
        case_id,
        conversation_id,
        variant,
        input,
        design,
        now,
        collected,
        None,
    )
    .await
}

/// `case.create{handover|complaint}` at instant `now`. Inputs: `reason` (one of [`REASONS`]; default
/// `customer_asked`), `subject`, `priority` ([`priority_of`]; a step arg, not in the catalogue's `input_fields`). Out of hours (T16a) the item is queued `outside_hours` and the call answers `ok`
/// with outcome code [`OUTSIDE_HOURS`] and `{outside_hours, next_open, next_open_local, out_of_hours}`.
#[allow(clippy::too_many_arguments)]
pub async fn create_at_hinted(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    conversation_id: &str,
    variant: &str,
    input: &Map<String, Value>,
    design: &CaseDesign,
    now: DateTime<Utc>,
    collected: &[Value],
    hint: Option<Value>,
) -> ApiResult<OpOutcome> {
    let reason = input
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| REASONS.contains(r))
        .unwrap_or("customer_asked")
        .to_owned();
    let subject = input
        .get("subject")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| variant.to_owned());
    let trigger = format!("case.create{{{variant}}}");
    let closed = closed_at(&mut *c, job, design, now).await?;
    let outside = closed.as_ref().map(|x| outside_of(x, now));
    let q = queue_with(
        c,
        job,
        case_id,
        conversation_id,
        &reason,
        &trigger,
        &subject,
        design,
        outside.as_ref(),
        Some(input),
        collected,
        hint,
    )
    .await?;
    let mut out = Map::new();
    Ok(match q {
        Queued::Created(id) => match closed {
            Some(x) => {
                let out = out_of_hours_fields(id, x.next_open, x.next_open_local, x.out_of_hours);
                outcome(OpState::Ok, Some(OUTSIDE_HOURS), None, out)
            }
            None => {
                out.insert("handover_id".into(), Value::String(id));
                outcome(OpState::Ok, None, None, out)
            }
        },
        Queued::Already => {
            out.insert("already_handed_over".into(), Value::Bool(true));
            outcome(OpState::Ok, None, None, out)
        }
        Queued::NoConversation => failed(
            "conversation_not_found",
            "The case's conversation does not exist.",
        ),
    })
}
