//! What the interaction daemon says for one `request.resolved` (B4 T8): the consumer view of the orchestrator's
//! resolution, the customer-facing text (template replies ported from care-bus-agents@96c608c
//! `crates/carebus/src/interaction.rs` `compose_reply`) and how the reply used knowledge
//! ([`ReplyGrounding`], recorded in the reply's transaction, §0 R3).
//!
//! No reply prints a record id (owner ruling 2026-10-26): orders, payments, shipments, refunds and return or claim
//! numbers are said in plain words ("your Jute rug ordered on 12 October", from the `resolved` fact's `item_label`,
//! [`crate::runtime::resolve::Pick::label`]). That includes the case reference (`CS-n`, a report's or claim's
//! number): this file never printed `CS-n`, and the other references it used to print (return, claim, report,
//! refund, payment, tracking) are dropped, since the product shows none to a customer (the staff console does). The
//! guard test `no_composed_reply_prints_an_id` holds every variant to it.
//!
//! The text is a template over the case facts: no LLM call (§0 R8 would bind one to `Use::Qualify`; phrasing by a
//! model is a later choice, the facts stay the same).
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::domain::{CaseOutcome, OutOfHoursNote, Resolution};
use super::orchestrator::DELIVERY_ON_TRACK;
use super::trace::TraceInput;
use crate::handover::hours::{OutOfHours, out_of_hours_text};
use crate::knowledge::analytics::ReplyGrounding;

/// What the customer reads for a resolved case of no known type when no write step ran (live CS-53: never a
/// "done" or an email it did not do).
pub const CHECKED: &str =
    "I've checked this for you. If anything else is needed, a colleague will follow up.";

/// What the customer reads for a refund-status case whose payment read found no payment.
pub const NO_PAYMENT: &str = "I couldn't find a payment for this order. If anything else is needed, a colleague will follow up.";

/// What the customer reads when the knowledge answer cites no source (an ungrounded answer escalates before a
/// reply in the runtime, spec 12 A9; this covers a resolution that still carries one).
pub const FAQ_FALLBACK: &str = "I couldn't find an answer. A colleague will follow up here.";

/// What the customer reads for a `not_eligible` case with no reason: a gate other than the platform's `rule.evaluate`
/// (a connector's own check) carries none on the resolution, since its free text is untrusted (orchestrator).
pub const NOT_ELIGIBLE: &str = "I can't do that for this order: it doesn't meet the conditions for this request. A colleague can explain the details.";

/// The interaction daemon's own fields of the `request.resolved` payload, read next to the orchestrator's
/// [`Resolution`] (T7, `runtime::domain`) from the same JSON. Both are optional: an orchestrator that does not send
/// them gives a reply with no knowledge use and a trace without the turn's steps.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplyExtras {
    /// How the turn used knowledge, when it ran a knowledge search; else read from `facts.faq`.
    #[serde(default)]
    pub knowledge: Option<KnowledgeUse>,
    /// What the turn did, for the test-env [`super::trace::ReplyTrace`]; ignored in prod.
    #[serde(default)]
    pub trace: Option<TraceInput>,
}

/// One knowledge search of the turn.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeUse {
    #[serde(default)]
    pub query: Option<String>,
    /// The chunk ids the answer cites.
    #[serde(default)]
    pub citations: Vec<String>,
    /// The cited sources (`AnswerSource` JSON with `layer`, `pack`, `provenance`: D109).
    #[serde(default)]
    pub sources: Vec<Value>,
    #[serde(default)]
    pub dont_know: bool,
}

/// A fact field as text (`""` when missing).
fn fact(f: &Map<String, Value>, obj: &str, k: &str) -> String {
    f.get(obj)
        .and_then(|o| o.get(k))
        .map(|v| match v.as_str() {
            Some(s) => s.to_string(),
            None => v.to_string(),
        })
        .unwrap_or_default()
}

/// The human label of the order the case is about ("Jute rug ordered on 12 October"), from the orchestrator's
/// `resolved` fact.
///
/// The name always belongs to the order the reply talks about ([`case_order_id`]): `item_label` is used only when
/// it was written for that order (`item_label_order`, set by the orchestrator beside it; a label with no recorded
/// order cannot be disproved and is kept); a label written for another order is never printed, the case order's
/// own `item_names` entry is.
fn label(f: &Map<String, Value>) -> Option<String> {
    let res = f.get("resolved")?;
    let text = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
    };
    let label = text(res.get("item_label"));
    let case = case_order_id(f);
    let for_other_order = match (&case, text(res.get("item_label_order"))) {
        (Some(case), Some(label_order)) => *case != label_order,
        _ => false,
    };
    if for_other_order {
        let case = case?;
        return text(res.get("item_names").and_then(|n| n.get(case.as_str())));
    }
    label
}

/// What a template calls the order: the label, else the order step's item, else "order".
fn item_of(f: &Map<String, Value>) -> String {
    label(f)
        .or_else(|| {
            let i = fact(f, "order", "item");
            (!i.is_empty()).then_some(i)
        })
        .unwrap_or_else(|| "order".to_owned())
}

/// The grounded answer of `facts.faq` (the knowledge step's `{answer, citations, grounded}`, D109): only an answer
/// citing ≥ 1 source, and not `grounded: false`, counts.
fn faq_answer(f: &Map<String, Value>) -> Option<String> {
    f.get("faq")
        .filter(|x| x["citations"].as_array().is_some_and(|c| !c.is_empty()))
        .filter(|x| x.get("grounded") != Some(&Value::Bool(false)))
        .and_then(|x| x["answer"].as_str())
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

/// The final answer of an assistant case (`facts.assistant`, `{answer}` recorded by the orchestrator's last round),
/// when it is not blank.
fn assistant_answer(f: &Map<String, Value>) -> Option<String> {
    f.get("assistant")
        .and_then(|x| x["answer"].as_str())
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

/// The out-of-hours reply of a case whose handover was queued while the scope was closed (B4 T16a): the
/// `case.create` step output of `agent_cases` (`handover_id`, `outside_hours: true`, `next_open_local`,
/// `out_of_hours`).
fn out_of_hours(f: &Map<String, Value>) -> Option<String> {
    let step = f.values().find(|v| {
        v.get("outside_hours") == Some(&Value::Bool(true))
            && v.get("handover_id").is_some_and(Value::is_string)
    })?;
    let when = step.get("next_open_local").and_then(Value::as_str)?;
    let mode = mode_of(step.get("out_of_hours").and_then(Value::as_str));
    Some(out_of_hours_text(mode, when))
}

/// The out-of-hours mode named `m` (`callback`, else `queue`).
fn mode_of(m: Option<&str>) -> OutOfHours {
    match m {
        Some("callback") => OutOfHours::Callback,
        _ => OutOfHours::Queue,
    }
}

/// What a failed case's reply says after its cause when its J21 item was queued out of hours (B4 T16a): when a
/// colleague follows up, not "by email today" (no channel is named: email is deferred).
fn closed_follow_up(n: &OutOfHoursNote) -> String {
    let when = &n.next_open_local;
    match mode_of(Some(n.out_of_hours.as_str())) {
        OutOfHours::Callback => {
            format!("Our team is closed right now; a colleague will call you back after {when}.")
        }
        OutOfHours::Queue => {
            format!("Our team is closed right now; a colleague will reply after {when}.")
        }
    }
}

/// What the customer reads when the return for the order was not accepted: no id, no reason invented.
const RETURN_REJECTED: &str =
    "We couldn't accept that return. A colleague can explain why and what happens next.";

/// The records of step `step` when it is a LIST: a `rows` array, else the columnar arrays zipped
/// ([`crate::runtime::conditions::rows_of`]). Empty when the output is no list.
fn step_rows(f: &Map<String, Value>, step: &str) -> Vec<Map<String, Value>> {
    let Some(d) = f.get(step) else {
        return Vec::new();
    };
    let rows = match d.get("rows") {
        Some(r @ Value::Array(_)) => Some(r.clone()),
        _ => crate::runtime::conditions::rows_of(d),
    };
    rows.as_ref()
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|r| r.as_object().cloned()).collect())
        .unwrap_or_default()
}

/// The order the case is about, server-side: the `resolved` fact's `order_id` or `focus_order`, else the id the
/// `order` step read, else the one order the `payments` step names. `None` when the facts do not say.
fn case_order_id(f: &Map<String, Value>) -> Option<String> {
    let res = f.get("resolved");
    let named = |v: Option<&Value>, k: &str| {
        v.and_then(|o| o.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    named(res, "order_id")
        .or_else(|| named(res, "focus_order"))
        .or_else(|| named(f.get("order"), "order_id"))
        .or_else(|| {
            let mut ids: Vec<String> = step_rows(f, "payments")
                .iter()
                .filter_map(|r| row_text(r, "order_id"))
                .collect();
            ids.dedup();
            (ids.len() == 1).then(|| ids.remove(0))
        })
        .or_else(|| named(f.get("payments"), "order_id"))
}

/// The return of the case's order: the `resolved` fact's `return` (the orchestrator picks it from the customer
/// context, `return.list`, server-side), else a `returns` step's newest row for that order (rows are in list
/// order, the last is the newest). `None` when neither holds a return of the case's order.
fn case_return(f: &Map<String, Value>) -> Option<Map<String, Value>> {
    if let Some(r) = f
        .get("resolved")
        .and_then(|r| r.get("return"))
        .and_then(Value::as_object)
    {
        return Some(r.clone());
    }
    let order = case_order_id(f)?;
    step_rows(f, "returns")
        .into_iter()
        .rev()
        .find(|r| row_text(r, "order_id").as_deref() == Some(order.as_str()))
}

/// The reply about the case's return, in plain words, no id: open (it is on its way back), pending_provider (it
/// arrived), rejected. `None` for `refunded` (the payment wording says it) and for no return at all.
fn return_reply(f: &Map<String, Value>) -> Option<String> {
    let r = case_return(f)?;
    let status = row_text(&r, "status")?.to_lowercase();
    let item = case_order_id(f)
        .and_then(|id| {
            f.get("resolved")?
                .get("item_names")?
                .get(id)?
                .as_str()
                .map(str::to_owned)
        })
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "your order".to_owned());
    let item = if item == "your order" {
        item
    } else {
        format!("your {item}")
    };
    let from = row_text(&r, "created_at")
        .and_then(|d| plain_date(f, &d))
        .map(|d| format!(" from {d}"))
        .unwrap_or_default();
    match status.as_str() {
        "open" => Some(format!(
            "We have your return for {item}{from}; your refund is issued as soon as it arrives back with us."
        )),
        "pending_provider" => Some(format!(
            "Your return for {item}{from} has arrived back with us; the refund is on its way."
        )),
        "rejected" => Some(RETURN_REJECTED.into()),
        _ => None,
    }
}

/// The reply of a resolved refund-status case (`payment.list` as step `payments`: `payment_id`, `status`,
/// `direction`, and the order's `payments` when the connector sends them): a refund found, else the payment and,
/// when the whole list was read, that it has no refund yet. Never "done": the case only read.
fn refund_status(f: &Map<String, Value>) -> String {
    if let Some(text) = return_reply(f) {
        return text;
    }
    let Some(p) = f.get("payments") else {
        // a refunded return with no payment read still tells the customer the money is back
        return match case_return(f)
            .and_then(|r| row_text(&r, "status"))
            .as_deref()
        {
            Some("refunded") => format!(
                "The refund{} has been paid back to you.",
                label(f)
                    .map(|l| format!(" for your {l}"))
                    .unwrap_or_default()
            ),
            _ => NO_PAYMENT.into(),
        };
    };
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let line = |v: &Value| text(v, "status").map(|st| st.replace('_', " "));
    let list = p.get("payments").and_then(Value::as_array);
    let of = label(f)
        .map(|l| format!(" for your {l}"))
        .unwrap_or_default();
    let refund = std::iter::once(p)
        .chain(list.into_iter().flatten())
        .filter(|&v| v.get("direction").and_then(Value::as_str) == Some("refund"))
        .find_map(&line);
    if let Some(status) = refund {
        if status == "refunded" {
            return format!("The refund{of} has been paid back to you.");
        }
        return format!("The refund{of} is {status}.");
    }
    match line(p) {
        Some(status) if list.is_some() => {
            format!("The payment{of} is {status}. I found no refund for this order yet.")
        }
        Some(status) => format!("The payment{of} is {status}."),
        None => NO_PAYMENT.into(),
    }
}

/// The reply of a resolved `delivery_problem` (a shipment on track, acceptance A4): where the parcel is, the end of
/// its expected window when `delivery.read` answered one (`expected_window`, or its `until`), and the reported case.
fn delivery_problem(f: &Map<String, Value>) -> String {
    let status = fact(f, "delivery", "status").replace('_', " ");
    let by = expected_by(f);
    let logged = if fact(f, "report", "case_id").is_empty() {
        ""
    } else {
        " I've logged your report."
    };
    let subject = label(f).map_or_else(|| "Your parcel".to_owned(), |l| format!("Your {l}"));
    format!("{subject} is {status}{by}.{logged}")
}

/// `, expected by <end>` of the `delivery.read` output's `expected_window` (step `delivery`), else nothing.
fn expected_by(f: &Map<String, Value>) -> String {
    f.get("delivery")
        .and_then(|d| d.get("expected_window"))
        // demo_retail@5 answers the window's end as one RFC3339 string; a `{from, until}` object is read by `until`
        .and_then(|w| {
            w.as_str()
                .or_else(|| w.get("until").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .and_then(|u| plain_date(f, u))
        .map(|u| format!(", expected by {u}"))
        .unwrap_or_default()
}

/// A date or RFC3339 timestamp in plain words ("21 October"); `None` when its first 10 characters are no date, so
/// a raw timestamp is left out rather than printed.
///
/// An instant (RFC3339, with `T` and `Z` or an offset) is read on the customer's day: the facts' `local_days` map
/// (instant text -> `YYYY-MM-DD`, written by the orchestrator in the scope's business-hours timezone, converted by
/// Postgres) names it. With no zone known (no map entry) the date part is read as written; a date-only string is
/// always read as written.
fn plain_date(f: &Map<String, Value>, s: &str) -> Option<String> {
    let s = s.trim();
    let local = f
        .get("local_days")
        .and_then(|m| m.get(s))
        .and_then(Value::as_str)
        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let d = match local {
        Some(d) => d,
        None => chrono::NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?,
    };
    Some(crate::runtime::resolve::day_month(d))
}

/// A non-blank string field `k` of step output `step`.
fn field<'a>(f: &'a Map<String, Value>, step: &str, k: &str) -> Option<&'a str> {
    f.get(step)
        .and_then(|o| o.get(k))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// What the customer reads for a parcel-tracking case whose shipment read found nothing (an empty answer, e.g.
/// demo_retail@5's `items: []`): never "unavailable", never a request for a tracking number.
pub const NO_TRACKING: &str =
    "I can't see a shipment for that order yet. If you'd like, I can pass this to a colleague.";

/// What the customer reads when none of their orders fits the request (the case ends `no_matching_order`): plain
/// words, no lookup key asked.
pub const NO_MATCHING_ORDER: &str = "I can't see an order of yours that matches. Tell me what you ordered, for example \"the rug\", or ask for a colleague.";

/// What the customer reads when none of their records of a kind fits the request (the case ends
/// `no_matching_record`, the reason names the kind): "I can't find a policy on your account." Only a noun of the
/// record registry is printed; anything else reads "record".
pub fn no_matching_record(noun: Option<&str>) -> String {
    let noun = noun
        .map(str::trim)
        .filter(|n| {
            crate::runtime::resolve::RECORD_KINDS
                .iter()
                .any(|k| k.noun == *n)
        })
        .unwrap_or("record");
    let article = if noun.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("I can't find {article} {noun} on your account.")
}

/// What the customer reads when a choice question lost its text (the question itself is composed from the
/// candidates, [`crate::runtime::resolve::choice_question`]).
pub const CHOICE_FALLBACK: &str =
    "Which of your orders do you mean? You can say what you ordered, for example \"the rug\".";

/// The reply of a resolved `track_parcel` case (live CS-58; `delivery.read{by_tracking_ref}` as step `delivery`:
/// `status`, `carrier`, `expected_window`, and `tracking_ref` when the system sends it): the shipment's status,
/// carrier, expected window end and tracking reference; else an order's status (step `order`, when a design reads
/// one); else [`NO_TRACKING`]. A blank field is left out, never printed empty.
fn track_parcel(f: &Map<String, Value>) -> String {
    if let Some(status) = field(f, "delivery", "status") {
        let with = field(f, "delivery", "carrier")
            .map(|c| format!(" with {c}"))
            .unwrap_or_default();
        let subject = label(f).map_or_else(|| "Your parcel".to_owned(), |l| format!("Your {l}"));
        return format!(
            "{subject} is {}{with}{}.",
            status.replace('_', " "),
            expected_by(f)
        );
    }
    match field(f, "order", "status") {
        Some(status) => format!(
            "{} is {}.",
            label(f).map_or_else(|| "Your order".to_owned(), |l| format!("Your {l}")),
            status.replace('_', " ")
        ),
        None => track_list(f).unwrap_or_else(|| NO_TRACKING.into()),
    }
}

/// The records of the `delivery` step when it is a LIST (`delivery.list`): a `rows` array, else the columnar
/// arrays zipped ([`crate::runtime::conditions::rows_of`]). `None` when the output is no list.
fn delivery_rows(f: &Map<String, Value>) -> Option<Vec<Map<String, Value>>> {
    let d = f.get("delivery")?;
    let rows = match d.get("rows") {
        Some(r @ Value::Array(_)) => r.clone(),
        _ => crate::runtime::conditions::rows_of(d)?,
    };
    Some(
        rows.as_array()?
            .iter()
            .filter_map(|r| r.as_object().cloned())
            .collect(),
    )
}

/// A non-blank text of a row; `expected_window` may be a string or `{until}`.
fn row_text(r: &Map<String, Value>, k: &str) -> Option<String> {
    r.get(k)
        .and_then(|w| {
            w.as_str()
                .or_else(|| w.get("until").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The product of a delivery row: the item name of its order (`resolved.item_names`, server-side), else `fallback`
/// (callers pass "Your parcel", or "One parcel" / "Another parcel" when several sentences must stay apart). Never an id.
fn row_subject(f: &Map<String, Value>, r: &Map<String, Value>, fallback: &str) -> String {
    r.get("order_id")
        .and_then(Value::as_str)
        .and_then(|id| f.get("resolved")?.get("item_names")?.get(id)?.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| fallback.to_owned(), |n| format!("Your {n}"))
}

/// The reply of a `track_parcel` case whose `delivery` step LISTED the customer's shipments: the parcels still on
/// their way ([`DELIVERY_ON_TRACK`]), newest first, at most 3, one sentence each; else the most recent delivered
/// one; `None` when the list is empty or nothing is readable (the caller says [`NO_TRACKING`]).
fn track_list(f: &Map<String, Value>) -> Option<String> {
    let rows: Vec<_> = delivery_rows(f)?
        .into_iter()
        .rev()
        .filter(|r| row_text(r, "status").is_some())
        .collect();
    // the order the customer asked about (server-side, `resolved.focus_order`): answer about its parcels only
    let focus = f
        .get("resolved")
        .and_then(|r| r.get("focus_order"))
        .and_then(Value::as_str);
    let rows: Vec<_> = match focus {
        Some(o)
            if rows
                .iter()
                .any(|r| row_text(r, "order_id").as_deref() == Some(o)) =>
        {
            rows.into_iter()
                .filter(|r| row_text(r, "order_id").as_deref() == Some(o))
                .collect()
        }
        _ => rows,
    };
    let on_way: Vec<_> = rows
        .iter()
        .filter(|r| row_text(r, "status").is_some_and(|s| DELIVERY_ON_TRACK.contains(&s.as_str())))
        .take(3)
        .collect();
    if !on_way.is_empty() {
        let several = on_way.len() > 1;
        let mut unnamed = 0usize;
        let mut line = |r: &Map<String, Value>| {
            let status = row_text(r, "status").unwrap_or_default().replace('_', " ");
            let with = row_text(r, "carrier")
                .map(|c| format!(" with {c}"))
                .unwrap_or_default();
            let by = row_text(r, "expected_window")
                .and_then(|u| plain_date(f, &u))
                .map(|u| format!(", expected by {u}"))
                .unwrap_or_default();
            // unnamed parcels stay apart without an id: "One parcel ... Another parcel ..."
            let fallback = if !several {
                "Your parcel"
            } else if unnamed == 0 {
                "One parcel"
            } else {
                "Another parcel"
            };
            let subject = row_subject(f, r, fallback);
            if subject == fallback {
                unnamed += 1;
            }
            format!("{subject} is {status}{with}{by}.")
        };
        return Some(on_way.iter().map(|r| line(r)).collect::<Vec<_>>().join(" "));
    }
    let r = rows
        .iter()
        .find(|r| row_text(r, "status").as_deref() == Some("delivered"))?;
    let on = row_text(r, "delivered_at")
        .and_then(|d| plain_date(f, &d))
        .map(|d| format!(" on {d}"))
        .unwrap_or_default();
    Some(format!(
        "{} was delivered{on}.",
        row_subject(f, r, "Your parcel")
    ))
}

/// The customer-safe cause of a `failed` case, by its machine code (the part before any `:`, as in
/// `ineligible:<id>`). Every phrase is plain English with no code, operation key or underscore; an unknown or
/// missing code reads the generic cause.
pub fn failure_cause(code: Option<&str>) -> &'static str {
    match code.map(|c| c.split(':').next().unwrap_or(c)) {
        Some("timeout" | "deadline_exceeded") => "one of our systems didn't answer in time",
        Some(
            "upstream_unavailable"
            | "not_offered"
            | "connector_auth_failed"
            | "connector_misconfigured"
            | "rate_limited"
            | "gate_unavailable"
            | "llm_unavailable"
            | "assistant_unbound"
            | "not_entitled",
        ) => "a system we need isn't available right now",
        Some("not_found") => "I couldn't find the details I needed",
        Some("unknown_outcome") => "I couldn't confirm whether the last step went through",
        Some(
            "success_criteria_unmet"
            | "postcondition_failed"
            | "precondition_unmet"
            | "invalid_state"
            | "conflict"
            | "duplicate"
            | "locked",
        ) => "the result didn't pass our checks",
        Some("invalid_input" | "invalid_target" | "ambiguous") => {
            "some details of the request didn't match what we have"
        }
        Some("case_cap_reached" | "attempts_exceeded" | "assistant_budget_exhausted") => {
            "it needs more steps than I can take on my own"
        }
        _ => "something went wrong on our side",
    }
}

/// The first non-blank string or number field `k` among the step outputs `steps` (the library's procedures name
/// their steps differently per tenant design, so a reply looks under each usual name).
fn pick(f: &Map<String, Value>, steps: &[&str], k: &str) -> Option<String> {
    steps.iter().find_map(|s| {
        match f.get(*s)?.get(k)? {
            Value::String(v) => Some(v.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
        .filter(|v| !v.is_empty())
    })
}

/// What a `refund_request` on an order that is neither cancelled nor returned is told (the library's `refundable`
/// precondition failed).
const REFUND_NEEDS_RETURN: &str =
    "A refund can be issued once the order is cancelled or its return is complete.";

/// The order the `order` step read has a status other than `cancelled` / `returned` (no status read: not decided).
fn order_not_refundable(f: &Map<String, Value>) -> bool {
    f.get("order")
        .and_then(|o| o.get("status"))
        .and_then(Value::as_str)
        .is_some_and(|s| !matches!(s.trim(), "cancelled" | "returned"))
}

/// The plain customer sentence of a known `not_eligible` code (the rule's `ineligible:<rule id>` and the runner's
/// own codes), `None` for any other. Never prints the code or the rule id.
pub fn not_eligible_by_code(code: Option<&str>) -> Option<&'static str> {
    Some(match code? {
        "ineligible:RET-01" => {
            "This item can't be returned: the return window has passed or the item isn't covered by returns."
        }
        "ineligible:WAR-01" => {
            "This item is out of warranty, so I can't register a warranty claim."
        }
        "ineligible:PAY-04" => {
            "This charge is still pending, not settled yet. It usually drops off by itself."
        }
        "ineligible:DIS-02" => {
            "I can't do that automatically; a colleague will take care of it personally."
        }
        "ineligible:ARR-01" => {
            "Your payment arrangement doesn't allow this change. A colleague can explain the options."
        }
        "not_cancellable" => {
            "This order can no longer be cancelled; once it arrives you can return it."
        }
        _ => return None,
    })
}

/// The reply of a `not_eligible` case with no authored reason (the failed gate was not the platform's
/// `rule.evaluate`, or a step's precondition was unmet): by request type, the library's three precondition
/// outcomes. The resolution carries no precondition id (the runner keeps it in the call detail), so the request
/// type names the check: `cancel_order` (cancellable), `refund_request` (not already refunded), `redirect` (parcel
/// still on track). Any other type reads [`NOT_ELIGIBLE`]. Never a code or a record value (K50).
fn not_eligible(r: &Resolution) -> String {
    // an authored reason is shown; one that carries an id is not (the owner's no-ids rule outranks the pass-through)
    if let Some(reason) = r
        .reason
        .as_ref()
        .filter(|reason| !super::id_guard::has_id_shape(reason))
    {
        return reason.clone();
    }
    if let Some(sentence) = not_eligible_by_code(r.code.as_deref()) {
        return sentence.into();
    }
    match r.request_type.as_str() {
        "cancel_order" => {
            "This order can no longer be cancelled; once it arrives you can return it.".into()
        }
        "refund_request" if return_reply(&r.facts).is_some() => {
            return_reply(&r.facts).unwrap_or_default()
        }
        "refund_request" if order_not_refundable(&r.facts) => REFUND_NEEDS_RETURN.into(),
        "refund_request" if r.facts.contains_key("payments") => refund_status(&r.facts),
        "redirect" => "This parcel can no longer be redirected.".into(),
        _ => NOT_ELIGIBLE.into(),
    }
}

/// The reply of a resolved `cancel_order` case (`order.cancel` answered ok). Never names the record id (KYC
/// ruling 2026-10-26: replies speak in plain words, an id is never shown to or asked of the customer).
fn cancelled(f: &Map<String, Value>) -> String {
    // The order in plain words: the case order's label ("Jute rug ordered on 12 October", or a bare item name),
    // else its item name, else nothing. Never the id.
    if let Some(l) = label(f) {
        return if l.contains(" ordered on ") {
            format!("Your {l} is cancelled.")
        } else {
            format!("Your {l} order is cancelled.")
        };
    }
    let name = case_order_id(f).and_then(|id| {
        f.get("resolved")?
            .get("item_names")?
            .get(id.as_str())?
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
    });
    match name {
        Some(n) => format!("Your {n} order is cancelled."),
        None => "Your order is cancelled.".into(),
    }
}

/// The reply of a resolved `refund_request` case (`payment.refund` answered ok).
fn refund_issued(f: &Map<String, Value>) -> String {
    let steps = ["refund", "payment_refund"];
    match pick(f, &steps, "amount") {
        Some(a) => match pick(f, &steps, "currency") {
            Some(c) => format!("Refund of {a} {c} is issued."),
            None => format!("Refund of {a} is issued."),
        },
        None => "Your refund is issued.".into(),
    }
}

/// The reply of a resolved `redirect` case (`delivery.redirect` answered ok).
fn redirected(f: &Map<String, Value>) -> String {
    match pick(f, &["redirect", "delivery"], "address") {
        Some(a) => format!("Your parcel will now go to {a}."),
        None => "Your parcel will now go to the new address.".into(),
    }
}

/// The template reply of `r`.
///
/// Customer-safety rule: a `failed` reply never prints `r.reason` or `r.code`. The orchestrator's failure reasons
/// (`failure_reason`, the case deadline) name the operation ("order.read is unavailable") — safe for staff (the J21
/// trigger, K50) but internal to the customer — so the reply shows only [`failure_cause`] of the code. A
/// `not_eligible` reply passes the resolution's `reason` through: the orchestrator keeps it only for the platform's
/// `rule.evaluate` (the rule's authored, customer-facing text), else [`NOT_ELIGIBLE`]. The templates are English only
/// (no per-language mechanism exists yet).
pub fn compose(r: &Resolution) -> String {
    let f = &r.facts;
    let s = |obj: &str, k: &str| fact(f, obj, k);
    match r.outcome {
        CaseOutcome::Failed => {
            let cause = failure_cause(r.code.as_deref());
            match &r.out_of_hours {
                Some(n) => format!(
                    "I couldn't finish this automatically: {cause}. {}",
                    closed_follow_up(n)
                ),
                None => format!(
                    "I couldn't finish this automatically: {cause}. A colleague will follow up here today."
                ),
            }
        }
        CaseOutcome::NotEligible if r.code.as_deref() == Some("no_matching_order") => {
            NO_MATCHING_ORDER.into()
        }
        CaseOutcome::NotEligible if r.code.as_deref() == Some("no_matching_record") => {
            no_matching_record(r.reason.as_deref())
        }
        CaseOutcome::NotEligible => not_eligible(r),
        // the orchestrator's note of the case's item (any agent path), else a `case.create` step output (a probe)
        CaseOutcome::Handover => r
            .out_of_hours
            .as_ref()
            .map(|n| out_of_hours_text(mode_of(Some(n.out_of_hours.as_str())), &n.next_open_local))
            .or_else(|| out_of_hours(f))
            .unwrap_or_else(|| {
                if r.code.as_deref() == Some("step_up_required") {
                    "A colleague needs to confirm one more thing with you, so I've passed this on."
                        .into()
                } else {
                    "I've passed this to a colleague, who will take it from here.".into()
                }
            }),
        // the customer is known from the start (KYC): never ask them to prove who they are
        CaseOutcome::IdentityRequired => {
            "I've passed this to a colleague, who will take it from here.".into()
        }
        // the case parked to ask which order the customer means: the question names the candidates in plain words
        CaseOutcome::ConfirmationRequired
            if matches!(
                r.code.as_deref(),
                Some("choice_required" | "input_required")
            ) =>
        {
            r.reason
                .clone()
                .filter(|q| !q.trim().is_empty())
                .unwrap_or_else(|| CHOICE_FALLBACK.into())
        }
        CaseOutcome::ConfirmationRequired => {
            "Shall I go ahead? Please reply yes to confirm.".into()
        }
        CaseOutcome::CancelledOffboard => {
            "This service is no longer available here. Please contact us another way.".into()
        }
        // D109: a knowledge-answered turn (whatever the space's request type) replies with its cited answer
        CaseOutcome::Resolved if f.contains_key("faq") => {
            faq_answer(f).unwrap_or_else(|| FAQ_FALLBACK.into())
        }
        // an assistant case (any caller, A4) replies with the model's own final answer
        CaseOutcome::Resolved if assistant_answer(f).is_some() => {
            assistant_answer(f).unwrap_or_default()
        }
        CaseOutcome::Resolved => match r.request_type.as_str() {
            "order_status" if !s("order", "status").is_empty() => format!(
                "{} is {}.",
                label(f).map_or_else(|| "Your order".to_owned(), |l| format!("Your {l}")),
                s("order", "status").replace('_', " ")
            ),
            "delivery_status" if s("shipment", "status") == "delivered" => format!(
                "Your {} was delivered on {}.",
                item_of(f),
                s("shipment", "delivered_at")
            ),
            "delivery_status" => format!(
                "Your {} is with {}. Last update: {}. Expected: {}.",
                item_of(f),
                s("shipment", "carrier"),
                s("shipment", "last_event"),
                s("shipment", "eta")
            ),
            "delivery_problem" if !s("delivery", "status").is_empty() => delivery_problem(f),
            "track_parcel" => track_parcel(f),
            "return" => format!(
                "Return approved for your {}. We'll send the prepaid {} label here as soon as it's ready.",
                item_of(f),
                s("rma", "carrier")
            ),
            "warranty_claim" => format!(
                "Warranty claim registered for your {}. {}",
                item_of(f),
                s("claim", "next_step")
            ),
            "payment_error" => format!(
                "That was a duplicate charge. A refund of €{} is issued and arrives within {}.",
                s("refund", "amount"),
                s("refund", "eta")
            ),
            "faq" => faq_answer(f).unwrap_or_else(|| FAQ_FALLBACK.into()),
            "refunds" | "refund_status" => refund_status(f),
            // the library's write procedures: claimed only after a write step of the case answered ok (CS-53)
            "cancel_order" if r.wrote => cancelled(f),
            "refund_request" if r.wrote => refund_issued(f),
            "redirect" if r.wrote => redirected(f),
            "delivery_problem" if r.wrote => {
                "I've reported the problem with your delivery and passed it to a colleague.".into()
            }
            "payment_errors" | "pending_charge" if r.wrote => {
                "I've opened a dispute for this charge. A colleague will follow up.".into()
            }
            "complaint_intake" if r.wrote => {
                "Your complaint is registered. A colleague will contact you.".into()
            }
            "data_rights" if r.wrote => {
                "I've passed your request to our privacy team. They will contact you.".into()
            }
            // live CS-53: "done" and "a confirmation will follow" only after a write step of the case answered ok
            _ if !r.wrote => CHECKED.into(),
            "" => "Your request is done. A confirmation will follow here.".into(),
            other => format!(
                "Your {} request is done. A confirmation will follow here.",
                other.replace('_', " ")
            ),
        },
    }
}

/// The chunk id of one citation: a string, or an object's `chunk_id` / `id`.
fn citation_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o
            .get("chunk_id")
            .or_else(|| o.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// How the reply of `r` used knowledge: the orchestrator's [`KnowledgeUse`], else the `facts.faq` answer of a
/// knowledge turn, else no knowledge ([`ReplyGrounding::default`]).
pub fn grounding(r: &Resolution, knowledge: Option<&KnowledgeUse>) -> ReplyGrounding {
    if let Some(k) = knowledge {
        return ReplyGrounding {
            knowledge_query: k.query.clone(),
            used_knowledge: true,
            citations: k.citations.clone(),
            sources: k.sources.clone(),
            dont_know: k.dont_know,
        };
    }
    let Some(faq) = r.facts.get("faq").and_then(Value::as_object) else {
        return ReplyGrounding::default();
    };
    let cited = faq.get("citations").and_then(Value::as_array);
    let citations: Vec<String> = cited
        .map(|a| a.iter().filter_map(citation_id).collect())
        .unwrap_or_default();
    // D109: the citations are `AnswerSource`s themselves; an older answer listed its `sources` apart
    let sources = faq
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| {
            cited
                .map(|a| a.iter().filter(|c| c.is_object()).cloned().collect())
                .unwrap_or_default()
        });
    ReplyGrounding {
        knowledge_query: faq.get("query").and_then(Value::as_str).map(str::to_string),
        used_knowledge: true,
        dont_know: citations.is_empty() || faq.get("grounded") == Some(&Value::Bool(false)),
        citations,
        sources,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn res(mut v: Value) -> Resolution {
        v["conversation_id"] = json!("conv-1");
        v["context_space"] = json!("retail:order-support:returns");
        serde_json::from_value(v).unwrap()
    }

    fn ne(request_type: &str, extra: Value) -> Resolution {
        let mut v = json!({"case_id": "CS-9", "request_type": request_type, "outcome": "not_eligible",
                           "code": "precondition_unmet"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        res(v)
    }

    /// Acceptance data: `delivery.list` answers COLUMNAR; the reply names the parcel still on its way by product.
    #[test]
    fn track_parcel_reads_a_delivery_list() {
        let at = |facts: Value| {
            compose(&res(
                json!({"case_id": "CS-9", "request_type": "track_parcel", "outcome": "resolved", "facts": facts}),
            ))
        };
        let names =
            json!({"item_names": {"RET-01": "Desk lamp", "RET-02": "Stoneware mug set (4)"}});
        let list = json!({
            "status": ["delivered", "in_transit"], "order_id": ["RET-01", "RET-02"],
            "carrier": ["DHL", "PostNL"], "tracking_ref": ["SHP-1", "SHP-2"],
            "expected_window": ["2026-10-10T12:00Z", "2026-10-21T15:00Z"]
        });
        let text = at(json!({"resolved": names, "delivery": list}));
        assert_eq!(
            text,
            "Your Stoneware mug set (4) is in transit with PostNL, expected by 21 October."
        );
        // several in flight: newest first, one sentence each
        let two = json!({"status": ["in_transit", "ready"], "order_id": ["RET-01", "RET-02"], "carrier": ["DHL", "PostNL"]});
        assert_eq!(
            at(json!({"resolved": names, "delivery": two})),
            "Your Stoneware mug set (4) is ready with PostNL. Your Desk lamp is in transit with DHL."
        );
        // all delivered: the most recent
        let done = json!({"status": ["delivered", "delivered"], "order_id": ["RET-01", "RET-02"]});
        assert_eq!(
            at(json!({"resolved": names, "delivery": done})),
            "Your Stoneware mug set (4) was delivered."
        );
        // delivered date in plain words; an unreadable date is left out, never printed raw
        let dated = json!({"status": ["delivered"], "order_id": ["RET-02"], "delivered_at": ["2026-10-07T09:30:00Z"]});
        assert_eq!(
            at(json!({"resolved": names, "delivery": dated})),
            "Your Stoneware mug set (4) was delivered on 7 October."
        );
        let junk =
            json!({"status": ["in_transit"], "order_id": ["RET-02"], "expected_window": ["soon"]});
        assert_eq!(
            at(json!({"resolved": names, "delivery": junk})),
            "Your Stoneware mug set (4) is in transit."
        );
        // no name known for the row
        assert_eq!(at(json!({"delivery": done})), "Your parcel was delivered.");
        // a rows array works too
        let rows =
            json!({"rows": [{"status": "in_transit", "order_id": "RET-02", "carrier": "PostNL"}]});
        assert_eq!(
            at(json!({"resolved": names, "delivery": rows})),
            "Your Stoneware mug set (4) is in transit with PostNL."
        );
        // empty list
        assert_eq!(
            at(json!({"delivery": {"status": [], "order_id": [], "carrier": []}})),
            NO_TRACKING
        );
        for t in [text, at(json!({"resolved": names, "delivery": done}))] {
            assert!(!t.contains("RET-") && !t.contains("SHP-"), "{t}");
        }
    }

    fn track_at(facts: Value) -> String {
        compose(&res(
            json!({"case_id": "CS-9", "request_type": "track_parcel", "outcome": "resolved", "facts": facts}),
        ))
    }

    /// "where is my rug": the rug order is delivered, a lamp order is in transit; the focus order wins.
    #[test]
    fn track_parcel_answers_about_the_order_asked_for() {
        let names = json!({"item_names": {"ORD-RUG": "Jute rug", "ORD-LAMP": "Desk lamp"}, "focus_order": "ORD-RUG"});
        let list = json!({
            "status": ["delivered", "in_transit"], "order_id": ["ORD-RUG", "ORD-LAMP"],
            "carrier": ["DHL", "PostNL"], "delivered_at": ["2026-10-07T09:30:00Z", ""],
        });
        assert_eq!(
            track_at(json!({"resolved": names, "delivery": list})),
            "Your Jute rug was delivered on 7 October."
        );
        // the focus order is the one in flight
        let lamp = json!({"item_names": {"ORD-RUG": "Jute rug", "ORD-LAMP": "Desk lamp"}, "focus_order": "ORD-LAMP"});
        assert_eq!(
            track_at(json!({"resolved": lamp, "delivery": list})),
            "Your Desk lamp is in transit with PostNL."
        );
        // a focus that matches no row keeps the old behaviour (all in-flight parcels)
        let other = json!({"item_names": {"ORD-RUG": "Jute rug", "ORD-LAMP": "Desk lamp"}, "focus_order": "ORD-GONE"});
        assert_eq!(
            track_at(json!({"resolved": other, "delivery": list})),
            "Your Desk lamp is in transit with PostNL."
        );
        // no in-flight parcel and a focus that matches nothing: the newest delivered one, as before
        let done =
            json!({"status": ["delivered", "delivered"], "order_id": ["ORD-RUG", "ORD-LAMP"]});
        assert_eq!(
            track_at(json!({"resolved": other, "delivery": done})),
            "Your Desk lamp was delivered."
        );
    }

    #[test]
    fn unnamed_parcels_in_one_reply_stay_apart_without_an_id() {
        let two = json!({"status": ["in_transit", "ready"], "order_id": ["ORD-1", "ORD-2"], "carrier": ["DHL", "PostNL"]});
        let t = track_at(json!({"delivery": two}));
        assert_eq!(
            t,
            "One parcel is ready with PostNL. Another parcel is in transit with DHL."
        );
        assert!(!t.contains("ORD-"), "{t}");
        // a single unnamed parcel keeps "Your parcel"
        let one = json!({"status": ["in_transit"], "order_id": ["ORD-1"]});
        assert_eq!(
            track_at(json!({"delivery": one})),
            "Your parcel is in transit."
        );
    }

    /// A two-item order is named as one order (the slots step stores "order with <items>"), singular verb.
    #[test]
    fn a_two_item_order_reads_as_one_order() {
        let names = json!({"item_names": {"ORD-1": "order with Desk lamp and Jute rug"}});
        let one = json!({"status": ["in_transit"], "order_id": ["ORD-1"]});
        assert_eq!(
            track_at(json!({"resolved": names, "delivery": one})),
            "Your order with Desk lamp and Jute rug is in transit."
        );
    }

    #[test]
    fn cancel_not_eligible_offers_a_return() {
        let text = compose(&ne("cancel_order", json!({})));
        assert_eq!(
            text,
            "This order can no longer be cancelled; once it arrives you can return it."
        );
        // a rule gate keeps its authored reason
        let r = ne(
            "cancel_order",
            json!({"reason": "Cancellation closed at dispatch."}),
        );
        assert_eq!(compose(&r), "Cancellation closed at dispatch.");
    }

    #[test]
    fn not_eligible_codes_give_their_plain_sentence() {
        use crate::runtime::resolve::contains_id_pattern;
        let known = [
            "ineligible:RET-01",
            "ineligible:WAR-01",
            "ineligible:PAY-04",
            "ineligible:DIS-02",
            "ineligible:ARR-01",
            "not_cancellable",
        ];
        let mut seen = Vec::new();
        for code in known {
            let r = res(json!({"case_id": "CS-9", "outcome": "not_eligible", "code": code}));
            let text = compose(&r);
            assert_eq!(
                Some(text.as_str()),
                not_eligible_by_code(Some(code)),
                "{code}"
            );
            assert_ne!(text, NOT_ELIGIBLE, "{code}");
            assert!(!seen.contains(&text), "{code}: duplicate {text}");
            assert!(!contains_id_pattern(&text), "{code}: {text}");
            assert!(
                !text.contains(code) && !text.contains("RET-"),
                "{code}: {text}"
            );
            assert!(!text.to_lowercase().contains("email"), "{code}: {text}");
            seen.push(text);
        }
        // unknown codes (a connector's own) read the generic reason, which still says why and carries no code
        for code in ["ineligible:x", "precondition_unmet", "not_eligible"] {
            let r = res(json!({"case_id": "CS-9", "outcome": "not_eligible", "code": code}));
            assert_eq!(compose(&r), NOT_ELIGIBLE, "{code}");
        }
        assert!(NOT_ELIGIBLE.contains("doesn't meet the conditions"));
        assert!(!contains_id_pattern(NOT_ELIGIBLE));
    }

    /// Email is deferred for early access: no constant or composed customer reply promises (or names) email.
    #[test]
    fn no_customer_reply_names_email() {
        use crate::runtime::resolve::contains_id_pattern;
        let mut texts: Vec<String> = [
            NO_PAYMENT,
            CHECKED,
            FAQ_FALLBACK,
            NOT_ELIGIBLE,
            NO_TRACKING,
            NO_MATCHING_ORDER,
            CHOICE_FALLBACK,
        ]
        .iter()
        .map(|t| t.to_string())
        .collect();
        for t in ["return", "faq", "gift_card", ""] {
            for wrote in [true, false] {
                let r = res(
                    json!({"case_id": "CS-9", "request_type": t, "outcome": "resolved", "wrote": wrote,
                    "facts": {"rma": {"carrier": "DHL"}, "order": {"item": "kettle"}}}),
                );
                texts.push(compose(&r));
            }
        }
        for code in [None, Some("timeout"), Some("some_new_code")] {
            let r = res(json!({"case_id": "CS-9", "outcome": "failed", "code": code}));
            texts.push(compose(&r));
        }
        for t in &texts {
            assert!(!t.to_lowercase().contains("email"), "{t}");
            assert!(!contains_id_pattern(t), "{t}");
        }
    }

    #[test]
    fn refund_already_refunded_answers_its_status() {
        let r = ne(
            "refund_request",
            json!({"facts": {"payments": {"payment_id": "PAY-1", "status": "refunded", "direction": "refund"}}}),
        );
        // the payment id never reaches the customer
        assert_eq!(compose(&r), "The refund has been paid back to you.");
        // no payment read: the fixed line, never a code
        assert_eq!(compose(&ne("refund_request", json!({}))), NOT_ELIGIBLE);
        // an order that is neither cancelled nor returned: the refund waits for the return or the cancellation
        let r = ne(
            "refund_request",
            json!({"facts": {"order": {"status": "delivered"}, "payments": {"payment_id": "PAY-1", "status": "captured"}}}),
        );
        assert_eq!(compose(&r), REFUND_NEEDS_RETURN);
    }

    fn with_returns(status: &str, extra: Value) -> Value {
        let mut facts = json!({
            "resolved": {"order_id": "RET-01", "item_label": "Jute rug ordered on 12 October",
                         "item_names": {"RET-01": "Jute rug", "RET-02": "kettle"}},
            "order": {"status": "delivered", "order_id": "RET-01"},
            "returns": {"return_id": ["RT-9", "RT-8"], "order_id": ["RET-02", "RET-01"],
                        "status": ["open", status], "created_at": ["2026-10-02", "2026-10-20T09:00:00Z"]},
        });
        for (k, x) in extra.as_object().unwrap() {
            facts[k] = x.clone();
        }
        facts
    }

    /// A `returns` step (a tenant procedure reading `return.list`): the reply says where the return of THIS order stands.
    #[test]
    fn a_refund_reply_says_where_the_return_of_the_order_stands() {
        use crate::runtime::resolve::contains_id_pattern;
        let status = |st: &str| {
            res(
                json!({"case_id": "CS-5", "request_type": "refunds", "outcome": "resolved",
                       "facts": with_returns(st, json!({}))}),
            )
        };
        let open = compose(&status("open"));
        assert_eq!(
            open,
            "We have your return for your Jute rug from 20 October; your refund is issued as soon as it arrives back with us."
        );
        let arrived = compose(&status("pending_provider"));
        assert_eq!(
            arrived,
            "Your return for your Jute rug from 20 October has arrived back with us; the refund is on its way."
        );
        let rejected = compose(&status("rejected"));
        assert_eq!(rejected, RETURN_REJECTED);
        // refunded: today's wording
        let refunded = compose(&status("refunded"));
        assert_eq!(
            refunded,
            "The refund for your Jute rug ordered on 12 October has been paid back to you."
        );
        // the same through a not_eligible refund_request
        let r = ne(
            "refund_request",
            json!({"facts": with_returns("open", json!({"payments": {"payment_id": "PAY-1", "status": "captured"}}))}),
        );
        assert_eq!(compose(&r), open);
        for t in [&open, &arrived, &rejected, &refunded] {
            assert!(!contains_id_pattern(t), "{t}");
        }
    }

    /// The procedures run no returns step: the orchestrator puts the case order's return (customer context,
    /// `return.list`) in the `resolved` fact, and the reply says where it stands. A tenant with no `return.list`
    /// has no `resolved.return` and keeps the payment wording.
    #[test]
    fn a_refund_reply_reads_the_return_the_orchestrator_resolved() {
        use crate::runtime::resolve::contains_id_pattern;
        let facts = |ret: Value| {
            json!({"payments": {"payment_id": "PAY-1", "status": "captured"},
                   "resolved": {"order_id": "RET-01", "item_names": {"RET-01": "Jute rug"}, "return": ret}})
        };
        let r = res(
            json!({"case_id": "CS-5", "request_type": "refunds", "outcome": "resolved",
                           "facts": facts(json!({"status": "open", "created_at": "2026-10-20T09:00:00Z"}))}),
        );
        let text = compose(&r);
        assert_eq!(
            text,
            "We have your return for your Jute rug from 20 October; your refund is issued as soon as it arrives back with us."
        );
        assert!(!contains_id_pattern(&text), "{text}");
        let r = res(
            json!({"case_id": "CS-5", "request_type": "refunds", "outcome": "resolved",
                           "facts": facts(Value::Null)}),
        );
        assert_eq!(compose(&r), "The payment is captured.");
    }

    /// Only a return of the case's own order counts; none (or no returns step) keeps the payment wording.
    #[test]
    fn another_orders_return_is_not_told() {
        let mut facts = with_returns(
            "open",
            json!({"payments": {"payment_id": "PAY-1", "status": "captured"}}),
        );
        facts["resolved"]["order_id"] = json!("RET-03");
        facts["resolved"]
            .as_object_mut()
            .unwrap()
            .remove("item_label");
        let r = res(
            json!({"case_id": "CS-5", "request_type": "refunds", "outcome": "resolved",
                           "facts": facts}),
        );
        assert_eq!(compose(&r), "The payment is captured.");
        let r = res(
            json!({"case_id": "CS-5", "request_type": "refunds", "outcome": "resolved",
                           "facts": {"payments": {"payment_id": "PAY-1", "status": "captured"}}}),
        );
        assert_eq!(compose(&r), "The payment is captured.");
    }

    #[test]
    fn redirect_not_eligible_says_so() {
        assert_eq!(
            compose(&ne("redirect", json!({}))),
            "This parcel can no longer be redirected."
        );
        assert_eq!(compose(&ne("warranty_claim", json!({}))), NOT_ELIGIBLE);
    }

    #[test]
    fn resolved_replies_name_no_internal_code() {
        let cases = [
            (
                "cancel_order",
                json!({"order": {"order_id": "ORD-1"}}),
                // the record id is never shown to the customer (KYC)
                "Your order is cancelled.",
            ),
            (
                "refund_request",
                json!({"refund": {"amount": "12.50"}}),
                "Refund of 12.50 is issued.",
            ),
            (
                "redirect",
                json!({"redirect": {"address": "Main St 1"}}),
                "Your parcel will now go to Main St 1.",
            ),
            (
                "delivery_problem",
                json!({}),
                "I've reported the problem with your delivery and passed it to a colleague.",
            ),
            (
                "payment_errors",
                json!({}),
                "I've opened a dispute for this charge. A colleague will follow up.",
            ),
            (
                "pending_charge",
                json!({}),
                "I've opened a dispute for this charge. A colleague will follow up.",
            ),
            (
                "complaint_intake",
                json!({}),
                "Your complaint is registered. A colleague will contact you.",
            ),
            (
                "data_rights",
                json!({}),
                "I've passed your request to our privacy team. They will contact you.",
            ),
        ];
        for (t, facts, want) in cases {
            let r = res(
                json!({"case_id": "CS-9", "request_type": t, "outcome": "resolved",
                               "wrote": true, "facts": facts}),
            );
            let text = compose(&r);
            assert_eq!(text, want, "{t}");
            assert!(
                !text.contains('_') && !text.contains("precondition"),
                "{t}: {text}"
            );
            // no write answered ok: never claimed
            let r = res(
                json!({"case_id": "CS-9", "request_type": t, "outcome": "resolved", "facts": {}}),
            );
            assert_eq!(compose(&r), CHECKED, "{t}");
        }
    }

    #[test]
    fn cancelled_names_the_order_in_plain_words_never_an_id() {
        use crate::runtime::resolve::contains_id_pattern;
        let at = |facts: Value| {
            compose(&res(
                json!({"case_id": "CS-9", "request_type": "cancel_order", "outcome": "resolved",
                       "wrote": true, "facts": facts}),
            ))
        };
        let names = json!({"ORD-1": "Jute rug"});
        // label (product and date)
        let t = at(json!({"resolved": {"order_id": "ORD-1",
            "item_label": "Jute rug ordered on 12 October", "item_label_order": "ORD-1"}}));
        assert_eq!(t, "Your Jute rug ordered on 12 October is cancelled.");
        assert!(!contains_id_pattern(&t) && !t.contains("ORD-1"), "{t}");
        // bare item name as the label
        let t = at(json!({"resolved": {"order_id": "ORD-1", "item_label": "Jute rug"}}));
        assert_eq!(t, "Your Jute rug order is cancelled.");
        // no label: the case order's item name
        let t = at(json!({"resolved": {"order_id": "ORD-1", "item_names": names}}));
        assert_eq!(t, "Your Jute rug order is cancelled.");
        assert!(!contains_id_pattern(&t) && !t.contains("ORD-1"), "{t}");
        // another order's label/name is never used
        let t = at(json!({"resolved": {"order_id": "ORD-2",
            "item_label": "Jute rug ordered on 12 October", "item_label_order": "ORD-1",
            "item_names": names}}));
        assert_eq!(t, "Your order is cancelled.");
        // nothing readable: plain sentence
        let t = at(json!({"order": {"order_id": "ORD-1"}}));
        assert_eq!(t, "Your order is cancelled.");
        assert!(!contains_id_pattern(&t) && !t.contains("ORD-1"), "{t}");
    }

    #[test]
    fn templates_per_outcome_and_type() {
        let r = res(
            json!({"case_id": "CS-1", "request_type": "return", "outcome": "resolved",
            "facts": {"rma": {"rma_id": "RMA-9", "carrier": "DHL"}, "order": {"item": "kettle"}}}),
        );
        assert_eq!(
            compose(&r),
            "Return approved for your kettle. We'll send the prepaid DHL label here as soon as it's ready."
        );
        let r = res(
            json!({"case_id": "CS-1", "outcome": "not_eligible", "reason": "Past the 30-day window."}),
        );
        assert_eq!(compose(&r), "Past the 30-day window.");
        let r = res(json!({"case_id": "CS-1", "outcome": "not_eligible"}));
        assert_eq!(compose(&r), NOT_ELIGIBLE);
        let r = res(
            json!({"case_id": "CS-1", "request_type": "order_status", "outcome": "resolved",
            "facts": {"order": {"status": "in_transit", "lines": []}}}),
        );
        assert_eq!(compose(&r), "Your order is in transit.");
        let r = res(
            json!({"case_id": "CS-1", "request_type": "delivery_problem", "outcome": "resolved",
            "facts": {"delivery": {"status": "in_transit", "expected_window": {"from": "2026-10-21", "until": "2026-10-23"}},
                      "report": {"case_id": "DP-7"}}}),
        );
        assert_eq!(
            compose(&r),
            "Your parcel is in transit, expected by 23 October. I've logged your report."
        );
        let r = res(
            json!({"case_id": "CS-1", "request_type": "delivery_problem", "outcome": "resolved",
            "facts": {"delivery": {"status": "in_transit", "expected_window": "2026-10-23T18:00:00Z"}}}),
        );
        assert_eq!(
            compose(&r),
            "Your parcel is in transit, expected by 23 October."
        );
        // a type with no template of its own claims completion only after a write step answered ok (CS-53)
        let r = res(
            json!({"case_id": "CS-1", "request_type": "gift_card", "outcome": "resolved", "wrote": true}),
        );
        assert!(compose(&r).starts_with("Your gift card request is done."));
    }

    /// Live CS-53: a read-only case (refund_status@1 ran only `payment.list`) was told "Your … request is done. A
    /// confirmation will follow here." With no write, the generic reply claims neither.
    #[test]
    fn a_resolved_case_with_no_write_claims_no_completion() {
        for t in ["gift_card", "loyalty_points", ""] {
            let r = res(
                json!({"case_id": "CS-53", "request_type": t, "outcome": "resolved",
                               "facts": {"lookup": {"status": "active"}}}),
            );
            let text = compose(&r);
            assert_eq!(text, CHECKED, "{t}");
            assert!(
                !text.contains("done") && !text.contains("email"),
                "{t}: {text}"
            );
        }
        let r = res(json!({"case_id": "CS-53", "outcome": "resolved", "wrote": true}));
        assert_eq!(
            compose(&r),
            "Your request is done. A confirmation will follow here."
        );
    }

    /// Live CS-58: a parcel-tracking case answers from the shipment read, else from the order read; a blank field
    /// is never printed.
    #[test]
    fn a_track_parcel_reply_gives_the_shipment_status_else_the_order_status() {
        let at = |facts: Value| {
            res(
                json!({"case_id": "CS-58", "request_type": "track_parcel", "outcome": "resolved",
                       "facts": facts}),
            )
        };
        let order = json!({"status": "shipped", "lines": []});
        let r = at(json!({"order": order, "delivery": {
            "status": "in_transit", "carrier": "PostNL", "tracking_ref": "3S-1",
            "expected_window": {"from": "2026-10-21", "until": "2026-10-23"}}}));
        assert_eq!(
            compose(&r),
            "Your parcel is in transit with PostNL, expected by 23 October."
        );
        let r = at(
            json!({"order": order, "delivery": {"status": "delivered", "carrier": " ", "tracking_ref": null}}),
        );
        assert_eq!(compose(&r), "Your parcel is delivered.");
        // no shipment answer: the order's status
        for facts in [
            json!({"order": order}),
            json!({"order": order, "delivery": {"status": ""}}),
        ] {
            assert_eq!(compose(&at(facts)), "Your order is shipped.");
        }
        assert_eq!(compose(&at(json!({}))), NO_TRACKING);
        for facts in [json!({"order": order}), json!({})] {
            let text = compose(&at(facts));
            assert!(
                !text.contains("done") && !text.contains("email") && !text.contains("  "),
                "{text}"
            );
        }
    }

    /// refund_status@1 (space `refunds`): the reply names what `payment.list` found, never "done".
    #[test]
    fn a_refund_status_reply_names_the_payment_or_refund_found() {
        let at = |payments: Value| {
            res(
                json!({"case_id": "CS-53", "request_type": "refunds", "outcome": "resolved",
                       "procedure": "refund_status@1", "facts": {"payments": payments}}),
            )
        };
        let charge = json!({"payment_id": "PAY-1006", "direction": "charge", "status": "captured"});
        assert_eq!(compose(&at(charge.clone())), "The payment is captured.");
        let mut listed = charge.clone();
        listed["payments"] = json!([charge.clone()]);
        assert_eq!(
            compose(&at(listed.clone())),
            "The payment is captured. I found no refund for this order yet."
        );
        listed["payments"] = json!([charge,
            {"payment_id": "REF-77", "direction": "refund", "status": "pending_settlement"}]);
        assert_eq!(compose(&at(listed)), "The refund is pending settlement.");
        assert_eq!(compose(&at(json!({}))), NO_PAYMENT);
        let r = res(json!({"case_id": "CS-53", "request_type": "refunds", "outcome": "resolved"}));
        assert_eq!(compose(&r), NO_PAYMENT);
        for text in [
            compose(&r),
            compose(&at(json!({"payment_id": "PAY-1", "status": "refunded"}))),
        ] {
            assert!(!text.contains("done") && !text.contains("email"), "{text}");
        }
    }

    /// B4 T16a: a failed case whose own item was queued out of hours says when a colleague follows up, not "by email
    /// today"; the cause stays the customer-safe phrase.
    #[test]
    fn a_failed_reply_out_of_hours_names_the_next_open_time() {
        let note = |mode: &str| {
            json!({"handover_id": "HO-1", "next_open": "2026-10-26T08:00:00Z", "out_of_hours": mode,
                   "next_open_local": "Mon 26 Oct 09:00 (Europe/Amsterdam)"})
        };
        let r = res(
            json!({"case_id": "CS-1", "outcome": "failed", "code": "timeout",
                           "out_of_hours": note("callback")}),
        );
        assert_eq!(
            compose(&r),
            "I couldn't finish this automatically: one of our systems didn't answer in time. Our team is closed \
             right now; a colleague will call you back after Mon 26 Oct 09:00 (Europe/Amsterdam)."
        );
        let r = res(
            json!({"case_id": "CS-1", "outcome": "failed", "code": "timeout",
                           "out_of_hours": note("queue")}),
        );
        let text = compose(&r);
        assert!(
            text.ends_with(
                "Our team is closed right now; a colleague will reply after Mon 26 Oct 09:00 (Europe/Amsterdam)."
            ) && !text.contains("today"),
            "{text}"
        );
        // the note on a handover reads as the case.create step output does
        let r = res(
            json!({"case_id": "CS-1", "outcome": "handover", "out_of_hours": note("callback")}),
        );
        assert_eq!(
            compose(&r),
            "We're closed right now. A colleague will call you back after Mon 26 Oct 09:00 (Europe/Amsterdam)."
        );
    }

    /// A failed reply never shows the code or the operation key of the reason (live CS-42: "(order.read is
    /// unavailable)"); every known code has its own cause, an unknown one the generic cause.
    #[test]
    fn a_failed_reply_never_shows_a_code_or_an_operation_key() {
        let codes = [
            "not_offered",
            "upstream_unavailable",
            "success_criteria_unmet",
            "postcondition_failed",
            "precondition_unmet",
            "tool_failed",
            "case_cap_reached",
            "attempts_exceeded",
            "unknown_outcome",
            "timeout",
            "deadline_exceeded",
            "not_found",
            "connector_auth_failed",
            "connector_misconfigured",
            "rate_limited",
            "invalid_input",
            "invalid_state",
            "ineligible:refund-window",
            "denied",
            "error",
            "some_new_code",
        ];
        for code in codes {
            for reason in [None, Some("order.read is unavailable")] {
                let r = res(
                    json!({"case_id": "CS-42", "outcome": "failed", "code": code, "reason": reason}),
                );
                let text = compose(&r);
                assert!(
                    text.starts_with("I couldn't finish this automatically: ")
                        && text.ends_with(" A colleague will follow up here today."),
                    "{code}: {text}"
                );
                assert!(!text.contains('_'), "{code}: {text}");
                assert!(
                    !text.contains("order.read") && !text.contains('('),
                    "{code}: {text}"
                );
                assert!(
                    !text.contains(code.split(':').next().unwrap()),
                    "{code}: {text}"
                );
            }
        }
        let r = res(
            json!({"case_id": "CS-42", "outcome": "failed", "code": "upstream_unavailable",
                           "reason": "order.read is unavailable"}),
        );
        assert_eq!(
            compose(&r),
            "I couldn't finish this automatically: a system we need isn't available right now. \
             A colleague will follow up here today."
        );
        let none = res(json!({"case_id": "CS-42", "outcome": "failed"}));
        assert_eq!(
            compose(&none),
            "I couldn't finish this automatically: something went wrong on our side. \
             A colleague will follow up here today."
        );
        assert_eq!(
            failure_cause(Some("timeout")),
            failure_cause(Some("deadline_exceeded"))
        );
        assert_ne!(
            failure_cause(Some("timeout")),
            failure_cause(Some("not_found"))
        );
        assert_ne!(
            failure_cause(Some("tool_failed")),
            failure_cause(Some("not_offered"))
        );
    }

    #[test]
    fn a_handover_queued_out_of_hours_names_the_next_open_time() {
        let step = |mode: &str| {
            json!({"handover_id": "HO-1", "outside_hours": true, "out_of_hours": mode,
                   "next_open_local": "Mon 26 Oct 09:00 (Europe/Amsterdam)"})
        };
        let r = res(
            json!({"case_id": "CS-1", "outcome": "handover", "facts": {"hand_over": step("callback")}}),
        );
        assert_eq!(
            compose(&r),
            "We're closed right now. A colleague will call you back after Mon 26 Oct 09:00 (Europe/Amsterdam)."
        );
        let r = res(
            json!({"case_id": "CS-1", "outcome": "handover", "facts": {"hand_over": step("queue")}}),
        );
        assert_eq!(
            compose(&r),
            "We're closed right now; a colleague will reply after Mon 26 Oct 09:00 (Europe/Amsterdam)."
        );
        // open hours, or an output that is not agent_cases' (no handover id): the plain handover text
        let mut open = step("queue");
        open["outside_hours"] = json!(false);
        let mut foreign = step("callback");
        foreign.as_object_mut().unwrap().remove("handover_id");
        for f in [open, foreign] {
            let r =
                res(json!({"case_id": "CS-1", "outcome": "handover", "facts": {"hand_over": f}}));
            assert_eq!(
                compose(&r),
                "I've passed this to a colleague, who will take it from here."
            );
        }
    }

    #[test]
    fn faq_needs_a_citation_and_grounding_follows_it() {
        let cited = res(
            json!({"case_id": "CS-1", "request_type": "faq", "outcome": "resolved",
            "facts": {"faq": {"answer": " 30 days. ", "citations": ["c1", {"chunk_id": "c2"}],
                              "sources": [{"document_id": "d1"}], "query": "return window"}}}),
        );
        assert_eq!(compose(&cited), "30 days.");
        let g = grounding(&cited, None);
        assert!(g.used_knowledge && !g.dont_know);
        assert_eq!(g.citations, vec!["c1".to_string(), "c2".to_string()]);
        assert_eq!(g.knowledge_query.as_deref(), Some("return window"));
        let bare = res(
            json!({"case_id": "CS-1", "request_type": "faq", "outcome": "resolved",
            "facts": {"faq": {"answer": "maybe", "citations": []}}}),
        );
        assert_eq!(compose(&bare), FAQ_FALLBACK);
        assert!(grounding(&bare, None).dont_know);
        let none = res(json!({"case_id": "CS-1", "request_type": "return", "outcome": "resolved"}));
        assert_eq!(grounding(&none, None), ReplyGrounding::default());
    }

    #[test]
    fn a_knowledge_answer_replies_whatever_the_request_type_and_never_ungrounded() {
        let src = json!({"chunk_id": "c1", "layer": "platform", "pack": "kh7/faq@1", "title": "Returns", "cite": 1});
        let r = res(
            json!({"case_id": "CS-1", "request_type": "gift_cards", "outcome": "resolved",
            "facts": {"faq": {"answer": "Gift cards never expire.", "citations": [src.clone()], "grounded": true,
                              "query": "do gift cards expire"}}}),
        );
        assert_eq!(compose(&r), "Gift cards never expire.");
        let g = grounding(&r, None);
        assert_eq!(g.citations, vec!["c1".to_string()]);
        assert_eq!(
            g.sources,
            vec![src.clone()],
            "the citations are the sources"
        );
        assert!(!g.dont_know);
        let r = res(
            json!({"case_id": "CS-1", "request_type": "gift_cards", "outcome": "resolved",
            "facts": {"faq": {"answer": "maybe", "citations": [src], "grounded": false}}}),
        );
        assert_eq!(compose(&r), FAQ_FALLBACK);
        assert!(grounding(&r, None).dont_know);
    }

    /// The `resolved` fact's label replaces every id a template used to print.
    #[test]
    fn replies_say_the_order_in_plain_words() {
        let label = json!({"item_label": "Jute rug ordered on 12 October"});
        let at = |t: &str, facts: Value| {
            compose(&res(
                json!({"case_id": "CS-9", "request_type": t, "outcome": "resolved", "facts": facts}),
            ))
        };
        assert_eq!(
            at(
                "order_status",
                json!({"resolved": label, "order": {"status": "in_transit"}})
            ),
            "Your Jute rug ordered on 12 October is in transit."
        );
        assert_eq!(
            at(
                "track_parcel",
                json!({"resolved": label, "delivery": {"status": "in_transit", "carrier": "PostNL",
                       "tracking_ref": "SHP-9102", "expected_window": "2026-10-21"}})
            ),
            "Your Jute rug ordered on 12 October is in transit with PostNL, expected by 21 October."
        );
        assert_eq!(
            at(
                "delivery_problem",
                json!({"resolved": label, "delivery": {"status": "delayed"}, "report": {"case_id": "DP-1"}})
            ),
            "Your Jute rug ordered on 12 October is delayed. I've logged your report."
        );
        assert_eq!(
            at(
                "refunds",
                json!({"resolved": label, "payments": {"payment_id": "PAY-1", "status": "refunded"}})
            ),
            "The payment for your Jute rug ordered on 12 October is refunded."
        );
        assert_eq!(
            at(
                "return",
                json!({"resolved": label, "rma": {"rma_id": "RMA-1", "carrier": "DHL"}})
            ),
            "Return approved for your Jute rug ordered on 12 October. We'll send the prepaid DHL label here as soon as it's ready."
        );
    }

    /// A parked choice speaks the question; no match speaks a plain sentence; neither asks for a key.
    #[test]
    fn a_choice_and_a_miss_are_plain_sentences() {
        let q = "Is this about your Jute rug from 12 October or your Ceramic lamp from 20 October?";
        let r = res(
            json!({"case_id": "CS-9", "outcome": "confirmation_required",
                           "code": "choice_required", "reason": q}),
        );
        assert_eq!(compose(&r), q);
        let r = res(
            json!({"case_id": "CS-9", "outcome": "confirmation_required", "code": "choice_required"}),
        );
        assert_eq!(compose(&r), CHOICE_FALLBACK);
        let r =
            res(json!({"case_id": "CS-9", "outcome": "not_eligible", "code": "no_matching_order"}));
        assert_eq!(compose(&r), NO_MATCHING_ORDER);
    }

    #[test]
    fn a_missing_record_names_its_kind_and_never_an_id() {
        use crate::runtime::resolve::{RECORD_KINDS, contains_id_pattern};
        for k in RECORD_KINDS {
            let r = res(
                json!({"case_id": "CS-9", "outcome": "not_eligible", "code": "no_matching_record", "reason": k.noun}),
            );
            let t = compose(&r);
            assert!(
                t.starts_with("I can't find a ") || t.starts_with("I can't find an "),
                "{t}"
            );
            assert!(
                t.contains(k.noun) && t.ends_with(" on your account."),
                "{t}"
            );
            assert!(!contains_id_pattern(&t), "{t}");
        }
        let r = res(
            json!({"case_id": "CS-9", "outcome": "not_eligible", "code": "no_matching_record", "reason": "POL-0042"}),
        );
        assert_eq!(compose(&r), "I can't find a record on your account.");
    }

    /// Owner ruling 2026-10-26: no customer-facing text carries a record id. Every outcome and request type is
    /// composed with facts FULL of ids (and the case reference `CS-n` in the resolution itself); none may print one.
    #[test]
    fn no_composed_reply_prints_an_id() {
        use crate::runtime::resolve::contains_id_pattern;
        let ids = json!({
            "resolved": {"item_label": "Jute rug ordered on 12 October"},
            "order": {"status": "in_transit", "item": "kettle", "order_id": "RET-01", "lines": []},
            "shipment": {"status": "in_transit", "carrier": "PostNL", "shipment_id": "SHP-9102",
                         "eta": "2026-10-21", "last_event": "Sorted", "delivered_at": "2026-10-07"},
            "delivery": {"status": "in_transit", "carrier": "PostNL", "tracking_ref": "SHP-9102",
                         "delivery_id": "SHP-9102", "expected_window": {"until": "2026-10-23"}},
            "report": {"case_id": "DP-7"},
            "rma": {"rma_id": "RMA-9", "carrier": "DHL"},
            "claim": {"claim_id": "WAR-3", "next_step": "We will email you."},
            "rules": {"refund_payment_id": "PAY-1001"},
            "refund": {"refund_id": "REF-77", "amount": "42.90", "eta": "5 days"},
            "payments": {"payment_id": "PAY-1006", "status": "captured",
                         "payments": [{"payment_id": "PAY-1006", "direction": "charge", "status": "captured"},
                                      {"payment_id": "REF-77", "direction": "refund", "status": "pending"}]},
            "returns": {"return_id": ["RT-9"], "order_id": ["RET-01"], "status": ["open"],
                        "created_at": ["2026-10-20"]},
        });
        let types = [
            "",
            "return",
            "warranty_claim",
            "order_status",
            "delivery_status",
            "delivery_problem",
            "track_parcel",
            "payment_error",
            "refunds",
            "refund_status",
            "faq",
            "gift_card",
        ];
        let outcomes = [
            "resolved",
            "not_eligible",
            "failed",
            "handover",
            "identity_required",
            "confirmation_required",
            "cancelled_offboard",
        ];
        let codes = [
            None,
            Some("choice_required"),
            Some("no_matching_order"),
            Some("no_matching_record"),
            Some("step_up_required"),
            Some("timeout"),
        ];
        let mut n = 0;
        for t in types {
            for o in outcomes {
                for code in codes {
                    for facts in [ids.clone(), json!({})] {
                        let mut v = json!({"case_id": "CS-12", "request_type": t, "outcome": o, "facts": facts,
                                           "wrote": true});
                        if let Some(c) = code {
                            v["code"] = json!(c);
                        }
                        let text = compose(&res(v));
                        assert!(!contains_id_pattern(&text), "{t} {o} {code:?}: {text}");
                        let low = text.to_lowercase();
                        for w in [
                            "order number",
                            "order id",
                            "postcode",
                            "postal code",
                            "tracking number",
                            "your email",
                        ] {
                            assert!(!low.contains(w), "{t} {o}: {text}");
                        }
                        n += 1;
                    }
                }
            }
        }
        assert_eq!(n, types.len() * outcomes.len() * codes.len() * 2);
        for c in [
            NO_PAYMENT,
            CHECKED,
            FAQ_FALLBACK,
            NOT_ELIGIBLE,
            NO_TRACKING,
            NO_MATCHING_ORDER,
            CHOICE_FALLBACK,
        ] {
            assert!(!contains_id_pattern(c), "{c}");
        }
        // a failed reply shows only the cause, whatever the code or reason carries
        let r = res(
            json!({"case_id": "CS-12", "outcome": "failed", "code": "ineligible:RET-01", "reason": "order.read RET-01 failed"}),
        );
        assert!(!contains_id_pattern(&compose(&r)));
    }

    /// A product name always comes from the order the reply is about: a label written for another order is never
    /// printed, the case order's own name is.
    #[test]
    fn a_label_for_another_order_is_never_printed() {
        let at = |resolved: Value| {
            compose(&res(
                json!({"case_id": "CS-1", "request_type": "delivery_problem", "outcome": "resolved",
                       "facts": {"resolved": resolved, "delivery": {"status": "in_transit"}}}),
            ))
        };
        let names = json!({"ORD-A": "Jute rug", "ORD-B": "Desk lamp"});
        // the label is for ORD-A, the reply is about ORD-B
        assert_eq!(
            at(
                json!({"order_id": "ORD-B", "item_label": "Jute rug ordered on 12 October",
                      "item_label_order": "ORD-A", "item_names": names})
            ),
            "Your Desk lamp is in transit."
        );
        // no name for the case order: no other order's name either
        assert_eq!(
            at(
                json!({"order_id": "ORD-C", "item_label": "Jute rug ordered on 12 October",
                      "item_label_order": "ORD-A", "item_names": names})
            ),
            "Your parcel is in transit."
        );
        // written for the case order: kept, with its date
        assert_eq!(
            at(
                json!({"order_id": "ORD-A", "item_label": "Jute rug ordered on 12 October",
                      "item_label_order": "ORD-A", "item_names": names})
            ),
            "Your Jute rug ordered on 12 October is in transit."
        );
    }

    /// An instant is read on the customer's day (`local_days`, the scope's timezone); a date-only string and an
    /// instant with no known zone are read as written.
    #[test]
    fn an_instant_reads_on_the_customers_day() {
        let at = |window: &str, local_days: Value| {
            let mut facts =
                json!({"delivery": {"status": "in_transit", "expected_window": window}});
            if !local_days.is_null() {
                facts["local_days"] = local_days;
            }
            compose(&res(
                json!({"case_id": "CS-1", "request_type": "delivery_problem", "outcome": "resolved", "facts": facts}),
            ))
        };
        // 23:30 UTC on 21 October is 22 October 01:30 in Europe/Amsterdam (CEST)
        assert_eq!(
            at(
                "2026-10-21T23:30:00Z",
                json!({"2026-10-21T23:30:00Z": "2026-10-22"})
            ),
            "Your parcel is in transit, expected by 22 October."
        );
        // date-only: unchanged, even beside a map
        assert_eq!(
            at("2026-10-21", json!({"2026-10-21T23:30:00Z": "2026-10-22"})),
            "Your parcel is in transit, expected by 21 October."
        );
        // no zone known: the date as written
        assert_eq!(
            at("2026-10-21T23:30:00Z", Value::Null),
            "Your parcel is in transit, expected by 21 October."
        );
    }
}
