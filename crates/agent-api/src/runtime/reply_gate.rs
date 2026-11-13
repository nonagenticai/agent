//! B5 R1 in the interaction daemon (T12, B4 owner ruling: inside the reply's transaction, before
//! `conversations::store::append`). Kept out of crate::runtime::interaction so that file only calls [`ReplyHook::check`].
//!
//! - `allow` → `None`: the daemon stores the reply as composed.
//! - `handover` → a handover item (`gate_blocked` / `gate_unavailable`, one per case) and, instead of the agent's
//!   reply, the deciding question's customer text (F-16, else the care-bus default) stored as the case's reply
//!   (`external_id` `reply:{case}`, author `handover_notice`), so a redelivery relays it and never gates twice.
//! - `block` → nothing for the customer; a `gate` case event.
//!
//! A case that already ended `handover` is not gated (its reply is the handover notice). Customer text is data:
//! never logged.
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::design::DesignCache;
use super::domain::{CaseOutcome, Resolution};
use super::gates::{Decision, ReplyGate, ReplyInput, VerdictCause};
use super::reply;
use super::runner::agent_cases;
use super::scope::Scope;
use crate::conversations::{handover, store};
use crate::design::{CaseDesign, JobRef, Registry};
use crate::state::AppState;

/// R1's heartbeat role (`hb:post_gate:{consumer}`, `starts:post_gate`): the interaction daemon beats it, under its
/// own consumer name, while it runs with the hook ([`ReplyHook::of`]). R1 has no stream or consumer group of its
/// own — it runs inside the interaction consumer's turn — so `busAgents` reports it from these beats.
pub const ROLE: &str = "post_gate";

/// What R1 runs with: the state the gate engine reads (provider resolution, metering), the hook and the daemon's
/// design cache.
#[derive(Clone)]
pub struct ReplyHook {
    pub st: AppState,
    pub gate: Arc<dyn ReplyGate>,
    pub designs: Arc<DesignCache>,
}

/// The notice R1 stored as the case's reply; `item` = the handover item queued now (if one was).
pub struct Notice {
    pub message_id: Uuid,
    pub at_ms: i64,
    pub text: String,
    pub item: Option<String>,
}

/// What R1 did instead of the agent's reply.
pub enum Gated {
    /// Handover: the notice instead of the reply.
    Notice(Notice),
    /// Block: nothing for the customer.
    Blocked,
}

/// R1 of the daemon's hook, if it has one (`None` = store the reply as composed).
pub async fn check(
    h: Option<&ReplyHook>,
    tx: &mut PgConnection,
    c: ReplyCase<'_>,
) -> Result<Option<Gated>> {
    match h {
        Some(h) => h.check(tx, c).await,
        None => Ok(None),
    }
}

/// The reply R1 is asked about.
pub struct ReplyCase<'a> {
    pub scope: &'a Scope,
    pub reg: &'a Registry,
    pub job: &'a JobRef,
    pub r: &'a Resolution,
    pub conversation_id: &'a str,
    /// The case's design stamp (`agent_case.release_id` / `draft_rev`).
    pub release_id: Option<String>,
    pub draft_rev: Option<i64>,
}

impl<'a> ReplyCase<'a> {
    pub fn of(
        scope: &'a Scope,
        reg: &'a Registry,
        job: &'a JobRef,
        r: &'a Resolution,
        conversation_id: &'a str,
        release_id: Option<String>,
        draft_rev: Option<i64>,
    ) -> Self {
        ReplyCase {
            scope,
            reg,
            job,
            r,
            conversation_id,
            release_id,
            draft_rev,
        }
    }
}

async fn case_event(c: &mut PgConnection, job_id: Uuid, case_id: &str, body: Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO case_event (tenant_id, job_id, case_id, kind, body) \
         VALUES (app_current_tenant(), $1, $2, 'gate', $3)",
    )
    .bind(job_id)
    .bind(case_id)
    .bind(body)
    .execute(c)
    .await?;
    Ok(())
}

impl ReplyHook {
    /// The hook of a daemon running with the runtime state `st` (its `gates.reply`), else none.
    pub fn of(st: Option<&AppState>, designs: &Arc<DesignCache>) -> Option<ReplyHook> {
        st.map(|st| ReplyHook {
            st: st.clone(),
            gate: st.gates.reply.clone(),
            designs: Arc::clone(designs),
        })
    }

    /// Run R1 on the daemon's open, job-scoped transaction (module docs). `None` = store the reply as composed.
    pub async fn check(&self, tx: &mut PgConnection, c: ReplyCase<'_>) -> Result<Option<Gated>> {
        let r = c.r;
        if r.outcome == CaseOutcome::Handover {
            return Ok(None);
        }
        let case_id = r.case_id.as_str();
        let d = self
            .designs
            .get_or_load(&mut *tx, c.reg, c.scope, c.job)
            .await?;
        let text = reply::compose(r);
        let operations: Vec<(String, String)> = r
            .facts
            .keys()
            .map(|k| (k.clone(), "ok".to_owned()))
            .collect();
        let v = self
            .gate
            .check(
                &self.st,
                ReplyInput {
                    tenant: c.scope.tenant,
                    job: c.job,
                    env: c.scope.env,
                    conversation_id: c.conversation_id,
                    case_id,
                    reply: &text,
                    operations: &operations,
                    design: &d,
                },
            )
            .await;
        if v.decision == Decision::Allow {
            return Ok(None);
        }
        case_event(&mut *tx, c.job.id, case_id, v.event_body("R1")).await?;
        let Some(reason) = v.handover_reason() else {
            tracing::info!(case = case_id, "agent_reply_gate_blocked");
            return Ok(Some(Gated::Blocked));
        };
        let stamp = match c.draft_rev {
            Some(rev) => CaseDesign::Draft { draft_rev: rev },
            None => CaseDesign::Live {
                release_id: c.release_id.clone(),
            },
        };
        let mut item = None;
        if handover::open_for_case(&mut *tx, case_id).await?.is_none() {
            let conv = store::conversation_or_404(&mut *tx, c.conversation_id).await?;
            let trigger = match v.cause {
                VerdictCause::Fired => "a reply gate fired",
                VerdictCause::Unavailable => "the gate service did not answer",
            };
            let subject = match r.request_type.as_str() {
                "" => "Handover",
                t => t,
            };
            let new = handover::NewItem {
                job_id: c.job.id,
                case_id,
                conv: &conv,
                reason,
                trigger,
                subject,
                actor: "agent",
                design: &stamp,
            };
            // B4 T16a (F-14): closed at the database's now(), the item's clocks start at the next open time (policy /
            // calendar SELECTs, no lock). The customer reads the gate's own authored notice below.
            let closed = agent_cases::closed_at_now(&mut *tx, c.job, &stamp).await?;
            item =
                Some(handover::create_with(&mut *tx, new, closed.as_ref().map(|(_, o)| o)).await?);
        }
        let notice = crate::job_design::gates::hooks::customer_text(&d, &v).unwrap_or_default();
        let reply_key = format!("reply:{}", r.reply_ref());
        let s = store::append(
            &mut *tx,
            store::NewMessage {
                job_id: c.job.id,
                conversation_id: c.conversation_id,
                role: "agent",
                author: handover::NOTICE_AUTHOR,
                text: &notice,
                case_id: Some(case_id),
                external_id: Some(&reply_key),
            },
        )
        .await?
        .ok_or_else(|| anyhow!("reply_conflict: {case_id}"))?;
        tracing::info!(case = case_id, reason, "agent_reply_gate_handover");
        Ok(Some(Gated::Notice(Notice {
            message_id: s.message_id,
            at_ms: s.at_ms,
            text: notice,
            item,
        })))
    }

    /// After commit: tell the workers about a handover item R1 queued.
    pub async fn notify(&self, tenant: Uuid, job_id: Uuid, item: &str) {
        handover::notify_workers(&self.st, tenant, job_id, item).await;
    }
}
