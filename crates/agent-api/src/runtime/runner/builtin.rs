//! The built-in operations (B4 T6, K49: they run inside agent, never through a connector):
//!
//! - `knowledge.search` / `knowledge.answer` → [`knowledge`]: B3 `knowledge::kinds::search(st, tenant, job, stage,
//!   kind, &of, &req)` over the scope's design, per knowledge kind of the step's scope (leaf ids only, PK-7). A
//!   degraded answer (R11: a pinned pack left out) is `error knowledge_degraded`, never a partial answer passed off
//!   as whole. `knowledge.answer` (KH T7, D109) answers `{answer, citations, grounded}`: the job's chat model
//!   (`providers::llm::resolve(.., Use::Qualify)`, ruling R6) with the ONE system message of
//!   `runtime::prompt::assemble(design, channel, passages)` and task `[task answer]`, or the template over the top
//!   passage when the job has no chat model (or it does not answer in time). Each `[n]` the model cites is passage
//!   `n` → its hit → `layer` / `pack` (a platform hit only from a pin of the job's leaf set, `release_pack_leaf` for
//!   a release; a tenant hit cites as `tenant`). No passage, or an answer citing none, is `grounded: false`: the
//!   case escalates (spec 12 A9, the orchestrator's success criterion).
//! - `rule.evaluate{id}` → B2 `release::rules_eval::evaluate(id, &facts, &values, today)` with the design's
//!   `rule_values` of the rule and the facts the case has read (the step's inputs).
use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::ops::{OpOutcome, OpState, failed, outcome};
use crate::connectors::runtime::cap_outcome;
use crate::design::JobRef;
use crate::error::ApiResult;
use crate::knowledge::{citations, kinds};
use crate::providers::llm::{self, ResolvedLlm, Use};
use crate::release::rules_eval;
use crate::runtime::assistant::turn as chat;
use crate::runtime::design::RuntimeDesign;
use crate::runtime::prompt::{self, Passage};
use crate::state::AppState;
use crate::strata::rag::{Hit, Layer, QueryReq};
use crate::usage::LlmUsage;

/// The stage of a kind folded at every stage (D107).
pub const ANY_STAGE: &str = agent_model::kinds::stage_knowledge::ANY_STAGE;
/// The answer operation (D109).
pub const ANSWER_OP: &str = "knowledge.answer";
/// The search operation.
pub const SEARCH_OP: &str = "knowledge.search";
/// The stage of a support turn's knowledge kinds (spec 12 A9).
pub const SUPPORT_STAGE: &str = "support";
/// The stage a step naming one `kind` without a `stage` searches (B4's default).
const DEFAULT_STAGE: &str = "answer";
/// How many passages an answer reads when the step names no `top_k`.
pub const DEFAULT_TOP_K: usize = 5;
/// The longest the synthesis call may take; past it the turn escalates. Sized for the slowest local-llm seen
/// (Nanbeige4.3-33B-FP8, ~7.5 tok/s, owner 2026-11-03: wait time is no concern): [`ANSWER_MAX_TOKENS`] take ~68 s,
/// so 150 s is ~2x headroom. The runner's step 9 timeout ([`crate::runtime::runner::LIVE_TIMEOUT`]) must stay above it.
pub const SYNTHESIS_BUDGET: Duration = Duration::from_secs(150);
const _: () = assert!(
    SYNTHESIS_BUDGET.as_secs() < crate::runtime::runner::LIVE_TIMEOUT.as_secs(),
    "the step 9 timeout must outlast the synthesis call it wraps"
);
/// The synthesis answer's token budget.
const ANSWER_MAX_TOKENS: u64 = 512;
/// The `usage_event.use` metering label of a synthesis call.
pub const USAGE_LABEL: &str = "knowledge_answer";

/// A canonical input as text (`""` for null / missing; numbers and booleans as JSON prints them).
fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

/// What a knowledge step is asked from outside its input: the operation key, the conversation's channel (the D108
/// channel constraints of the answer's prompt) and the customer's question, used when the input names none (a
/// support turn's step reads the inbound message it was qualified from).
#[derive(Clone, Copy, Debug, Default)]
pub struct Asked<'a> {
    pub operation: &'a str,
    pub channel: Option<&'a str>,
    pub question: Option<&'a str>,
}

/// A knowledge step's outcome and the usage of its synthesis call (`None`: no model was called).
#[derive(Debug)]
pub struct KnowledgeRun {
    pub outcome: OpOutcome,
    pub usage: Option<LlmUsage>,
}

impl From<OpOutcome> for KnowledgeRun {
    fn from(outcome: OpOutcome) -> Self {
        KnowledgeRun {
            outcome,
            usage: None,
        }
    }
}

/// The `(stage, kind)` pairs a step searches: its `scope` (`[{stage, kind}]`, stage default [`SUPPORT_STAGE`]: the
/// support-turn route, D109), else its one `kind` at `stage` (default `answer`).
pub fn scopes_of(input: &Map<String, Value>) -> (Vec<(String, String)>, bool) {
    if let Some(list) = input.get("scope").and_then(Value::as_array) {
        let mut out: Vec<(String, String)> = Vec::new();
        for v in list {
            let kind = text(v.get("kind")).trim().to_owned();
            if kind.is_empty() {
                continue;
            }
            let stage = match text(v.get("stage")).trim() {
                "" => SUPPORT_STAGE.to_owned(),
                s => s.to_owned(),
            };
            if !out.contains(&(stage.clone(), kind.clone())) {
                out.push((stage, kind));
            }
        }
        return (out, true);
    }
    let kind = text(input.get("kind"));
    if kind.trim().is_empty() {
        return (Vec::new(), false);
    }
    let stage = match text(input.get("stage")) {
        s if s.trim().is_empty() => DEFAULT_STAGE.to_owned(),
        s => s,
    };
    (vec![(stage, kind)], false)
}

/// The lowest [`score`] a hit may have and still reach the model or the customer. Strata's `/api/query` ignores its
/// own `min_score` for `leaf_ids` queries (it only gates routing, and the leaf-set path never routes), so the floor
/// is applied here, on the hits strata returns. 0.35 is strata's own documented default `min_score`
/// (`strata_rag::query::d_min_score`); scores are normalised to roughly 0..1 (the platform penalty is bounded to
/// [0, 1] on the same scale). A job whose every hit falls below it has "no hits": the answer is not grounded and the
/// case hands over (`ANSWER_GROUNDED`), rather than a weak passage being presented as knowledge.
pub const MIN_HIT_SCORE: f32 = 0.35;

/// Drops every hit scoring below `floor` (a hit with no score at all counts as 0 and goes).
pub fn above_floor(hits: &mut Vec<Hit>, floor: f32) {
    hits.retain(|h| score(h) >= floor);
}

/// A hit's rank across kinds: rerank, else dense, else lexical.
fn score(h: &Hit) -> f32 {
    h.scores
        .rerank
        .or(h.scores.dense)
        .or(h.scores.lexical)
        .unwrap_or(0.0)
}

/// `knowledge.search` / `knowledge.answer` (module docs). Inputs: `query` (or `question`; else
/// [`Asked::question`]), `scope` (`[{stage, kind}]`) or `kind` + `stage`, `top_k`.
pub async fn knowledge(
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    design: &RuntimeDesign,
    asked: &Asked<'_>,
    input: &Map<String, Value>,
) -> KnowledgeRun {
    let mut query = text(input.get("query").or_else(|| input.get("question")));
    if query.trim().is_empty() {
        query = asked.question.unwrap_or_default().to_owned();
    }
    let query = query.trim().to_owned();
    let (scopes, scoped) = scopes_of(input);
    // a support turn whose customer message is missing or unreadable has nothing to answer: not grounded, so the
    // case hands over (fix round 1 ruling), never `invalid_input` → failed
    if scoped && query.is_empty() && asked.operation == ANSWER_OP {
        let mut out = Map::new();
        out.insert("query".into(), Value::String(String::new()));
        out.insert("answer".into(), Value::String(String::new()));
        out.insert("citations".into(), Value::Array(Vec::new()));
        out.insert("grounded".into(), Value::Bool(false));
        out.insert("synthesis".into(), Value::String("no_question".into()));
        return outcome(OpState::Ok, None, None, out).into();
    }
    if query.is_empty() || (scopes.is_empty() && !scoped) {
        return failed(
            "invalid_input",
            "knowledge.* needs a query and a knowledge kind.",
        )
        .into();
    }
    let top_k = input
        .get("top_k")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    let req = QueryReq {
        query: query.clone(),
        top_k,
        ..QueryReq::default()
    };
    let mut hits: Vec<Hit> = Vec::new();
    let mut unsearched: Vec<String> = Vec::new();
    // R14: each platform hit's `pack_layer`, from the scope that found it first (chunk id → layer)
    let mut pack_layers: BTreeMap<Uuid, Option<String>> = BTreeMap::new();
    for (stage, kind) in &scopes {
        let mut searched_stage = stage.as_str();
        let mut found = kinds::search(st, tenant, job, stage, kind, &design.of, &req).await;
        // a kind folded at every stage is stored under `*`, one of a concrete stage under that stage: a scope naming
        // the other finds it there
        if scoped && matches!(&found, Err(e) if e.code() == "kind_not_found") {
            let other = if stage.as_str() == ANY_STAGE {
                SUPPORT_STAGE
            } else {
                ANY_STAGE
            };
            found = kinds::search(st, tenant, job, other, kind, &design.of, &req).await;
            searched_stage = other;
        }
        let answer = match found {
            Ok(a) => a,
            // a support turn's kind the job has no row of (D115 (4): another department's, a tombstoned one) has
            // nothing to search; a step that names its one kind still fails on it
            Err(e) if scoped && e.code() == "kind_not_found" => {
                tracing::warn!(job = %job.key, stage = %stage, kind = %kind, "knowledge_kind_not_found");
                continue;
            }
            Err(e) => return failed(e.code(), "The knowledge search failed.").into(),
        };
        if !answer.degraded.is_empty() {
            return failed(
                "knowledge_degraded",
                format!(
                    "{} knowledge pack(s) could not be searched.",
                    answer.degraded.len()
                ),
            )
            .into();
        }
        // draft only: a pack the cache could not be warmed for is not searched; the turn goes on over the rest
        if !answer.cold.is_empty() {
            tracing::warn!(job = %job.key, stage = %stage, kind = %kind, packs = ?answer.cold,
                           "knowledge_draft_pack_cache_cold; answered without them");
            for p in &answer.cold {
                if !unsearched.contains(p) {
                    unsearched.push(p.clone());
                }
            }
        }
        // D109: a platform hit cites the pin of the job's leaf set it came from; one naming no searched pin is no
        // passage of this job
        let searched = answer.searched;
        for h in answer.resp.results.into_iter().filter(|h| match h.layer {
            Layer::Tenant => true,
            Layer::Platform => h.pack.as_ref().is_some_and(|p| searched.contains(p)),
        }) {
            let layer = h.pack.as_deref().and_then(|pin| {
                citations::pack_layer(&design.knowledge, searched_stage, kind, pin)
            });
            pack_layers
                .entry(h.chunk_id)
                .or_insert_with(|| layer.map(str::to_owned));
            hits.push(h);
        }
    }
    // below the floor is no hit (see MIN_HIT_SCORE)
    above_floor(&mut hits, MIN_HIT_SCORE);
    // one ranking over every kind; a chunk once
    hits.sort_by(|a, b| score(b).total_cmp(&score(a)));
    let mut seen: Vec<Uuid> = Vec::new();
    hits.retain(|h| {
        let new = !seen.contains(&h.chunk_id);
        seen.push(h.chunk_id);
        new
    });
    hits.truncate(top_k.unwrap_or(DEFAULT_TOP_K));

    let mut out = Map::new();
    out.insert("query".into(), Value::String(query.clone()));
    if !unsearched.is_empty() {
        out.insert("unsearched_packs".into(), json!(unsearched));
    }
    let mut usage = None;
    if asked.operation == ANSWER_OP {
        let (answer, cited, how) = if hits.is_empty() {
            (String::new(), Vec::new(), "none")
        } else {
            let passages = citations::passages(&hits);
            let (synthesis, u) =
                synthesize(st, tenant, job, design, asked.channel, &query, &passages).await;
            usage = u;
            match synthesis {
                Synthesis::Answered(c) => {
                    let text = citations::without_markers(&c, hits.len());
                    // the model's refusal is no answer, even when it dressed it with a marker (`NO_ANSWER [1]`)
                    if citations::is_no_answer(&text) {
                        (String::new(), Vec::new(), "model")
                    } else {
                        (text, citations::cited(&c, hits.len()), "model")
                    }
                }
                // D109: only a job with NO chat model answers with the template over the top passage
                Synthesis::NoModel => (citations::template_answer(&hits[0]), vec![1], "template"),
                // a bound model that fails, refuses or runs out of time escalates (not grounded → handover)
                Synthesis::Failed => (String::new(), Vec::new(), "failed"),
            }
        };
        let grounded = !cited.is_empty() && !answer.trim().is_empty();
        let cites: Vec<Value> = if grounded {
            cited
                .iter()
                .map(|n| {
                    let h = &hits[n - 1];
                    citations::answer_citation(h, *n, layer_of(&pack_layers, h))
                })
                .collect()
        } else {
            Vec::new()
        };
        out.insert(
            "answer".into(),
            Value::String(if grounded { answer } else { String::new() }),
        );
        out.insert("citations".into(), Value::Array(cites));
        out.insert("grounded".into(), Value::Bool(grounded));
        out.insert("synthesis".into(), Value::String(how.to_owned()));
    } else {
        let passages: Vec<Value> = hits
            .iter()
            .enumerate()
            .map(|(i, h)| json!({"cite": i + 1, "chunk_id": h.chunk_id, "text": h.text}))
            .collect();
        let cites: Vec<Value> = hits
            .iter()
            .enumerate()
            .map(|(i, h)| citations::answer_citation(h, i + 1, layer_of(&pack_layers, h)))
            .collect();
        out.insert("passages".into(), Value::Array(passages));
        out.insert("citations".into(), Value::Array(cites));
    }
    // the hit text is document data: framed as data, capped like connector output
    let mut o = outcome(OpState::Ok, None, None, cap_outcome(out));
    o.untrusted = true;
    KnowledgeRun { outcome: o, usage }
}

/// The `pack_layer` recorded for `h` (R14), if any.
fn layer_of<'a>(layers: &'a BTreeMap<Uuid, Option<String>>, h: &Hit) -> Option<&'a str> {
    layers.get(&h.chunk_id).and_then(|l| l.as_deref())
}

/// The job's chat model for the answer (ruling R6: the `qualify` binding the runtime resolves for the job, in the
/// scope's design), read in its own short transaction.
async fn resolve_chat(
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    design: &RuntimeDesign,
) -> ApiResult<Option<ResolvedLlm>> {
    let mut tx = crate::conversations::store::begin_job(&st.db, tenant, job.id).await?;
    let r = llm::resolve(&mut tx, st, tenant, job, Use::Qualify, &design.of).await?;
    tx.commit().await?;
    Ok(r)
}

/// The chat-completions body of one answer: the ONE system message (the design's layers for `channel`, the
/// `[task answer]`, the numbered passages) and the customer's question.
pub fn answer_body(
    model: &str,
    design: &RuntimeDesign,
    channel: Option<&str>,
    question: &str,
    passages: &[Passage],
) -> Value {
    let system = prompt::assemble(&design.into(), channel, Some(passages))
        .with_task("answer", citations::ANSWER_TASK);
    json!({
        "model": model,
        "messages": [system.message(), {"role": "user", "content": question}],
        "temperature": 0,
        "max_tokens": ANSWER_MAX_TOKENS,
    })
}

/// What one synthesis attempt gave.
#[derive(Debug, PartialEq, Eq)]
pub enum Synthesis {
    /// The job has no chat model (the use is unbound): the template answers (D109).
    NoModel,
    /// The model's answer text.
    Answered(String),
    /// A chat model is bound but cannot serve (deleted provider, not OpenAI-compatible, refused, unreachable), failed
    /// or ran past [`SYNTHESIS_BUDGET`]: the turn escalates.
    Failed,
}

/// One synthesis call and its usage (`None`: no model was called, or the call ran out of time).
async fn synthesize(
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    design: &RuntimeDesign,
    channel: Option<&str>,
    question: &str,
    passages: &[Passage],
) -> (Synthesis, Option<LlmUsage>) {
    let r = match resolve_chat(st, tenant, job, design).await {
        Ok(Some(r)) => r,
        Ok(None) => return (Synthesis::NoModel, None),
        Err(e) => {
            tracing::warn!(job = %job.key, code = e.code(), "knowledge_answer_model_unresolved");
            return (Synthesis::Failed, None);
        }
    };
    let model = match chat::model(st, tenant, r).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            tracing::warn!(job = %job.key, "knowledge_answer_model_unusable");
            return (Synthesis::Failed, None);
        }
        Err(e) => {
            tracing::warn!(job = %job.key, code = e.code(), "knowledge_answer_model_refused");
            return (Synthesis::Failed, None);
        }
    };
    let mut body = answer_body(&model.model, design, channel, question, passages);
    if model.internal() {
        // thinking off: its reasoning tokens would spend the budget (crate::turn::qualify::for_internal)
        body = crate::turn::qualify::for_internal(body);
    }
    match tokio::time::timeout(SYNTHESIS_BUDGET, chat::chat(&model, &body)).await {
        Ok((Ok(reply), usage)) => (Synthesis::Answered(reply.content), Some(usage)),
        Ok((Err(what), usage)) => {
            tracing::warn!(job = %job.key, what, "knowledge_answer_model_failed");
            (Synthesis::Failed, Some(usage))
        }
        Err(_) => {
            tracing::warn!(job = %job.key, "knowledge_answer_model_timeout");
            (Synthesis::Failed, None)
        }
    }
}

/// The facts a rule reads (`order.delivered_at`, `offering.category`, …) from the step's inputs: a dotted input
/// name is a fact as is; an object input (`order: {...}`) gives `<name>.<field>` facts; the plain names the static
/// procedures send (`delivered_at`, `placed_at`, `category`) map to their object's fact.
pub fn facts_of(input: &Map<String, Value>) -> BTreeMap<String, String> {
    let mut facts = BTreeMap::new();
    let scalar = |v: &Value| match v {
        Value::Object(_) | Value::Array(_) | Value::Null => None,
        other => Some(text(Some(other))),
    };
    for (k, v) in input {
        if let Value::Object(o) = v {
            for (f, x) in o {
                if let Some(t) = scalar(x) {
                    facts.insert(format!("{k}.{f}"), t);
                }
            }
            continue;
        }
        let Some(t) = scalar(v) else { continue };
        let alias = match k.as_str() {
            "delivered_at" | "placed_at" | "purchase_date" => Some("order.delivered_at"),
            "category" => Some("offering.category"),
            _ => None,
        };
        if let Some(a) = alias {
            facts.entry(a.to_owned()).or_insert_with(|| t.clone());
        }
        facts.insert(k.clone(), t);
    }
    facts
}

/// The values of rule `id` in the design (`rule_values` place `{id}`): `(key, value)` per key, sorted. A rule with
/// no values place has one unset value (`evaluate` answers `placeholder`: the agent never decides on a guess).
pub fn rule_values(design: &RuntimeDesign, id: &str) -> Vec<(String, Option<String>)> {
    let snap = design
        .rule_values
        .iter()
        .find(|r| r.r#ref.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|r| r.snap.as_ref())
        .and_then(Value::as_object);
    match snap {
        Some(m) if !m.is_empty() => m
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                };
                (k.clone(), v)
            })
            .collect(),
        _ => vec![("values".to_owned(), None)],
    }
}

/// `rule.evaluate{id}`: `eligible` → ok; `ineligible` → ok with `eligible: false` (a gate step ends the case
/// `not_eligible`); `escalate` / `placeholder` → ok with `eligible: false` and the outcome, so the gate stops the
/// case rather than guessing.
pub fn rule(design: &RuntimeDesign, id: &str, input: &Map<String, Value>) -> OpOutcome {
    if id.trim().is_empty() {
        return failed(
            "invalid_input",
            "rule.evaluate needs a rule id (its variant).",
        );
    }
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let definition = design.rules.iter().find(|r| r.id == id);
    let values = match definition.filter(|r| r.evaluation.is_some()) {
        Some(slot) => {
            let snap = design
                .rule_values
                .iter()
                .find(|r| r.r#ref.get("id").and_then(Value::as_str) == Some(id))
                .and_then(|r| r.snap.clone());
            crate::release::rules::slot_values(slot, &snap)
        }
        None => rule_values(design, id),
    };
    let ev = rules_eval::evaluate_definition(
        id,
        definition.and_then(|r| r.evaluation.as_ref()),
        &facts_of(input),
        &values,
        &today,
    );
    let mut out = Map::new();
    out.insert("rule".into(), Value::String(ev.rule.clone()));
    out.insert("eligible".into(), Value::Bool(ev.outcome == "eligible"));
    out.insert("outcome".into(), Value::String(ev.outcome.to_owned()));
    out.insert("code".into(), Value::String(ev.code.clone()));
    out.insert("reason".into(), Value::String(ev.reason.clone()));
    out.insert(
        "trace".into(),
        Value::Array(ev.trace.iter().cloned().map(Value::String).collect()),
    );
    outcome(OpState::Ok, None, None, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn scored(n: u128, scores: Value) -> Hit {
        serde_json::from_value(json!({
            "chunk_id": Uuid::from_u128(n), "document_id": Uuid::from_u128(100 + n),
            "text": "t", "scores": scores,
        }))
        .unwrap()
    }

    #[test]
    fn the_floor_drops_weak_hits_by_the_score_that_ranks_them() {
        let mut hits = vec![
            scored(1, json!({"dense": 0.9})),
            scored(2, json!({"dense": 0.34})),
            scored(3, json!({"dense": 0.9, "rerank": 0.2})), // the rerank score wins over a strong dense one
            scored(4, json!({"dense": 0.1, "rerank": 0.8})),
            scored(5, json!({"dense": 0.35})), // the floor itself stays
            scored(6, json!({})),              // no score at all counts as 0
        ];
        above_floor(&mut hits, MIN_HIT_SCORE);
        let kept: Vec<u128> = hits.iter().map(|h| h.chunk_id.as_u128()).collect();
        assert_eq!(kept, [1, 4, 5]);
    }

    #[test]
    fn facts_map_plain_names_and_objects() {
        let f = facts_of(&obj(
            json!({"delivered_at": "2026-10-01", "order_id": "RET-01",
                                     "offering": {"category": "rugs"}, "lines": [1]}),
        ));
        assert_eq!(f["order.delivered_at"], "2026-10-01");
        assert_eq!(f["offering.category"], "rugs");
        assert_eq!(f["order_id"], "RET-01");
        assert!(!f.contains_key("lines"));
    }
}
