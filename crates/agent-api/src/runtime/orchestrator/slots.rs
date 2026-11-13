//! The orchestrator's `slots` step (B4 known customer, plan §2.3; owner ruling 2026-10-26): before the plan runs, the
//! system slots of the case's context space (`order_id`, `payment_id`, `delivery_id`, `tracking_ref`) are filled from
//! the customer's own records ([`crate::runtime::resolve`]); the customer is never asked for them.
//!
//! | Case hash field | Meaning |
//! |---|---|
//! | `slots:resolved` | the resolved system slots (JSON map), written once; [`crate::runtime::case::CaseStore::context`] adds them to the argument context's `request` |
//! | `result:resolved` | `{"item_label": "Jute rug ordered on 12 October", "item_label_order": order_id the label is about, "item_names": {order_id: items in words}, "focus_order": order_id, "order_id": the case's order}` (`focus_order` only for a request with no order slot and a hint that points at one order), facts the reply prints instead of an id |
//!
//! - **One candidate**: filled silently, the plan runs.
//! - **Several**: the case parks. The candidates are stored in the conversation's `chat:{conversation}` hash (field
//!   `choice`, ids server-side), the case ends `confirmation_required` with code [`CHOICE_REQUIRED`] and the question
//!   in plain words as its reply. The customer's next message is mapped to a candidate by the drain
//!   ([`crate::turn::drain`]), which qualifies a new case with the resolved slots (no model call).
//! - **Input** (A2): a call that answers `input_required` parks the case the same way in the field `input`
//!   ([`park_input`](Orchestrator::park_input)): `{case_id, context_space, slots, message_id, key, prompt, secret, retries,
//!   attempts, until_ms}`, code [`INPUT_REQUIRED`], the prompt as the reply. The drain takes the next message as the value.
//! - **None**: the case ends `not_eligible` with code [`NO_MATCHING_ORDER`]; the reply is a plain sentence.
//! - A request that already carries its system slots (a staff request, the case born from a choice) is not
//!   resolved again; only its label is looked up.
use std::collections::BTreeMap;

use anyhow::Result;
use redis::AsyncCommands;
use serde_json::json;

use super::{Orchestrator, ProcedureSource};
use crate::runtime::customer_context::CustomerContext;
use crate::runtime::domain::{CaseOutcome, Plan, QualifiedRequest};
use crate::runtime::resolve::{
    self, CHOICE_TTL_MS, Choice, HINT_SLOT, RecordKind, Resolved, UPN_SLOT, is_system_slot,
};
use crate::turn::qualify::is_lookup_key;

/// The code of a case that ended because none of the customer's orders fits the request.
pub const NO_MATCHING_ORDER: &str = "no_matching_order";

/// The code of a case that ended because none of the customer's records of a kind (policy, claim ...) fits the
/// request; its reason is the kind's noun ([`crate::runtime::reply::no_matching_record`]).
pub const NO_MATCHING_RECORD: &str = "no_matching_record";

/// The code of a case handed over because a lookup key could not be resolved from the customer's records (the
/// records could not be read, are not offered, or the space declares a key nothing resolves). The customer is never
/// asked for it. The J21 reason falls to `tool_failed` ([`super`]'s `hand_over_reason`).
pub const UNRESOLVABLE_SLOT: &str = "unresolvable_slot";

/// The code of a case that parked to ask which order the customer means.
pub const CHOICE_REQUIRED: &str = "choice_required";

/// The code of a case that parked because a tool needs a value from the user (A2 `ask_user`); the question is its
/// reply. The drain stores the next message as that state key and re-runs the request (field `input`).
pub const INPUT_REQUIRED: &str = crate::runtime::runner::declared::INPUT_REQUIRED;

/// What the generic records say about the registered slots a request still misses.
#[derive(Debug, PartialEq)]
pub(super) enum RecordStep {
    /// Every missing slot has exactly one record: the slot values.
    Filled(BTreeMap<String, String>),
    /// Several records fit `kind`: ask which, in plain words.
    Park {
        kind: &'static RecordKind,
        picks: Vec<resolve::Pick>,
        more: bool,
    },
    /// None of the customer's records of `kind` fits.
    NoMatch(&'static RecordKind),
    /// The records of `slot` cannot be read (failed, not offered, no customer context): a colleague takes it.
    Unresolvable(String),
}

/// Resolve the registered `missing` slots from `ctx` (`None`: no customer context was loaded). Distinct outcomes: an
/// unreadable source is [`RecordStep::Unresolvable`], a readable one with no fit [`RecordStep::NoMatch`], with
/// several [`RecordStep::Park`]. The first slot that is not exactly one record decides.
pub(super) fn resolve_registered(
    missing: &[&str],
    ctx: Option<&CustomerContext>,
    hint: Option<&str>,
    today: chrono::NaiveDate,
    exclude_case: &str,
) -> RecordStep {
    let mut filled = BTreeMap::new();
    for slot in missing {
        let Some(kind) = resolve::record_kind(slot) else {
            return RecordStep::Unresolvable((*slot).to_owned());
        };
        let Some(ctx) = ctx.filter(|c| resolve::records_available(kind, c)) else {
            return RecordStep::Unresolvable((*slot).to_owned());
        };
        match resolve::resolve_records_excluding(kind, ctx, hint, today, Some(exclude_case)) {
            Resolved::One(p) => {
                filled.insert((*slot).to_owned(), p.order_id);
            }
            Resolved::Nothing => return RecordStep::NoMatch(kind),
            Resolved::Ambiguous { picks, more } => {
                // the question names dates the way the source that answered words them
                let kind = resolve::effective_kind(kind, ctx);
                return RecordStep::Park { kind, picks, more };
            }
        }
    }
    RecordStep::Filled(filled)
}

/// Does a step of `plan` bind an argument to `$slot` (or `$request.slot`)?
pub(super) fn plan_uses_slot(plan: &Plan, slot: &str) -> bool {
    let direct = format!("${slot}");
    let nested = format!("$request.{slot}");
    plan.phases
        .iter()
        .flat_map(|p| &p.steps)
        .flat_map(|s| s.args.values())
        .any(|v| v.as_str().is_some_and(|a| a == direct || a == nested))
}

/// A lookup key the space declares that no registry entry resolves and the request does not carry (never asked).
pub(super) fn unresolvable_lookup<'a>(
    space_slots: &'a [String],
    given: &BTreeMap<String, String>,
) -> Option<&'a str> {
    space_slots.iter().map(String::as_str).find(|s| {
        !is_system_slot(s) && is_lookup_key(s) && given.get(*s).is_none_or(|v| v.trim().is_empty())
    })
}

impl<P: ProcedureSource> Orchestrator<P> {
    /// May the case run its plan now? `true` once its system slots are resolved (or there is nothing to resolve);
    /// `false` when the case parked for a choice or ended.
    pub(super) async fn slots_ready(&self, req: &QualifiedRequest, plan: &Plan) -> Result<bool> {
        let key = self.cases.key(&req.case_id);
        let mut c = self.bus.conn();
        if c.hexists(&key, "slots:resolved").await? {
            return Ok(true);
        }
        let design = self.runtime_design().await?;
        let space_slots: &[String] = design
            .spaces
            .by_key(&req.context_space)
            .map(|s| s.slots.as_slice())
            .unwrap_or_default();
        // a lookup key nothing resolves is never asked: a colleague takes the case
        if let Some(slot) = unresolvable_lookup(space_slots, &req.slots) {
            tracing::info!(case_id = %req.case_id, slot, "slots: a lookup key with no registered record kind");
            self.hand_over(
                req,
                plan,
                UNRESOLVABLE_SLOT,
                "a lookup key of the request cannot be resolved",
            )
            .await?;
            return Ok(false);
        }
        let needs: Vec<&str> = design
            .spaces
            .by_key(&req.context_space)
            .map(|s| {
                s.slots
                    .iter()
                    .map(String::as_str)
                    .filter(|n| {
                        is_system_slot(n) && *n != UPN_SLOT && !resolve::is_attachment_slot(n)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // the employee's own login: the case's verified party, never asked and never read from the message
        let wants_upn = space_slots.iter().any(|s| s == UPN_SLOT)
            && req.slots.get(UPN_SLOT).is_none_or(|v| v.trim().is_empty());
        let mut upn: Option<String> = None;
        if wants_upn {
            upn = resolve::staff_upn(
                self.job_is_employees().await?,
                self.verified_party(req).await?.as_deref(),
            );
            if upn.is_none() {
                tracing::info!(case_id = %req.case_id, "slots: no verified employee for the upn slot");
                self.hand_over(
                    req,
                    plan,
                    UNRESOLVABLE_SLOT,
                    "the signed-in employee's login could not be determined",
                )
                .await?;
                return Ok(false);
            }
        }
        let (missing_records, missing): (Vec<&str>, Vec<&str>) = needs
            .iter()
            .copied()
            .filter(|n| req.slots.get(*n).is_none_or(|v| v.trim().is_empty()))
            .partition(|n| resolve::record_kind(n).is_some());
        let raw = self.cases.customer_context_raw(&req.case_id).await?;
        let ctx: Option<CustomerContext> = raw.and_then(|r| serde_json::from_str(&r).ok());
        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        resolved.extend(upn.map(|u| (UPN_SLOT.to_owned(), u)));
        let mut label: Option<String> = None;
        // the order `label` was written for (the reply prints the label only for that order)
        let mut label_order: Option<String> = None;
        let mut focus_order: Option<String> = None;
        if let Some(ctx) = &ctx {
            if missing.is_empty() {
                label = req
                    .slots
                    .get("order_id")
                    .and_then(|id| resolve::label_of(ctx, id));
                label_order = label
                    .as_ref()
                    .and_then(|_| req.slots.get("order_id").cloned());
                // no order slot (track_parcel): what the customer said still points at one order, kept
                // server-side so the list reply answers about that order (never an id in a reply)
                if !req.slots.contains_key("order_id")
                    && let Some(hint) = req.slots.get(HINT_SLOT).map(String::as_str)
                    && let Resolved::One(p) = resolve::resolve(
                        "focus",
                        ctx,
                        Some(hint),
                        &[],
                        chrono::Utc::now().date_naive(),
                    )
                {
                    focus_order = Some(p.order_id);
                }
            } else {
                if ctx.orders.is_empty() && ctx.degraded.iter().any(|d| d == "order.list") {
                    // the records could not be read: a colleague takes it, the customer is not asked
                    self.hand_over(
                        req,
                        plan,
                        "tool_failed",
                        "the customer's orders could not be read",
                    )
                    .await?;
                    return Ok(false);
                }
                let space = req.context_space.rsplit(':').next().unwrap_or_default();
                let hint = req.slots.get(HINT_SLOT).map(String::as_str);
                match resolve::resolve(space, ctx, hint, &missing, chrono::Utc::now().date_naive())
                {
                    Resolved::One(p) => {
                        resolved.extend(p.slots(&missing));
                        label = Some(p.label());
                        label_order = Some(p.order_id.clone());
                    }
                    Resolved::Nothing => {
                        self.finish(
                            req,
                            Some(plan),
                            CaseOutcome::NotEligible,
                            Some(NO_MATCHING_ORDER),
                            None,
                        )
                        .await?;
                        return Ok(false);
                    }
                    Resolved::Ambiguous { picks, more } => {
                        self.park_choice(req, plan, &missing, picks, more).await?;
                        return Ok(false);
                    }
                }
            }
        }
        if !missing_records.is_empty() {
            let hint = req.slots.get(HINT_SLOT).map(String::as_str);
            match resolve_registered(
                &missing_records,
                ctx.as_ref(),
                hint,
                chrono::Utc::now().date_naive(),
                &req.case_id,
            ) {
                RecordStep::Filled(m) => resolved.extend(m),
                RecordStep::NoMatch(kind) => {
                    self.finish(
                        req,
                        Some(plan),
                        CaseOutcome::NotEligible,
                        Some(NO_MATCHING_RECORD),
                        Some(kind.noun.to_owned()),
                    )
                    .await?;
                    return Ok(false);
                }
                RecordStep::Park { kind, picks, more } => {
                    let question = resolve::record_choice_question(kind, &picks, more);
                    self.park(req, plan, &[kind.slot], picks, question).await?;
                    return Ok(false);
                }
                RecordStep::Unresolvable(slot) => {
                    tracing::info!(case_id = %req.case_id, slot, "slots: the records of the slot cannot be read");
                    self.hand_over(
                        req,
                        plan,
                        UNRESOLVABLE_SLOT,
                        "the customer's records could not be read",
                    )
                    .await?;
                    return Ok(false);
                }
            }
        }
        // `photo_ref`: the newest image the customer sent in this conversation, when the space declares the slot or a
        // step binds `$photo_ref`. None sent: the slot stays absent (the customer is never asked for it).
        let photo = resolve::PHOTO_SLOT;
        if (space_slots.iter().any(|s| s == photo) || plan_uses_slot(plan, photo))
            && req.slots.get(photo).is_none_or(|v| v.trim().is_empty())
        {
            match self.newest_customer_image(&req.conversation_id).await {
                Ok(Some(id)) => {
                    resolved.insert(photo.to_owned(), id.to_string());
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(case_id = %req.case_id, "slots: the customer's images could not be read: {e:#}")
                }
            }
        }
        let _: bool = c
            .hset_nx(&key, "slots:resolved", serde_json::to_string(&resolved)?)
            .await?;
        // the item name of each order, server-side: a list reply (`delivery.list`) names its parcels by product
        let item_names: BTreeMap<&str, String> = ctx
            .as_ref()
            .map(|c| {
                c.orders
                    .iter()
                    .filter_map(|o| {
                        let names: Vec<String> = o.items.iter().map(|i| i.name.clone()).collect();
                        let phrase = resolve::items_phrase(&names);
                        // a reply reads "Your <name> is ..."; several items read as one order, so the verb stays singular
                        let name = if o.items.len() > 1 {
                            format!("order with {phrase}")
                        } else {
                            phrase
                        };
                        (!name.is_empty()).then_some((o.order_id.as_str(), name))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // the case's own order, server-side: a refund reply finds that order's return row by it
        let order_id = resolved
            .get("order_id")
            .or_else(|| req.slots.get("order_id"))
            .filter(|v| !v.trim().is_empty())
            .cloned();
        if label.is_some() || !item_names.is_empty() || focus_order.is_some() || order_id.is_some()
        {
            let mut fact = json!({ "item_names": item_names });
            if let Some(o) = order_id {
                // the newest return of that order, from the customer context (a tenant whose connector has no
                // `return.list` simply has none): the refund reply says where the return is, never its id
                if let Some(r) = ctx.as_ref().and_then(|c| {
                    c.returns
                        .iter()
                        .rev()
                        .find(|r| r.order_id.as_deref() == Some(o.as_str()))
                }) {
                    fact["return"] = json!({ "status": r.status, "created_at": r.created_at });
                }
                fact["order_id"] = json!(o);
            }
            if let Some(f) = focus_order {
                fact["focus_order"] = json!(f);
            }
            if let Some(l) = label {
                fact["item_label"] = json!(l);
                if let Some(o) = label_order {
                    fact["item_label_order"] = json!(o);
                }
            }
            let _: bool = c.hset_nx(&key, "result:resolved", fact.to_string()).await?;
        }
        Ok(true)
    }

    /// Does the scope's job serve employees (`jobs.audience = 'employees'`)? `agent_runtime` has table-level SELECT on
    /// `jobs` (0065).
    async fn job_is_employees(&self) -> Result<bool> {
        let (mut tx, job) = self.begin().await?;
        let audience: Option<String> =
            sqlx::query_scalar("SELECT audience FROM jobs WHERE id = $1")
                .bind(job.id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(audience.as_deref() == Some("employees"))
    }

    /// The case's verified party: the `subject` state of its conversation when the conversation's customer is known
    /// (identity level 2).
    async fn verified_party(&self, req: &QualifiedRequest) -> Result<Option<String>> {
        let (mut tx, _job) = self.begin().await?;
        use crate::conversations::state as st;
        let row: Option<(i16, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {level}::int2, CASE WHEN {known} THEN {subject} END FROM agent_case ac WHERE ac.case_id = $1",
            level = st::level_sql("ac"),
            known = st::known_sql("ac"),
            subject = st::sub("ac", st::SUBJECT),
        )))
        .bind(&req.case_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row
            .filter(|(level, _)| *level >= 2)
            .and_then(|(_, party)| party)
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty()))
    }

    /// The newest image the customer sent in `conversation` (tenant + job scoped by RLS).
    async fn newest_customer_image(&self, conversation: &str) -> Result<Option<uuid::Uuid>> {
        let (mut tx, _) = self.begin().await?;
        let id = crate::attachments::store::newest_customer_image(&mut tx, conversation)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        tx.commit().await?;
        Ok(id)
    }

    /// Park the case: store the candidates for the customer's next message, end the case with the question.
    async fn park_choice(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        needs: &[&str],
        picks: Vec<resolve::Pick>,
        more: bool,
    ) -> Result<()> {
        let question = resolve::choice_question(&picks, more);
        self.park(req, plan, needs, picks, question).await
    }

    /// A2: park the case on the question of an `input_required` answer `r` (its `output` is the
    /// [`Ask`](crate::runtime::runner::declared::Ask)); an unreadable pause hands the case over.
    pub(super) async fn park_input(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        r: &crate::runtime::domain::OpResult,
    ) -> Result<()> {
        use crate::runtime::runner::declared::{Ask, INPUT_FIELD, INPUT_TTL_MS, InputPause};
        let Some(ask) = r.output.as_ref().and_then(Ask::from_output) else {
            return self
                .hand_over(
                    req,
                    plan,
                    "tool_failed",
                    "a tool asked for input without a question",
                )
                .await;
        };
        let pause = InputPause {
            case_id: req.case_id.clone(),
            context_space: req.context_space.clone(),
            slots: req
                .slots
                .iter()
                .filter(|(k, _)| !is_system_slot(k) && k.as_str() != HINT_SLOT)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            message_id: req.message_id.clone(),
            key: ask.key,
            prompt: ask.prompt.clone(),
            secret: ask.secret,
            retries: ask.retries,
            attempts: 0,
            until_ms: chrono::Utc::now().timestamp_millis() + INPUT_TTL_MS,
            task: None,
        };
        let key = crate::turn::chat_key(&self.bus, &req.conversation_id, "");
        let mut c = self.bus.conn();
        let _: () = c
            .hset(&key, INPUT_FIELD, serde_json::to_string(&pause)?)
            .await?;
        let _: i64 = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(crate::turn::TTL_SECS)
            .query_async(&mut c)
            .await?;
        self.finish(
            req,
            Some(plan),
            CaseOutcome::ConfirmationRequired,
            Some(INPUT_REQUIRED),
            Some(ask.prompt),
        )
        .await
    }

    /// Store `picks` for the customer's next message and end the case with `question`.
    async fn park(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        needs: &[&str],
        picks: Vec<resolve::Pick>,
        question: String,
    ) -> Result<()> {
        let choice = Choice {
            case_id: req.case_id.clone(),
            context_space: req.context_space.clone(),
            slots: req
                .slots
                .iter()
                .filter(|(k, _)| !is_system_slot(k) && k.as_str() != HINT_SLOT)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            needs: needs.iter().map(|n| (*n).to_owned()).collect(),
            picks,
            until_ms: chrono::Utc::now().timestamp_millis() + CHOICE_TTL_MS,
        };
        let key = crate::turn::chat_key(&self.bus, &req.conversation_id, "");
        let mut c = self.bus.conn();
        let _: () = c
            .hset(&key, "choice", serde_json::to_string(&choice)?)
            .await?;
        let _: i64 = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(crate::turn::TTL_SECS)
            .query_async(&mut c)
            .await?;
        self.finish(
            req,
            Some(plan),
            CaseOutcome::ConfirmationRequired,
            Some(CHOICE_REQUIRED),
            Some(question),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::customer_context::CaseCtx;
    use crate::runtime::resolve::{
        CASES_SOURCE, RECORD_KINDS, contains_id_pattern, record_choice_question,
    };

    fn today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 27).unwrap()
    }

    type Row = BTreeMap<String, String>;

    /// Two rows of `k`'s list: different labels (or dates), opaque ids.
    fn two_rows(k: &RecordKind) -> (Row, Row) {
        let mk = |n: usize, label: &str, date: &str| -> Row {
            let mut r = Row::new();
            if k.label_field == Some(k.id_field) {
                r.insert(k.id_field.into(), label.into());
            } else {
                r.insert(k.id_field.into(), format!("XQ-{n}00{n}"));
            }
            if let Some(f) = k.label_field {
                r.insert(f.into(), label.into());
            }
            if let Some(f) = k.date_field {
                r.insert(f.into(), date.into());
            }
            r
        };
        (mk(1, "Alpha", "2026-03-03"), mk(2, "Beta", "2026-05-01"))
    }

    fn ctx_with(k: &RecordKind, rows: Vec<Row>) -> CustomerContext {
        let mut c = CustomerContext {
            party: "C-1".into(),
            ..CustomerContext::default()
        };
        if k.list_op == CASES_SOURCE {
            c.cases = rows
                .iter()
                .map(|r| CaseCtx {
                    case_id: r[k.id_field].clone(),
                    state: "open".into(),
                    title: r["title"].clone(),
                    opened_at: r["opened_at"].clone(),
                })
                .collect();
        } else {
            c.records.insert(k.list_op.into(), rows);
        }
        c
    }

    fn kinds() -> impl Iterator<Item = &'static RecordKind> {
        RECORD_KINDS.iter()
    }

    #[test]
    fn one_record_fills_the_slot_with_its_value_for_every_kind() {
        for k in kinds() {
            let (a, _) = two_rows(k);
            let id = a[k.id_field].clone();
            let ctx = ctx_with(k, vec![a]);
            assert_eq!(
                resolve_registered(&[k.slot], Some(&ctx), None, today(), "CS-0"),
                RecordStep::Filled([(k.slot.to_owned(), id)].into()),
                "{}",
                k.slot
            );
        }
    }

    #[test]
    fn several_records_park_a_question_with_no_id_for_every_kind() {
        for k in kinds() {
            let (a, b) = two_rows(k);
            let ids = [a[k.id_field].clone(), b[k.id_field].clone()];
            let ctx = ctx_with(k, vec![a, b]);
            let step = resolve_registered(&[k.slot], Some(&ctx), None, today(), "CS-0");
            let RecordStep::Park { kind, picks, more } = step else {
                panic!("{}: park expected, got {step:?}", k.slot)
            };
            assert_eq!(kind.slot, k.slot);
            assert_eq!(picks.len(), 2);
            assert!(!more);
            let q = record_choice_question(kind, &picks, more);
            assert!(q.starts_with("Is this about your "), "{q}");
            assert!(!contains_id_pattern(&q), "{}: {q}", k.slot);
            if k.label_field != Some(k.id_field) {
                for id in &ids {
                    assert!(!q.contains(id.as_str()), "{}: {q}", k.slot);
                }
            }
            // the answer maps back to the record's value for the slot
            assert_eq!(picks[0].slots(&[k.slot]).get(k.slot), Some(&ids[0]));
        }
    }

    #[test]
    fn no_record_ends_not_eligible_with_the_kind_and_an_unread_one_is_not_none() {
        for k in kinds() {
            let ctx = ctx_with(k, vec![]);
            assert_eq!(
                resolve_registered(&[k.slot], Some(&ctx), None, today(), "CS-0"),
                RecordStep::NoMatch(k),
                "{}",
                k.slot
            );
        }
    }

    #[test]
    fn an_unread_unrouted_or_absent_source_is_a_handover_never_a_question_for_every_kind() {
        for k in kinds() {
            let (a, _) = two_rows(k);
            let mut failed = ctx_with(k, vec![a.clone()]);
            failed.records.clear();
            failed.degraded = vec![if k.list_op == CASES_SOURCE {
                "cases".into()
            } else {
                k.list_op.into()
            }];
            let unrouted = CustomerContext {
                party: "C-1".into(),
                ..CustomerContext::default()
            };
            let expect = RecordStep::Unresolvable(k.slot.into());
            // the cases kind is read from the party's cases: only a failed read makes it unavailable
            if k.list_op != CASES_SOURCE {
                assert_eq!(
                    resolve_registered(&[k.slot], Some(&unrouted), None, today(), "CS-0"),
                    expect,
                    "{} unrouted",
                    k.slot
                );
            }
            assert_eq!(
                resolve_registered(&[k.slot], Some(&failed), None, today(), "CS-0"),
                expect,
                "{} failed",
                k.slot
            );
            assert_eq!(
                resolve_registered(&[k.slot], None, None, today(), "CS-0"),
                expect,
                "{} no context",
                k.slot
            );
        }
    }

    #[test]
    fn the_hint_picks_one_of_several_and_the_current_case_is_not_a_candidate() {
        let k = resolve::record_kind("policy_id").unwrap();
        let row = |id: &str, lob: &str, d: &str| -> Row {
            [
                ("policy_id", id),
                ("line_of_business", lob),
                ("renewal_date", d),
            ]
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .into()
        };
        let ctx = ctx_with(
            k,
            vec![
                row("P-1001", "Car insurance", "2026-03-03"),
                row("P-2002", "Home insurance", "2026-05-01"),
            ],
        );
        assert_eq!(
            resolve_registered(
                &["policy_id"],
                Some(&ctx),
                Some("the home one"),
                today(),
                "CS-0"
            ),
            RecordStep::Filled([("policy_id".to_owned(), "P-2002".to_owned())].into())
        );
        let RecordStep::Park { picks, .. } =
            resolve_registered(&["policy_id"], Some(&ctx), None, today(), "CS-0")
        else {
            panic!("park expected")
        };
        assert_eq!(
            record_choice_question(k, &picks, false),
            "Is this about your Car insurance renewing 3 March or your Home insurance renewing 1 May?"
        );
        // the cases kind never offers the case being served
        let kc = resolve::record_kind("case_id").unwrap();
        let mut c = ctx_with(kc, vec![]);
        c.cases = vec![CaseCtx {
            case_id: "CS-7".into(),
            state: "open".into(),
            title: "Billing question".into(),
            opened_at: "2026-10-27T09:00:00Z".into(),
        }];
        assert_eq!(
            resolve_registered(&["case_id"], Some(&c), None, today(), "CS-7"),
            RecordStep::NoMatch(kc)
        );
    }

    #[test]
    fn a_lookup_key_with_no_registry_entry_is_unresolvable_never_asked() {
        let slots: Vec<String> = [
            "order_id",
            "reason",
            "member_number",
            "promo_code",
            "policy_id",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            unresolvable_lookup(&slots, &BTreeMap::new()),
            Some("member_number")
        );
        let given: BTreeMap<String, String> = [("member_number".to_owned(), "x".to_owned())].into();
        assert_eq!(unresolvable_lookup(&slots, &given), None);
        let ok: Vec<String> = ["reason", "policy_id", "promo_code"]
            .map(String::from)
            .into();
        assert_eq!(unresolvable_lookup(&ok, &BTreeMap::new()), None);
    }
}
