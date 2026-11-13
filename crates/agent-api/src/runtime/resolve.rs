//! Resolving "my order / my parcel / my refund" from the identified customer's own records (B4 known customer,
//! plan §2.3; owner ruling 2026-10-26: the customer is never asked for an order number, a payment id or a tracking
//! reference).
//!
//! - **System slots** ([`SYSTEM_SLOTS`]): `order_id`, `payment_id`, `delivery_id`, `tracking_ref`. The qualifier never
//!   asks for them and never keeps what the model extracted for them; a product or date the customer mentions
//!   ("the rug", "the one from 12 October", "the first one") is kept as the transient [`HINT_SLOT`].
//! - **Resolution** ([`resolve`]) runs in the orchestrator once the [`CustomerContext`] is loaded. Order of the rules:
//!   1. the hint narrows the customer's orders (a date, product words, an ordinal); a hint that matches nothing is
//!      [`Resolved::Nothing`];
//!   2. one order left: that one;
//!   3. else the space's own ranking ([`Kind`]): status and tracking prefer the undelivered order (the one in trouble
//!      first, then the newest); delivery problems prefer the late or failed shipment; returns and warranty the
//!      delivered orders (returns inside the window first); refunds the orders with a refund or a payment problem;
//!      cancellations the orders still open;
//!   4. exactly one best candidate resolves silently, several are [`Resolved::Ambiguous`] (at most
//!      [`MAX_CHOICES`], the newest, and `more` when older ones were left out).
//! - **Ambiguity** is asked in plain words ([`choice_question`]) and the answer is mapped by the same matcher
//!   ([`choose`]); the ids of the candidates stay on the server ([`Choice`]).
//! - **Labels** ([`Pick::label`], [`label_of`]): "Jute rug ordered on 12 October". Replies print the label, never an id.
//!
//! Everything here is pure (no I/O, the date is a parameter), so it is table-tested.
use std::collections::BTreeMap;

use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use super::customer_context::{CustomerContext, DeliveryCtx, OrderCtx, PaymentCtx};

/// Slots that are a record's lookup key: resolved from the customer's records, never asked.
pub const SYSTEM_SLOTS: [&str; 4] = ["order_id", "payment_id", "delivery_id", "tracking_ref"];

/// Slots resolved from the conversation's own attachments, never asked: `photo_ref` is the newest image the
/// customer sent in THIS conversation ([`crate::attachments::store::newest_customer_image`]).
pub const ATTACHMENT_SLOTS: [&str; 1] = [PHOTO_SLOT];

/// The slot holding the newest customer image of the conversation.
pub const PHOTO_SLOT: &str = "photo_ref";

/// The transient slot holding what the customer said to point at one of their orders.
pub const HINT_SLOT: &str = "item_hint";

/// A return is offered for an order delivered at most this many days ago (a ranking preference, not the rule).
pub const RETURN_WINDOW_DAYS: i64 = 30;

/// A delivery problem is about a parcel delivered at most this many days ago, when none is still on its way.
pub const PROBLEM_WINDOW_DAYS: i64 = 14;

/// The most candidates a question names.
pub const MAX_CHOICES: usize = 3;

/// How long a choice question waits for its answer.
pub const CHOICE_TTL_MS: i64 = 30 * 60 * 1000;

/// Is `slot` a system slot: a retail one ([`SYSTEM_SLOTS`]), an attachment one ([`ATTACHMENT_SLOTS`]) or the lookup
/// key of a registered record kind ([`RECORD_KINDS`])? None is ever asked.
pub fn is_system_slot(slot: &str) -> bool {
    SYSTEM_SLOTS.contains(&slot)
        || slot == UPN_SLOT
        || is_attachment_slot(slot)
        || record_kind(slot).is_some()
}

/// The employee's own login (their UPN, which is their verified Keycloak email). A system slot, never asked: filled
/// server-side from the case's verified party ([`staff_upn`]), never from message text.
pub const UPN_SLOT: &str = "upn";

/// The employee's UPN for a case of a job, from the case's verified party and nothing else. `employees` is the job's
/// `jobs.audience = 'employees'` (their conversations are bound to the signed-in employee's verified email, see
/// `customer_context::staff_party`) and the verified party must be that email. A customer-audience job, a customer
/// party (an id) or no party yields `None`.
pub fn staff_upn(employees: bool, verified_party: Option<&str>) -> Option<String> {
    let party = verified_party?.trim();
    (employees && party.contains('@') && !party.contains(char::is_whitespace))
        .then(|| party.to_lowercase())
}

/// Is `slot` resolved from the conversation's attachments ([`ATTACHMENT_SLOTS`])?
pub fn is_attachment_slot(slot: &str) -> bool {
    ATTACHMENT_SLOTS.contains(&slot)
}

// ---- the generic record registry ----------------------------------------------------------------------------------

/// The `list_op` of the kind read from the customer's own cases (no connector read).
pub const CASES_SOURCE: &str = "party_cases";

/// One kind of record a known customer holds (policy, claim, prescription ...): the slot that names it, the list
/// operation that reads the customer's records of that kind, and which fields make a plain-words label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordKind {
    /// The lookup-key slot a space declares (`policy_id`); never asked.
    pub slot: &'static str,
    /// The `context` read that lists the customer's records ([`CASES_SOURCE`] for the customer's cases).
    pub list_op: &'static str,
    /// The field of a row holding the value the slot takes (stays on the server).
    pub id_field: &'static str,
    pub label_field: Option<&'static str>,
    pub date_field: Option<&'static str>,
    /// "renewing", "reported" ... ("" without a date field).
    pub date_word: &'static str,
    /// The thing in plain words ("policy"), for questions and the no-match sentence.
    pub noun: &'static str,
    /// The label already names the thing ("Car insurance"), so the noun is not appended.
    pub bare_label: bool,
}

#[allow(clippy::too_many_arguments)]
const fn rk(
    slot: &'static str,
    list_op: &'static str,
    id_field: &'static str,
    label_field: Option<&'static str>,
    date_field: Option<&'static str>,
    date_word: &'static str,
    noun: &'static str,
    bare_label: bool,
) -> RecordKind {
    RecordKind {
        slot,
        list_op,
        id_field,
        label_field,
        date_field,
        date_word,
        noun,
        bare_label,
    }
}

pub const RECORD_KINDS: &[RecordKind] = &[
    rk(
        "policy_id",
        "policy.list",
        "policy_id",
        Some("line_of_business"),
        Some("renewal_date"),
        "renewing",
        "policy",
        true,
    ),
    rk(
        "claim_number",
        "claim.list",
        "claim_number",
        Some("loss_type"),
        Some("reported_at"),
        "reported",
        "claim",
        false,
    ),
    rk(
        "prescription_ref",
        "prescription.list",
        "prescription_id",
        Some("medication"),
        None,
        "",
        "prescription",
        false,
    ),
    rk(
        "appointment_ref",
        "appointment.list",
        "appointment_id",
        None,
        Some("slot"),
        "on",
        "appointment",
        false,
    ),
    rk(
        "payslip_id",
        "payslip.list",
        "payslip_id",
        Some("period"),
        Some("issue_date"),
        "issued",
        "payslip",
        false,
    ),
    rk(
        "account_id",
        "account.list",
        "account_id",
        Some("kind"),
        Some("opened_at"),
        "opened",
        "account",
        false,
    ),
    rk(
        "account_ref",
        "account.list",
        "account_ref",
        Some("kind"),
        Some("opened_at"),
        "opened",
        "account",
        false,
    ),
    rk(
        "document_ref",
        "invoice.list",
        "document_ref",
        Some("kind"),
        Some("issue_date"),
        "issued",
        "invoice",
        false,
    ),
    rk(
        "payment_ref",
        "payment.list",
        "payment_id",
        Some("method"),
        Some("date"),
        "on",
        "payment",
        false,
    ),
    rk(
        "loyalty_ref",
        "loyalty_account.read",
        "programme",
        Some("programme"),
        None,
        "",
        "loyalty account",
        true,
    ),
    rk(
        "campaign_ref",
        "send.list",
        "campaign_ref",
        Some("campaign_name"),
        Some("sent_at"),
        "sent",
        "campaign message",
        false,
    ),
    // insurance premium notices (seed v16 `premium_notice.list`): the period is the plain word, the due date the date
    rk(
        "premium_notice_id",
        "premium_notice.list",
        "premium_notice_id",
        Some("period"),
        Some("due_date"),
        "due",
        "premium notice",
        false,
    ),
    // the customer's own emails, phones and addresses (seed v16 `contact_point.list`); the label is the kind with the
    // masked value (see `contact_label`), never the raw value
    rk(
        "contact_point_id",
        "contact_point.list",
        "contact_point_id",
        Some("kind"),
        None,
        "",
        "contact detail",
        false,
    ),
    // telecom (seed v1..v14 `subscription.list`): the customer's lines. The number is the key and stays on the server;
    // the plain words are the line's status and the end of its minimum term (no plan name: `offering_id` is an id)
    rk(
        "msisdn",
        "subscription.list",
        "msisdn",
        Some("status"),
        Some("commitment_end"),
        "committed until",
        "line",
        false,
    ),
    // `contract_id` is the billing account of the customer (`contract.read` and `usage.list` own it through
    // `account.list.account_id`); the account kind ("electricity") is the plain word
    rk(
        "contract_id",
        "account.list",
        "account_id",
        Some("kind"),
        Some("opened_at"),
        "opened",
        "contract",
        false,
    ),
    // a bill / a document is an invoice of the customer's (`document.read` owns `document_id` through
    // `invoice.list.document_ref`)
    rk(
        "bill_id",
        "invoice.list",
        "document_ref",
        Some("kind"),
        Some("issue_date"),
        "issued",
        "bill",
        false,
    ),
    rk(
        "document_id",
        "invoice.list",
        "document_ref",
        Some("kind"),
        Some("issue_date"),
        "issued",
        "document",
        false,
    ),
    // the employee's own devices (`asset.list`, owned by `party_id`): the serial is the key and stays on the server;
    // the list has no product name, so the plain words are the warranty date ("device under warranty until 3 March")
    rk(
        "asset_serial",
        "asset.list",
        "serial",
        None,
        Some("warranty_until"),
        "under warranty until",
        "device",
        false,
    ),
    // the IT desk's own tickets (seed v14 `case.list`); the names a ticket-status space would use
    rk(
        "ticket_id",
        "case.list",
        "case_id",
        Some("title"),
        Some("updated_at"),
        "updated",
        "ticket",
        false,
    ),
    rk(
        "incident_ref",
        "case.list",
        "case_id",
        Some("title"),
        Some("updated_at"),
        "updated",
        "ticket",
        false,
    ),
    rk(
        "case_id",
        CASES_SOURCE,
        "case_id",
        Some("title"),
        Some("opened_at"),
        "opened",
        "request",
        false,
    ),
];

/// The `case_id` kind when the design offers `case.list` (the employee's own tickets, ranked and labelled by title and
/// last update); HR's `case_id` otherwise reads the party's cases ([`CASES_SOURCE`]).
const CASE_ID_AS_TICKETS: RecordKind = rk(
    "case_id",
    "case.list",
    "case_id",
    Some("title"),
    Some("updated_at"),
    "updated",
    "request",
    false,
);

/// The kind to resolve `kind` with against `ctx`: `case_id` prefers `case.list` rows when that read answered.
pub fn effective_kind(kind: &'static RecordKind, ctx: &CustomerContext) -> &'static RecordKind {
    if kind.list_op == CASES_SOURCE && ctx.records.contains_key("case.list") {
        &CASE_ID_AS_TICKETS
    } else {
        kind
    }
}

/// The registered kind of `slot`.
pub fn record_kind(slot: &str) -> Option<&'static RecordKind> {
    RECORD_KINDS.iter().find(|k| k.slot == slot)
}

/// The distinct list operations the registry reads (the cases pseudo source excluded), in registry order.
pub fn record_ops() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    for k in RECORD_KINDS {
        if k.list_op != CASES_SOURCE && !v.contains(&k.list_op) {
            v.push(k.list_op);
        }
    }
    v
}

// ---- picks and labels ---------------------------------------------------------------------------------------------

/// One order of the customer, as the resolver hands it on. The ids stay on the server. A pick of a registered record
/// kind ([`RECORD_KINDS`]) reuses the shape: `order_id` holds the record's value for the kind's slot, `items` its label
/// (empty without one), `placed_on` its date.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pick {
    pub order_id: String,
    #[serde(default)]
    pub payment_id: Option<String>,
    #[serde(default)]
    pub delivery_id: Option<String>,
    /// The product names of the order.
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub placed_on: Option<NaiveDate>,
}

pub(crate) fn day_month(d: NaiveDate) -> String {
    d.format("%-d %B").to_string()
}

fn clean(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(60)
        .collect()
}

/// The products in plain words: "Wool rug", "Wool rug and Linen throw", "Wool rug, Linen throw and 2 more".
pub fn items_phrase(items: &[String]) -> String {
    let names: Vec<String> = items
        .iter()
        .map(|n| clean(n))
        .filter(|n| !n.is_empty())
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

impl Pick {
    /// "Jute rug ordered on 12 October" (a reply reads "Your <label> is ...").
    pub fn label(&self) -> String {
        let items = items_phrase(&self.items);
        match (items.is_empty(), self.placed_on) {
            (false, Some(d)) => format!("{items} ordered on {}", day_month(d)),
            (false, None) => items,
            (true, Some(d)) => format!("order placed on {}", day_month(d)),
            (true, None) => "order".to_owned(),
        }
    }

    /// "Jute rug from 12 October" (a question names the candidates this way).
    pub fn short(&self) -> String {
        let items = items_phrase(&self.items);
        match (items.is_empty(), self.placed_on) {
            (false, Some(d)) => format!("{items} from {}", day_month(d)),
            (false, None) => items,
            (true, Some(d)) => format!("order from {}", day_month(d)),
            (true, None) => "order".to_owned(),
        }
    }

    /// The values of the system slots in `needs`.
    pub fn slots(&self, needs: &[&str]) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for n in needs {
            let v = match *n {
                "order_id" => Some(self.order_id.clone()),
                "payment_id" => self.payment_id.clone(),
                "delivery_id" | "tracking_ref" => self.delivery_id.clone(),
                // a record pick (see `resolve_records`) carries its record's value in `order_id`
                r if record_kind(r).is_some() => Some(self.order_id.clone()),
                _ => None,
            };
            if let Some(v) = v {
                m.insert((*n).to_owned(), v);
            }
        }
        m
    }
}

/// The label of `order_id` in `ctx` (None when the customer has no such order).
pub fn label_of(ctx: &CustomerContext, order_id: &str) -> Option<String> {
    let o = ctx.orders.iter().find(|o| o.order_id == order_id)?;
    Some(
        Pick {
            order_id: o.order_id.clone(),
            payment_id: None,
            delivery_id: None,
            items: o.items.iter().map(|i| i.name.clone()).collect(),
            placed_on: o.placed_at.as_deref().and_then(parse_date),
        }
        .label(),
    )
}

// ---- the parked choice --------------------------------------------------------------------------------------------

/// A question waiting for the customer's answer (`chat:{conversation}` field `choice`, written by the orchestrator,
/// read and cleared by the next turn of the drain). `picks` are oldest first, as the question names them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub case_id: String,
    pub context_space: String,
    /// The request's other slots (no system slot, no hint).
    pub slots: BTreeMap<String, String>,
    /// The system slots the space needs.
    pub needs: Vec<String>,
    pub picks: Vec<Pick>,
    /// Epoch ms after which the question is stale.
    pub until_ms: i64,
}

/// The plain-words question naming the candidates (oldest first); never an id.
pub fn choice_question(picks: &[Pick], more: bool) -> String {
    let mut parts: Vec<String> = picks
        .iter()
        .map(|p| format!("your {}", p.short()))
        .collect();
    if more {
        parts.push("an older order".to_owned());
    }
    let last = parts.pop().unwrap_or_default();
    let joined = if parts.is_empty() {
        last
    } else {
        format!("{} or {last}", parts.join(", "))
    };
    format!("Is this about {joined}?")
}

/// The candidate the customer's answer points at ("the rug", "the second one", "the one from 16 October"), if it
/// points at exactly one.
pub fn choose(picks: &[Pick], answer: &str) -> Option<usize> {
    let hint = clean_hint(answer)?;
    let cands: Vec<Cand> = picks
        .iter()
        .map(|p| Cand {
            names: p.items.clone(),
            placed: p.placed_on,
        })
        .collect();
    let idx = narrow(&cands, &hint);
    (idx.len() == 1).then(|| idx[0])
}

// ---- the resolver -------------------------------------------------------------------------------------------------

/// What the customer's records say about the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    One(Pick),
    /// Several equally plausible (oldest first, at most [`MAX_CHOICES`]); `more` when older ones were left out.
    Ambiguous {
        picks: Vec<Pick>,
        more: bool,
    },
    /// No order of the customer fits.
    Nothing,
}

/// How a context space ranks the customer's orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Status,
    Problem,
    Return,
    Warranty,
    Refund,
    Cancel,
    Other,
}

/// The ranking of the space `space` (its key's last segment).
pub fn kind_of(space: &str) -> Kind {
    match space {
        "order_status" | "track_parcel" | "delivery_status" => Kind::Status,
        "delivery_problem" => Kind::Problem,
        "returns" | "return" => Kind::Return,
        "warranty" => Kind::Warranty,
        "refunds" | "refund_status" | "pending_charge" | "payment_error" => Kind::Refund,
        s if s.contains("cancel") || s.contains("modify") || s.contains("change") => Kind::Cancel,
        _ => Kind::Other,
    }
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

fn low(s: &str) -> String {
    s.trim().to_lowercase()
}

const TROUBLE: [&str; 6] = [
    "delayed",
    "exception",
    "lost",
    "failed",
    "returned_to_sender",
    "undeliverable",
];
const PAYMENT_TROUBLE: [&str; 7] = [
    "failed",
    "pending",
    "declined",
    "error",
    "disputed",
    "chargeback",
    "requires_action",
];
const CANCELLABLE: [&str; 8] = [
    "processing",
    "pending",
    "paid",
    "confirmed",
    "created",
    "placed",
    "open",
    "new",
];

struct Row<'a> {
    o: &'a OrderCtx,
    d: Option<&'a DeliveryCtx>,
    pays: Vec<&'a PaymentCtx>,
    placed: Option<NaiveDate>,
}

impl Row<'_> {
    fn delivered(&self) -> bool {
        self.d
            .is_some_and(|d| low(&d.status) == "delivered" || d.delivered_at.is_some())
            || low(&self.o.status) == "delivered"
    }

    fn cancelled(&self) -> bool {
        matches!(low(&self.o.status).as_str(), "cancelled" | "canceled")
    }

    fn delivered_on(&self) -> Option<NaiveDate> {
        self.d
            .and_then(|d| d.delivered_at.as_deref())
            .and_then(parse_date)
    }

    fn in_trouble(&self, today: NaiveDate) -> bool {
        let d_status = self.d.map(|d| low(&d.status)).unwrap_or_default();
        TROUBLE.contains(&d_status.as_str())
            || TROUBLE.contains(&low(&self.o.status).as_str())
            || (!self.delivered()
                && self
                    .d
                    .and_then(|d| d.eta.as_deref())
                    .and_then(parse_date)
                    .is_some_and(|eta| eta < today))
    }

    fn refund(&self) -> bool {
        self.pays.iter().any(|p| {
            p.refunded_amount.is_some_and(|a| a > 0.0) || low(&p.status).contains("refund")
        })
    }

    fn payment_trouble(&self) -> bool {
        self.pays
            .iter()
            .any(|p| PAYMENT_TROUBLE.contains(&low(&p.status).as_str()))
    }

    fn pick(&self, kind: Kind) -> Pick {
        let payment = if kind == Kind::Refund {
            self.pays
                .iter()
                .find(|p| {
                    p.refunded_amount.is_some_and(|a| a > 0.0) || low(&p.status).contains("refund")
                })
                .or_else(|| {
                    self.pays
                        .iter()
                        .find(|p| PAYMENT_TROUBLE.contains(&low(&p.status).as_str()))
                })
                .or_else(|| self.pays.first())
        } else {
            self.pays.first()
        };
        Pick {
            order_id: self.o.order_id.clone(),
            payment_id: payment.map(|p| p.payment_id.clone()),
            delivery_id: self.d.map(|d| d.delivery_id.clone()),
            items: self.o.items.iter().map(|i| i.name.clone()).collect(),
            placed_on: self.placed,
        }
    }
}

/// The customer's orders that can serve the slots in `needs`, oldest first. Cancelled orders are left out.
fn rows<'a>(ctx: &'a CustomerContext, needs: &[&str]) -> Vec<Row<'a>> {
    let needs_delivery = needs
        .iter()
        .any(|n| matches!(*n, "delivery_id" | "tracking_ref"));
    let needs_payment = needs.contains(&"payment_id");
    let mut v: Vec<Row> = ctx
        .orders
        .iter()
        .rev()
        .map(|o| Row {
            o,
            d: ctx
                .deliveries
                .iter()
                .find(|d| d.order_id.as_deref() == Some(o.order_id.as_str())),
            pays: ctx
                .payments
                .iter()
                .filter(|p| p.order_id.as_deref() == Some(o.order_id.as_str()))
                .collect(),
            placed: o.placed_at.as_deref().and_then(parse_date),
        })
        .collect();
    v.sort_by_key(|r| r.placed);
    v.retain(|r| {
        !r.cancelled()
            && (!needs_delivery || r.d.is_some())
            && (!needs_payment || !r.pays.is_empty())
    });
    v
}

fn many(v: Vec<&Row>, kind: Kind) -> Resolved {
    match v.len() {
        0 => Resolved::Nothing,
        1 => Resolved::One(v[0].pick(kind)),
        n => Resolved::Ambiguous {
            picks: v[n.saturating_sub(MAX_CHOICES)..]
                .iter()
                .map(|r| r.pick(kind))
                .collect(),
            more: n > MAX_CHOICES,
        },
    }
}

fn newest(v: Vec<&Row>, kind: Kind) -> Resolved {
    match v.last() {
        Some(r) => Resolved::One(r.pick(kind)),
        None => Resolved::Nothing,
    }
}

/// Resolve the request of context space `space` (its key's last segment) for the customer of `ctx`. `hint` is what the
/// customer said to point at an order; `needs` the system slots the space has; `today` the date for the windows.
pub fn resolve(
    space: &str,
    ctx: &CustomerContext,
    hint: Option<&str>,
    needs: &[&str],
    today: NaiveDate,
) -> Resolved {
    let kind = kind_of(space);
    let pool = rows(ctx, needs);
    let mut cand: Vec<&Row> = pool.iter().collect();
    if let Some(h) = hint.and_then(clean_hint) {
        let keyed: Vec<Cand> = cand
            .iter()
            .map(|r| Cand {
                names: r.o.items.iter().map(|i| i.name.clone()).collect(),
                placed: r.placed,
            })
            .collect();
        let idx = narrow(&keyed, &h);
        cand = idx.into_iter().map(|i| cand[i]).collect();
    }
    if cand.len() <= 1 {
        return many(cand, kind);
    }
    let pick = |f: &dyn Fn(&&Row) -> bool| -> Vec<&Row> {
        cand.iter().copied().filter(|r| f(r)).collect()
    };
    match kind {
        Kind::Status => {
            let open = pick(&|r| !r.delivered());
            let pool = if open.is_empty() { cand.clone() } else { open };
            let trouble: Vec<&Row> = pool
                .iter()
                .filter(|r| r.in_trouble(today))
                .copied()
                .collect();
            newest(if trouble.is_empty() { pool } else { trouble }, kind)
        }
        Kind::Problem => {
            let trouble = pick(&|r| !r.delivered() && r.in_trouble(today));
            if !trouble.is_empty() {
                return many(trouble, kind);
            }
            let open = pick(&|r| !r.delivered());
            if !open.is_empty() {
                return many(open, kind);
            }
            let recent = pick(&|r| {
                r.delivered_on()
                    .is_none_or(|d| (today - d) <= Duration::days(PROBLEM_WINDOW_DAYS))
            });
            if recent.is_empty() {
                newest(cand.clone(), kind)
            } else {
                many(recent, kind)
            }
        }
        Kind::Return | Kind::Warranty => {
            let delivered = pick(&|r| r.delivered());
            if kind == Kind::Return {
                let window = Duration::days(RETURN_WINDOW_DAYS);
                let inside: Vec<&Row> = delivered
                    .iter()
                    .filter(|r| r.delivered_on().is_none_or(|d| (today - d) <= window))
                    .copied()
                    .collect();
                if !inside.is_empty() {
                    return many(inside, kind);
                }
            }
            many(delivered, kind)
        }
        Kind::Refund => {
            let hit = pick(&|r| r.refund() || r.payment_trouble());
            if !hit.is_empty() {
                return many(hit, kind);
            }
            let paid = pick(&|r| !r.pays.is_empty());
            newest(if paid.is_empty() { cand.clone() } else { paid }, kind)
        }
        Kind::Cancel => {
            let open =
                pick(&|r| !r.delivered() && CANCELLABLE.contains(&low(&r.o.status).as_str()));
            many(if open.is_empty() { cand.clone() } else { open }, kind)
        }
        Kind::Other => many(cand.clone(), kind),
    }
}

// ---- the generic record resolver ----------------------------------------------------------------------------------

/// One record of a registered kind, as the resolver ranks it.
struct Rec {
    id: String,
    label: Option<String>,
    date: Option<NaiveDate>,
    status: Option<String>,
}

const OPEN_STATUS: [&str; 8] = [
    "open",
    "active",
    "pending",
    "in_progress",
    "scheduled",
    "upcoming",
    "booked",
    "confirmed",
];

fn cell(row: &BTreeMap<String, String>, field: &str) -> Option<String> {
    row.get(field)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// A label for a customer: no control characters, no id-shaped text.
/// A plainer name a list may carry next to its kind's label field (`policy.list`'s optional `product_name`, "Home
/// Plus insurance", over its `line_of_business`); `None` when the row has none (or it reads like an id).
const PREFERRED_LABELS: &[(&str, &str)] = &[("policy.list", "product_name")];

fn preferred_label(list_op: &str, row: &BTreeMap<String, String>) -> Option<String> {
    if list_op == "contact_point.list" {
        return contact_label(row);
    }
    PREFERRED_LABELS
        .iter()
        .filter(|(op, _)| *op == list_op)
        .find_map(|(_, f)| safe_label(cell(row, f)))
}

/// "email (j•••@example.org)", "phone (•••• 4821)": a contact detail by its kind and the already masked value
/// (`value_masked`); the kind alone when the row has no masked value or it reads like an id. The raw value is never read.
fn contact_label(row: &BTreeMap<String, String>) -> Option<String> {
    let kind = safe_label(cell(row, "kind"))?;
    let masked = safe_label(cell(row, "value_masked"));
    Some(match masked {
        Some(m) => format!("{kind} ({m})"),
        None => kind,
    })
}

fn safe_label(raw: Option<String>) -> Option<String> {
    raw.map(|l| clean(&l))
        .filter(|l| !l.is_empty() && !contains_id_pattern(l))
}

/// Can the records of `kind` be read from `ctx`? `false` when the read failed or was never made (the design has no
/// such operation, an old snapshot): the caller hands over rather than asks. `true` with no rows is a real "none".
pub fn records_available(kind: &'static RecordKind, ctx: &CustomerContext) -> bool {
    let kind = effective_kind(kind, ctx);
    if kind.list_op == CASES_SOURCE {
        return !ctx.degraded.iter().any(|d| d == "cases");
    }
    !ctx.degraded.iter().any(|d| d == kind.list_op) && ctx.records.contains_key(kind.list_op)
}

fn recs(kind: &RecordKind, ctx: &CustomerContext, exclude: Option<&str>) -> Vec<Rec> {
    let mut v: Vec<Rec> = if kind.list_op == CASES_SOURCE {
        ctx.cases
            .iter()
            .filter(|c| exclude != Some(c.case_id.as_str()))
            .map(|c| Rec {
                id: c.case_id.clone(),
                label: safe_label(Some(c.title.clone())),
                date: parse_date(&c.opened_at),
                status: Some(low(&c.state)),
            })
            .collect()
    } else {
        ctx.records
            .get(kind.list_op)
            .map(|rows| {
                rows.iter()
                    // a refund row of a payment list is no payment of the customer's to point at
                    .filter(|r| r.get("direction").map(|d| low(d)).as_deref() != Some("refund"))
                    .filter_map(|r| {
                        Some(Rec {
                            id: cell(r, kind.id_field)?,
                            label: preferred_label(kind.list_op, r)
                                .or_else(|| safe_label(kind.label_field.and_then(|f| cell(r, f)))),
                            date: kind
                                .date_field
                                .and_then(|f| cell(r, f))
                                .and_then(|d| parse_date(&d)),
                            status: cell(r, "status").map(|s| low(&s)),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    v.sort_by_key(|r| r.date);
    v
}

/// "Car insurance renewing 3 March", "Amoxicillin prescription", "appointment on 4 November": the label (never the
/// id) with the kind's noun and date in plain words. A reply reads "your <phrase>".
pub fn record_phrase(kind: &RecordKind, label: Option<&str>, date: Option<NaiveDate>) -> String {
    let mut s = match label {
        Some(l) if kind.bare_label || l.to_lowercase().contains(kind.noun) => l.to_owned(),
        Some(l) => format!("{l} {}", kind.noun),
        None => kind.noun.to_owned(),
    };
    if let Some(d) = date
        && !kind.date_word.is_empty()
    {
        s = format!("{s} {} {}", kind.date_word, day_month(d));
    }
    s
}

/// The plain-words question naming the candidates of a record kind (oldest first); never an id.
pub fn record_choice_question(kind: &RecordKind, picks: &[Pick], more: bool) -> String {
    let mut parts: Vec<String> = picks
        .iter()
        .map(|p| {
            format!(
                "your {}",
                record_phrase(kind, p.items.first().map(String::as_str), p.placed_on)
            )
        })
        .collect();
    if more {
        parts.push(format!("an older {}", kind.noun));
    }
    let last = parts.pop().unwrap_or_default();
    let joined = if parts.is_empty() {
        last
    } else {
        format!("{} or {last}", parts.join(", "))
    };
    format!("Is this about {joined}?")
}

/// Resolve the record of `kind` the customer means, from their own records ([`CustomerContext::records`]). The hint
/// narrows (label words, a date, an ordinal); several left prefer the open ones; one is [`Resolved::One`], several are
/// [`Resolved::Ambiguous`] (newest [`MAX_CHOICES`], oldest first), none [`Resolved::Nothing`]. Whether the records
/// could be read at all is [`records_available`], asked first.
pub fn resolve_records(
    kind: &'static RecordKind,
    ctx: &CustomerContext,
    hint: Option<&str>,
    today: NaiveDate,
) -> Resolved {
    resolve_records_excluding(kind, ctx, hint, today, None)
}

/// [`resolve_records`] leaving out the record `exclude` (the current case, for the cases kind).
pub fn resolve_records_excluding(
    kind: &'static RecordKind,
    ctx: &CustomerContext,
    hint: Option<&str>,
    _today: NaiveDate,
    exclude: Option<&str>,
) -> Resolved {
    let kind = effective_kind(kind, ctx);
    let all = recs(kind, ctx, exclude);
    let mut idx: Vec<usize> = (0..all.len()).collect();
    if let Some(h) = hint.and_then(clean_hint) {
        let cands: Vec<Cand> = all
            .iter()
            .map(|r| Cand {
                names: r
                    .label
                    .iter()
                    .cloned()
                    .chain(std::iter::once(kind.noun.to_owned()))
                    .collect(),
                placed: r.date,
            })
            .collect();
        idx = narrow(&cands, &h);
    }
    if idx.len() > 1 {
        let open: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|&i| {
                all[i]
                    .status
                    .as_deref()
                    .is_some_and(|s| OPEN_STATUS.contains(&s))
            })
            .collect();
        if !open.is_empty() && open.len() < idx.len() {
            idx = open;
        }
    }
    let pick = |i: usize| Pick {
        order_id: all[i].id.clone(),
        payment_id: None,
        delivery_id: None,
        items: all[i].label.iter().cloned().collect(),
        placed_on: all[i].date,
    };
    match idx.len() {
        0 => Resolved::Nothing,
        1 => Resolved::One(pick(idx[0])),
        n => Resolved::Ambiguous {
            picks: idx[n.saturating_sub(MAX_CHOICES)..]
                .iter()
                .map(|&i| pick(i))
                .collect(),
            more: n > MAX_CHOICES,
        },
    }
}

// ---- the matcher: product words, dates, ordinals ----------------------------------------------------------------

/// One order as the matcher sees it.
struct Cand {
    names: Vec<String>,
    placed: Option<NaiveDate>,
}

const STOP: &[&str] = &[
    "the",
    "a",
    "an",
    "my",
    "one",
    "ones",
    "from",
    "of",
    "on",
    "order",
    "orders",
    "parcel",
    "package",
    "delivery",
    "item",
    "items",
    "that",
    "this",
    "it",
    "i",
    "ordered",
    "bought",
    "purchased",
    "placed",
    "and",
    "with",
    "for",
    "to",
    "in",
    "at",
    "is",
    "was",
    "those",
    "these",
    "thing",
    "things",
    "please",
    "about",
    "regarding",
    "about",
    "st",
    "nd",
    "rd",
    "th",
    "again",
    "also",
    "yes",
    "yeah",
    "sure",
    "is",
    "it's",
    "its",
    // the record kinds' own nouns: "my September payslip" names the kind, not which one (a label never repeats it)
    "payslip",
    "payslips",
    "policy",
    "policies",
    "prescription",
    "prescriptions",
    "ticket",
    "tickets",
    "invoice",
    "invoices",
    "bill",
    "bills",
    "claim",
    "claims",
    "appointment",
    "appointments",
    "account",
    "accounts",
    "request",
    "requests",
    "campaign",
    "campaigns",
    "message",
    "messages",
];

const MONTHS: [(&str, &str); 12] = [
    ("jan", "january"),
    ("feb", "february"),
    ("mar", "march"),
    ("apr", "april"),
    ("may", "may"),
    ("jun", "june"),
    ("jul", "july"),
    ("aug", "august"),
    ("sep", "september"),
    ("oct", "october"),
    ("nov", "november"),
    ("dec", "december"),
];

fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

fn month_of(token: &str) -> Option<u32> {
    if token.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|(abbr, full)| {
            token.starts_with(abbr) && (full.starts_with(token) || token == "sept")
        })
        .map(|i| u32::try_from(i + 1).unwrap_or(0))
}

fn ordinal_suffixed(token: &str) -> bool {
    let digits = token.trim_end_matches(|c: char| c.is_alphabetic());
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && matches!(&token[digits.len()..], "st" | "nd" | "rd" | "th")
}

fn day_of(token: &str) -> Option<u32> {
    let digits = token.trim_end_matches(|c: char| c.is_alphabetic());
    if digits.len() != token.len() && !ordinal_suffixed(token) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|d| (1..=31).contains(d))
}

/// A token that looks like a record id ("RET-01" splits into tokens, so this sees the raw whitespace-split word):
/// letters and digits mixed (not "12th" or "3pm"), or a long number.
fn id_like(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_alphanumeric());
    let has_digit = w.chars().any(|c| c.is_ascii_digit());
    let has_alpha = w.chars().any(char::is_alphabetic);
    if has_digit && has_alpha {
        let t = w.to_lowercase();
        // a quarter ("Q4") is a period word, not an id
        return !(matches!(t.as_str(), "q1" | "q2" | "q3" | "q4")
            || ordinal_suffixed(&t)
            || t.ends_with("am")
            || t.ends_with("pm")
            || t.contains('x'));
    }
    w.len() >= 5 && w.chars().all(|c| c.is_ascii_digit())
}

/// `text` without id-like words; `None` when nothing is left. A customer who types an id still gets the order
/// resolved from their records, the id is simply not used.
pub fn clean_hint(text: &str) -> Option<String> {
    let kept: Vec<&str> = text.split_whitespace().filter(|w| !id_like(w)).collect();
    let s = kept.join(" ");
    (!s.trim().is_empty()).then_some(s)
}

/// (day, month) of a date in the text: "12 October", "October 12th", "12 oct", "12/10", "2026-10-12".
fn day_month_in(text: &str) -> Option<(u32, u32)> {
    let toks = tokens(text);
    for (i, t) in toks.iter().enumerate() {
        if t.len() == 4
            && t.chars().all(|c| c.is_ascii_digit())
            && let (Some(m), Some(d)) = (toks.get(i + 1), toks.get(i + 2))
            && let (Ok(m), Some(d)) = (m.parse::<u32>(), day_of(d))
            && (1..=12).contains(&m)
        {
            return Some((d, m));
        }
    }
    if let Some(mi) = toks.iter().position(|t| month_of(t).is_some()) {
        let month = month_of(&toks[mi])?;
        let near = |j: Option<usize>| j.and_then(|j| toks.get(j)).and_then(|t| day_of(t));
        let skip = |mut j: usize, back: bool| -> Option<usize> {
            // step over "of" / "the" between the day and the month
            loop {
                let t = toks.get(j)?;
                if t == "of" || t == "the" {
                    j = if back { j.checked_sub(1)? } else { j + 1 };
                } else {
                    return Some(j);
                }
            }
        };
        let before = mi.checked_sub(1).and_then(|j| skip(j, true));
        let after = skip(mi + 1, false);
        return near(before).or_else(|| near(after)).map(|d| (d, month));
    }
    if text.contains('/') {
        let nums: Vec<u32> = toks
            .iter()
            .filter(|t| t.len() <= 2)
            .filter_map(|t| t.parse().ok())
            .collect();
        if let [d, m, ..] = nums.as_slice()
            && (1..=31).contains(d)
            && (1..=12).contains(m)
        {
            return Some((*d, *m));
        }
    }
    None
}

/// The month a hint names without a day. "may" is also a verb, so it counts only when it is the whole hint or
/// follows a preposition or article ("from May", "the May payslip").
fn month_alone_in(toks: &[String]) -> Option<u32> {
    toks.iter().enumerate().find_map(|(i, t)| {
        let m = month_of(t)?;
        let ambiguous = t == "may"
            && toks.len() > 1
            && !i
                .checked_sub(1)
                .and_then(|j| toks.get(j))
                .is_some_and(|p| matches!(p.as_str(), "from" | "in" | "of" | "for" | "the" | "my"));
        (!ambiguous).then_some(m)
    })
}

enum Ordinal {
    Nth(usize),
    Last,
    Oldest,
}

fn ordinal_in(toks: &[String]) -> Option<Ordinal> {
    toks.iter().find_map(|t| match t.as_str() {
        "first" | "1st" => Some(Ordinal::Nth(1)),
        "second" | "2nd" => Some(Ordinal::Nth(2)),
        "third" | "3rd" => Some(Ordinal::Nth(3)),
        "fourth" | "4th" => Some(Ordinal::Nth(4)),
        "last" | "latest" | "newest" | "recent" => Some(Ordinal::Last),
        "oldest" | "earliest" => Some(Ordinal::Oldest),
        _ => None,
    })
}

fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        w[..w.len() - 1].to_owned()
    } else {
        w
    }
}

fn word_match(a: &str, b: &str) -> bool {
    let (a, b) = (stem(a), stem(b));
    a == b
        || (a.len().min(b.len()) >= 3
            && (a.starts_with(&b) || b.starts_with(&a))
            && a.len().abs_diff(b.len()) <= 2)
}

fn item_matches(names: &[String], word: &str) -> bool {
    names
        .iter()
        .any(|n| tokens(n).iter().any(|t| word_match(t, word)))
}

/// The indexes (into `cands`, oldest first) the hint points at. A date or product words that match nothing give an
/// empty answer; a hint with no usable word, date or ordinal narrows nothing.
fn narrow(cands: &[Cand], hint: &str) -> Vec<usize> {
    let toks = tokens(hint);
    let mut idx: Vec<usize> = (0..cands.len()).collect();
    let date = day_month_in(hint);
    if let Some((d, m)) = date {
        idx.retain(|&i| {
            cands[i]
                .placed
                .is_some_and(|p| chrono::Datelike::day(&p) == d && chrono::Datelike::month(&p) == m)
        });
        if idx.is_empty() {
            return idx;
        }
    }
    if date.is_none()
        && let Some(m) = month_alone_in(&toks)
    {
        // A month with no day ("my September payslip", "the one from October"): the record's date falls in that
        // month, or its label names the month (a payslip period "September 2026").
        // a label that names the month wins over a date in it: the August payslip is issued on 1 September
        let labelled = |i: usize| {
            cands[i]
                .names
                .iter()
                .any(|n| tokens(n).iter().any(|t| month_of(t) == Some(m)))
        };
        if idx.iter().any(|&i| labelled(i)) {
            idx.retain(|&i| labelled(i));
        } else {
            idx.retain(|&i| {
                cands[i]
                    .placed
                    .is_some_and(|p| chrono::Datelike::month(&p) == m)
            });
        }
        if idx.is_empty() {
            return idx;
        }
    }
    let ordinal = if date.is_none() {
        ordinal_in(&toks)
    } else {
        None
    };
    let words: Vec<&String> = toks
        .iter()
        .filter(|t| {
            t.len() >= 2
                && !STOP.contains(&t.as_str())
                && month_of(t).is_none()
                && !t.starts_with(|c: char| c.is_ascii_digit())
                && !matches!(
                    t.as_str(),
                    "first"
                        | "second"
                        | "third"
                        | "fourth"
                        | "last"
                        | "latest"
                        | "newest"
                        | "recent"
                        | "oldest"
                        | "earliest"
                )
        })
        .collect();
    if !words.is_empty() {
        let all: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|&i| words.iter().all(|w| item_matches(&cands[i].names, w)))
            .collect();
        idx = if all.is_empty() {
            idx.iter()
                .copied()
                .filter(|&i| words.iter().any(|w| item_matches(&cands[i].names, w)))
                .collect()
        } else {
            all
        };
        if idx.is_empty() {
            return idx;
        }
    }
    match ordinal {
        Some(Ordinal::Nth(n)) => idx.get(n - 1).copied().into_iter().collect(),
        Some(Ordinal::Last) => idx.last().copied().into_iter().collect(),
        Some(Ordinal::Oldest) => idx.first().copied().into_iter().collect(),
        None => idx,
    }
}

// ---- the guard ----------------------------------------------------------------------------------------------------

/// Does `text` hold something shaped like a record id? Customer replies must hold none (owner ruling 2026-10-26).
/// The definition lives in [`super::id_guard`], which the runtime applies log-only to every customer-facing reply
/// (it never gates one); this stays the tests' (and `safe_label`'s) predicate. It matches:
/// - the demo estate's prefixed ids (`RET-01`, `ORD-LATE-01`, `O-12`, `SHP-9102`, `CS-3`, `PAY-1`, `RMA-7`; prefixes
///   RET, ORD, O, SHP, CS, PAY, RMA, REF, DP, WAR, C, DEL, TRK, then a dash and a digit or capital);
/// - a generic code: 2-5 capitals starting a word, an optional dash, >= 2 digits, ending the word (`AB12345`,
///   `ORD-1234`; not `WH-1000XM6`);
/// - a run of >= 6 digits that is not money (`38.00`, `1234567.50`);
/// - a UUID, or any >= 24-character token of letters, digits and `-_+/=` holding both a letter and a digit.
///
/// Not dates (`2026-10-21`, `2026-10-21T15:00:00Z`), times, money, product or carrier names, "21 October".
pub fn contains_id_pattern(text: &str) -> bool {
    super::id_guard::has_id_shape(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::customer_context::ItemCtx;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    const TODAY: (i32, u32, u32) = (2026, 10, 26);

    fn today() -> NaiveDate {
        d(TODAY.0, TODAY.1, TODAY.2)
    }

    fn order(id: &str, placed: &str, status: &str, items: &[&str]) -> OrderCtx {
        OrderCtx {
            order_id: id.into(),
            status: status.into(),
            placed_at: Some(format!("{placed}T09:00:00Z")),
            items: items
                .iter()
                .map(|n| ItemCtx {
                    name: (*n).into(),
                    qty: 1,
                })
                .collect(),
        }
    }

    fn delivery(
        id: &str,
        order: &str,
        status: &str,
        eta: Option<&str>,
        at: Option<&str>,
    ) -> DeliveryCtx {
        DeliveryCtx {
            delivery_id: id.into(),
            order_id: Some(order.into()),
            carrier: Some("PostNL".into()),
            status: status.into(),
            eta: eta.map(Into::into),
            delivered_at: at.map(Into::into),
            tracking_url: None,
        }
    }

    fn payment(id: &str, order: &str, status: &str, refunded: Option<f64>) -> PaymentCtx {
        PaymentCtx {
            payment_id: id.into(),
            order_id: Some(order.into()),
            amount: Some(10.0),
            currency: Some("EUR".into()),
            status: status.into(),
            refunded_amount: refunded,
            at: None,
        }
    }

    /// cust-1001 (Emma): RET-01 delivered 7 Oct, RET-02 in transit.
    fn emma() -> CustomerContext {
        CustomerContext {
            party: "cust-1001".into(),
            orders: vec![
                order(
                    "RET-02",
                    "2026-10-16",
                    "in_transit",
                    &["Stoneware mug set (4)"],
                ),
                order(
                    "RET-01",
                    "2026-10-02",
                    "delivered",
                    &["Linen throw", "Wool rug 160x230"],
                ),
            ],
            payments: vec![
                payment("PAY-1002", "RET-02", "captured", None),
                payment("PAY-1001", "RET-01", "captured", None),
            ],
            deliveries: vec![
                delivery(
                    "SHP-9102",
                    "RET-02",
                    "in_transit",
                    Some("2026-10-21, 14:00-17:00"),
                    None,
                ),
                delivery(
                    "SHP-9101",
                    "RET-01",
                    "delivered",
                    None,
                    Some("2026-10-07T13:40:00Z"),
                ),
            ],
            ..CustomerContext::default()
        }
    }

    /// cust-1002 (Lukas): one delivered order.
    fn lukas() -> CustomerContext {
        CustomerContext {
            party: "cust-1002".into(),
            orders: vec![order(
                "RET-03",
                "2026-09-28",
                "delivered",
                &["Oak side table", "Cotton cushion cover"],
            )],
            payments: vec![payment("PAY-1003", "RET-03", "captured", None)],
            deliveries: vec![delivery(
                "SHP-9103",
                "RET-03",
                "delivered",
                None,
                Some("2026-10-03T11:20:00Z"),
            )],
            ..CustomerContext::default()
        }
    }

    /// cust-1003 (Camille): one order, processing, pickup (no shipment).
    fn camille() -> CustomerContext {
        CustomerContext {
            party: "cust-1003".into(),
            orders: vec![order(
                "RET-04",
                "2026-10-19",
                "processing",
                &["Linen bedding set"],
            )],
            payments: vec![payment("PAY-1004", "RET-04", "captured", None)],
            ..CustomerContext::default()
        }
    }

    /// cust-1004 (Sanne): a lamp ready for pickup and a doormat delivered long ago.
    fn sanne() -> CustomerContext {
        CustomerContext {
            party: "cust-1004".into(),
            orders: vec![
                order(
                    "RET-05",
                    "2026-10-14",
                    "pickup_available",
                    &["Ceramic table lamp"],
                ),
                order("RET-06", "2026-09-12", "delivered", &["Jute doormat"]),
            ],
            payments: vec![
                payment("PAY-1005", "RET-05", "captured", None),
                payment("PAY-1006", "RET-06", "captured", None),
            ],
            deliveries: vec![
                delivery("SHP-9105", "RET-05", "ready_for_pickup", None, None),
                delivery(
                    "SHP-9106",
                    "RET-06",
                    "delivered",
                    None,
                    Some("2026-09-15T16:10:00Z"),
                ),
            ],
            ..CustomerContext::default()
        }
    }

    /// C-20001 (the probe customer): ORD-LATE-01 delayed (ETA two days ago) and an older delivered pair of headphones.
    fn probe() -> CustomerContext {
        CustomerContext {
            party: "C-20001".into(),
            orders: vec![
                order(
                    "ORD-LATE-01",
                    "2026-10-20",
                    "delayed",
                    &["Philips Airfryer XXL HD9285"],
                ),
                order(
                    "ORD-0007",
                    "2026-10-01",
                    "delivered",
                    &["Sony WH-1000XM6 headphones"],
                ),
            ],
            payments: vec![
                payment("PAY-LATE-01", "ORD-LATE-01", "captured", None),
                payment("PAY-0007", "ORD-0007", "captured", None),
            ],
            deliveries: vec![
                delivery(
                    "SHP-LATE-01",
                    "ORD-LATE-01",
                    "delayed",
                    Some("2026-10-24, 14:00-17:00"),
                    None,
                ),
                delivery(
                    "SHP-0007",
                    "ORD-0007",
                    "delivered",
                    None,
                    Some("2026-10-05T10:00:00Z"),
                ),
            ],
            ..CustomerContext::default()
        }
    }

    /// Two undelivered-or-delivered orders that make a question: Mia's rug and lamp, both delivered a week ago.
    fn mia() -> CustomerContext {
        CustomerContext {
            party: "cust-mia".into(),
            orders: vec![
                order("A-2", "2026-10-20", "delivered", &["Ceramic lamp"]),
                order("A-1", "2026-10-12", "delivered", &["Jute rug"]),
            ],
            payments: vec![
                payment("P-2", "A-2", "captured", None),
                payment("P-1", "A-1", "captured", None),
            ],
            deliveries: vec![
                delivery(
                    "S-2",
                    "A-2",
                    "delivered",
                    None,
                    Some("2026-10-22T10:00:00Z"),
                ),
                delivery(
                    "S-1",
                    "A-1",
                    "delivered",
                    None,
                    Some("2026-10-15T10:00:00Z"),
                ),
            ],
            ..CustomerContext::default()
        }
    }

    fn id_of(r: &Resolved) -> &str {
        match r {
            Resolved::One(p) => &p.order_id,
            other => panic!("one expected: {other:?}"),
        }
    }

    #[test]
    fn table_the_space_ranking_per_demo_customer() {
        // (customer, space, needs, hint, expected order id; None = Nothing)
        type Case = (
            CustomerContext,
            &'static str,
            &'static [&'static str],
            Option<&'static str>,
            Option<&'static str>,
        );
        let order_id: &[&str] = &["order_id"];
        let tracking: &[&str] = &["tracking_ref"];
        let table: Vec<Case> = vec![
            // "where is my parcel?": the in-transit shipment, never the delivered one
            (emma(), "track_parcel", tracking, None, Some("RET-02")),
            (emma(), "order_status", order_id, None, Some("RET-02")),
            (
                emma(),
                "order_status",
                order_id,
                Some("the mugs"),
                Some("RET-02"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("the rug"),
                Some("RET-01"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("my wool rug"),
                Some("RET-01"),
            ),
            // returns: the delivered order
            (emma(), "returns", order_id, None, Some("RET-01")),
            (
                emma(),
                "returns",
                order_id,
                Some("the throw"),
                Some("RET-01"),
            ),
            (lukas(), "returns", order_id, None, Some("RET-03")),
            (lukas(), "order_status", order_id, None, Some("RET-03")),
            // Camille: one order, no shipment
            (camille(), "order_status", order_id, None, Some("RET-04")),
            (camille(), "track_parcel", tracking, None, None),
            (camille(), "returns", order_id, None, Some("RET-04")),
            // Sanne: the lamp is undelivered; the doormat is delivered outside the window but is the only candidate
            (sanne(), "order_status", order_id, None, Some("RET-05")),
            (sanne(), "returns", order_id, None, Some("RET-06")),
            (
                sanne(),
                "returns",
                order_id,
                Some("the lamp"),
                Some("RET-05"),
            ),
            // the probe customer: the late parcel, the headphones
            (
                probe(),
                "delivery_problem",
                order_id,
                None,
                Some("ORD-LATE-01"),
            ),
            (probe(), "track_parcel", tracking, None, Some("ORD-LATE-01")),
            (
                probe(),
                "returns",
                order_id,
                Some("the headphones"),
                Some("ORD-0007"),
            ),
            (
                probe(),
                "order_status",
                order_id,
                Some("the headphones"),
                Some("ORD-0007"),
            ),
            (
                probe(),
                "order_status",
                order_id,
                Some("the airfryer"),
                Some("ORD-LATE-01"),
            ),
            // a product the customer never bought, a date with no order
            (emma(), "order_status", order_id, Some("the kettle"), None),
            (
                emma(),
                "order_status",
                order_id,
                Some("the one from 3 November"),
                None,
            ),
            // dates and ordinals
            (
                emma(),
                "order_status",
                order_id,
                Some("the one from 2 October"),
                Some("RET-01"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("order of October 16th"),
                Some("RET-02"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("the one from 16/10"),
                Some("RET-02"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("the first one"),
                Some("RET-01"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("the second one"),
                Some("RET-02"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("the latest"),
                Some("RET-02"),
            ),
            (
                emma(),
                "order_status",
                order_id,
                Some("the oldest"),
                Some("RET-01"),
            ),
            // an id typed anyway is not used: the order still resolves from the records
            (
                emma(),
                "order_status",
                order_id,
                Some("RET-01"),
                Some("RET-02"),
            ),
        ];
        for (ctx, space, needs, hint, want) in table {
            let got = resolve(space, &ctx, hint, needs, today());
            match want {
                Some(w) => assert_eq!(id_of(&got), w, "{} {space} {hint:?}: {got:?}", ctx.party),
                None => assert_eq!(got, Resolved::Nothing, "{} {space} {hint:?}", ctx.party),
            }
        }
    }

    #[test]
    fn slots_follow_the_pick_and_the_label_has_no_id() {
        let r = resolve("track_parcel", &emma(), None, &["tracking_ref"], today());
        let Resolved::One(p) = r else { panic!("one") };
        assert_eq!(p.slots(&["tracking_ref"])["tracking_ref"], "SHP-9102");
        assert_eq!(p.label(), "Stoneware mug set (4) ordered on 16 October");
        let r = resolve(
            "refunds",
            &emma(),
            Some("the rug"),
            &["order_id", "payment_id"],
            today(),
        );
        let Resolved::One(p) = r else { panic!("one") };
        let s = p.slots(&["order_id", "payment_id"]);
        assert_eq!(
            (s["order_id"].as_str(), s["payment_id"].as_str()),
            ("RET-01", "PAY-1001")
        );
        assert_eq!(
            p.label(),
            "Linen throw and Wool rug 160x230 ordered on 2 October"
        );
        assert!(!contains_id_pattern(&p.label()));
        assert_eq!(
            label_of(&emma(), "RET-02").unwrap(),
            "Stoneware mug set (4) ordered on 16 October"
        );
        assert!(label_of(&emma(), "RET-99").is_none());
    }

    #[test]
    fn several_returnable_orders_are_a_question_in_plain_words_and_the_answer_resolves() {
        let r = resolve("returns", &mia(), None, &["order_id"], today());
        let Resolved::Ambiguous { picks, more } = r else {
            panic!("ambiguous expected: {r:?}")
        };
        assert!(!more);
        assert_eq!(
            picks
                .iter()
                .map(|p| p.order_id.as_str())
                .collect::<Vec<_>>(),
            ["A-1", "A-2"]
        );
        let q = choice_question(&picks, more);
        assert_eq!(
            q,
            "Is this about your Jute rug from 12 October or your Ceramic lamp from 20 October?"
        );
        assert!(!contains_id_pattern(&q));
        for (answer, want) in [
            ("the rug", Some(0)),
            ("the lamp please", Some(1)),
            ("the first one", Some(0)),
            ("the second one", Some(1)),
            ("the one from 20 October", Some(1)),
            ("October 12", Some(0)),
            ("the latest", Some(1)),
            ("yes", None),
            ("the kettle", None),
            ("", None),
            ("both", None),
        ] {
            assert_eq!(choose(&picks, answer), want, "{answer}");
        }
    }

    #[test]
    fn a_hint_that_names_two_orders_is_ranked_then_asked() {
        let mut ctx = mia();
        ctx.orders[0].items[0].name = "Jute rug small".into();
        // "the rug" is in both orders: returns have both delivered in the window -> a question
        assert!(matches!(
            resolve("returns", &ctx, Some("the rug"), &["order_id"], today()),
            Resolved::Ambiguous { .. }
        ));
        // order status picks the newest silently
        assert_eq!(
            id_of(&resolve(
                "order_status",
                &ctx,
                Some("the rug"),
                &["order_id"],
                today()
            )),
            "A-2"
        );
    }

    #[test]
    fn at_most_three_candidates_are_named_and_older_ones_are_one_phrase() {
        let mut ctx = CustomerContext::default();
        for (i, n) in ["Mug", "Lamp", "Rug", "Vase", "Bowl"]
            .into_iter()
            .enumerate()
        {
            let id = format!("X-{i}");
            ctx.orders.insert(
                0,
                order(&id, &format!("2026-10-{:02}", 2 + i), "delivered", &[n]),
            );
            ctx.deliveries.push(delivery(
                &format!("S-{i}"),
                &id,
                "delivered",
                None,
                Some("2026-10-24T10:00:00Z"),
            ));
        }
        let Resolved::Ambiguous { picks, more } =
            resolve("returns", &ctx, None, &["order_id"], today())
        else {
            panic!("ambiguous")
        };
        assert!(more);
        assert_eq!(picks.len(), MAX_CHOICES);
        assert_eq!(picks[2].order_id, "X-4", "the newest last");
        let q = choice_question(&picks, more);
        assert!(q.ends_with("or an older order?"), "{q}");
    }

    #[test]
    fn a_refund_is_found_through_its_order() {
        let mut ctx = emma();
        ctx.payments[1].refunded_amount = Some(429.0);
        assert_eq!(
            id_of(&resolve(
                "refunds",
                &ctx,
                None,
                &["order_id", "payment_id"],
                today()
            )),
            "RET-01"
        );
        // no refund anywhere: the newest paid order
        assert_eq!(
            id_of(&resolve("refunds", &emma(), None, &["order_id"], today())),
            "RET-02"
        );
        // a failed charge
        let mut ctx = emma();
        ctx.payments[0].status = "failed".into();
        assert_eq!(
            id_of(&resolve(
                "pending_charge",
                &ctx,
                None,
                &["order_id"],
                today()
            )),
            "RET-02"
        );
    }

    #[test]
    fn no_orders_or_only_cancelled_ones_is_nothing() {
        assert_eq!(
            resolve(
                "order_status",
                &CustomerContext::default(),
                None,
                &["order_id"],
                today()
            ),
            Resolved::Nothing
        );
        let mut ctx = camille();
        ctx.orders[0].status = "cancelled".into();
        assert_eq!(
            resolve("order_status", &ctx, None, &["order_id"], today()),
            Resolved::Nothing
        );
    }

    #[test]
    fn the_id_guard_catches_the_demo_id_shapes_and_nothing_else() {
        for bad in [
            "RET-01",
            "send ORD-LATE-01 now",
            "O-12",
            "SHP-9102.",
            "your CS-3 case",
            "(PAY-1)",
            "RMA-7",
        ] {
            assert!(contains_id_pattern(bad), "{bad}");
        }
        for ok in [
            "Your Jute rug ordered on 12 October is with PostNL.",
            "Wool rug 160x230",
            "Stoneware mug set (4)",
            "Sony WH-1000XM6 headphones",
            "a well-known e-mail",
            "3S-1",
        ] {
            assert!(!contains_id_pattern(ok), "{ok}");
        }
    }

    #[test]
    fn id_guard_matches_generic_id_shapes() {
        for bad in [
            "order ORD-1234 is late",
            "AB12345",
            "ref XYZ-77.",
            "number 1234567",
            "id 123e4567-e89b-12d3-a456-426614174000 here",
            "token dGhpc2lzYW5vcGFxdWV0b2tlbjEyMw==",
            "a3f9c2d17b8e4a6f90c1d2e3f4a5b6c7",
        ] {
            assert!(contains_id_pattern(bad), "{bad}");
        }
    }

    #[test]
    fn id_guard_spares_plain_words_dates_money_names() {
        for ok in [
            "Your Stoneware mug set (4) is in transit with PostNL, expected by 21 October.",
            "Your Jute rug ordered on 12 October was delivered on 7 October.",
            "Delivered 2026-10-21 at 15:00, or 2026-10-21T15:00:00Z.",
            "The refund of 38.00 EUR, 1234567.50 in total, 1,234,567 units.",
            "Sony WH-1000XM6 headphones with DHL and UPS",
            "Our team is closed right now; a colleague will reply after 08:00.",
            "state-of-the-art-international-shipping-service",
            "Call us on weekdays; USB-C cable, 4K screen, PS5 and H2O bottle",
        ] {
            assert!(!contains_id_pattern(ok), "{ok}");
        }
    }

    fn cand(names: &[&str], placed: Option<NaiveDate>) -> Cand {
        Cand {
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            placed,
        }
    }

    /// A month with no day picks by the record's date. Layer: unit.
    #[test]
    fn a_month_alone_narrows_by_date() {
        let c = [
            cand(&["Payslip"], Some(d(2026, 8, 28))),
            cand(&["Payslip"], Some(d(2026, 9, 25))),
        ];
        assert_eq!(narrow(&c, "my September payslip"), [1]);
        assert_eq!(narrow(&c, "the one from august"), [0]);
    }

    /// A month with no day picks by the month in the label when the date is elsewhere. Layer: unit.
    #[test]
    fn a_month_alone_narrows_by_label() {
        let c = [
            cand(&["August 2026", "Payslip"], Some(d(2026, 9, 1))),
            cand(&["September 2026", "Payslip"], Some(d(2026, 10, 1))),
        ];
        assert_eq!(narrow(&c, "my September payslip"), [1]);
    }

    /// A policy row naming its product reads by the product; without one it falls back to the line of business.
    #[test]
    fn a_policy_reads_by_its_product_name_when_the_list_gives_one() {
        let row = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let named = row(&[
            ("policy_id", "POL-1"),
            ("line_of_business", "Home"),
            ("product_name", "Home Plus"),
        ]);
        assert_eq!(
            preferred_label("policy.list", &named).as_deref(),
            Some("Home Plus")
        );
        let plain = row(&[("policy_id", "POL-1"), ("line_of_business", "Home")]);
        assert_eq!(preferred_label("policy.list", &plain), None);
        let id_like = row(&[("policy_id", "POL-1"), ("product_name", "POL-0042")]);
        assert_eq!(preferred_label("policy.list", &id_like), None);
        assert_eq!(preferred_label("claim.list", &named), None);
    }

    /// A month that matches no date and no label gives an empty answer, as a day and month does. Layer: unit.
    #[test]
    fn a_month_matching_nothing_is_empty() {
        let c = [
            cand(&["Payslip"], Some(d(2026, 8, 28))),
            cand(&["Payslip"], Some(d(2026, 9, 25))),
        ];
        assert!(narrow(&c, "my March payslip").is_empty());
    }

    /// Retail: product word plus month alone picks the rug placed in that month. Layer: unit.
    #[test]
    fn the_rug_from_october_picks_the_october_rug() {
        let c = [
            cand(&["Jute rug"], Some(d(2026, 9, 12))),
            cand(&["Jute rug"], Some(d(2026, 10, 12))),
        ];
        assert_eq!(narrow(&c, "the rug from October"), [1]);
    }

    /// "may" the verb narrows nothing; "from May" is the month. Layer: unit.
    #[test]
    fn may_is_a_month_only_where_it_reads_as_one() {
        let c = [
            cand(&["Rug"], Some(d(2026, 5, 2))),
            cand(&["Rug"], Some(d(2026, 6, 2))),
        ];
        assert_eq!(narrow(&c, "the rug from may"), [0]);
        assert_eq!(narrow(&c, "i may want the rug").len(), 2);
    }

    // ---- the telecom / billing record kinds ----------------------------------------------------------------------

    fn rec_row(cells: &[(&str, &str)]) -> BTreeMap<String, String> {
        cells
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn with_records(op: &str, rows: Vec<BTreeMap<String, String>>) -> CustomerContext {
        let mut c = CustomerContext {
            party: "cust-1".into(),
            ..CustomerContext::default()
        };
        c.records.insert(op.to_owned(), rows);
        c
    }

    fn kind_of(slot: &str) -> &'static RecordKind {
        record_kind(slot).unwrap_or_else(|| panic!("{slot} is not registered"))
    }

    fn lines() -> CustomerContext {
        with_records(
            "subscription.list",
            vec![
                rec_row(&[
                    ("subscription_id", "SUB-77"),
                    ("offering_id", "OFF-1"),
                    ("msisdn", "+31612345678"),
                    ("status", "suspended"),
                    ("commitment_end", "2026-03-03"),
                ]),
                rec_row(&[
                    ("subscription_id", "SUB-78"),
                    ("offering_id", "OFF-2"),
                    ("msisdn", "+31687654321"),
                    ("status", "active"),
                    ("commitment_end", "2027-01-15"),
                ]),
            ],
        )
    }

    fn accounts() -> CustomerContext {
        with_records(
            "account.list",
            vec![
                rec_row(&[
                    ("account_id", "ACC-3001"),
                    ("kind", "Electricity"),
                    ("status", "active"),
                    ("opened_at", "2024-02-01"),
                ]),
                rec_row(&[
                    ("account_id", "ACC-3002"),
                    ("kind", "Gas"),
                    ("status", "active"),
                    ("opened_at", "2025-06-10"),
                ]),
            ],
        )
    }

    fn invoices() -> CustomerContext {
        with_records(
            "invoice.list",
            vec![
                rec_row(&[
                    ("document_ref", "INV-2026-0042"),
                    ("kind", "Energy bill"),
                    ("issue_date", "2026-09-01"),
                ]),
                rec_row(&[
                    ("document_ref", "INV-2026-0051"),
                    ("kind", "Energy bill"),
                    ("issue_date", "2026-10-01"),
                ]),
            ],
        )
    }

    fn asked(kind: &RecordKind, picks: &[Pick], more: bool) -> String {
        let q = record_choice_question(kind, picks, more);
        for p in picks {
            assert!(
                !q.contains(&p.order_id),
                "the question names the key {}: {q}",
                p.order_id
            );
        }
        q
    }

    #[test]
    fn the_telecom_and_billing_slots_are_system_slots_never_asked() {
        for s in ["msisdn", "contract_id", "bill_id", "document_id"] {
            assert!(is_system_slot(s), "{s}");
        }
        let ops = record_ops();
        assert!(ops.contains(&"subscription.list"));
    }

    #[test]
    fn msisdn_one_line_resolves_silently() {
        let ctx = with_records(
            "subscription.list",
            vec![rec_row(&[
                ("msisdn", "+31612345678"),
                ("status", "active"),
                ("commitment_end", "2027-01-15"),
            ])],
        );
        let k = kind_of("msisdn");
        assert!(records_available(k, &ctx));
        match resolve_records(k, &ctx, None, today()) {
            Resolved::One(p) => assert_eq!(p.order_id, "+31612345678"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn msisdn_several_lines_are_named_by_status_and_term_never_by_number() {
        let k = kind_of("msisdn");
        // the active line is the open one, so it wins without a question
        match resolve_records(k, &lines(), None, today()) {
            Resolved::One(p) => assert_eq!(p.order_id, "+31687654321"),
            other => panic!("{other:?}"),
        }
        let all = resolve_records(k, &lines(), Some("the suspended one"), today());
        let Resolved::One(p) = all else {
            panic!("the hint narrows to the suspended line");
        };
        assert_eq!(p.order_id, "+31612345678");
        let phrase = record_phrase(k, p.items.first().map(String::as_str), p.placed_on);
        assert_eq!(phrase, "suspended line committed until 3 March");
        assert!(!phrase.contains("+31") && !phrase.contains("SUB-"));
    }

    #[test]
    fn msisdn_two_open_lines_are_asked_in_plain_words() {
        let ctx = with_records(
            "subscription.list",
            vec![
                rec_row(&[
                    ("msisdn", "+31611111111"),
                    ("status", "active"),
                    ("commitment_end", "2026-12-01"),
                ]),
                rec_row(&[
                    ("msisdn", "+31622222222"),
                    ("status", "active"),
                    ("commitment_end", "2027-02-01"),
                ]),
            ],
        );
        let k = kind_of("msisdn");
        let Resolved::Ambiguous { picks, more } = resolve_records(k, &ctx, None, today()) else {
            panic!("two lines");
        };
        let q = asked(k, &picks, more);
        assert_eq!(
            q,
            "Is this about your active line committed until 1 December or your active line committed until 1 February?"
        );
    }

    #[test]
    fn msisdn_none_is_nothing() {
        let ctx = with_records("subscription.list", vec![]);
        let k = kind_of("msisdn");
        assert!(records_available(k, &ctx));
        assert_eq!(resolve_records(k, &ctx, None, today()), Resolved::Nothing);
        assert!(!records_available(k, &CustomerContext::default()));
    }

    #[test]
    fn contract_id_is_the_account_named_by_its_kind() {
        let k = kind_of("contract_id");
        assert_eq!((k.list_op, k.id_field), ("account.list", "account_id"));
        let Resolved::Ambiguous { picks, more } = resolve_records(k, &accounts(), None, today())
        else {
            panic!("two contracts");
        };
        assert_eq!(
            asked(k, &picks, more),
            "Is this about your Electricity contract opened 1 February or your Gas contract opened 10 June?"
        );
        match resolve_records(k, &accounts(), Some("the gas one"), today()) {
            Resolved::One(p) => assert_eq!(p.order_id, "ACC-3002"),
            other => panic!("{other:?}"),
        }
        let one = with_records(
            "account.list",
            vec![rec_row(&[("account_id", "ACC-9"), ("kind", "Gas")])],
        );
        assert!(matches!(
            resolve_records(k, &one, None, today()),
            Resolved::One(_)
        ));
        let none = with_records("account.list", vec![]);
        assert_eq!(resolve_records(k, &none, None, today()), Resolved::Nothing);
    }

    #[test]
    fn bill_id_and_document_id_are_the_invoices_named_by_kind_and_date() {
        for (slot, noun) in [("bill_id", "bill"), ("document_id", "document")] {
            let k = kind_of(slot);
            assert_eq!((k.list_op, k.id_field), ("invoice.list", "document_ref"));
            let Resolved::Ambiguous { picks, more } =
                resolve_records(k, &invoices(), None, today())
            else {
                panic!("two invoices");
            };
            let q = asked(k, &picks, more);
            assert!(
                q.contains("Energy bill") && q.contains("issued 1 September"),
                "{q}"
            );
            assert!(q.contains(noun) || q.contains("Energy bill"), "{q}");
            match resolve_records(k, &invoices(), Some("the one from october"), today()) {
                Resolved::One(p) => assert_eq!(p.order_id, "INV-2026-0051"),
                other => panic!("{other:?}"),
            }
            let none = with_records("invoice.list", vec![]);
            assert_eq!(resolve_records(k, &none, None, today()), Resolved::Nothing);
        }
    }

    #[test]
    fn an_id_shaped_label_is_dropped_so_the_key_never_reaches_the_customer() {
        // a connector that puts the key in the label field must not leak it into the phrase
        let ctx = with_records(
            "invoice.list",
            vec![rec_row(&[
                ("document_ref", "INV-2026-0042"),
                ("kind", "INV-2026-0042"),
                ("issue_date", "2026-09-01"),
            ])],
        );
        let Resolved::One(p) = resolve_records(kind_of("bill_id"), &ctx, None, today()) else {
            panic!("one invoice");
        };
        assert!(p.items.is_empty());
        let phrase = record_phrase(kind_of("bill_id"), None, p.placed_on);
        assert_eq!(phrase, "bill issued 1 September");
    }

    // ---- premium notices and contact details (seed v16) ----------------------------------------------------------

    fn notices(n: usize) -> CustomerContext {
        let all = [
            ("PN-9001", "POL-1", "Q3 2026", "2026-08-01"),
            ("PN-9002", "POL-1", "Q4 2026", "2026-11-01"),
        ];
        with_records(
            "premium_notice.list",
            all[..n]
                .iter()
                .map(|(id, pol, period, due)| {
                    rec_row(&[
                        ("premium_notice_id", id),
                        ("policy_id", pol),
                        ("period", period),
                        ("due_date", due),
                        ("status", "open"),
                    ])
                })
                .collect(),
        )
    }

    #[test]
    fn premium_notices_resolve_one_ask_by_period_and_none_is_nothing() {
        let k = kind_of("premium_notice_id");
        assert!(is_system_slot("premium_notice_id"));
        assert_eq!(k.list_op, "premium_notice.list");
        assert!(record_ops().contains(&"premium_notice.list"));
        match resolve_records(k, &notices(1), None, today()) {
            Resolved::One(p) => {
                assert_eq!(p.order_id, "PN-9001");
                assert_eq!(
                    record_phrase(k, p.items.first().map(String::as_str), p.placed_on),
                    "Q3 2026 premium notice due 1 August"
                );
            }
            other => panic!("{other:?}"),
        }
        match resolve_records(k, &notices(2), None, today()) {
            Resolved::Ambiguous { picks, more } => {
                assert_eq!(picks.len(), 2);
                let q = record_choice_question(k, &picks, more);
                assert!(!contains_id_pattern(&q) && !q.contains("PN-"), "{q}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            resolve_records(k, &notices(2), Some("the Q4 notice"), today()),
            Resolved::One(p) if p.order_id == "PN-9002"
        ));
        let none = with_records("premium_notice.list", vec![]);
        assert!(records_available(k, &none));
        assert!(matches!(
            resolve_records(k, &none, None, today()),
            Resolved::Nothing
        ));
    }

    fn contacts(rows: &[(&str, &str, &str)]) -> CustomerContext {
        with_records(
            "contact_point.list",
            rows.iter()
                .map(|(id, kind, masked)| {
                    rec_row(&[
                        ("contact_point_id", id),
                        ("kind", kind),
                        ("value_masked", masked),
                        ("preferred", "true"),
                    ])
                })
                .collect(),
        )
    }

    #[test]
    fn contact_details_are_named_by_kind_and_masked_value_never_raw_or_id() {
        let k = kind_of("contact_point_id");
        assert!(is_system_slot("contact_point_id"));
        assert_eq!(k.date_field, None);
        let ctx = contacts(&[
            ("CP-1", "email", "j\u{2022}\u{2022}\u{2022}@example.org"),
            ("CP-2", "phone", "\u{2022}\u{2022}\u{2022}\u{2022} 4821"),
        ]);
        match resolve_records(k, &ctx, None, today()) {
            Resolved::Ambiguous { picks, more } => {
                let labels: Vec<_> = picks.iter().map(|p| p.items.join("")).collect();
                assert_eq!(
                    labels,
                    [
                        "email (j\u{2022}\u{2022}\u{2022}@example.org)",
                        "phone (\u{2022}\u{2022}\u{2022}\u{2022} 4821)"
                    ]
                );
                let q = record_choice_question(k, &picks, more);
                assert!(!q.contains("CP-"), "{q}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            resolve_records(k, &ctx, Some("my phone"), today()),
            Resolved::One(p) if p.order_id == "CP-2"
        ));
        // one detail resolves silently
        let one = contacts(&[("CP-1", "address", "")]);
        match resolve_records(k, &one, None, today()) {
            Resolved::One(p) => assert_eq!(p.items, ["address"]),
            other => panic!("{other:?}"),
        }
        // a masked value that reads like an id is dropped; the kind stays
        let leaky = contacts(&[("CP-1", "phone", "+31612345678")]);
        match resolve_records(k, &leaky, None, today()) {
            Resolved::One(p) => assert_eq!(p.items, ["phone"]),
            other => panic!("{other:?}"),
        }
        let none = contacts(&[]);
        assert!(matches!(
            resolve_records(k, &none, None, today()),
            Resolved::Nothing
        ));
    }

    // ---- an employees job never asks the signed-in employee for a login or a device serial ----

    #[test]
    fn upn_is_a_system_slot_filled_from_the_verified_email_of_an_employees_job_only() {
        assert!(is_system_slot(UPN_SLOT));
        assert_eq!(
            staff_upn(true, Some(" Anna.K@Northwind-IT.example ")).as_deref(),
            Some("anna.k@northwind-it.example")
        );
        // a customer id is no login
        assert_eq!(staff_upn(true, Some("cust-1")), None);
        // a customers-audience job never fills it, whatever the party looks like
        assert_eq!(staff_upn(false, Some("anna@corp.example")), None);
        // no verified party: nothing to fill, and nothing a message could supply
        assert_eq!(staff_upn(true, None), None);
        assert_eq!(staff_upn(true, Some("  ")), None);
        assert_eq!(staff_upn(true, Some("a b@c.d")), None);
    }

    fn devices(rows: &[(&str, &str)]) -> CustomerContext {
        with_records(
            "asset.list",
            rows.iter()
                .map(|(serial, until)| {
                    rec_row(&[
                        ("asset_id", "AS-9"),
                        ("serial", serial),
                        ("warranty_until", until),
                        ("status", "assigned"),
                    ])
                })
                .collect(),
        )
    }

    #[test]
    fn asset_serial_one_device_is_filled_several_are_asked_in_plain_words_none_is_none() {
        let k = kind_of("asset_serial");
        assert_eq!(k.list_op, "asset.list");
        assert!(is_system_slot("asset_serial"));
        let one = devices(&[("C02XK4LMQ7", "2027-03-03")]);
        assert!(records_available(k, &one));
        match resolve_records(k, &one, None, today()) {
            Resolved::One(p) => assert_eq!(p.order_id, "C02XK4LMQ7"),
            other => panic!("{other:?}"),
        }
        let two = devices(&[("C02XK4LMQ7", "2027-03-03"), ("F5KT93ZZ1", "2028-05-01")]);
        match resolve_records(k, &two, None, today()) {
            Resolved::Ambiguous { picks, more } => {
                assert!(!more);
                let q = record_choice_question(k, &picks, more);
                assert_eq!(
                    q,
                    "Is this about your device under warranty until 3 March or your device under warranty until 1 May?"
                );
                assert!(!contains_id_pattern(&q), "{q}");
                for serial in ["C02XK4LMQ7", "F5KT93ZZ1", "AS-9"] {
                    assert!(!q.contains(serial), "{q}");
                }
                assert_eq!(
                    picks[0]
                        .slots(&["asset_serial"])
                        .get("asset_serial")
                        .map(String::as_str),
                    Some("C02XK4LMQ7")
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            resolve_records(k, &devices(&[]), None, today()),
            Resolved::Nothing
        ));
    }
}
