//! B2: an MCP task started in a conversation talks back to that conversation (owner 2026-10-30).
//!
//! The runner's task poller (crate::mcp_client::store::TaskPoller) stores every change of a task; agent-api's relay
//! ([`spawn`], every [`RELAY_EVERY`]) finds the tasks linked to a case that need the conversation (definer
//! `agent_mcp_task_relay_due`, 0241) and relays them. It runs in agent-api because delivery needs the edge link the
//! runner does not hold. While the task is `input_required`, its first open question is answered by the job's own
//! model from the conversation (`crate::runtime::task_answer`; owner 2026-10-31: every fr task message is handled by
//! the platform's agents unless there is an absolute need for user input). A field the model leaves out — the
//! requester's consent, an id it could not find, a choice only they can make, anything the conversation does not
//! establish — is asked in the
//! conversation and parked like an A2 question (`chat:{conversation}` field `input`, [`InputPause::task`]); the drain
//! sends the user's reply to the task, one schema field at a time, as the job's service actor (`agent:<tenant>`, the
//! job in the actor meta; owner 2026-10-31). A terminal task's outcome is posted to the conversation once
//! (`delivered_at`). The model is asked at most once per sweep, and a question at most once per ten minutes.
//!
//! A live task's `_meta["ai.nonagentic/fr"]` is relayed too (owner 2026-11-06): the project id into conversation
//! state ([`state::FR_PROJECT_ID`], with the task that named it in [`state::FR_PROJECT_TASK`]; a task started later
//! supersedes an older one's project), one line per new activity `seq`, and one escalation (with one notice) per
//! severe signal — `critical` and `high`, carried through the existing [`escalate`] path, where a strictly worse
//! signal upgrades the reason and a notice that could not be posted is retried by the next sweep.
//!
//! A tenant's conveyor emits its own feed on its own channel instead (owner 2026-11-08): a task whose
//! `_meta["ai.nonagentic/conveyor"]` carries `{project_id, run_id, status, events}` is relayed the same way, into
//! its own state keys ([`state::CONVEYOR_PROJECT_ID`], [`state::CONVEYOR_PROJECT_TASK`]) under its own lock. The fr
//! block wins when a task carries both; a conveyor emits no signals, so only fr's feed escalates.
//!
//! The message goes where a staff line goes: stored as an `agent` line (author [`AUTHOR`]), published on the
//! conversation's chat feed and sent through the edge to the channel (`staff::notify_customer`). An `api`
//! conversation has no channel address: the line stays in its history, and when its caller's latest async request
//! carried a `callback_url` the message is also delivered there, signed like any interaction callback
//! (crate::interaction::webhook::notify_message; kinds `question`, `result`, `escalated`). The request's own answer
//! never comes from a relay line (backend `outcome` skips [`AUTHOR`]), so nothing is delivered twice.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::conversations::edge_link::DeliverKind;
use crate::conversations::{staff, state, store};
use crate::error::{ApiError, ApiResult};
use crate::interaction::Scope as JobScope;
use crate::mcp_client::store::{
    AnswerInput, answer as answer_task, escalate, escalate_upgrade, scoped, severity_priority,
    severity_rank,
};
use crate::mcp_client::tasks::{
    CONVEYOR_META, FR_META, InputQuestion, StaffActor, input_questions, validate_answer,
};
use crate::runtime::runner::declared::{INPUT_FIELD, INPUT_TTL_MS, InputPause, TaskAsk};
use crate::runtime::task_answer::{self, Filled};
use crate::runtime::{Bus, Env, Scope};
use crate::state::AppState;
use crate::usage::LlmUsage;

/// The `author` of every line this relay stores.
pub const AUTHOR: &str = "mcp_task";
/// The state key a secret task answer is sealed under at acceptance (A2 `seal_secret_answer`) until it is sent.
pub const SECRET_KEY: &str = "mcp_task_answer";
/// Empty or invalid replies to one field before the question is left to staff.
const RETRIES: u8 = 2;
/// How often the relay looks for work.
pub const RELAY_EVERY: std::time::Duration = std::time::Duration::from_secs(3);
/// Tasks relayed per sweep at most.
const RELAY_LIMIT: i32 = 50;

pub const DONE_TEXT: &str = "The task you asked for is done.";
pub const FAILED_TEXT: &str = "The task you asked for could not be completed. Someone from the team will look into it and reply here.";
pub const CANCELLED_TEXT: &str = "The task you asked for was cancelled.";
/// The task went to staff (crate::mcp_client::store::escalate): the requester is told once and not asked any more.
pub const ESCALATED_TEXT: &str = "Someone from the team will pick this up and reply here.";
/// Posted once, when the server first names the project the task started.
pub const PROJECT_STARTED_TEXT: &str = "The project has started.";
pub const ANSWERED_TEXT: &str = "Thank you, I have passed that on.";
pub const CLOSED_TEXT: &str = "That question is no longer open.";

/// Which `_meta` channel a task's progress rides: the platform MCP server's fr block (`ai.nonagentic/fr`) or a
/// tenant's conveyor (`ai.nonagentic/conveyor`). Both carry a project and a run; fr's feed is `activity`, the
/// conveyor's is `events`. Only fr carries `signals`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Meta {
    Fr,
    Conveyor,
}

impl Meta {
    /// The conversation state key the project id is written under.
    fn project_id_key(self) -> &'static str {
        match self {
            Meta::Fr => state::FR_PROJECT_ID,
            Meta::Conveyor => state::CONVEYOR_PROJECT_ID,
        }
    }

    /// The state key naming the task that wrote [`Meta::project_id_key`].
    fn project_task_key(self) -> &'static str {
        match self {
            Meta::Fr => state::FR_PROJECT_TASK,
            Meta::Conveyor => state::CONVEYOR_PROJECT_TASK,
        }
    }

    /// The conversation-wide advisory lock around the project compare-and-write: one per channel, so an fr task and
    /// a conveyor task never wait on each other (they write different state keys).
    fn project_lock(self, conversation_id: &str) -> String {
        match self {
            Meta::Fr => format!("fr-project:{conversation_id}"),
            Meta::Conveyor => format!("conveyor-project:{conversation_id}"),
        }
    }
}

/// The progress block a task's last `tasks/get` carries: fr's when the server names one, else the conveyor's.
/// `None` when it names neither (nothing to relay).
fn progress_meta(detail: Option<&Value>) -> Option<(Meta, &Value)> {
    let meta = detail?.get("_meta")?;
    if let Some(fr) = meta.get(FR_META) {
        return Some((Meta::Fr, fr));
    }
    meta.get(CONVEYOR_META).map(|block| (Meta::Conveyor, block))
}

/// Relays task changes to conversations.
pub struct TaskRelay {
    pub st: AppState,
    pub root: Bus,
}

/// Start the relay (agent-api only; needs the runtime bus).
pub fn spawn(st: AppState) -> Option<tokio::task::JoinHandle<()>> {
    let root = st.redis.clone()?;
    let relay = TaskRelay { st, root };
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(RELAY_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(e) = relay.sweep().await {
                tracing::warn!(error = %e, "mcp task relay sweep failed");
            }
        }
    }))
}

impl TaskRelay {
    /// One pass over the tasks that need their conversation; returns how many were looked at.
    pub async fn sweep(&self) -> ApiResult<usize> {
        let due: Vec<(Uuid, Uuid, String)> =
            sqlx::query_as("SELECT tenant_id, job_id, task_id FROM agent_mcp_task_relay_due($1)")
                .bind(RELAY_LIMIT)
                .fetch_all(&self.st.db)
                .await?;
        // one model call per sweep (owner 2026-10-31): a second task needing an answer in this sweep waits for the
        // next sweep, and is never asked of the requester before the agent has had its turn
        let mut answer_budget = true;
        for (tenant, job, task_id) in &due {
            if let Err(e) = self.relay(*tenant, *job, task_id, &mut answer_budget).await {
                tracing::warn!(task_id = %task_id, error = %e, "mcp task relay failed");
            }
        }
        Ok(due.len())
    }
}

/// Where a task's case lives.
struct Place {
    conversation_id: String,
    case_id: String,
    env: String,
    job_key: String,
}

impl TaskRelay {
    /// Relay one task. `answer_budget` is the sweep's one model call: it is spent only when the agent is really
    /// asked to answer (owner 2026-10-31).
    pub async fn relay(
        &self,
        tenant: Uuid,
        job: Uuid,
        task_id: &str,
        answer_budget: &mut bool,
    ) -> ApiResult<()> {
        let mut tx = scoped(&self.st.db, tenant, job).await?;
        let row = sqlx::query(
            "SELECT t.status, t.case_id, t.last_detail, t.result, t.delivered_at IS NOT NULL AS delivered,
                    t.escalated_at IS NOT NULL AS escalated, t.escalation_notified_at IS NULL AS notify,
                    t.escalation_reason,
                    t.last_relayed_seq, t.last_relayed_status, t.operation,
                    array(SELECT input_key FROM mcp_task_answers a
                           WHERE a.task_id = t.task_id AND a.outcome IS DISTINCT FROM 'refused') AS answered,
                    c.conversation_id, c.env, j.key AS job_key
               FROM mcp_tasks t
               JOIN agent_case c ON c.case_id = t.case_id
               JOIN jobs j ON j.id = t.job_id
              WHERE t.task_id = $1",
        )
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        // no case: a staff-started task, followed in the Tasks inbox only
        let Some(r) = row else { return Ok(()) };
        let place = Place {
            conversation_id: r.get("conversation_id"),
            case_id: r.get("case_id"),
            env: r.get("env"),
            job_key: r.get("job_key"),
        };
        let scope = JobScope {
            tenant,
            job_id: job,
            job_key: place.job_key.clone(),
        };
        let Some(env) = Env::parse(&place.env) else {
            return Ok(());
        };
        let bus = self
            .root
            .scoped(&Scope::new(tenant, place.job_key.clone(), env)?);
        let status: String = r.get("status");
        let detail: Option<Value> = r.get("last_detail");
        let operation: String = r.get("operation");
        // What the server puts on the task's `_meta` is relayed while the task is live; a terminal task's story ends
        // with its outcome line (the delivered branch below).
        if !matches!(
            status.as_str(),
            "completed" | "cancelled" | "failed" | "lost"
        ) {
            self.progress(
                tenant,
                job,
                task_id,
                &place,
                &scope,
                &bus,
                &status,
                detail.as_ref(),
                r.get::<i64, _>("last_relayed_seq"),
                &operation,
            )
            .await?;
        } else {
            // A terminal task still carries the server's signals — a failed fr project IS a `completed` task whose
            // result is an error, and `project_failed` (critical) exists only on that terminal state. Skipping the
            // signals here would post "someone will look into it" and never actually escalate it.
            self.terminal_signals(tenant, job, task_id, &place, &scope, &bus, detail.as_ref())
                .await?;
        }
        if status == "input_required" && r.get::<bool, _>("escalated") {
            // staff own it: nothing more is asked here; the requester hears so once
            if r.get::<bool, _>("notify")
                && mark_notified(&self.st.db, tenant, job, task_id).await?
            {
                clear_task_pause(&bus, &place.conversation_id, task_id).await;
                let sent = post(&self.st, &bus, &scope, &place, "escalated", ESCALATED_TEXT).await;
                if let Err(err) = sent {
                    // The claim is what makes the notice once-only; release it so the next sweep tells them.
                    // A repeat is better than an escalation the requester never hears about.
                    let reason: Option<String> = r.get("escalation_reason");
                    if let Some(reason) = reason {
                        release_notified(&self.st.db, tenant, job, task_id, &reason).await?;
                    }
                    return Err(err);
                }
            }
            return Ok(());
        }
        if status == "input_required" {
            let answered: Vec<String> = r.get("answered");
            let open = detail
                .as_ref()
                .map(input_questions)
                .unwrap_or_default()
                .into_iter()
                .find(|q| !answered.contains(&q.key));
            if let Some(q) = open {
                self.ask(&bus, &scope, &place, env, task_id, &q, answer_budget)
                    .await?;
            }
            return Ok(());
        }
        if r.get::<bool, _>("delivered") {
            return Ok(());
        }
        let result: Option<Value> = r.get("result");
        let Some(text) = outcome_text(&status, result.as_ref()) else {
            return Ok(());
        };
        if !crate::mcp_client::store::mark_delivered(&self.st.db, tenant, job, task_id).await? {
            return Ok(()); // another poller delivered it
        }
        clear_task_pause(&bus, &place.conversation_id, task_id).await;
        if let Err(err) = post(&self.st, &bus, &scope, &place, "result", &text).await {
            // The claim above is what makes delivery once-only; release it so the next sweep retries.
            // A repeat is better than a result the requester never sees.
            if let Err(undo) =
                crate::mcp_client::store::unmark_delivered(&self.st.db, tenant, job, task_id).await
            {
                tracing::warn!(
                    task_id = %task_id, error = %undo,
                    "mcp task result not posted and its delivery claim could not be released"
                );
            }
            return Err(err);
        }
        Ok(())
    }

    /// Answer `q` as the job when it can (owner 2026-10-31), then ask the requester for whatever is left; unless the
    /// conversation already waits on it.
    #[allow(clippy::too_many_arguments)]
    async fn ask(
        &self,
        bus: &Bus,
        scope: &JobScope,
        place: &Place,
        env: Env,
        task_id: &str,
        q: &InputQuestion,
        answer_budget: &mut bool,
    ) -> ApiResult<()> {
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(p) = parked(bus, &place.conversation_id).await
            && p.fresh(now)
            && p.task
                .as_ref()
                .is_some_and(|t| t.task_id == task_id && t.question == q.key)
        {
            return Ok(());
        }
        // Nothing a person could type is a field the agent could fill either: staff answer it in the Tasks inbox.
        if next_field(&q.requested_schema, &BTreeMap::new()).is_none() {
            return Ok(());
        }

        // The agent answers first, and the requester is asked only once the agent is done with the question. One model
        // call per sweep: a question the budget does not reach waits for the next sweep. A question another relay is
        // answering (its claim is held, at most ten minutes) is left alone.
        let mut filled = Filled::None;
        if !agent_done(&self.st.db, scope.tenant, scope.job_id, task_id, &q.key).await? {
            if !*answer_budget
                || !claim_agent_attempt(&self.st.db, scope.tenant, scope.job_id, task_id, &q.key)
                    .await?
            {
                return Ok(());
            }
            *answer_budget = false;
            let ask = task_answer::Ask {
                tenant: scope.tenant,
                job: scope.job_id,
                job_key: &scope.job_key,
                env,
                case_id: &place.case_id,
                conversation_id: &place.conversation_id,
            };
            // the usage is the one call's, metered below (use `task_answer`); a call that timed out has none
            let (what, usage) = task_answer::fields(&self.st, &ask, q).await;
            filled = what;
            if let Some(usage) = &usage {
                self.meter_answer(scope, place, usage).await;
            }
        }
        // C0 (owner 2026-11-01): a decision without its answer text is not an answer — fr spends the approval and
        // leaves the card blocked. It counts as none, so the requester is asked (and their reply carries the text).
        let collected = match filled {
            Filled::Values(values) if !decision_without_text(&q.requested_schema, &values) => {
                values
            }
            Filled::Values(_) | Filled::None => BTreeMap::new(),
        };

        // The agent gave the whole answer: fr gets it as the job's service actor and the requester is not asked.
        // fr's own validator must take it, else the question stays here for the field that is missing.
        if !collected.is_empty()
            && next_field(&q.requested_schema, &collected).is_none()
            && validate_answer(&q.requested_schema, "accept", Some(&json!(collected))).is_ok()
        {
            let actor = StaffActor::for_job(scope.tenant, scope.job_id);
            let input = AnswerInput {
                key: q.key.clone(),
                action: "accept".to_owned(),
                content: Some(serde_json::to_value(&collected).map_err(anyhow::Error::from)?),
            };
            // audit: the requester's messages the model's answer was built from (fr records only the job)
            let source =
                source_messages(&self.st, scope.tenant, scope.job_id, &place.conversation_id)
                    .await?;
            let sent = answer_task(
                &self.st.db,
                &self.st.task_clients(),
                &actor,
                scope.job_id,
                task_id,
                &input,
                &source,
            )
            .await;
            if let Err(e) = sent {
                // an unavailable backend is not sent: the claim is released so a later sweep tries again, never the
                // requester
                if matches!(&e, ApiError::Unavailable("mcp_unconfigured", _, _)) {
                    tracing::warn!(task_id = %task_id, code = "mcp_unavailable", "mcp task answer not sent");
                    release_agent_attempt(&self.st.db, scope.tenant, scope.job_id, task_id).await?;
                    return Ok(());
                }
                release_agent_attempt(&self.st.db, scope.tenant, scope.job_id, task_id).await?;
                return Err(e);
            }
            mark_agent_done(&self.st.db, scope.tenant, scope.job_id, task_id, &q.key).await?;
            return Ok(());
        }

        // The agent is done with the question; whatever it left out (or all of it, when it gave nothing) is the
        // requester's to answer (owner 2026-11-01: an id is asked like any other field).
        mark_agent_done(&self.st.db, scope.tenant, scope.job_id, task_id, &q.key).await?;
        let Some(field) = next_field(&q.requested_schema, &collected) else {
            return Ok(()); // nothing a person can type: staff answer it in the Tasks inbox
        };
        // the relay asks only when the requester has never seen this question
        let pause = task_pause(&place.case_id, task_id, q, &field, collected, true, now);
        park(bus, &place.conversation_id, &pause).await?;
        post(&self.st, bus, scope, place, "question", &pause.prompt).await
    }

    /// Meter the task answer's ONE model call (one `usage_event` row, use [`task_answer::USAGE_LABEL`]), attributed
    /// like any other model call of the conversation's channel. Fail-soft: the attempt claim is already spent, so a
    /// ledger write that fails must neither lose the answer nor ask the requester again. That call is then unmetered
    /// — the same trade the runner makes for a turn it cannot meter, not a row something replays later.
    async fn meter_answer(&self, scope: &JobScope, place: &Place, usage: &LlmUsage) {
        let metered: ApiResult<()> = async {
            let mut tx = store::begin_job(&self.st.db, scope.tenant, scope.job_id).await?;
            let conv = store::conversation(&mut tx, &place.conversation_id).await?;
            let (source, actor) = crate::runtime::assistant::usage_party(conv.as_ref());
            crate::usage::emit(
                &mut tx,
                &self.st.cfg.metering,
                &crate::usage::UsageEvent {
                    job_id: Some(scope.job_id),
                    source,
                    use_: task_answer::USAGE_LABEL,
                    activity: None,
                    conversation_id: Some(&place.conversation_id),
                    case_id: Some(&place.case_id),
                    actor: &actor,
                    usage,
                },
            )
            .await?;
            tx.commit().await?;
            Ok(())
        }
        .await;
        if let Err(e) = metered {
            tracing::error!(error = %e, "task_answer_usage_failed");
        }
    }

    /// Relay the progress `_meta` a live task carries — fr's, or a tenant's conveyor ([`Meta`]) (owner 2026-11-06):
    /// the project id once, one line per activity entry the conversation has not been given, and, for fr, one
    /// escalation with one notice per severe signal. The activity seq advances only after its line is posted, so a
    /// failed post is retried by the next due sweep, not lost.
    #[allow(clippy::too_many_arguments)]
    async fn progress(
        &self,
        tenant: Uuid,
        job: Uuid,
        task_id: &str,
        place: &Place,
        scope: &JobScope,
        bus: &Bus,
        status: &str,
        detail: Option<&Value>,
        last_seq: i64,
        operation: &str,
    ) -> ApiResult<()> {
        let Some((kind, block)) = progress_meta(detail) else {
            // nothing to say, but the status seen is recorded so the sweep is not due for it again
            return mark_relayed(&self.st.db, tenant, job, task_id, last_seq, status).await;
        };
        // The server's own `started` activity entry already says the project began: the fixed line would be a second
        // "it started" message in the same sweep. It is the fallback for a server that names the project without one.
        self.project(
            tenant,
            job,
            task_id,
            place,
            scope,
            bus,
            kind,
            block,
            operation,
            !has_started_entry(kind, block),
        )
        .await?;
        let mut seq = last_seq;
        for (entry_seq, text) in activity_after(kind, block, last_seq) {
            if !text.is_empty() {
                post(&self.st, bus, scope, place, "progress", &text).await?;
            }
            seq = seq.max(entry_seq);
            mark_relayed(&self.st.db, tenant, job, task_id, seq, status).await?;
        }
        // a conveyor emits no signals: only fr's feed escalates a task
        if kind == Meta::Fr {
            self.signals(tenant, job, task_id, place, scope, bus, block)
                .await?;
        }
        mark_relayed(&self.st.db, tenant, job, task_id, seq, status).await
    }

    /// Write the conversation's project id (`state::FR_PROJECT_ID` for fr, the conveyor's own keys for a conveyor)
    /// and the id of the task that named it, when the server first names a project, or when THIS task started after
    /// the task that named the current one: the newest started project wins, so an older task's project seen later
    /// never overwrites it. The project-started line is posted on first sight only.
    #[allow(clippy::too_many_arguments)]
    async fn project(
        &self,
        tenant: Uuid,
        job: Uuid,
        task_id: &str,
        place: &Place,
        scope: &JobScope,
        bus: &Bus,
        kind: Meta,
        block: &Value,
        operation: &str,
        announce: bool,
    ) -> ApiResult<()> {
        let Some(project) = block["project_id"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty() && *p != "null")
        else {
            return Ok(());
        };
        let mut tx = store::begin_job(&self.st.db, tenant, job).await?;
        // one writer per conversation and channel: two tasks' sweeps must not interleave their compare-and-write
        store::lock(&mut tx, &kind.project_lock(&place.conversation_id)).await?;
        let entries = state::load(&mut tx, &place.conversation_id).await?;
        let known = entries.contains_key(kind.project_id_key());
        let previous = entries
            .get(kind.project_task_key())
            .and_then(state::StateValue::as_value);
        // One query reads both start times under the same lock: the task naming the project now, and the task that
        // named it before. A previous id that no longer has a row reads as no previous task.
        let (cur, prev): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT cur.created_at, prev.created_at FROM mcp_tasks cur \
             LEFT JOIN mcp_tasks prev ON prev.task_id = $2 WHERE cur.task_id = $1",
        )
        .bind(task_id)
        .bind(previous)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or((None, None));
        // First sight writes. After that only a task that started later replaces it, and only when the conversation
        // records which task named the current project: a key written before this rule existed (no
        // `fr_project_task`) has no start time to compare, so it is kept rather than guessed away.
        let newer = !known || (previous.is_some() && names_a_newer_project(cur, prev));
        if newer {
            let produced_by = format!("tool:{operation}");
            state::put_value(
                &mut tx,
                &place.conversation_id,
                kind.project_id_key(),
                project,
                &produced_by,
                None,
            )
            .await?;
            state::put_value(
                &mut tx,
                &place.conversation_id,
                kind.project_task_key(),
                task_id,
                &produced_by,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        if !known && announce {
            post(
                &self.st,
                bus,
                scope,
                place,
                "progress",
                PROJECT_STARTED_TEXT,
            )
            .await?;
        }
        Ok(())
    }

    /// The signals of a task that has already ended. The server reports `project_failed` / `start_refused` only on
    /// such a task's final `_meta`, so this is the only place a failed fr project can escalate.
    #[allow(clippy::too_many_arguments)]
    async fn terminal_signals(
        &self,
        tenant: Uuid,
        job: Uuid,
        task_id: &str,
        place: &Place,
        scope: &JobScope,
        bus: &Bus,
        detail: Option<&Value>,
    ) -> ApiResult<()> {
        let Some(fr) = detail
            .and_then(|d| d.get("_meta"))
            .and_then(|m| m.get(FR_META))
        else {
            return Ok(());
        };
        self.signals(tenant, job, task_id, place, scope, bus, fr)
            .await
    }

    /// Escalate a task on its severe signals. `critical` / `high` are the two that need a person: the first one
    /// escalates the task, and a strictly worse one upgrades the reason ([`escalate_upgrade`]); both clear the notice
    /// claim so the one notice naming the severity is posted below. The claim ([`mark_notified`]) is what makes the
    /// tell once-only across sweeps — and, because a post that failed releases it again, what retries it.
    #[allow(clippy::too_many_arguments)]
    async fn signals(
        &self,
        tenant: Uuid,
        job: Uuid,
        task_id: &str,
        place: &Place,
        scope: &JobScope,
        bus: &Bus,
        fr: &Value,
    ) -> ApiResult<()> {
        for signal in fr["signals"].as_array().into_iter().flatten() {
            let severity = signal["severity"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !matches!(severity_priority(&severity), "urgent" | "high") {
                continue;
            }
            // `escalation_reason` holds 80 characters at most: an over-long server code is cut, not an error
            let code: String = signal["code"]
                .as_str()
                .unwrap_or("")
                .trim()
                .chars()
                .take(64)
                .collect();
            if code.is_empty() {
                continue;
            }
            let reason = format!("fr:{severity}:{code}");
            // `notify = true`: this relay posts the notice itself, so the claim stays open for it to take below
            let mut tx = scoped(&self.st.db, tenant, job).await?;
            if !escalate(&mut tx, task_id, &reason, true).await? {
                // an escalated task takes a strictly worse signal's reason (and its notice); a repeat or a weaker one
                // is not news
                escalate_upgrade(&mut tx, task_id, &reason, severity_rank(&severity)).await?;
            }
            tx.commit().await?;
            // Always through the claim: it is both the once-only guard and the retry of a notice a failed post
            // released (escalation_notified_at back to NULL).
            if !mark_notified(&self.st.db, tenant, job, task_id).await? {
                continue;
            }
            let detail = signal["detail"].as_str().unwrap_or("");
            let notice = signal_notice(&severity, detail);
            let sent = post(&self.st, bus, scope, place, "escalated", &notice).await;
            if let Err(err) = sent {
                release_notified(&self.st.db, tenant, job, task_id, &reason).await?;
                return Err(err);
            }
        }
        Ok(())
    }
}

/// Whether the task being relayed started after the task that named the conversation's project: the newest started
/// project wins `fr_project_id`. True when there is no previous task (or its row is gone) and `cur` is known; false
/// when `cur` is unknown; on a tie the first stays, so this is strictly newer.
fn names_a_newer_project(cur: Option<DateTime<Utc>>, prev: Option<DateTime<Utc>>) -> bool {
    match (cur, prev) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(cur), Some(prev)) => cur > prev,
    }
}

/// Whether the server's feed already carries the `started` entry, so the fixed project-started line would repeat it.
/// fr names it (`activity[].kind == "started"`); the conveyor emits `run.started`, which [`conveyor_event_text`]
/// renders as no line, so the fixed line is its only "it began" message.
fn has_started_entry(kind: Meta, block: &Value) -> bool {
    match kind {
        Meta::Fr => block["activity"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| entry["kind"].as_str() == Some("started")),
        Meta::Conveyor => false,
    }
}

/// The entries the conversation has not been given yet: a numeric `seq` above `last_seq`, in seq order, one entry
/// per seq (a repeated seq is relayed once). An entry without a seq cannot be ordered and is left out. fr's feed is
/// `activity` (`{seq, kind, text}`); the conveyor's is `events` (`{seq, type, payload}`), rendered by
/// [`conveyor_event_text`].
fn activity_after(kind: Meta, block: &Value, last_seq: i64) -> Vec<(i64, String)> {
    let key = match kind {
        Meta::Fr => "activity",
        Meta::Conveyor => "events",
    };
    let mut out: Vec<(i64, String)> = Vec::new();
    for entry in block[key].as_array().into_iter().flatten() {
        let Some(seq) = entry["seq"].as_i64() else {
            continue;
        };
        if seq <= last_seq || out.iter().any(|(known, _)| *known == seq) {
            continue;
        }
        let text = match kind {
            Meta::Fr => entry["text"].as_str().unwrap_or("").trim().to_owned(),
            Meta::Conveyor => conveyor_event_text(entry),
        };
        out.push((seq, text));
    }
    out.sort_by_key(|(seq, _)| *seq);
    out
}

/// The customer-facing line for one conveyor event (`{seq, type, payload, created_at}`): the run's step, outcome or
/// cancellation in plain words, and any other type humanised rather than dropped.
fn conveyor_event_text(event: &Value) -> String {
    let Some(event_type) = event["type"].as_str().filter(|t| !t.is_empty()) else {
        return String::new();
    };
    let payload = &event["payload"];
    match event_type {
        // The fixed PROJECT_STARTED_TEXT already says the run began; these two would repeat it.
        "run.created" | "run.started" => String::new(),
        "run_step.changed" => {
            let position = payload["position"].as_i64();
            let status = payload["status"].as_str().map(str::trim).unwrap_or("");
            if position.is_none() || status.is_empty() {
                // A step event without both halves says nothing a person can act on; skip it rather than "Step : ".
                String::new()
            } else {
                format!("Step {}: {status}", position.unwrap_or_default())
            }
        }
        "run.succeeded" => "The project run finished successfully.".to_owned(),
        "run.failed" => match payload["error_class"].as_str().filter(|e| !e.is_empty()) {
            Some(error_class) => format!("The project run failed ({error_class})."),
            None => "The project run failed.".to_owned(),
        },
        "run.cancelled" => match payload["reason"].as_str().filter(|r| !r.is_empty()) {
            Some(reason) => format!("The project run was cancelled: {reason}."),
            None => "The project run was cancelled.".to_owned(),
        },
        other => {
            let mut text: String = other
                .chars()
                .map(|c| if c == '.' || c == '_' { ' ' } else { c })
                .collect();
            if let Some(first) = text.chars().next() {
                text.replace_range(..first.len_utf8(), &first.to_uppercase().to_string());
            }
            text.push('.');
            text
        }
    }
}

#[cfg(test)]
mod conveyor_event_text_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_step_changed_names_the_step_and_its_status() {
        let event = json!({
            "seq": 1, "type": "run_step.changed",
            "payload": {"position": 1, "status": "running"},
        });
        assert_eq!(conveyor_event_text(&event), "Step 1: running");
        // a missing half is no line at all, never a "Step : "
        let empty = json!({"seq": 2, "type": "run_step.changed", "payload": {}});
        assert_eq!(conveyor_event_text(&empty), "");
    }

    #[test]
    fn run_created_and_started_say_nothing_the_fixed_line_does_not() {
        // The live conveyor's `_meta.events` opens with these two (captured 2026-11-09); the relay posts the fixed
        // PROJECT_STARTED_TEXT once instead, so both render as no line at all.
        for t in ["run.created", "run.started"] {
            let event = json!({"seq": 4, "type": t, "payload": {"run_id": "r-1", "attempt": 1}});
            assert_eq!(conveyor_event_text(&event), "", "{t}");
        }
    }

    #[test]
    fn run_succeeded_and_failed_say_the_run_finished_or_failed() {
        let succeeded = json!({"type": "run.succeeded", "payload": {"outcome": "ok"}});
        assert_eq!(
            conveyor_event_text(&succeeded),
            "The project run finished successfully."
        );
        let failed = json!({"type": "run.failed", "payload": {"error_class": "policy_denied"}});
        assert_eq!(
            conveyor_event_text(&failed),
            "The project run failed (policy_denied)."
        );
        let bare = json!({"type": "run.failed", "payload": {}});
        assert_eq!(conveyor_event_text(&bare), "The project run failed.");
    }

    #[test]
    fn run_cancelled_names_its_reason_when_there_is_one() {
        let cancelled = json!({"type": "run.cancelled", "payload": {"reason": "user request"}});
        assert_eq!(
            conveyor_event_text(&cancelled),
            "The project run was cancelled: user request."
        );
        let bare = json!({"type": "run.cancelled", "payload": {}});
        assert_eq!(conveyor_event_text(&bare), "The project run was cancelled.");
    }
    #[test]
    fn an_unknown_type_is_humanised_not_dropped() {
        let unknown = json!({"type": "run.something_new", "payload": {}});
        assert_eq!(conveyor_event_text(&unknown), "Run something new.");
        let artifact = json!({"type": "artifact.updated"});
        assert_eq!(conveyor_event_text(&artifact), "Artifact updated.");
    }
}

/// The user-facing line for an escalated signal: what the server said, the severity in plain words, and who acts
/// next. `detail` is the signal's own text; the fixed line when it has none.
pub fn signal_notice(severity: &str, detail: &str) -> String {
    let severity = match severity {
        "critical" => "critical",
        "high" => "high",
        _ => "serious",
    };
    let detail = detail.trim();
    if detail.is_empty() {
        format!(
            "The project reported a {severity} problem. Someone from the team will look into it and reply here."
        )
    } else {
        format!(
            "{detail} This is a {severity} issue: someone from the team will look into it and reply here."
        )
    }
}

/// Record how far the conversation has been brought (the last activity seq relayed and the status seen there), so
/// the sweep is not due for what it already relayed (migration 0249).
async fn mark_relayed(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
    seq: i64,
    status: &str,
) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    sqlx::query(
        "UPDATE mcp_tasks SET last_relayed_seq = greatest(last_relayed_seq, $2), last_relayed_status = $3,
                              updated_at = now()
          WHERE task_id = $1 AND terminal_at IS NULL
            AND (last_relayed_status IS DISTINCT FROM $3 OR last_relayed_seq < $2)",
    )
    .bind(task_id)
    .bind(seq)
    .bind(status)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Claim the agent's one answer attempt for `question` on `task_id` (at most once per ten minutes): `true` = the
/// model may be asked now. The marker is per question, not per task: fr asks several questions on one task and each
/// gets its own attempt (owner 2026-10-31).
async fn claim_agent_attempt(
    db: &sqlx::PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
    question: &str,
) -> ApiResult<bool> {
    let mut tx = scoped(db, tenant, job).await?;
    let r = sqlx::query(
        "UPDATE mcp_tasks SET agent_attempted_at = now(), agent_attempted_question = $2
          WHERE task_id = $1
            AND (agent_attempted_question IS DISTINCT FROM $2
                 OR agent_attempted_at IS NULL
                 OR agent_attempted_at < now() - interval '10 minutes')",
    )
    .bind(task_id)
    .bind(question)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(r.rows_affected() > 0)
}

/// Whether the agent is done with `question` on `task_id`: it answered it, or left fields for the requester.
async fn agent_done(
    db: &sqlx::PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
    question: &str,
) -> ApiResult<bool> {
    let mut tx = scoped(db, tenant, job).await?;
    let done: Option<bool> = sqlx::query_scalar(
        "SELECT agent_done_question IS NOT DISTINCT FROM $2 FROM mcp_tasks WHERE task_id = $1",
    )
    .bind(task_id)
    .bind(question)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(done.unwrap_or(false))
}

/// Record that the agent is done with `question` on `task_id`: from now on the requester may be asked for it.
async fn mark_agent_done(
    db: &sqlx::PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
    question: &str,
) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    sqlx::query("UPDATE mcp_tasks SET agent_done_question = $2 WHERE task_id = $1")
        .bind(task_id)
        .bind(question)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Release the agent's claim on `task_id` after an answer it could not send: the next sweep tries again.
async fn release_agent_attempt(
    db: &sqlx::PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    sqlx::query("UPDATE mcp_tasks SET agent_attempted_at = NULL WHERE task_id = $1")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The requester's messages (`customer`, or `caller` for an API-key caller) the answer was built from: the same
/// window [`task_answer::fields`] reads. fr records only the job as the actor, so the messages behind its answer are
/// kept with it.
async fn source_messages(
    st: &AppState,
    tenant: Uuid,
    job: Uuid,
    conversation_id: &str,
) -> ApiResult<Vec<String>> {
    let mut tx = store::begin_job(&st.db, tenant, job).await?;
    let msgs = store::history(&mut tx, conversation_id, task_answer::HISTORY).await?;
    tx.commit().await?;
    Ok(msgs
        .iter()
        .filter(|m| m.role == "customer" || m.role == "caller")
        .map(|m| m.message_id.to_string())
        .collect())
}

/// Claim telling the requester about an escalation (once, across relays): `true` = this relay tells them.
async fn mark_notified(
    db: &sqlx::PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
) -> ApiResult<bool> {
    let mut tx = scoped(db, tenant, job).await?;
    let r = sqlx::query(
        "UPDATE mcp_tasks SET escalation_notified_at = now()
          WHERE task_id = $1 AND escalated_at IS NOT NULL AND escalation_notified_at IS NULL",
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(r.rows_affected() > 0)
}

/// Release the notice claim of an escalated task after its notice could not be posted, so the next sweep tells the
/// requester (a repeat is better than a notice nobody saw). `reason` is the claim's own escalation reason: a newer
/// escalation's claim is never cleared by an older notice's failure.
async fn release_notified(
    db: &sqlx::PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
    reason: &str,
) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    sqlx::query(
        "UPDATE mcp_tasks SET escalation_notified_at = NULL
          WHERE task_id = $1 AND escalated_at IS NOT NULL AND escalation_reason = $2",
    )
    .bind(task_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Cancel the unfinished tasks the conversation's cases started (the conversation was closed by staff), best effort:
/// a failure is logged, and a cancel the server did not take is retried by the poller.
pub async fn cancel_open(st: &AppState, scope: &JobScope, conversation_id: &str) {
    let open: ApiResult<Vec<String>> = async {
        let mut tx = scoped(&st.db, scope.tenant, scope.job_id).await?;
        let ids = sqlx::query_scalar(
            "SELECT t.task_id FROM mcp_tasks t JOIN agent_case c ON c.case_id = t.case_id
              WHERE c.conversation_id = $1 AND t.terminal_at IS NULL AND NOT t.cancel_requested",
        )
        .bind(conversation_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ids)
    }
    .await;
    let ids = match open {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(conversation = conversation_id, error = %e, "mcp task cancel lookup failed");
            return;
        }
    };
    for id in ids {
        let r = crate::mcp_client::store::cancel(
            &st.db,
            &st.task_clients(),
            scope.tenant,
            scope.job_id,
            &id,
        )
        .await;
        if let Err(e) = r {
            tracing::warn!(task_id = %id, error = %e, "mcp task cancel on close failed");
        }
    }
}

/// The pause that asks `field` of `q`. `first` = this is the requester's first sight of the question, so the
/// server's own message opens the prompt. It is not `collected.is_empty()`: values the job's model filled itself
/// (`crate::runtime::task_answer`, owner 2026-10-31) are never shown to the requester, so the first thing the
/// requester is asked still has to carry the question.
pub fn task_pause(
    case_id: &str,
    task_id: &str,
    q: &InputQuestion,
    field: &str,
    collected: BTreeMap<String, String>,
    first: bool,
    now_ms: i64,
) -> InputPause {
    InputPause {
        case_id: case_id.to_owned(),
        context_space: String::new(),
        slots: BTreeMap::new(),
        message_id: None,
        key: SECRET_KEY.to_owned(),
        prompt: field_prompt(&q.message, &q.requested_schema, field, first),
        secret: field_secret(&q.requested_schema, field),
        retries: RETRIES,
        attempts: 0,
        until_ms: now_ms + INPUT_TTL_MS,
        task: Some(TaskAsk {
            task_id: task_id.to_owned(),
            question: q.key.clone(),
            field: field.to_owned(),
            collected,
        }),
    }
}

/// The next schema field a person still has to give: required fields first (schema order), then the optional ones
/// that are not filled. A property that is not a string is not askable in chat. `None` when nothing is left.
/// Every string field is asked alike (owner 2026-11-01: the never-ask-for-ids rule governs our demos, not the
/// platform — a job may ask its requester for an id it could not find).
pub fn next_field(schema: &Value, collected: &BTreeMap<String, String>) -> Option<String> {
    let props = schema["properties"].as_object()?;
    let askable = |k: &String| {
        !collected.contains_key(k)
            && props[k.as_str()]["type"]
                .as_str()
                .is_none_or(|t| t == "string")
    };
    let required: Vec<String> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(str::to_owned))
        .collect();
    if let Some(k) = required
        .iter()
        .find(|k| props.contains_key(k.as_str()) && askable(k))
    {
        return Some(k.clone());
    }
    // C0 (owner 2026-11-01): an approve/reject always carries its answer text — fr spends the approval and leaves
    // the card blocked without one. When a decision is collected and no text field it offers is filled, that text
    // is what is asked next.
    if let Some(k) = decision_text_to_ask(schema, collected) {
        return Some(k);
    }
    // An optional field is asked only while a required one was asked before it: a question with no required field
    // gets one reply, into its first field.
    if collected.is_empty() {
        return props.keys().find(|k| askable(k)).cloned();
    }
    None
}

/// The decision a question offers: a string enum holding accept-like values (`approve`, `accept`, …) and
/// reject-like ones (`reject`, `decline`, …). `None` for any other enum (a plain choice is not a decision).
pub fn decision_field(schema: &Value) -> Option<String> {
    let props = schema["properties"].as_object()?;
    for (k, p) in props {
        let Some(values) = p["enum"].as_array() else {
            continue;
        };
        let values: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(|v| v.trim().to_ascii_lowercase())
            .collect();
        let accepts = values.iter().any(|v| ACCEPT_VALUES.contains(&v.as_str()));
        let rejects = values.iter().any(|v| REJECT_VALUES.contains(&v.as_str()));
        if accepts && rejects {
            return Some(k.clone());
        }
    }
    None
}

const ACCEPT_VALUES: [&str; 4] = ["approve", "approved", "accept", "accepted"];
const REJECT_VALUES: [&str; 4] = ["reject", "rejected", "decline", "declined"];

/// The text field a decision still needs, in the order fr's approval schema prefers it: an approve's answer is its
/// `comment`, a reject's is its `reason` (the server's grammar). Any other offered, non-secret string field counts
/// after those. `None` when there is no decision, it already carries text, or the question offers no text at all.
pub fn decision_text_to_ask(
    schema: &Value,
    collected: &BTreeMap<String, String>,
) -> Option<String> {
    let decision = decision_field(schema)?;
    let value = collected.get(&decision)?.trim().to_ascii_lowercase();
    let rejects = REJECT_VALUES.contains(&value.as_str());
    let props = schema["properties"].as_object()?;
    let filled = |k: &str| collected.get(k).is_some_and(|v| !v.trim().is_empty());
    let offered = |k: &str| {
        props.get(k).is_some_and(|p| {
            p["type"].as_str().is_none_or(|t| t == "string") && !field_secret(schema, k)
        })
    };
    // fr's own grammar (external/fr.py `decision_text`): an approve's answer text is its `comment`, a reject's is
    // its `reason`. A decision that fills the other one would still spend the approval with no text.
    let key = if rejects { "reason" } else { "comment" };
    if offered(key) {
        return (!filled(key)).then(|| key.to_owned());
    }
    // another schema's text field: any non-secret string field it offers
    let others: Vec<String> = props
        .iter()
        .filter(|(k, p)| {
            k.as_str() != decision
                && p["type"].as_str().is_none_or(|t| t == "string")
                && !field_secret(schema, k)
        })
        .map(|(k, _)| k.clone())
        .collect();
    if others.iter().any(|k| filled(k)) {
        return None; // the decision carries its text
    }
    others.first().cloned()
}

/// Does the answer carry a decision that has no answer text? Then it is not sent at all (C0): fr would spend the
/// approval and leave the card blocked. The requester is asked instead. True when the question offers no text field
/// either — the agent never sends a textless decision.
pub fn decision_without_text(schema: &Value, collected: &BTreeMap<String, String>) -> bool {
    let Some(decision) = decision_field(schema) else {
        return false;
    };
    if !collected.contains_key(&decision) {
        return false;
    }
    let Some(props) = schema["properties"].as_object() else {
        return true;
    };
    let text_offered = props.iter().any(|(k, p)| {
        k.as_str() != decision
            && p["type"].as_str().is_none_or(|t| t == "string")
            && !field_secret(schema, k)
    });
    !text_offered || decision_text_to_ask(schema, collected).is_some()
}

/// Whether a field's answer is a secret (a password field).
pub fn field_secret(schema: &Value, field: &str) -> bool {
    let p = &schema["properties"][field];
    p["format"] == "password" || p["writeOnly"] == true
}

/// The question text for one field: the server's message on the first field, then the field's title or description,
/// and the allowed values of an enum.
pub fn field_prompt(message: &str, schema: &Value, field: &str, first: bool) -> String {
    let p = &schema["properties"][field];
    let label = p["title"]
        .as_str()
        .or(p["description"].as_str())
        .unwrap_or(match field {
            // fr's approval text fields carry no description: say what the answer is for
            "comment" => "What note should go with it?",
            "reason" => "Why?",
            _ => field,
        });
    let mut out = if first && !message.trim().is_empty() {
        let single = schema["properties"]
            .as_object()
            .is_some_and(|m| m.len() == 1);
        if single && p["title"].is_null() && p["description"].is_null() {
            message.trim().to_owned()
        } else {
            format!("{}\n{label}", message.trim())
        }
    } else {
        label.to_owned()
    };
    let options = enum_options(p);
    if !options.is_empty() {
        out.push_str(&format!(" ({})", options.join(" / ")));
    }
    out
}

/// An enum field's values as offered to the user (its `enumNames` when they match one to one).
fn enum_options(p: &Value) -> Vec<String> {
    let values: Vec<&str> = p["enum"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let names: Vec<&str> = p["enumNames"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if names.len() == values.len() && !names.is_empty() {
        names.into_iter().map(str::to_owned).collect()
    } else {
        values.into_iter().map(str::to_owned).collect()
    }
}

/// The value a reply gives `field`: trimmed text, an enum value matched without case (or by its `enumNames`
/// entry). `Err` names what is wrong, as the next prompt.
pub fn accept_field(schema: &Value, field: &str, reply: &str) -> Result<String, String> {
    let text = reply.trim();
    if text.is_empty() {
        return Err(String::new());
    }
    let p = &schema["properties"][field];
    let Some(values) = p["enum"].as_array() else {
        return Ok(text.to_owned());
    };
    let names = p["enumNames"].as_array();
    for (i, v) in values.iter().enumerate() {
        let Some(v) = v.as_str() else { continue };
        let named = names
            .and_then(|n| n.get(i))
            .and_then(Value::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(text));
        if v.eq_ignore_ascii_case(text) || named {
            return Ok(v.to_owned());
        }
    }
    Err(format!(
        "Please answer one of: {}.",
        enum_options(p).join(" / ")
    ))
}

/// The line posted for a terminal task: the tool's own words when its result is text (or carries a
/// `message`/`summary`/`text` string), the same inside an MCP run envelope (`result.result`), a completed conveyor
/// run's status, result and error ([`conveyor_outcome_text`]), or a summary of a structured result — an fr project's
/// name and status plus the deliverables and links it closed with. A completed task whose tool reported an error is
/// a failure. `None` for a status that is not terminal. `result` is the stored `TaskOutcome::to_json`.
pub fn outcome_text(status: &str, result: Option<&Value>) -> Option<String> {
    match status {
        "completed" if result.is_some_and(|r| r["ok"] == false) => Some(FAILED_TEXT.to_owned()),
        "completed" => Some(
            result
                .and_then(|r| {
                    conveyor_outcome_text(&r["result"]).or_else(|| result_text(&r["result"], 0))
                })
                .unwrap_or_else(|| DONE_TEXT.to_owned()),
        ),
        "failed" | "lost" => Some(FAILED_TEXT.to_owned()),
        "cancelled" => Some(CANCELLED_TEXT.to_owned()),
        _ => None,
    }
}

/// A completed conveyor task's structured result (the conveyor's `structuredContent`:
/// `{run_id, project_id, status, result, error}`): the run's status, then its result text, then the error it
/// reported. Identified by its `run_id`, `project_id` and `status` — fr's run envelope carries no `project_id` — so
/// every fr rendering stays exactly as it was. `None` when `v` is not that shape.
fn conveyor_outcome_text(v: &Value) -> Option<String> {
    let status = v["status"].as_str().and_then(non_empty)?;
    v["run_id"].as_str().and_then(non_empty)?;
    v["project_id"].as_str().and_then(non_empty)?;
    let mut text = format!("Project {status}.");
    if let Some(result) = conveyor_result_text(&v["result"]) {
        text.push('\n');
        text.push_str(&result);
    }
    if let Some(error) = v["error"].as_str().and_then(non_empty) {
        text.push('\n');
        text.push_str(&error);
    }
    Some(text)
}

/// The text a conveyor run's `result` carries: a plain string or its `message`/`summary`/`text`, bounded like any
/// structured summary.
fn conveyor_result_text(v: &Value) -> Option<String> {
    let text = match v {
        Value::String(s) => non_empty(s),
        _ => ["message", "summary", "text"]
            .iter()
            .find_map(|k| v[*k].as_str())
            .and_then(non_empty),
    }?;
    Some(truncate_chars(text, SUMMARY_CHARS))
}

/// How many nested `result` envelopes to unwrap (the MCP run envelope wraps the tool's own result).
const RESULT_DEPTH: usize = 3;
/// Bound on a structured summary; the links are the only reason it can grow.
const SUMMARY_CHARS: usize = 1200;
/// Bound on result lines listed for one structured result.
const SUMMARY_ITEMS: usize = 6;

/// The human text of a completed tool result: a plain string, a `message`/`summary`/`text` field, the
/// same inside a run envelope's `result`, or a summary of a structured result. Structured data with
/// none of those gives `None`.
fn result_text(v: &Value, depth: usize) -> Option<String> {
    match v {
        Value::String(s) => non_empty(s),
        _ => {
            if let Some(text) = ["message", "summary", "text"]
                .iter()
                .find_map(|k| v[*k].as_str())
                .and_then(non_empty)
            {
                return Some(text);
            }
            // ⛔ Do not fall through to the envelope's own fields: its `status` is the RUN's status,
            // not the result's, and "The task is succeeded." is worse than the fixed line.
            if depth < RESULT_DEPTH
                && let Some(inner) = v.get("result").filter(|inner| !inner.is_null())
            {
                return result_text(inner, depth + 1);
            }
            structured_result_text(v)
        }
    }
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// A structured tool result as one message: its name and status, then the result lines it carries —
/// an fr project's deliverables, delivery evidence and URL. `None` when it has none of those (so the
/// caller keeps `DONE_TEXT`).
fn structured_result_text(v: &Value) -> Option<String> {
    let name = ["name", "title"]
        .iter()
        .find_map(|k| v[*k].as_str())
        .and_then(non_empty);
    let status = v["status"].as_str().and_then(non_empty);
    let phase = v["phase"]
        .as_str()
        .and_then(non_empty)
        .filter(|phase| Some(phase) != status.as_ref());
    let mut items: Vec<String> = Vec::new();
    collect_result_items(&v["deliverables"], 0, &mut items);
    collect_result_items(&v["delivery_evidence"], 0, &mut items);
    let link = v["url"].as_str().and_then(http_link);
    if name.is_none() && status.is_none() && items.is_empty() && link.is_none() {
        return None;
    }
    let mut text = name.unwrap_or_else(|| "The task".to_owned());
    if let Some(status) = status {
        text.push_str(&format!(" is {status}"));
        if let Some(phase) = phase {
            text.push_str(&format!(" ({phase})"));
        }
    } else if let Some(phase) = phase {
        text.push_str(&format!(" is {phase}"));
    }
    text.push('.');
    if !items.is_empty() {
        text.push_str("\nResult:");
        for item in items {
            text.push_str("\n- ");
            text.push_str(&item);
        }
    }
    if let Some(link) = link {
        text.push_str("\nProject: ");
        text.push_str(link);
    }
    Some(truncate_chars(text, SUMMARY_CHARS))
}

/// Collect the human lines of a result subtree. Deliverables are `{title, url}` objects; delivery
/// evidence can be a link, an object or a list of either.
fn collect_result_items(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 2 || out.len() >= SUMMARY_ITEMS {
        return;
    }
    match v {
        Value::Array(items) => {
            for item in items.iter().take(SUMMARY_ITEMS) {
                collect_result_items(item, depth + 1, out);
            }
        }
        Value::Object(map) => {
            if let Some(line) = result_item_line(v) {
                push_unique(out, line);
                return;
            }
            for value in map.values() {
                collect_result_items(value, depth + 1, out);
            }
        }
        _ => {
            if let Some(line) = result_item_line(v) {
                push_unique(out, line);
            }
        }
    }
}

/// One deliverable/evidence entry as a line: `title — url`, a bare link, or a bare title.
fn result_item_line(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => http_link(s).map(str::to_owned).or_else(|| non_empty(s)),
        Value::Object(map) => {
            let title = ["title", "name", "label"]
                .iter()
                .find_map(|k| map.get(*k)?.as_str())
                .and_then(non_empty);
            let link = ["url", "link", "href"]
                .iter()
                .find_map(|k| map.get(*k)?.as_str())
                .and_then(http_link);
            match (title, link) {
                (Some(title), Some(link)) => Some(format!("{title} — {link}")),
                (Some(title), None) => Some(title),
                (None, Some(link)) => Some(link.to_owned()),
                (None, None) => None,
            }
        }
        _ => None,
    }
}

fn http_link(candidate: &str) -> Option<&str> {
    let candidate = candidate.trim();
    let ok = (candidate.starts_with("https://") || candidate.starts_with("http://"))
        && candidate.len() <= 500;
    ok.then_some(candidate)
}

fn push_unique(out: &mut Vec<String>, line: String) {
    if out.len() < SUMMARY_ITEMS && !out.iter().any(|existing| existing == &line) {
        out.push(line);
    }
}

fn truncate_chars(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

async fn parked(bus: &Bus, conversation_id: &str) -> Option<InputPause> {
    let key = crate::turn::chat_key(bus, conversation_id, "");
    let raw: redis::RedisResult<Option<String>> = redis::cmd("HGET")
        .arg(&key)
        .arg(INPUT_FIELD)
        .query_async(&mut bus.conn())
        .await;
    serde_json::from_str(&raw.ok().flatten()?).ok()
}

/// Park `pause` as the conversation's open question.
pub async fn park(bus: &Bus, conversation_id: &str, pause: &InputPause) -> ApiResult<()> {
    let key = crate::turn::chat_key(bus, conversation_id, "");
    let raw = serde_json::to_string(pause).map_err(anyhow::Error::from)?;
    redis::pipe()
        .cmd("HSET")
        .arg(&key)
        .arg(INPUT_FIELD)
        .arg(raw)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(INPUT_TTL_MS / 1000 + 60)
        .ignore()
        .query_async::<()>(&mut bus.conn())
        .await
        .map_err(crate::turn::redis_failed)
}

/// Drop the conversation's open question when it belongs to `task_id` (the task ended).
async fn clear_task_pause(bus: &Bus, conversation_id: &str, task_id: &str) {
    if parked(bus, conversation_id)
        .await
        .is_some_and(|p| p.task.is_some_and(|t| t.task_id == task_id))
    {
        let key = crate::turn::chat_key(bus, conversation_id, "");
        let _: redis::RedisResult<i64> = redis::cmd("HDEL")
            .arg(&key)
            .arg(INPUT_FIELD)
            .query_async(&mut bus.conn())
            .await;
    }
}

/// Store `text` as an agent line of the case, publish it on the chat feed and send it to the channel (an `api`
/// conversation's caller gets it at its callback URL; `kind` is `question`, `result` or `escalated`).
async fn post(
    st: &AppState,
    bus: &Bus,
    scope: &JobScope,
    place: &Place,
    kind: &str,
    text: &str,
) -> ApiResult<()> {
    let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
    let stored = store::append(
        &mut tx,
        store::NewMessage {
            job_id: scope.job_id,
            conversation_id: &place.conversation_id,
            role: "agent",
            author: AUTHOR,
            text,
            case_id: Some(&place.case_id),
            external_id: None,
        },
    )
    .await?;
    tx.commit().await?;
    let message_id = stored.as_ref().map(|s| s.message_id);
    if let Some(s) = stored {
        let data = json!({
            "conversation_id": place.conversation_id, "message_id": s.message_id.to_string(), "role": "agent",
            "author": AUTHOR, "text": text, "at_ms": s.at_ms,
        });
        crate::turn::publish_message(bus, &place.conversation_id, &data).await;
    }
    staff::notify_customer(st, scope, &place.conversation_id, DeliverKind::Text, text).await;
    crate::interaction::webhook::notify_message(
        st,
        scope,
        &place.conversation_id,
        kind,
        message_id,
        text,
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision() -> Value {
        json!({"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"], "enumNames": ["Approve", "Reject"]},
            "reason": {"type": "string", "description": "Why"}}})
    }

    #[test]
    fn required_fields_come_first_then_the_decisions_text_then_optional_ones() {
        let s = decision();
        assert_eq!(
            next_field(&s, &BTreeMap::new()).as_deref(),
            Some("decision")
        );
        // C0: a collected decision still needs its answer text
        let mut c = BTreeMap::new();
        c.insert("decision".to_owned(), "approved".to_owned());
        assert_eq!(next_field(&s, &c).as_deref(), Some("reason"));
        c.insert("reason".to_owned(), "the rug arrived damaged".to_owned());
        assert_eq!(next_field(&s, &c), None);
        // an id is asked like any other field (owner 2026-11-01)
        let ids =
            json!({"required": ["project_id"], "properties": {"project_id": {"type": "string"}}});
        assert_eq!(
            next_field(&ids, &BTreeMap::new()).as_deref(),
            Some("project_id")
        );
        let free = json!({"properties": {"note": {"type": "string"}, "n": {"type": "integer"}}});
        assert_eq!(next_field(&free, &BTreeMap::new()).as_deref(), Some("note"));
        assert_eq!(next_field(&json!({}), &BTreeMap::new()), None);
    }

    /// C0 (owner 2026-11-01): fr's real approval schema — decision, reason, comment. An approve's text is its
    /// `comment`, a reject's is its `reason`; a decision with neither is not sent.
    #[test]
    fn a_decision_carries_the_answer_text_fr_needs() {
        let s = json!({"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]},
            "reason": {"type": "string"},
            "comment": {"type": "string"}}});
        assert_eq!(decision_field(&s).as_deref(), Some("decision"));
        let approved = BTreeMap::from([("decision".to_owned(), "approved".to_owned())]);
        assert!(decision_without_text(&s, &approved));
        assert_eq!(
            decision_text_to_ask(&s, &approved).as_deref(),
            Some("comment")
        );
        let mut with_comment = approved.clone();
        with_comment.insert("comment".to_owned(), "The plan is sound.".to_owned());
        assert!(!decision_without_text(&s, &with_comment));
        assert_eq!(decision_text_to_ask(&s, &with_comment), None);
        // fr reads an approve's `comment` only: a reason alone would still spend the approval with no text
        let mut reason_only = approved.clone();
        reason_only.insert("reason".to_owned(), "looks right".to_owned());
        assert!(decision_without_text(&s, &reason_only));
        assert_eq!(
            decision_text_to_ask(&s, &reason_only).as_deref(),
            Some("comment")
        );
        let rejected = BTreeMap::from([("decision".to_owned(), "rejected".to_owned())]);
        assert_eq!(
            decision_text_to_ask(&s, &rejected).as_deref(),
            Some("reason")
        );
        // a plain enum is not a decision
        let choice =
            json!({"properties": {"size": {"type": "string", "enum": ["small", "large"]}}});
        assert_eq!(decision_field(&choice), None);
        assert!(!decision_without_text(
            &choice,
            &BTreeMap::from([("size".to_owned(), "large".to_owned())])
        ));
        // a decision-only question can never be answered for the requester
        let only = json!({"required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]}}});
        assert!(decision_without_text(&s, &approved));
        assert!(decision_without_text(&only, &approved));
        assert_eq!(decision_text_to_ask(&only, &approved), None);
    }

    #[test]
    fn an_enum_reply_matches_without_case_or_by_its_name() {
        let s = decision();
        assert_eq!(
            accept_field(&s, "decision", " APPROVED ").unwrap(),
            "approved"
        );
        assert_eq!(accept_field(&s, "decision", "reject").unwrap(), "rejected");
        let e = accept_field(&s, "decision", "maybe").unwrap_err();
        assert!(e.contains("Approve / Reject"), "{e}");
        assert_eq!(accept_field(&s, "reason", "  late ").unwrap(), "late");
        assert_eq!(accept_field(&s, "reason", " ").unwrap_err(), "");
    }

    #[test]
    fn the_prompt_carries_the_question_label_and_options() {
        let s = decision();
        assert_eq!(
            field_prompt("Approve the refund?", &s, "decision", true),
            "Approve the refund?\ndecision (Approve / Reject)"
        );
        let one = json!({"properties": {"code": {"type": "string", "format": "password"}}});
        assert_eq!(
            field_prompt("Enter the code we sent you", &one, "code", true),
            "Enter the code we sent you"
        );
        assert!(field_secret(&one, "code"));
        assert!(!field_secret(&s, "reason"));
    }

    #[test]
    fn outcome_text_prefers_the_tools_words() {
        let r = json!({"outcome": "completed", "ok": true, "result": "Card moved to Done."});
        assert_eq!(
            outcome_text("completed", Some(&r)).unwrap(),
            "Card moved to Done."
        );
        let m = json!({"outcome": "completed", "ok": true, "result": {"id": 4, "message": "Refund approved"}});
        assert_eq!(
            outcome_text("completed", Some(&m)).unwrap(),
            "Refund approved"
        );
        let data = json!({"outcome": "completed", "ok": true, "result": {"id": 4}});
        assert_eq!(outcome_text("completed", Some(&data)).unwrap(), DONE_TEXT);
        let err = json!({"outcome": "completed", "ok": false, "error": "boom"});
        assert_eq!(outcome_text("completed", Some(&err)).unwrap(), FAILED_TEXT);
        assert_eq!(outcome_text("failed", None).unwrap(), FAILED_TEXT);
        assert_eq!(outcome_text("lost", None).unwrap(), FAILED_TEXT);
        assert_eq!(outcome_text("cancelled", None).unwrap(), CANCELLED_TEXT);
        assert!(outcome_text("working", None).is_none());
    }

    #[test]
    fn outcome_text_relays_an_fr_projects_closure() {
        let r = json!({
            "outcome": "completed", "ok": true,
            "result": {
                "run_id": "01a1b08b-07f0-7322-bf53-40eb2cdeacd3",
                "status": "succeeded",
                "error": null,
                "result": {
                    "id": "91afede0-f7f7-5633-9522-050c0db00196",
                    "name": "Release blockers",
                    "project_type": "it",
                    "status": "delivered",
                    "phase": "done",
                    "url": "https://fr.example/projects/91afede0",
                    "deliverables": [
                        {"title": "Release blockers tool"},
                        {"title": "Pull request", "url": "https://github.com/o/r/pull/7"}
                    ],
                    "delivery_evidence": {"links": ["https://example.test/evidence"]}
                }
            }
        });
        let text = outcome_text("completed", Some(&r)).unwrap();
        assert!(text.contains("Release blockers"), "{text}");
        assert!(text.contains("delivered"), "{text}");
        assert!(
            text.contains("https://fr.example/projects/91afede0"),
            "{text}"
        );
        assert!(
            text.contains("Pull request — https://github.com/o/r/pull/7"),
            "{text}"
        );
        assert!(text.contains("https://example.test/evidence"), "{text}");
        assert!(!text.contains(DONE_TEXT), "{text}");
    }

    #[test]
    fn outcome_text_does_not_report_the_run_envelopes_status_as_a_result() {
        let r = json!({
            "outcome": "completed", "ok": true,
            "result": {"run_id": "x", "status": "succeeded", "result": {"id": "p"}}
        });
        assert_eq!(outcome_text("completed", Some(&r)).unwrap(), DONE_TEXT);
    }

    /// A conveyor run's `structuredContent` (`{run_id, project_id, status, result, error}`) is its status, then its
    /// result text, then its error — not the inner result alone, and never the fr envelope's rendering.
    #[test]
    fn outcome_text_renders_a_conveyor_runs_result() {
        let r = json!({"outcome": "completed", "ok": true, "result": {
            "run_id": "r-1", "project_id": "c-1", "status": "succeeded", "result": "delivered"}});
        assert_eq!(
            outcome_text("completed", Some(&r)).unwrap(),
            "Project succeeded.\ndelivered"
        );
        let e = json!({"outcome": "completed", "ok": true, "result": {
            "run_id": "r-1", "project_id": "c-1", "status": "failed",
            "result": {"message": "out of stock"}, "error": "the supplier rejected the order"}});
        assert_eq!(
            outcome_text("completed", Some(&e)).unwrap(),
            "Project failed.\nout of stock\nthe supplier rejected the order"
        );
        // a failed task stays the fixed failure line, whatever the run said
        let failed = json!({"outcome": "completed", "ok": false, "error": "boom"});
        assert_eq!(
            outcome_text("completed", Some(&failed)).unwrap(),
            FAILED_TEXT
        );
    }

    #[test]
    fn names_a_newer_project_orders_tasks() {
        let older = Utc::now();
        let newer = older + chrono::Duration::seconds(1);
        assert!(names_a_newer_project(Some(newer), Some(older)));
        assert!(!names_a_newer_project(Some(older), Some(newer)));
        assert!(
            !names_a_newer_project(Some(newer), Some(newer)),
            "a tie keeps the first project written"
        );
        assert!(
            names_a_newer_project(Some(older), None),
            "with no previous task (or its row gone) the task names the project"
        );
        assert!(
            !names_a_newer_project(None, Some(older)),
            "without its own start time a task names nothing"
        );
        assert!(!names_a_newer_project(None, None));
    }
}
