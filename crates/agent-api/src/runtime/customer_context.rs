//! The customer context (B4 known customer, plan §2.2; owner ruling 2026-10-26): what the agent knows about an
//! identified conversation's customer BEFORE they type a word. At case start the orchestrator's `context` phase
//! reads the customer's orders, payments, deliveries and cases through the runner with provenance `context`
//! ([`crate::runtime::domain::PROVENANCE_CONTEXT`]); the facts are stored on the case (`ctx`) and in a per-conversation
//! snapshot the staff route (`GET …/conversations/{id}/customer-context`) reads.
//!
//! - **Reads (all R, all audited).** `order.list{party_id}`, `payment.list{party_id}`,
//!   `delivery.list{party_id}` (seed v9; a design without it contributes an empty list), `return.list{party_id}` (seed
//!   v13; likewise), then `order.read{order_id}` for each of the [`MAX_ORDER_READS`] newest orders whose list row carries
//!   no items. Worst case 4 + 6 = 10 reads (11 with the `delivery.read` fallback), never
//!   more than [`MAX_CONTEXT_CALLS`] per case (the runner counts them; a further one is `context_cap_reached`).
//! - **Allowlist.** [`CONTEXT_OPS`]; the runner refuses any other operation of a `context` call `not_context_op`.
//! - **Staff test customer.** A staff test chat types the test customer's EMAIL, but a real system keys its records by
//!   its own party id. Before the reads the phase resolves the email with `party.list{email}` ([`PARTY_LOOKUP_OP`], a
//!   `context` call the runner allows ONLY for a `staff_test` case and only for that case's own email,
//!   [`staff_test_lookup`]): exactly one party binds the case (and the snapshot remembers it, `resolved_from`); none,
//!   several or no route finish the context degraded ([`PARTY_NOT_FOUND`], [`PARTY_AMBIGUOUS`], [`PARTY_LOOKUP_UNAVAILABLE`])
//!   with no record read. A web chat's `assertion` party already IS the system's id and is never looked up.
//! - **Partial failure.** A failed read narrows the context ([`CustomerContext::degraded`] names the operation); it
//!   never blocks the case.
//! - **Cache.** One snapshot per conversation (`custctx:{tenant}:{job}:{conversation}` under the environment prefix):
//!   reused by a new case of the conversation for [`CACHE_TTL_MS`] unless a write marked it stale
//!   ([`invalidate`], called by the runner after a successful write). The staff route reads it whatever its age.
//!
//! Ids stay server-side: the model never sees this context and no reply prints an id.
use std::collections::BTreeMap;

use anyhow::Result;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::bus::Bus;

/// The operations a `context` call may run (side effect R). Everything else is refused `not_context_op`.
pub const CONTEXT_OPS: [&str; 21] = [
    "order.list",
    "order.read",
    "delivery.read",
    "delivery.list",
    "payment.list",
    "return.list",
    // the generic known-customer records ([`crate::runtime::resolve::RECORD_KINDS`]); `payment.list` is above
    "policy.list",
    "claim.list",
    "prescription.list",
    "appointment.list",
    "payslip.list",
    "account.list",
    "invoice.list",
    "loyalty_account.read",
    "send.list",
    "case.list",
    "subscription.list",
    // seed v16: the policyholder's premium notices and the customer's own contact details
    "premium_notice.list",
    "contact_point.list",
    // the employee's own devices (IT desk `asset_serial`); owned by `party_id`
    "asset.list",
    // the stage-2 fan-out after `prescription.list` ([`dispense_stage2`])
    "dispense.read",
];
/// The refusal code of a `context` call to an operation outside [`CONTEXT_OPS`].
pub const NOT_CONTEXT_OP: &str = "not_context_op";
/// The refusal code of a `context` call past [`MAX_CONTEXT_CALLS`] in one case.
pub const CONTEXT_CAP: &str = "context_cap_reached";
/// The retail reads of one case (plan §2.2: 11 reads): four first reads, [`MAX_ORDER_READS`] order reads and the
/// `delivery.read` fallback.
pub const BASE_CONTEXT_CALLS: i64 = 11;
/// The list reads of the generic records beyond the retail ones (distinct `RECORD_KINDS` operations other than
/// `payment.list`); only the operations a design includes are read.
pub const MAX_RECORD_READS: i64 = 14;
/// The newest/active prescriptions whose `dispense.read` is fanned out (stage 2, after `prescription.list`).
pub const MAX_DISPENSE_READS: usize = 3;
/// The most `context` calls one case may make (a hard cap): the retail reads plus the record lists plus the
/// [`MAX_DISPENSE_READS`] dispense reads (raised from 21 to 24 for them, to 25 for `subscription.list`, to 27 for `premium_notice.list` and `contact_point.list`, to 28 for `asset.list`).
/// One more for the staff test chat's party lookup ([`PARTY_LOOKUP_OP`]).
pub const MAX_CONTEXT_CALLS: i64 =
    BASE_CONTEXT_CALLS + MAX_RECORD_READS + MAX_DISPENSE_READS as i64 + 1;
/// The step name of the prescription list (the dispense fan-out hangs off it).
pub const PRESCRIPTION_LIST_STEP: &str = "ctx:prescription.list";
/// The newest orders whose `order.read` is fanned out.
pub const MAX_ORDER_READS: usize = 6;
/// The `delivery.read` fallback fan-out (design without `delivery.list`): what is left of [`MAX_CONTEXT_CALLS`] after
/// the three first reads that remain without `delivery.list` (order, payment, return) and [`MAX_ORDER_READS`] order
/// reads (of [`BASE_CONTEXT_CALLS`]). Two, as before `return.list` joined stage 1: that list replaces the missing
/// `delivery.list`, so the fallback never lost a read.
pub const MAX_DELIVERY_READS: usize = BASE_CONTEXT_CALLS as usize - 3 - MAX_ORDER_READS;
/// Order statuses that have no parcel on its way (lowercase); every other order is "still on its way".
const ORDER_SETTLED: [&str; 6] = [
    "delivered",
    "cancelled",
    "canceled",
    "returned",
    "refunded",
    "completed",
];
/// The most orders a context keeps.
pub const MAX_ORDERS: usize = 20;
/// How long a new case reuses a conversation's snapshot.
pub const CACHE_TTL_MS: i64 = 60_000;
/// How long a snapshot is kept for the staff route.
pub const SNAPSHOT_TTL_SECS: i64 = 86_400;
/// The identity source of a staff test chat's typed customer.
pub const STAFF_TEST: &str = "staff_test";
/// The staff test chat's party lookup: the typed email to the system's party id. Not in [`CONTEXT_OPS`]: the runner
/// admits it only for a `staff_test` case ([`staff_test_lookup`]).
pub const PARTY_LOOKUP_OP: &str = "party.list";
/// The step name of the party lookup.
pub const PARTY_LOOKUP_STEP: &str = "ctx:party.list";
/// `degraded` entry: the system has no customer with the typed email.
pub const PARTY_NOT_FOUND: &str = "party_not_found";
/// `degraded` entry: the system has several customers with the typed email (never guessed).
pub const PARTY_AMBIGUOUS: &str = "party_ambiguous";
/// `degraded` entry: the job cannot look a customer up by email (no `party.list` route, or the lookup failed).
pub const PARTY_LOOKUP_UNAVAILABLE: &str = "party_lookup_unavailable";
/// The plain note the test chat shows the admin for a `degraded` lookup entry, else `None`.
pub fn lookup_note(degraded: &[String]) -> Option<&'static str> {
    degraded.iter().find_map(|d| match d.as_str() {
        PARTY_NOT_FOUND => Some("No customer with that email in your connected system."),
        PARTY_AMBIGUOUS => Some(
            "More than one customer has that email in your connected system, so the test cannot pick one.",
        ),
        PARTY_LOOKUP_UNAVAILABLE => Some(
            "Your connected system cannot look a customer up by email (no customer lookup is set up for this job).",
        ),
        _ => None,
    })
}
/// The step-name prefix of a context call (`ctx:order.list`).
pub const STEP_PREFIX: &str = "ctx:";
/// The step name of the order list.
pub const ORDER_LIST_STEP: &str = "ctx:order.list";

/// Is `op` an operation a `context` call may run?
pub fn is_context_op(op: &str) -> bool {
    CONTEXT_OPS.contains(&op)
}

// ---- the context ------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemCtx {
    pub name: String,
    pub qty: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderCtx {
    pub order_id: String,
    pub status: String,
    pub placed_at: Option<String>,
    pub items: Vec<ItemCtx>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PaymentCtx {
    pub payment_id: String,
    pub order_id: Option<String>,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub status: String,
    pub refunded_amount: Option<f64>,
    pub at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeliveryCtx {
    pub delivery_id: String,
    pub order_id: Option<String>,
    pub carrier: Option<String>,
    pub status: String,
    pub eta: Option<String>,
    pub delivered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_url: Option<String>,
}

/// One return of a `return.list` answer (canonical status: open | pending_provider | refunded | rejected).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReturnCtx {
    pub return_id: String,
    pub order_id: Option<String>,
    pub status: String,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaseCtx {
    pub case_id: String,
    pub state: String,
    pub title: String,
    pub opened_at: String,
}

/// What is known about one identified customer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomerContext {
    /// The customer's id (the case's verified party).
    pub party: String,
    /// Newest first.
    pub orders: Vec<OrderCtx>,
    pub payments: Vec<PaymentCtx>,
    pub deliveries: Vec<DeliveryCtx>,
    /// Older cached snapshots carry none.
    #[serde(default)]
    pub returns: Vec<ReturnCtx>,
    pub cases: Vec<CaseCtx>,
    /// The operations whose read failed (the context is narrower than the records).
    pub degraded: Vec<String>,
    /// The customer's records of the generic kinds ([`crate::runtime::resolve::RECORD_KINDS`]), keyed by list
    /// operation (`policy.list`): one string map per row, field names as the connector answers them. A key is present
    /// exactly when that read answered (an empty list is a real "none"); a failed read is in `degraded` with no key,
    /// an operation the design does not include has no key either.
    #[serde(default)]
    pub records: BTreeMap<String, Vec<BTreeMap<String, String>>>,
    /// A staff test chat: the typed email (lowercase) that [`PARTY_LOOKUP_OP`] resolved to `party`. Later cases of the
    /// conversation rebind to `party` from it without a second lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_from: Option<String>,
}

// ---- reading the answers ----------------------------------------------------------------------------------------

/// Fields whose cell is itself a list (product lines), not a column of the answer.
const NESTED: [&str; 2] = ["items", "lines"];

/// The records of a list answer: the connector answers one array per declared output (a column), or scalars for one
/// record. Row `i` takes `column[i]`; a scalar is row 0; a nested list field is the whole list of a one-row answer, or
/// element `i` of an array of lists.
pub fn rows(answer: &Map<String, Value>) -> Vec<Map<String, Value>> {
    let n = answer
        .iter()
        .filter(|(k, _)| !NESTED.contains(&k.as_str()))
        .map(|(_, v)| match v {
            Value::Array(a) => a.len(),
            Value::Null => 0,
            _ => 1,
        })
        .max()
        .unwrap_or_else(|| usize::from(answer.values().any(|v| !v.is_null())));
    (0..n)
        .map(|i| {
            answer
                .iter()
                .filter_map(|(k, v)| {
                    let cell = match v {
                        Value::Array(a) if NESTED.contains(&k.as_str()) => {
                            if a.iter().all(Value::is_array) && !a.is_empty() {
                                a.get(i).cloned()
                            } else if n <= 1 {
                                (i == 0).then(|| v.clone())
                            } else {
                                a.get(i).map(|e| Value::Array(vec![e.clone()]))
                            }
                        }
                        Value::Array(a) => a.get(i).cloned(),
                        Value::Null => None,
                        other => (i == 0).then(|| other.clone()),
                    };
                    cell.filter(|c| !c.is_null()).map(|c| (k.clone(), c))
                })
                .collect()
        })
        .collect()
}

fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(o) => o
            .get("until")
            .or_else(|| o.get("to"))
            .or_else(|| o.get("end"))
            .and_then(|x| text(Some(x))),
        _ => None,
    }
}

fn number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Object(o) => number(o.get("amount")),
        _ => None,
    }
}

fn first<'a>(r: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| r.get(*k))
}

fn items(r: &Map<String, Value>) -> Vec<ItemCtx> {
    let Some(Value::Array(a)) = first(r, &["items", "lines"]) else {
        return Vec::new();
    };
    a.iter()
        .filter_map(|e| match e {
            Value::String(s) if !s.trim().is_empty() => Some(ItemCtx {
                name: s.trim().to_owned(),
                qty: 1,
            }),
            Value::Object(o) => {
                let name = text(first(o, &["name", "title", "product", "description"]))?;
                let qty = number(first(o, &["qty", "quantity"])).map_or(1, |q| q as i64);
                Some(ItemCtx { name, qty })
            }
            _ => None,
        })
        .collect()
}

/// The orders of an `order.list` answer, newest first (`placed_at` descending when every row has one, else the
/// answer's order), at most [`MAX_ORDERS`].
pub fn parse_orders(answer: &Map<String, Value>) -> Vec<OrderCtx> {
    let mut v: Vec<OrderCtx> = rows(answer)
        .into_iter()
        .filter_map(|r| {
            Some(OrderCtx {
                order_id: text(r.get("order_id"))?,
                status: text(r.get("status")).unwrap_or_default(),
                placed_at: text(first(&r, &["placed_at", "date", "created_at"])),
                items: items(&r),
            })
        })
        .collect();
    if v.iter().all(|o| o.placed_at.is_some()) {
        v.sort_by(|a, b| b.placed_at.cmp(&a.placed_at));
    }
    v.truncate(MAX_ORDERS);
    v
}

/// The payments of a `payment.list` answer; a refund row (`direction: refund`) is listed too, and its amount is
/// added to `refunded_amount` of the payments of the same order.
pub fn parse_payments(answer: &Map<String, Value>) -> Vec<PaymentCtx> {
    let rs = rows(answer);
    let is_refund = |r: &Map<String, Value>| text(r.get("direction")).as_deref() == Some("refund");
    let refunded = |order: &Option<String>| -> f64 {
        rs.iter()
            .filter(|r| is_refund(r) && order.is_some() && text(r.get("order_id")) == *order)
            .filter_map(|r| number(r.get("amount")))
            .map(f64::abs)
            .sum()
    };
    rs.iter()
        .filter_map(|r| {
            let order_id = text(r.get("order_id"));
            let currency = text(r.get("currency")).or_else(|| match r.get("amount") {
                Some(Value::Object(o)) => text(o.get("currency")),
                _ => None,
            });
            Some(PaymentCtx {
                payment_id: text(r.get("payment_id"))?,
                amount: number(r.get("amount")),
                currency,
                status: text(r.get("status")).unwrap_or_default(),
                refunded_amount: number(r.get("refunded_amount"))
                    .or_else(|| (!is_refund(r)).then(|| refunded(&order_id))),
                at: text(first(r, &["at", "date", "created_at"])),
                order_id,
            })
        })
        .collect()
}

/// The deliveries of a `delivery.list` answer.
pub fn parse_deliveries(answer: &Map<String, Value>) -> Vec<DeliveryCtx> {
    rows(answer)
        .iter()
        .filter_map(|r| {
            Some(DeliveryCtx {
                delivery_id: text(first(r, &["delivery_id", "shipment_id", "tracking_ref"]))?,
                order_id: text(r.get("order_id")),
                carrier: text(r.get("carrier")),
                status: text(r.get("status")).unwrap_or_default(),
                eta: text(first(r, &["eta", "expected_window", "expected_at"])),
                delivered_at: text(r.get("delivered_at")),
                tracking_url: text(r.get("tracking_url")),
            })
        })
        .collect()
}

/// The returns of a `return.list` answer.
pub fn parse_returns(answer: &Map<String, Value>) -> Vec<ReturnCtx> {
    rows(answer)
        .iter()
        .filter_map(|r| {
            Some(ReturnCtx {
                return_id: text(r.get("return_id"))?,
                order_id: text(r.get("order_id")),
                status: text(r.get("status")).unwrap_or_default(),
                created_at: text(r.get("created_at")),
            })
        })
        .collect()
}

/// The most rows one record list keeps.
pub const MAX_RECORD_ROWS: usize = 50;

/// The rows of a record list answer as string maps (strings, numbers and booleans; a nested list or null is left
/// out), at most [`MAX_RECORD_ROWS`].
pub fn parse_records(answer: &Map<String, Value>) -> Vec<BTreeMap<String, String>> {
    rows(answer)
        .iter()
        .take(MAX_RECORD_ROWS)
        .map(|r| {
            r.iter()
                .filter_map(|(k, v)| {
                    let t = match v {
                        Value::Bool(b) => Some(b.to_string()),
                        other => text(Some(other)),
                    }?;
                    Some((k.clone(), t))
                })
                .collect()
        })
        .collect()
}

// ---- the steps ---------------------------------------------------------------------------------------------------

/// One read of the context phase.
#[derive(Clone, Debug, PartialEq)]
pub struct CtxStep {
    /// `ctx:order.list`, `ctx:order.read:2`.
    pub name: String,
    pub op: &'static str,
    pub input: Map<String, Value>,
}

fn step(name: &str, op: &'static str, key: &str, value: &str) -> CtxStep {
    CtxStep {
        name: name.to_owned(),
        op,
        input: Map::from_iter([(key.to_owned(), Value::String(value.to_owned()))]),
    }
}

/// The party a STAFF-audience conversation (the signed-in employee of an IT desk / HR job) is read as: the session's
/// Keycloak email, lowercased, and only when Keycloak vouches for it (`email_verified`). It takes the session claims and
/// nothing else, so neither the message, the model's slots nor a typed address can name the party.
pub fn staff_party(verified_email: Option<&str>, email_verified: bool) -> Option<String> {
    verified_email
        .map(|e| e.trim().to_lowercase())
        .filter(|e| email_verified && e.contains('@') && !e.contains(char::is_whitespace))
}

// ---- the staff test customer's party lookup ---------------------------------------------------------------------

/// May this `context` call run as the staff test chat's party lookup? Only `party.list` carrying nothing but the
/// case's own typed email (the conversation's `subject` state is that email until the lookup binds the system's id), on a `staff_test`
/// conversation. Everything else keeps the allowlist.
pub fn staff_test_lookup(
    identity_source: Option<&str>,
    op: &str,
    input: &Map<String, Value>,
    verified_party: Option<&str>,
) -> bool {
    identity_source == Some(STAFF_TEST)
        && op == PARTY_LOOKUP_OP
        && input.len() == 1
        && match (input.get("email"), verified_party) {
            (Some(Value::String(e)), Some(me)) => {
                let (e, me) = (e.trim(), me.trim());
                !e.is_empty() && e.eq_ignore_ascii_case(me)
            }
            _ => false,
        }
}

/// Does a case of this conversation still hold the typed email as its party (so the lookup has not bound the system's
/// id yet)? Only a `staff_test` conversation whose `subject` state equals its `email` state.
pub fn needs_lookup(
    identity_source: Option<&str>,
    verified_party: &str,
    customer_email: Option<&str>,
) -> bool {
    identity_source == Some(STAFF_TEST)
        && customer_email.is_some_and(|e| {
            let e = e.trim();
            !e.is_empty() && e.eq_ignore_ascii_case(verified_party.trim())
        })
}

/// The lookup step for the typed `email`.
pub fn lookup_step(email: &str) -> CtxStep {
    step(PARTY_LOOKUP_STEP, PARTY_LOOKUP_OP, "email", email)
}

/// What the party lookup found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartyLookup {
    /// Exactly one party: the system's id.
    One(String),
    NotFound,
    Ambiguous,
}

/// The verdict of a `party.list{email}` answer: the distinct party ids it names (one per row).
pub fn lookup_outcome(answer: &Map<String, Value>) -> PartyLookup {
    let mut ids: Vec<String> = rows(answer)
        .iter()
        .filter_map(|r| text(r.get("party_id")))
        .collect();
    ids.sort();
    ids.dedup();
    match ids.len() {
        0 => PartyLookup::NotFound,
        1 => PartyLookup::One(ids.remove(0)),
        _ => PartyLookup::Ambiguous,
    }
}

/// Does the design include any read of the customer context (`order.list` or a record list)? Without one the phase
/// has nothing to read.
pub fn offers_any(offers: impl Fn(&str) -> bool) -> bool {
    offers("order.list")
        || crate::runtime::resolve::record_ops()
            .into_iter()
            .any(offers)
}

/// The first reads for `party`: the lists the design offers (`offers(op)`): `order.list`, the retail lists and every
/// operation of the generic record kinds. Only included operations are read, so retail jobs read exactly what they
/// did. `party_id` is always the verified party. A design without `order.list` has no order fan-out.
pub fn stage1(party: &str, offers: impl Fn(&str) -> bool) -> Vec<CtxStep> {
    let mut v = Vec::new();
    if offers("order.list") {
        v.push(step(ORDER_LIST_STEP, "order.list", "party_id", party));
    }
    for op in ["payment.list", "delivery.list", "return.list"] {
        if offers(op) {
            v.push(step(&format!("{STEP_PREFIX}{op}"), op, "party_id", party));
        } else {
            tracing::debug!(
                op,
                "customer context: the design does not offer the operation; its list is empty"
            );
        }
    }
    for op in crate::runtime::resolve::record_ops() {
        if op != "payment.list" && offers(op) {
            v.push(step(&format!("{STEP_PREFIX}{op}"), op, "party_id", party));
        }
    }
    v
}

/// Does the context fan out `delivery.read` per in-flight order? Only when the design has no `delivery.list` (which
/// then answers the customer's deliveries in one read) but does offer `delivery.read`.
pub fn reads_deliveries(offers: impl Fn(&str) -> bool) -> bool {
    !offers("delivery.list") && offers("delivery.read")
}

/// The fan-out after `order.list`: `order.read` of the newest [`MAX_ORDER_READS`] orders that list no items and, when
/// `delivery_reads` ([`reads_deliveries`]), `delivery.read` of the newest [`MAX_DELIVERY_READS`] orders still on their way.
/// The step index is the order's position in the (newest first) list.
pub fn stage2(orders: &[OrderCtx], delivery_reads: bool) -> Vec<CtxStep> {
    let mut v: Vec<CtxStep> = orders
        .iter()
        .take(MAX_ORDER_READS)
        .enumerate()
        .filter(|(_, o)| o.items.is_empty())
        .map(|(i, o)| {
            step(
                &format!("{STEP_PREFIX}order.read:{i}"),
                "order.read",
                "order_id",
                &o.order_id,
            )
        })
        .collect();
    if delivery_reads {
        v.extend(
            orders
                .iter()
                .enumerate()
                .filter(|(_, o)| !ORDER_SETTLED.contains(&o.status.trim().to_lowercase().as_str()))
                .take(MAX_DELIVERY_READS)
                .map(|(i, o)| {
                    step(
                        &format!("{STEP_PREFIX}delivery.read:{i}"),
                        "delivery.read",
                        "order_id",
                        &o.order_id,
                    )
                }),
        );
    }
    v
}

/// Does the context fan out `dispense.read` per listed prescription? Only when the design offers both
/// `prescription.list` and `dispense.read`.
pub fn reads_dispenses(offers: impl Fn(&str) -> bool) -> bool {
    offers("prescription.list") && offers("dispense.read")
}

/// Prescription statuses with nothing left to collect or deliver (lowercase); every other row is active.
const PRESCRIPTION_SETTLED: [&str; 7] = [
    "dispensed",
    "collected",
    "delivered",
    "completed",
    "cancelled",
    "canceled",
    "expired",
];
/// The columns that date a prescription row, first present wins.
const PRESCRIPTION_DATES: [&str; 4] = ["prescribed_at", "issued_at", "created_at", "date"];

fn prescription_settled(r: &BTreeMap<String, String>) -> bool {
    r.get("status")
        .is_some_and(|s| PRESCRIPTION_SETTLED.contains(&s.trim().to_lowercase().as_str()))
}

fn prescription_date(r: &BTreeMap<String, String>) -> &str {
    PRESCRIPTION_DATES
        .iter()
        .find_map(|k| r.get(*k))
        .map_or("", |d| d.trim())
}

/// The fan-out after `prescription.list`: `dispense.read{prescription_id}` of at most [`MAX_DISPENSE_READS`] listed
/// prescriptions, active ones first, then newest first (rows without a date keep list order after dated ones). The step
/// index is the rank, so the same rows always give the same steps. Rows without an id and repeated ids are skipped.
pub fn dispense_stage2(prescriptions: &[BTreeMap<String, String>]) -> Vec<CtxStep> {
    let mut ranked: Vec<&BTreeMap<String, String>> = prescriptions.iter().collect();
    ranked.sort_by(|a, b| {
        prescription_settled(a)
            .cmp(&prescription_settled(b))
            .then_with(|| prescription_date(b).cmp(prescription_date(a)))
    });
    let mut seen: Vec<&str> = Vec::new();
    for r in ranked {
        let Some(id) = r
            .get("prescription_id")
            .map(|i| i.trim())
            .filter(|i| !i.is_empty())
        else {
            continue;
        };
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        if seen.len() == MAX_DISPENSE_READS {
            break;
        }
    }
    seen.iter()
        .enumerate()
        .map(|(i, id)| {
            step(
                &format!("{STEP_PREFIX}dispense.read:{i}"),
                "dispense.read",
                "prescription_id",
                id,
            )
        })
        .collect()
}

/// The prescription rows as the answers so far give them (empty when `prescription.list` failed or has not answered).
pub fn prescriptions_of(results: &BTreeMap<String, StepRes>) -> Vec<BTreeMap<String, String>> {
    results
        .get(PRESCRIPTION_LIST_STEP)
        .filter(|r| r.ok)
        .map(|r| parse_records(&r.output))
        .unwrap_or_default()
}

/// A read's recorded answer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StepRes {
    pub ok: bool,
    pub code: Option<String>,
    pub output: Map<String, Value>,
}

/// The order list as the answers so far give it (empty when `order.list` failed or has not answered).
pub fn orders_of(results: &BTreeMap<String, StepRes>) -> Vec<OrderCtx> {
    results
        .get(ORDER_LIST_STEP)
        .filter(|r| r.ok)
        .map(|r| parse_orders(&r.output))
        .unwrap_or_default()
}

/// Has every read answered? `stage1_names` are the steps dispatched first; the fan-out is derived from the order list
/// and `delivery_reads` (the [`reads_deliveries`] decision taken when stage 1 was dispatched).
pub fn complete(
    results: &BTreeMap<String, StepRes>,
    stage1_names: &[String],
    delivery_reads: bool,
) -> bool {
    complete_with(results, stage1_names, delivery_reads, false)
}

/// [`complete`] that also waits for the [`dispense_stage2`] reads when `dispense_reads` ([`reads_dispenses`], decided
/// when stage 1 was dispatched).
pub fn complete_with(
    results: &BTreeMap<String, StepRes>,
    stage1_names: &[String],
    delivery_reads: bool,
    dispense_reads: bool,
) -> bool {
    if !stage1_names.iter().all(|n| results.contains_key(n)) {
        return false;
    }
    stage2(&orders_of(results), delivery_reads)
        .iter()
        .chain(
            dispense_reads
                .then(|| dispense_stage2(&prescriptions_of(results)))
                .iter()
                .flatten(),
        )
        .all(|s| results.contains_key(&s.name))
}

/// The context the answers give.
pub fn assemble(party: &str, results: &BTreeMap<String, StepRes>) -> CustomerContext {
    let mut c = CustomerContext {
        party: party.to_owned(),
        orders: orders_of(results),
        ..CustomerContext::default()
    };
    for (name, r) in results {
        if !r.ok {
            c.degraded
                .push(name.trim_start_matches(STEP_PREFIX).to_owned());
        }
    }
    if let Some(r) = results.get("ctx:payment.list").filter(|r| r.ok) {
        c.payments = parse_payments(&r.output);
    }
    if let Some(r) = results.get("ctx:delivery.list").filter(|r| r.ok) {
        c.deliveries = parse_deliveries(&r.output);
    }
    if let Some(r) = results.get("ctx:return.list").filter(|r| r.ok) {
        c.returns = parse_returns(&r.output);
    }
    // a record list that answered is kept, even empty; a failed one is only in `degraded`
    for op in crate::runtime::resolve::record_ops() {
        if let Some(r) = results.get(&format!("{STEP_PREFIX}{op}")).filter(|r| r.ok) {
            c.records.insert(op.to_owned(), parse_records(&r.output));
        }
    }
    // the dispense reads that answered: rows of `dispense.read`, each carrying the prescription it was read for
    for s in dispense_stage2(&prescriptions_of(results)) {
        let Some(r) = results.get(&s.name).filter(|r| r.ok) else {
            continue;
        };
        let Some(id) = s.input.get("prescription_id").and_then(Value::as_str) else {
            continue;
        };
        let read = c.records.entry("dispense.read".to_owned()).or_default();
        for mut row in parse_records(&r.output) {
            row.insert("prescription_id".to_owned(), id.to_owned());
            read.push(row);
        }
    }
    for s in stage2(&c.orders.clone(), true) {
        let Some(r) = results.get(&s.name).filter(|r| r.ok) else {
            continue;
        };
        let Some(id) = s.input.get("order_id").and_then(Value::as_str) else {
            continue;
        };
        if s.op == "delivery.read" {
            for mut d in parse_deliveries(&r.output) {
                d.order_id.get_or_insert_with(|| id.to_owned());
                c.deliveries.push(d);
            }
            continue;
        }
        let mut read = items(&r.output);
        if read.is_empty() {
            read = rows(&r.output).first().map(items).unwrap_or_default();
        }
        if let Some(o) = c.orders.iter_mut().find(|o| o.order_id == id) {
            o.items = read;
            if o.status.is_empty() {
                o.status = text(r.output.get("status")).unwrap_or_default();
            }
        }
    }
    c
}

// ---- the in-process driver ---------------------------------------------------------------------------------------

/// Something that runs one context read (the runner with provenance `context`).
pub trait ContextCaller: Send + Sync {
    fn call<'a>(&'a self, step: &'a CtxStep) -> futures_util::future::BoxFuture<'a, StepRes>;
}

/// Load the context of `party` by running the steps one stage after the other through `caller`. The orchestrator
/// runs the same stages over the bus; this is for callers that can wait for the answer in process.
pub async fn load(
    caller: &dyn ContextCaller,
    party: &str,
    offers: impl Fn(&str) -> bool,
) -> CustomerContext {
    let mut results: BTreeMap<String, StepRes> = BTreeMap::new();
    let delivery_reads = reads_deliveries(&offers);
    for s in stage1(party, &offers) {
        let r = caller.call(&s).await;
        results.insert(s.name.clone(), r);
    }
    for s in stage2(&orders_of(&results), delivery_reads) {
        let r = caller.call(&s).await;
        results.insert(s.name.clone(), r);
    }
    if reads_dispenses(&offers) {
        for s in dispense_stage2(&prescriptions_of(&results)) {
            let r = caller.call(&s).await;
            results.insert(s.name.clone(), r);
        }
    }
    assemble(party, &results)
}

// ---- the snapshot ------------------------------------------------------------------------------------------------

/// The Redis hash of conversation `conversation`'s snapshot: fields `snapshot` (JSON), `at` (epoch ms), `stale`.
pub fn cache_key(bus: &Bus, tenant: Uuid, job_key: &str, conversation: &str) -> String {
    bus.root_key(&format!("custctx:{tenant}:{job_key}:{conversation}"))
}

/// Store the snapshot (fresh, not stale).
pub async fn put_snapshot(
    bus: &Bus,
    tenant: Uuid,
    job_key: &str,
    conversation: &str,
    ctx: &CustomerContext,
    now_ms: i64,
) -> Result<()> {
    let key = cache_key(bus, tenant, job_key, conversation);
    let mut c = bus.conn();
    let _: () = redis::pipe()
        .hset(&key, "snapshot", serde_json::to_string(ctx)?)
        .hset(&key, "at", now_ms)
        .hdel(&key, "stale")
        .expire(&key, SNAPSHOT_TTL_SECS)
        .query_async(&mut c)
        .await?;
    Ok(())
}

/// The stored snapshot and its age mark, whatever its age: `(context, at_ms, stale)`.
pub async fn read_snapshot(
    bus: &Bus,
    tenant: Uuid,
    job_key: &str,
    conversation: &str,
) -> Result<Option<(CustomerContext, i64, bool)>> {
    let key = cache_key(bus, tenant, job_key, conversation);
    let mut c = bus.conn();
    let (snap, at, stale): (Option<String>, Option<i64>, Option<String>) = redis::pipe()
        .hget(&key, "snapshot")
        .hget(&key, "at")
        .hget(&key, "stale")
        .query_async(&mut c)
        .await?;
    let Some(snap) = snap else { return Ok(None) };
    Ok(Some((
        serde_json::from_str(&snap)?,
        at.unwrap_or(0),
        stale.is_some(),
    )))
}

/// Is a snapshot reusable now by a case of `party`?
pub fn is_fresh(snap: &(CustomerContext, i64, bool), party: &str, now_ms: i64) -> bool {
    let (ctx, at, stale) = snap;
    !stale && ctx.party == party && now_ms.saturating_sub(*at) < CACHE_TTL_MS
}

/// A write happened in the conversation: its snapshot is stale (the next case reads again). Best effort.
pub async fn invalidate(bus: &Bus, tenant: Uuid, job_key: &str, conversation: &str) {
    let key = cache_key(bus, tenant, job_key, conversation);
    let mut c = bus.conn();
    let res: redis::RedisResult<()> = async {
        let exists: bool = c.exists(&key).await?;
        if exists {
            let _: () = c.hset(&key, "stale", 1).await?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(conversation, "customer context not invalidated: {e:#}");
    }
}

// ---- the party's cases -------------------------------------------------------------------------------------------

/// The newest cases of `party` (and of conversation `conversation`), at most 20. In the caller's tenant + job scope.
pub async fn party_cases(
    c: &mut PgConnection,
    party: &str,
    conversation: &str,
) -> Result<Vec<CaseCtx>> {
    use crate::conversations::state as st;
    let rows: Vec<(String, String, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT case_id, status, title, started_at FROM agent_case \
          WHERE (CASE WHEN {known} THEN {subject} END) = $1 OR conversation_id = $2 \
          ORDER BY started_at DESC, case_id DESC LIMIT 20",
            known = st::known_sql("agent_case"),
            subject = st::sub("agent_case", st::SUBJECT),
        )))
        .bind(party)
        .bind(conversation)
        .fetch_all(&mut *c)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(case_id, state, title, at)| CaseCtx {
            case_id,
            state,
            title,
            opened_at: crate::conversations::iso(at),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn retail_only(op: &str) -> bool {
        matches!(
            op,
            "order.list"
                | "order.read"
                | "delivery.read"
                | "delivery.list"
                | "payment.list"
                | "return.list"
        )
    }

    #[test]
    fn the_allowlist_is_reads_only() {
        assert_eq!(CONTEXT_OPS.len(), 21);
        assert_eq!(MAX_RECORD_READS, 14);
        assert_eq!(MAX_CONTEXT_CALLS, 29);
        assert!(
            !CONTEXT_OPS.contains(&PARTY_LOOKUP_OP),
            "the lookup is admitted per case, not allowlisted"
        );
        for op in ["premium_notice.list", "contact_point.list", "asset.list"] {
            assert!(CONTEXT_OPS.contains(&op), "{op}");
        }
        for op in CONTEXT_OPS {
            assert!(is_context_op(op));
        }
        for op in [
            "payment.refund",
            "order.cancel",
            "delivery.redirect",
            "case.create",
        ] {
            assert!(!is_context_op(op), "{op}");
        }
    }

    #[test]
    fn columns_become_records_and_a_scalar_is_one() {
        let two = rows(&m(json!({"order_id": ["A", "B"], "status": ["x", "y"]})));
        assert_eq!(two.len(), 2);
        assert_eq!(two[1]["order_id"], "B");
        let one = rows(&m(
            json!({"order_id": "A", "items": [{"name": "Rug", "qty": 2}, {"name": "Lamp"}]}),
        ));
        assert_eq!(one.len(), 1);
        assert_eq!(
            items(&one[0]).len(),
            2,
            "a one-row answer keeps its whole item list"
        );
    }

    #[test]
    fn orders_come_newest_first_and_only_itemless_ones_are_read() {
        let list = m(json!({
            "order_id": ["O1", "O2", "O3"], "status": ["delivered", "shipped", "paid"],
            "placed_at": ["2026-10-01", "2026-10-20", "2026-10-10"],
            "items": [[{"name": "Rug", "qty": 1}], [], [{"name": "Lamp", "qty": 1}]]
        }));
        let orders = parse_orders(&list);
        assert_eq!(
            orders
                .iter()
                .map(|o| o.order_id.as_str())
                .collect::<Vec<_>>(),
            ["O2", "O3", "O1"]
        );
        let reads = stage2(&orders, false);
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].input["order_id"], "O2");
    }

    #[test]
    fn the_fan_out_is_capped_and_stays_within_the_call_budget() {
        let ids: Vec<String> = (0..15).map(|i| format!("O{i:02}")).collect();
        let list = m(json!({"order_id": ids, "status": vec!["paid"; 15]}));
        let orders = parse_orders(&list);
        let s1 = stage1("C-1", |_| true);
        // order.list and the three retail lists, then every registered record list (payment.list is read once)
        let records = crate::runtime::resolve::record_ops()
            .into_iter()
            .filter(|op| *op != "payment.list")
            .count();
        assert_eq!(s1.len(), 4 + records, "every record list is offered");
        let s2 = stage2(&orders, false);
        assert_eq!(s2.len(), MAX_ORDER_READS);
        assert!(i64::try_from(s1.len() + s2.len()).unwrap() <= MAX_CONTEXT_CALLS);
    }

    #[test]
    fn a_design_without_delivery_list_asks_nothing_for_it() {
        let s = stage1("C-1", |op| retail_only(op) && op != "delivery.list");
        let ops: Vec<&str> = s.iter().map(|s| s.op).collect();
        assert_eq!(ops, ["order.list", "payment.list", "return.list"]);
    }

    #[test]
    fn the_first_reads_and_the_fan_outs_share_one_call_budget() {
        let s1 = stage1("C-1", retail_only);
        assert_eq!(
            s1.iter().map(|s| s.op).collect::<Vec<_>>(),
            ["order.list", "payment.list", "delivery.list", "return.list"]
        );
        assert!(is_context_op("return.list"));
        // with `delivery.list` there are four first reads and no delivery.read fallback
        assert_eq!(s1.len() + MAX_ORDER_READS, 10);
        // without it the fallback keeps its two reads: three first reads + six order reads + two = the cap
        assert_eq!(MAX_DELIVERY_READS, 2);
        let s1_fallback = stage1("C-1", |op| retail_only(op) && op != "delivery.list");
        assert_eq!(
            i64::try_from(s1_fallback.len() + MAX_ORDER_READS + MAX_DELIVERY_READS).unwrap(),
            BASE_CONTEXT_CALLS,
            "three first reads + six order reads + two delivery reads = the cap"
        );
    }

    #[test]
    fn a_design_without_return_list_asks_nothing_for_it() {
        let s = stage1("C-1", |op| retail_only(op) && op != "return.list");
        assert!(s.iter().all(|s| s.op != "return.list"));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn returns_parse_from_columns_and_a_failed_read_leaves_them_empty() {
        let list = ok(json!({
            "return_id": ["RT-1", "RT-2"], "order_id": ["O1", "O2"],
            "status": ["open", "refunded"], "created_at": ["2026-10-20", "2026-09-01"]
        }));
        let mut r = BTreeMap::new();
        r.insert(
            ORDER_LIST_STEP.to_owned(),
            ok(json!({"order_id": ["O1"], "status": ["paid"], "items": [[{"name": "Mug"}]]})),
        );
        r.insert("ctx:return.list".into(), list);
        let c = assemble("C-1", &r);
        assert_eq!(c.returns.len(), 2);
        assert_eq!(c.returns[0].order_id.as_deref(), Some("O1"));
        assert_eq!(c.returns[0].status, "open");
        assert_eq!(c.returns[1].created_at.as_deref(), Some("2026-09-01"));
        assert!(c.degraded.is_empty());

        r.insert(
            "ctx:return.list".into(),
            StepRes {
                ok: false,
                code: Some("timeout".into()),
                output: Map::new(),
            },
        );
        let c = assemble("C-1", &r);
        assert!(c.returns.is_empty());
        assert_eq!(c.orders.len(), 1, "the other reads still count");
        assert_eq!(c.degraded, ["return.list"]);
    }

    #[test]
    fn an_old_snapshot_without_returns_still_decodes() {
        let c: CustomerContext = serde_json::from_value(json!({
            "party": "C-1", "orders": [], "payments": [], "deliveries": [], "cases": [], "degraded": []
        }))
        .unwrap();
        assert!(c.returns.is_empty());
    }

    #[test]
    fn refunds_are_summed_per_order() {
        let p = parse_payments(&m(json!({
            "payment_id": ["P1", "R1"], "order_id": ["O1", "O1"], "direction": ["payment", "refund"],
            "amount": [100.0, 30.0], "status": ["captured", "completed"]
        })));
        assert_eq!(p[0].refunded_amount, Some(30.0));
        assert_eq!(p[1].refunded_amount, None);
    }

    #[test]
    fn a_failed_read_narrows_the_context_and_is_named() {
        let mut r = BTreeMap::new();
        r.insert(
            ORDER_LIST_STEP.to_owned(),
            StepRes {
                ok: true,
                code: None,
                output: m(json!({"order_id": ["O1"], "status": ["paid"]})),
            },
        );
        r.insert(
            "ctx:payment.list".to_owned(),
            StepRes {
                ok: false,
                code: Some("timeout".into()),
                output: Map::new(),
            },
        );
        let c = assemble("C-1", &r);
        assert_eq!(c.orders.len(), 1);
        assert!(c.payments.is_empty());
        assert_eq!(c.degraded, ["payment.list"]);
        let names = vec![ORDER_LIST_STEP.to_owned(), "ctx:payment.list".to_owned()];
        assert!(
            !complete(&r, &names, false),
            "order.read:0 has not answered"
        );
    }

    fn delivery_orders() -> Vec<OrderCtx> {
        let ids: Vec<String> = (0..8).map(|i| format!("O{i}")).collect();
        let st = [
            "delivered",
            "in_transit",
            "cancelled",
            "processing",
            "shipped",
            "paid",
            "in_transit",
            "returned",
        ];
        parse_orders(&m(json!({"order_id": ids, "status": st})))
    }

    #[test]
    fn without_delivery_list_in_flight_orders_get_a_capped_delivery_read() {
        assert!(reads_deliveries(|op| op != "delivery.list"));
        let s = stage2(&delivery_orders(), true);
        let d: Vec<&CtxStep> = s.iter().filter(|s| s.op == "delivery.read").collect();
        assert_eq!(d.len(), MAX_DELIVERY_READS);
        assert_eq!(
            d.iter()
                .map(|s| s.input["order_id"].clone())
                .collect::<Vec<_>>(),
            [json!("O1"), json!("O3")],
            "in flight only, newest first, within what the three list reads leave"
        );
        assert_eq!(d[0].name, "ctx:delivery.read:1");
        let s1 = stage1("C-1", |op| retail_only(op) && op != "delivery.list");
        assert!(i64::try_from(s1.len() + s.len()).unwrap() <= BASE_CONTEXT_CALLS);
    }

    #[test]
    fn delivery_list_wins_over_delivery_read() {
        assert!(!reads_deliveries(|_| true));
        assert!(!reads_deliveries(|op| op != "delivery.read"));
        let s = stage2(&delivery_orders(), false);
        assert!(s.iter().all(|s| s.op != "delivery.read"));
    }

    fn ok(v: Value) -> StepRes {
        StepRes {
            ok: true,
            code: None,
            output: m(v),
        }
    }

    #[test]
    fn deliveries_come_from_delivery_read_answers_and_equal_the_list_shape() {
        let list = ok(json!({"order_id": ["O1", "O2"], "status": ["in_transit", "paid"]}));
        let mut by_read = BTreeMap::new();
        by_read.insert(ORDER_LIST_STEP.to_owned(), list.clone());
        by_read.insert("ctx:order.read:0".into(), ok(json!({"items": ["Mug"]})));
        by_read.insert("ctx:order.read:1".into(), ok(json!({"items": ["Rug"]})));
        by_read.insert(
            "ctx:delivery.read:0".into(),
            ok(
                json!({"tracking_ref": "SHP-1", "status": "in_transit", "carrier": "DHL",
                      "expected_window": "21 Oct, 14-17"}),
            ),
        );
        by_read.insert(
            "ctx:delivery.read:1".into(),
            StepRes {
                ok: false,
                code: Some("timeout".into()),
                output: Map::new(),
            },
        );
        let c = assemble("C-1", &by_read);
        assert_eq!(c.deliveries.len(), 1);
        assert_eq!(c.deliveries[0].order_id.as_deref(), Some("O1"));
        assert_eq!(c.degraded, ["delivery.read:1"]);

        let mut by_list = BTreeMap::new();
        by_list.insert(ORDER_LIST_STEP.to_owned(), list);
        by_list.insert("ctx:order.read:0".into(), ok(json!({"items": ["Mug"]})));
        by_list.insert("ctx:order.read:1".into(), ok(json!({"items": ["Rug"]})));
        by_list.insert(
            "ctx:delivery.list".into(),
            ok(
                json!({"order_id": ["O1"], "tracking_ref": ["SHP-1"], "status": ["in_transit"],
                      "carrier": ["DHL"], "expected_window": ["21 Oct, 14-17"]}),
            ),
        );
        assert_eq!(
            assemble("C-1", &by_list).deliveries,
            c.deliveries,
            "the same parcel parses to the same DeliveryCtx by either path"
        );
    }

    #[test]
    fn complete_waits_for_the_delivery_reads_only_when_asked() {
        let mut r = BTreeMap::new();
        r.insert(
            ORDER_LIST_STEP.to_owned(),
            ok(json!({"order_id": ["O1"], "status": ["in_transit"], "items": [[{"name": "Mug"}]]})),
        );
        let names = vec![ORDER_LIST_STEP.to_owned()];
        assert!(complete(&r, &names, false), "no fan-out without the flag");
        assert!(
            !complete(&r, &names, true),
            "delivery.read:0 has not answered"
        );
        r.insert(
            "ctx:delivery.read:0".into(),
            ok(json!({"status": "in_transit"})),
        );
        assert!(complete(&r, &names, true));
    }

    #[test]
    fn a_retail_design_reads_no_record_list_and_the_cap_holds_with_all() {
        let ops: Vec<&str> = stage1("C-1", retail_only).iter().map(|s| s.op).collect();
        assert!(ops.iter().all(|o| retail_only(o)));
        for k in crate::runtime::resolve::RECORD_KINDS {
            assert!(
                k.list_op == crate::runtime::resolve::CASES_SOURCE || is_context_op(k.list_op),
                "{} is not on the allowlist",
                k.list_op
            );
        }
        let all = stage1("C-1", |_| true);
        let extra = all.iter().filter(|s| !retail_only(s.op)).count();
        assert_eq!(i64::try_from(extra).unwrap(), MAX_RECORD_READS);
        assert!(i64::try_from(all.len() + MAX_ORDER_READS).unwrap() <= MAX_CONTEXT_CALLS);
        // the party is always the verified one
        assert!(all.iter().all(|s| s.input["party_id"] == "C-1"));
    }

    #[test]
    fn an_insurance_design_without_order_list_reads_only_its_records() {
        let offers = |op: &str| matches!(op, "policy.list" | "claim.list");
        assert!(offers_any(offers));
        assert!(!offers_any(|_| false));
        let s = stage1("C-1", offers);
        assert_eq!(
            s.iter().map(|s| s.op).collect::<Vec<_>>(),
            ["policy.list", "claim.list"]
        );
    }

    #[test]
    fn record_lists_keep_answered_reads_even_empty_and_name_failed_ones() {
        let mut r = BTreeMap::new();
        r.insert(
            "ctx:policy.list".into(),
            ok(
                json!({"policy_id": ["P1", "P2"], "line_of_business": ["Car insurance", "Home"],
                      "renewal_date": ["2026-03-03", "2026-05-01"]}),
            ),
        );
        r.insert("ctx:claim.list".into(), ok(json!({})));
        r.insert(
            "ctx:payslip.list".into(),
            StepRes {
                ok: false,
                code: Some("timeout".into()),
                output: Map::new(),
            },
        );
        let c = assemble("C-1", &r);
        assert_eq!(c.records["policy.list"].len(), 2);
        assert_eq!(c.records["policy.list"][1]["policy_id"], "P2");
        assert!(c.records["claim.list"].is_empty(), "answered, none");
        assert!(!c.records.contains_key("payslip.list"), "failed: no key");
        assert_eq!(c.degraded, ["payslip.list"]);
        let old: CustomerContext = serde_json::from_value(json!({
            "party": "C-1", "orders": [], "payments": [], "deliveries": [], "cases": [], "degraded": []
        }))
        .unwrap();
        assert!(old.records.is_empty());
    }

    #[test]
    fn a_staff_conversation_is_read_as_the_verified_email_never_a_typed_one() {
        // the session says anna@corp.example (verified); the message and the model's slots say someone else
        let typed_in_message = "my laptop is broken, I am boss@corp.example";
        let model_slots = [
            ("email", "boss@corp.example"),
            ("party_id", "boss@corp.example"),
        ];
        let party = staff_party(Some(" Anna@Corp.Example "), true).unwrap();
        assert_eq!(party, "anna@corp.example");
        let steps = stage1(&party, |_| true);
        assert!(!steps.is_empty());
        for s in &steps {
            assert_eq!(s.input["party_id"], "anna@corp.example", "{}", s.name);
            assert!(!typed_in_message.contains(s.input["party_id"].as_str().unwrap()));
            for (_, v) in model_slots {
                assert_ne!(s.input["party_id"], v);
            }
        }
        // an unverified or absent email reads nothing
        assert_eq!(staff_party(Some("anna@corp.example"), false), None);
        assert_eq!(staff_party(None, true), None);
        assert_eq!(staff_party(Some("not an email"), true), None);
    }

    // ---- dispense fan-out (prescription.list -> dispense.read) --------------------------------------------------

    fn rx(id: &str, status: &str, at: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("prescription_id".to_owned(), id.to_owned()),
            ("status".to_owned(), status.to_owned()),
            ("prescribed_at".to_owned(), at.to_owned()),
        ])
    }

    #[test]
    fn the_dispense_fan_out_needs_both_operations() {
        assert!(reads_dispenses(|_| true));
        assert!(!reads_dispenses(|op| op == "prescription.list"));
        assert!(!reads_dispenses(|op| op == "dispense.read"));
        assert!(is_context_op("dispense.read"));
    }

    #[test]
    fn the_dispense_fan_out_is_capped_active_and_newest_first() {
        let rows = vec![
            rx("RX-1", "dispensed", "2026-09-01"),
            rx("RX-2", "active", "2026-01-01"),
            rx("RX-3", "active", "2026-03-01"),
            rx("RX-4", "active", "2026-02-01"),
            rx("RX-5", "active", "2026-04-01"),
            rx("RX-5", "active", "2026-04-01"),
        ];
        let s = dispense_stage2(&rows);
        assert_eq!(s.len(), MAX_DISPENSE_READS);
        let ids: Vec<&str> = s
            .iter()
            .map(|x| x.input["prescription_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["RX-5", "RX-3", "RX-4"]);
        assert_eq!(s[0].name, "ctx:dispense.read:0");
        assert!(s.iter().all(|x| x.op == "dispense.read"));
        assert!(dispense_stage2(&[]).is_empty());
        // all stage-1 reads + order reads + dispense reads stay within the call budget
        let s1 = stage1("C-1", |_| true);
        assert!(i64::try_from(s1.len() + MAX_ORDER_READS + s.len()).unwrap() <= MAX_CONTEXT_CALLS);
    }

    #[test]
    fn dispense_rows_carry_their_prescription_and_complete_waits_for_them() {
        let mut r = BTreeMap::new();
        r.insert(
            PRESCRIPTION_LIST_STEP.to_owned(),
            ok(json!({"prescription_id": ["RX-1", "RX-2"], "status": ["active", "active"]})),
        );
        let names = vec![PRESCRIPTION_LIST_STEP.to_owned()];
        assert!(complete(&r, &names, false), "off: nothing more to wait for");
        assert!(!complete_with(&r, &names, false, true));
        r.insert(
            "ctx:dispense.read:0".into(),
            ok(json!({"dispense_id": "D-1", "status": "prepared", "fulfilment": "collection"})),
        );
        assert!(!complete_with(&r, &names, false, true));
        r.insert(
            "ctx:dispense.read:1".into(),
            ok(json!({"dispense_id": "D-2"})),
        );
        assert!(complete_with(&r, &names, false, true));
        let c = assemble("C-1", &r);
        let d = &c.records["dispense.read"];
        assert_eq!(d.len(), 2);
        assert_eq!(d[0]["prescription_id"], "RX-1");
        assert_eq!(d[0]["dispense_id"], "D-1");
        assert_eq!(d[1]["prescription_id"], "RX-2");
    }

    #[test]
    fn a_failed_dispense_read_is_degraded_never_fatal() {
        let mut r = BTreeMap::new();
        r.insert(
            PRESCRIPTION_LIST_STEP.to_owned(),
            ok(json!({"prescription_id": ["RX-1", "RX-2"], "status": ["active", "active"]})),
        );
        r.insert(
            "ctx:dispense.read:0".into(),
            StepRes {
                ok: false,
                code: Some("unavailable".into()),
                output: Map::new(),
            },
        );
        r.insert(
            "ctx:dispense.read:1".into(),
            ok(json!({"dispense_id": "D-2"})),
        );
        let names = vec![PRESCRIPTION_LIST_STEP.to_owned()];
        assert!(
            complete_with(&r, &names, false, true),
            "a failed read has answered"
        );
        let c = assemble("C-1", &r);
        assert_eq!(c.degraded, ["dispense.read:0"]);
        let d = &c.records["dispense.read"];
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["prescription_id"], "RX-2");
        // every read failed: no key, still assembled
        r.insert(
            "ctx:dispense.read:1".into(),
            StepRes {
                ok: false,
                code: None,
                output: Map::new(),
            },
        );
        assert!(!assemble("C-1", &r).records.contains_key("dispense.read"));
    }

    #[test]
    fn the_staff_test_lookup_is_admitted_for_its_own_email_only() {
        let go = |src, op, input: Value, me| staff_test_lookup(src, op, &m(input), me);
        let own = json!({"email": "Ann@Example.org"});
        assert!(go(
            Some("staff_test"),
            "party.list",
            own.clone(),
            Some("ann@example.org")
        ));
        // not a web chat's assertion, not another operation, not another email, not extra keys, not without a party
        assert!(!go(
            Some("assertion"),
            "party.list",
            own.clone(),
            Some("ann@example.org")
        ));
        assert!(!go(
            Some("staff_test"),
            "party.read",
            own.clone(),
            Some("ann@example.org")
        ));
        assert!(!go(
            Some("staff_test"),
            "party.list",
            own.clone(),
            Some("bo@example.org")
        ));
        assert!(!go(Some("staff_test"), "party.list", own, None));
        assert!(!go(
            Some("staff_test"),
            "party.list",
            json!({"email": "ann@example.org", "phone": "1"}),
            Some("ann@example.org")
        ));
        assert!(needs_lookup(
            Some("staff_test"),
            "ann@example.org",
            Some("Ann@example.org")
        ));
        assert!(!needs_lookup(
            Some("staff_test"),
            "C-1",
            Some("ann@example.org")
        ));
        assert!(!needs_lookup(
            Some("assertion"),
            "ann@example.org",
            Some("ann@example.org")
        ));
    }

    #[test]
    fn the_lookup_binds_exactly_one_party() {
        assert_eq!(
            lookup_outcome(&m(json!({"party_id": ["C-1"], "roles": [["customer"]]}))),
            PartyLookup::One("C-1".into())
        );
        assert_eq!(
            lookup_outcome(&m(json!({"party_id": "C-1"}))),
            PartyLookup::One("C-1".into())
        );
        assert_eq!(
            lookup_outcome(&m(json!({"party_id": ["C-1", "C-1"]}))),
            PartyLookup::One("C-1".into()),
            "the same party twice is one party"
        );
        assert_eq!(
            lookup_outcome(&m(json!({"party_id": []}))),
            PartyLookup::NotFound
        );
        assert_eq!(lookup_outcome(&m(json!({}))), PartyLookup::NotFound);
        assert_eq!(
            lookup_outcome(&m(json!({"party_id": ["C-1", "C-2"]}))),
            PartyLookup::Ambiguous
        );
        assert!(
            lookup_note(&[PARTY_NOT_FOUND.to_owned()])
                .unwrap()
                .contains("No customer")
        );
        assert!(lookup_note(&["order.list".to_owned()]).is_none());
    }
}
