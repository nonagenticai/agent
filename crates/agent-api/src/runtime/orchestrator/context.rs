//! The orchestrator's `context` phase (B4 known customer, plan §2.2): before the first step of a case whose
//! conversation identified its customer (level 2 and a verified party), the customer's orders, payments, deliveries
//! and cases are read through the runner with provenance `context` ([`crate::runtime::customer_context`]).
//!
//! The phase is a small state machine over the case hash, so a redelivery or a crash repeats safely:
//!
//! | Field | Meaning |
//! |---|---|
//! | `ctx:state` | `skip` (no identified customer, or the design has no `order.list`), `loading`, `done` |
//! | `ctx:party` / `ctx:s1` | the customer and the names of the first reads |
//! | `ctx:res:{step}` | one read's answer ([`StepRes`]), written once |
//! | `ctx:final` | the first finisher assembles the context |
//! | `ctx` | the assembled [`CustomerContext`] (also in `customer_context` of the argument context) |
//!
//! A staff test chat (`identity_source` `staff_test`) types the customer's EMAIL, which the case holds as its party until
//! the phase resolves it: `party.list{email}` (state `loading`, `ctx:lookup`) binds exactly one external party to the
//! conversation (state `subject` replaces the typed email, and `ctx:from` so the snapshot remembers it for later cases); none / several / no
//! route finish a context with no reads and a `degraded` reason (`party_not_found`, `party_ambiguous`,
//! `party_lookup_unavailable`) - the case proceeds as an unknown customer and never asks for an id.
//!
//! The first reads are the lists; the answer of `order.list` triggers the `order.read` fan-out; the last answer
//! assembles the context, caches it per conversation and proceeds with the case. A failed read narrows the context and
//! never blocks the case.
use std::collections::BTreeMap;

use anyhow::Result;
use redis::AsyncCommands;
use serde_json::Value;

use super::{Orchestrator, ProcedureSource, ROLE};
use crate::conversations::store;
use crate::runtime::bus::Envelope;
use crate::runtime::catalog::OperationCatalog;
use crate::runtime::conditions::CaseFacts;
use crate::runtime::customer_context::{self as cc, CtxStep, StepRes};
use crate::runtime::domain::{
    self, OP_CALL, OpCall, OpResult, OpState, Plan, QualifiedRequest, ROLE_PROCEDURE,
};

/// What `context_party` reads from the case's conversation state: identity level, the known `subject`, identity source,
/// typed customer email.
type CaseIdentityRow = (i16, Option<String>, Option<String>, Option<String>);

impl<P: ProcedureSource> Orchestrator<P> {
    /// May the case run its plan now? `true` for a case with no identified customer (the phase is skipped) and once the
    /// context is loaded; `false` while the reads are out (the last answer proceeds the case).
    pub(super) async fn context_ready(&self, req: &QualifiedRequest, plan: &Plan) -> Result<bool> {
        let key = self.cases.key(&req.case_id);
        let mut c = self.bus.conn();
        let state: Option<String> = c.hget(&key, "ctx:state").await?;
        match state.as_deref() {
            Some("loading") => return Ok(false),
            Some(_) => return Ok(true),
            None => {}
        }
        let Some((mut party, lookup_email)) = self.context_party(req).await? else {
            let _: bool = c.hset_nx(&key, "ctx:state", "skip").await?;
            return Ok(true);
        };
        let design = self.runtime_design().await?;
        // retail reads start from order.list; every other industry from its own record lists (policy.list, ...)
        if !cc::offers_any(|op: &str| design.catalog.get(op).is_some()) {
            tracing::debug!(case_id = %req.case_id, "customer context: the design offers no customer list; skipped");
            let _: bool = c.hset_nx(&key, "ctx:state", "skip").await?;
            return Ok(true);
        }
        let now = self.cases.now_ms().await?;
        let cached = cc::read_snapshot(
            &self.bus,
            self.scope.tenant,
            &self.scope.job,
            &req.conversation_id,
        )
        .await
        .ok()
        .flatten();
        // a staff test chat holds the typed email as its party: resolve it to the system's party first
        if let Some(email) = lookup_email {
            let from = email.to_lowercase();
            let earlier = cached
                .as_ref()
                .filter(|s| s.0.resolved_from.as_deref() == Some(from.as_str()))
                .map(|s| s.0.party.clone());
            if let Some(id) = earlier {
                // an earlier case of the conversation resolved this email: the same party, no second lookup
                self.bind_party(req, &id).await?;
                let _: () = c.hset(&key, "ctx:from", &from).await?;
                party = id;
            } else if !cached
                .as_ref()
                .is_some_and(|s| cc::is_fresh(s, &party, now))
            {
                if design.catalog.get(cc::PARTY_LOOKUP_OP).is_none() {
                    self.finish_unresolved(req, &party, cc::PARTY_LOOKUP_UNAVAILABLE)
                        .await?;
                    return Ok(true);
                }
                let _: () = c.hset(&key, "ctx:party", &party).await?;
                let _: () = c.hset(&key, "ctx:from", &from).await?;
                let _: () = c.hset(&key, "ctx:lookup", 1).await?;
                self.dispatch_context(req, plan, &cc::lookup_step(&email))
                    .await?;
                let _: bool = c.hset_nx(&key, "ctx:state", "loading").await?;
                return Ok(false);
            }
        }
        if let Some(snap) = cached.filter(|s| cc::is_fresh(s, &party, now)) {
            let _: () = c.hset(&key, "ctx", serde_json::to_string(&snap.0)?).await?;
            let _: () = c.hset(&key, "ctx:party", &party).await?;
            let _: () = c.hset(&key, "ctx:state", "done").await?;
            return Ok(true);
        }
        self.start_reads(req, plan, &party).await?;
        Ok(false)
    }

    /// Dispatch the first reads for `party` and mark the phase `loading`.
    async fn start_reads(&self, req: &QualifiedRequest, plan: &Plan, party: &str) -> Result<()> {
        let key = self.cases.key(&req.case_id);
        let mut c = self.bus.conn();
        let design = self.runtime_design().await?;
        let offers = |op: &str| design.catalog.get(op).is_some();
        let steps = cc::stage1(party, offers);
        let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        let _: () = c.hset(&key, "ctx:party", party).await?;
        let _: () = c
            .hset(&key, "ctx:dr", u8::from(cc::reads_deliveries(offers)))
            .await?;
        let _: () = c
            .hset(&key, "ctx:dp", u8::from(cc::reads_dispenses(offers)))
            .await?;
        let _: () = c
            .hset(&key, "ctx:s1", serde_json::to_string(&names)?)
            .await?;
        for s in &steps {
            self.dispatch_context(req, plan, s).await?;
        }
        let _: bool = c.hset_nx(&key, "ctx:state", "loading").await?;
        Ok(())
    }

    /// Bind the conversation's customer to the system's party `id` (a staff test chat's typed email resolved to it):
    /// state `subject` replaces the typed email, so this case and the conversation's later cases read the system's id.
    async fn bind_party(&self, req: &QualifiedRequest, id: &str) -> Result<()> {
        let (mut tx, _job) = self.begin().await?;
        crate::conversations::state::put_value(
            &mut tx,
            &req.conversation_id,
            crate::conversations::state::SUBJECT,
            id,
            "runtime:customer_context",
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        tx.commit().await?;
        Ok(())
    }

    /// The lookup found no single party (`reason`: a `degraded` code): store a context with no reads for the typed
    /// email, in the case and the conversation snapshot. The caller proceeds with the case as an unknown customer.
    async fn finish_unresolved(
        &self,
        req: &QualifiedRequest,
        email: &str,
        reason: &str,
    ) -> Result<()> {
        let key = self.cases.key(&req.case_id);
        let mut c = self.bus.conn();
        let _: bool = c.hset_nx(&key, "ctx:final", 1).await?;
        let mut ctx = cc::CustomerContext {
            party: email.to_owned(),
            degraded: vec![reason.to_owned()],
            ..cc::CustomerContext::default()
        };
        let (mut tx, _job) = self.begin().await?;
        if let Ok(cases) = cc::party_cases(&mut tx, email, &req.conversation_id).await {
            ctx.cases = cases;
        }
        tx.commit().await?;
        tracing::info!(case_id = %req.case_id, reason, "customer context: the typed email resolved to no single party");
        let _: () = c.hset(&key, "ctx", serde_json::to_string(&ctx)?).await?;
        let _: () = c.hset(&key, "ctx:party", email).await?;
        let _: () = c.hset(&key, "ctx:state", "done").await?;
        let now = self.cases.now_ms().await?;
        if let Err(e) = cc::put_snapshot(
            &self.bus,
            self.scope.tenant,
            &self.scope.job,
            &req.conversation_id,
            &ctx,
            now,
        )
        .await
        {
            tracing::warn!(case_id = %req.case_id, "customer context snapshot not stored: {e:#}");
        }
        Ok(())
    }

    /// The party lookup's answer: one party binds the case and the reads start; anything else finishes the context
    /// degraded and proceeds.
    async fn context_lookup_result(&self, req: &QualifiedRequest, res: &StepRes) -> Result<()> {
        let key = self.cases.key(&req.case_id);
        let mut c = self.bus.conn();
        let email: Option<String> = c.hget(&key, "ctx:party").await?;
        let email = email.unwrap_or_default();
        let found = res.ok.then(|| cc::lookup_outcome(&res.output));
        let Some(plan) = self.cases.plan(&req.case_id).await? else {
            tracing::warn!(case_id = %req.case_id, "customer context lookup answered for a case with no plan");
            return Ok(());
        };
        match found {
            Some(cc::PartyLookup::One(id)) => {
                self.bind_party(req, &id).await?;
                self.start_reads(req, &plan, &id).await
            }
            other => {
                let reason = match other {
                    Some(cc::PartyLookup::NotFound) => cc::PARTY_NOT_FOUND,
                    Some(cc::PartyLookup::Ambiguous) => cc::PARTY_AMBIGUOUS,
                    _ => cc::PARTY_LOOKUP_UNAVAILABLE,
                };
                self.finish_unresolved(req, &email, reason).await?;
                self.proceed(req, &plan).await
            }
        }
    }

    /// The case's verified party when the conversation identified its customer (level 2 and a party), else `None`; with
    /// it the typed email still to be looked up (a `staff_test` chat whose party is still its email).
    async fn context_party(
        &self,
        req: &QualifiedRequest,
    ) -> Result<Option<(String, Option<String>)>> {
        let (mut tx, _job) = self.begin().await?;
        use crate::conversations::state as st;
        let row: Option<CaseIdentityRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {level}::int2, CASE WHEN {known} THEN {subject} END, {source}, {email} \
               FROM agent_case ac WHERE ac.case_id = $1",
            level = st::level_sql("ac"),
            known = st::known_sql("ac"),
            subject = st::sub("ac", st::SUBJECT),
            source = st::sub("ac", st::IDENTITY_SOURCE),
            email = st::sub("ac", st::EMAIL),
        )))
        .bind(&req.case_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.and_then(|(level, party, source, email)| {
            let party = party.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty());
            let party = party.filter(|_| level >= 2)?;
            let lookup = cc::needs_lookup(source.as_deref(), &party, email.as_deref())
                .then(|| party.clone());
            Some((party, lookup))
        }))
    }

    /// Send one context read: an ordinary `op.call` with provenance `context`, phase 0, no confirmation, no key.
    async fn dispatch_context(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        s: &CtxStep,
    ) -> Result<()> {
        let level = self.current_level(req).await?;
        let deadline_ms = match self.cases.due_ms(&req.case_id).await? {
            Some(due) => u64::try_from(due).unwrap_or(0),
            None => {
                let now = u64::try_from(self.cases.now_ms().await?).unwrap_or(0);
                now.saturating_add(u64::try_from(self.cases.deadline().as_millis()).unwrap_or(0))
            }
        };
        let (release_id, draft_rev) = store::stamp(&plan.design);
        let call = OpCall {
            call_id: domain::call_id(&req.case_id, 0, &s.name, 1),
            case_id: req.case_id.clone(),
            conversation_id: req.conversation_id.clone(),
            phase: 0,
            step: s.name.clone(),
            attempt: 1,
            agent_role: ROLE_PROCEDURE.to_owned(),
            activity: req.activity.clone(),
            operation: s.op.to_owned(),
            variant: None,
            input: s.input.clone(),
            idempotency_key: None,
            env: self.scope.env,
            release_id,
            draft_rev,
            confirm_ref: None,
            initiator: format!("{ROLE_PROCEDURE}@{}", self.consumer),
            initiator_sub: req.initiator_sub.clone(),
            identity_level: level,
            procedure: plan.procedure_label(),
            deadline_ms,
            preconditions: Vec::new(),
            postconditions: Vec::new(),
            performed_by: None,
            max_per_case: None,
            facts: CaseFacts::default(),
            provenance: Some(domain::PROVENANCE_CONTEXT.to_owned()),
        };
        let env = Envelope::new(&req.topic(OP_CALL), ROLE, &req.case_id, &call)?
            .with_procedure(plan.procedure_label().as_deref());
        self.bus.publish(&self.bus.streams.op(), &env).await?;
        Ok(())
    }

    /// The runner's answer to a context read: record it once, fan out after the order list, and when every read has
    /// answered assemble the context and proceed with the case.
    pub(super) async fn context_result(&self, req: &QualifiedRequest, r: OpResult) -> Result<()> {
        let res = StepRes {
            ok: r.state == OpState::Ok,
            code: r.outcome_code.clone(),
            output: r
                .output
                .as_ref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        };
        if !res.ok {
            tracing::debug!(case_id = %req.case_id, step = %r.step,
                code = r.outcome_code.as_deref().unwrap_or(""), "customer context: a read failed");
        }
        let key = self.cases.key(&req.case_id);
        let mut c = self.bus.conn();
        let first: bool = c
            .hset_nx(
                &key,
                format!("ctx:res:{}", r.step),
                serde_json::to_string(&res)?,
            )
            .await?;
        if first
            && r.step == cc::ORDER_LIST_STEP
            && res.ok
            && let Some(plan) = self.cases.plan(&req.case_id).await?
        {
            let dr: Option<u8> = c.hget(&key, "ctx:dr").await?;
            for s in cc::stage2(&cc::parse_orders(&res.output), dr == Some(1)) {
                self.dispatch_context(req, &plan, &s).await?;
            }
        }
        // the pharmacy fan-out: one dispense.read per listed prescription (capped), so a dispense change can be owned
        if first
            && r.step == cc::PRESCRIPTION_LIST_STEP
            && res.ok
            && let Some(plan) = self.cases.plan(&req.case_id).await?
        {
            let dp: Option<u8> = c.hget(&key, "ctx:dp").await?;
            if dp == Some(1) {
                for s in cc::dispense_stage2(&cc::parse_records(&res.output)) {
                    self.dispatch_context(req, &plan, &s).await?;
                }
            }
        }
        if first && r.step == cc::PARTY_LOOKUP_STEP {
            return self.context_lookup_result(req, &res).await;
        }
        self.context_progress(req).await
    }

    /// When every read has answered, the first caller assembles the context, stores it (case hash, conversation
    /// snapshot) and proceeds with the case.
    async fn context_progress(&self, req: &QualifiedRequest) -> Result<()> {
        let key = self.cases.key(&req.case_id);
        let mut c = self.bus.conn();
        let s1: Option<String> = c.hget(&key, "ctx:s1").await?;
        let Some(s1) = s1 else { return Ok(()) };
        let names: Vec<String> = serde_json::from_str(&s1)?;
        let dr: Option<u8> = c.hget(&key, "ctx:dr").await?;
        let dp: Option<u8> = c.hget(&key, "ctx:dp").await?;
        let all: Vec<(String, String)> = c.hgetall(&key).await?;
        let results: BTreeMap<String, StepRes> = all
            .into_iter()
            .filter_map(|(f, raw)| {
                let name = f.strip_prefix("ctx:res:")?.to_owned();
                Some((name, serde_json::from_str(&raw).ok()?))
            })
            .collect();
        if !cc::complete_with(&results, &names, dr == Some(1), dp == Some(1)) {
            return Ok(());
        }
        let first: bool = c.hset_nx(&key, "ctx:final", 1).await?;
        if !first {
            return Ok(());
        }
        let party: Option<String> = c.hget(&key, "ctx:party").await?;
        let party = party.unwrap_or_default();
        let mut ctx = cc::assemble(&party, &results);
        let from: Option<String> = c.hget(&key, "ctx:from").await?;
        ctx.resolved_from = from;
        let (mut tx, _job) = self.begin().await?;
        match cc::party_cases(&mut tx, &party, &req.conversation_id).await {
            Ok(cases) => ctx.cases = cases,
            Err(e) => {
                tracing::debug!(case_id = %req.case_id, "customer context: cases not read: {e:#}");
                ctx.degraded.push("cases".to_owned());
            }
        }
        tx.commit().await?;
        let _: () = c.hset(&key, "ctx", serde_json::to_string(&ctx)?).await?;
        let _: () = c.hset(&key, "ctx:state", "done").await?;
        let now = self.cases.now_ms().await?;
        if let Err(e) = cc::put_snapshot(
            &self.bus,
            self.scope.tenant,
            &self.scope.job,
            &req.conversation_id,
            &ctx,
            now,
        )
        .await
        {
            tracing::warn!(case_id = %req.case_id, "customer context snapshot not stored: {e:#}");
        }
        let Some(plan) = self.cases.plan(&req.case_id).await? else {
            tracing::warn!(case_id = %req.case_id, "customer context loaded for a case with no plan");
            return Ok(());
        };
        self.proceed(req, &plan).await
    }
}
