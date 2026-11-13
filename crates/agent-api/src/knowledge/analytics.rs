//! Knowledge analytics of a job (B3 T11; plan §5.7, §0 R10): a 1:1 port of `web/src/lib/grounding/grounding.ts`
//! `knowledgeAnalyticsOf` ([`analytics_of`], pure) plus the loaders the route feeds it from.
//!
//! - **Answers** are the job's agent replies in `prod` conversations in the window that carry a grounding record
//!   ([`prod_answers`]): an `answer_grounding` row (0122) keyed 1:1 to the reply's B12 `conversation_message`, which
//!   holds the time and text; the question is the customer message before it. The conversation runtime writes the
//!   record with the reply ([`record_grounding`]); before it does, the analytics are zeros, never an error.
//!   **Feedback** is every rating of the job's replies in the window, from every surface ([`feedback_since`]).
//! - **Never cited** reads the documents of the job's bound TENANT leaves only ([`bound_tenant_leaves`]: the tenant
//!   leaf of each kind's [`kinds::leaf_set`] in the LIVE design, which is what production answers searched). Platform
//!   pack documents are not listable by the tenant and not the tenant's to curate (R10).
//! - **Most cited** counts every cited source; a platform source (`layer: "platform"`, `pack: "<key>@<v>"`) is
//!   labelled with the store path `["platform:<key>@<v>"]` (R10), a tenant source keeps its own path.
//! - The window is `[as_of - days, ∞)` (a rating made after `as_of` counts too, as in the reference); times are
//!   rendered as whole-second UTC (`2026-10-18T12:00:00Z`).
use std::cmp::Reverse;
use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::{Serialize, Serializer};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::kinds;
use crate::design::{self, DesignOf, JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::strata::StrataStoreId;

/// The longest window (days).
pub const MAX_DAYS: i64 = 90;
/// Feedback reasons in the reference's order (ties keep it).
pub const REASONS: [&str; 5] = ["wrong", "incomplete", "not_grounded", "tone", "other"];
/// Where an answer was rated.
pub const SURFACES: [&str; 2] = ["test_chat", "case"];
/// Grounding verdicts.
pub const GROUNDINGS: [&str; 4] = ["grounded", "not_grounded", "dont_know", "no_knowledge"];

const TOP_UNANSWERED: usize = 8;
const TOP_CITED: usize = 8;
const TOP_NEVER: usize = 10;

/// Whole-second UTC ISO (`2026-10-18T12:00:00Z`).
pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn ser_iso<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&iso(*t))
}

/// The grounding verdict of one answer (`groundingOf`): no knowledge searched → `no_knowledge`; ≥1 citation →
/// `grounded`; else `dont_know` when the agent said so, `not_grounded` otherwise.
pub fn grounding_of(used_knowledge: bool, cites: bool, dont_know: bool) -> &'static str {
    if !used_knowledge {
        "no_knowledge"
    } else if cites {
        "grounded"
    } else if dont_know {
        "dont_know"
    } else {
        "not_grounded"
    }
}

/// One cited source of an answer (`AnswerSource` + R10 `layer` / `pack`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Source {
    pub document_id: String,
    pub title: String,
    pub store_path: Vec<String>,
    /// `tenant` | `platform` (R10); absent on tenant-only answers.
    pub layer: Option<String>,
    /// `<key>@<version>` of a platform hit.
    pub pack: Option<String>,
    /// Ruling R14: the template layer key the cited pack is referenced on (`industry/retail`); `None` for a tenant
    /// hit and for a pack no template layer references.
    pub pack_layer: Option<String>,
}

impl Source {
    /// One entry of `answer_grounding.sources`; `None` when it names no document.
    pub fn from_json(v: &Value) -> Option<Self> {
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            document_id: text("document_id")?,
            title: text("title").unwrap_or_default(),
            store_path: v
                .get("store_path")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            layer: text("layer"),
            pack: text("pack"),
            pack_layer: text("pack_layer"),
        })
    }

    /// The store path most-cited shows: `["platform:<key>@<v>"]` for a platform source (R10), else its own.
    pub fn cited_path(&self) -> Vec<String> {
        match (self.layer.as_deref(), self.pack.as_deref()) {
            (Some("platform"), Some(pack)) => vec![format!("platform:{pack}")],
            _ => self.store_path.clone(),
        }
    }
}

/// One agent answer as the analytics count it (`AnswerRow`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnswerRow {
    pub at: DateTime<Utc>,
    pub question: String,
    /// The knowledge query the turn ran; `None` when it ran none.
    pub query: Option<String>,
    pub used_knowledge: bool,
    pub citations: Vec<String>,
    pub sources: Vec<Source>,
    pub dont_know: bool,
}

impl AnswerRow {
    pub fn grounding(&self) -> &'static str {
        grounding_of(
            self.used_knowledge,
            !self.citations.is_empty(),
            self.dont_know,
        )
    }
}

/// One rating as the analytics count it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedbackRow {
    pub vote: String,
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

/// A document of a bound tenant leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocRow {
    pub id: String,
    /// The agent store id.
    pub store_id: String,
    pub title: String,
}

/// A store of the tree (for the root-first path of a never-cited document).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreNode {
    pub id: String,
    pub key: String,
    pub parent_id: Option<String>,
}

/// `KnowledgeAnalyticsInput`.
#[derive(Clone, Debug)]
pub struct Input {
    pub job: String,
    pub days: i64,
    pub as_of: DateTime<Utc>,
    pub answers: Vec<AnswerRow>,
    pub feedback: Vec<FeedbackRow>,
    pub documents: Vec<DocRow>,
    pub stores: Vec<StoreNode>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReasonCount {
    pub reason: &'static str,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FeedbackSummary {
    pub up: usize,
    pub down: usize,
    pub reasons: Vec<ReasonCount>,
}

/// `UnansweredQuestion`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Unanswered {
    pub query: String,
    pub example: String,
    pub count: usize,
    #[serde(serialize_with = "ser_iso")]
    pub last_at: DateTime<Utc>,
}

/// `CitedDocument`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CitedDocument {
    pub document_id: String,
    pub title: String,
    pub store_path: Vec<String>,
    /// R14: the template layer key of the most recent citation's pack (`industry/retail`); absent for a tenant
    /// document and a pack no template layer references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack_layer: Option<String>,
    pub citations: usize,
    #[serde(serialize_with = "ser_iso")]
    pub last_cited_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NeverCited {
    pub document_id: String,
    pub title: String,
    pub store_path: Vec<String>,
}

/// `KnowledgeAnalytics` (web `types/grounding.ts`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KnowledgeAnalytics {
    pub job: String,
    pub days: i64,
    #[serde(serialize_with = "ser_iso")]
    pub since: DateTime<Utc>,
    #[serde(serialize_with = "ser_iso")]
    pub as_of: DateTime<Utc>,
    pub answers: usize,
    pub knowledge_answers: usize,
    pub grounded: usize,
    pub not_grounded: usize,
    pub dont_know: usize,
    pub grounded_rate: Option<f64>,
    pub uncited_rate: Option<f64>,
    pub dont_know_rate: Option<f64>,
    pub feedback: FeedbackSummary,
    pub unanswered: Vec<Unanswered>,
    pub most_cited: Vec<CitedDocument>,
    pub never_cited: Vec<NeverCited>,
    pub never_cited_total: usize,
    pub documents_total: usize,
}

fn rate(n: usize, of: usize) -> Option<f64> {
    (of > 0).then(|| n as f64 / of as f64)
}

/// The start of a `days` window ending at `as_of`.
pub fn since_of(as_of: DateTime<Utc>, days: i64) -> DateTime<Utc> {
    as_of - TimeDelta::days(days)
}

/// Root-first keys of `store` (an unknown store has no path; a cycle stops at the tree's size).
fn path_of(by_id: &HashMap<&str, &StoreNode>, store: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = by_id.get(store).copied();
    while let Some(s) = at {
        if out.len() > by_id.len() {
            break;
        }
        out.push(s.key.clone());
        at = s.parent_id.as_deref().and_then(|p| by_id.get(p).copied());
    }
    out.reverse();
    out
}

/// The knowledge analytics of one job over `days` up to `as_of` (`knowledgeAnalyticsOf`).
pub fn analytics_of(i: &Input) -> KnowledgeAnalytics {
    let since = since_of(i.as_of, i.days);
    let answers: Vec<&AnswerRow> = i.answers.iter().filter(|a| a.at >= since).collect();
    let knowledge: Vec<&AnswerRow> = answers
        .iter()
        .copied()
        .filter(|a| a.used_knowledge)
        .collect();
    let verdicts: Vec<(&AnswerRow, &'static str)> =
        knowledge.iter().map(|a| (*a, a.grounding())).collect();
    let count = |g: &str| verdicts.iter().filter(|(_, v)| *v == g).count();
    let grounded = count("grounded");
    let not_grounded = count("not_grounded");
    let dont_know = count("dont_know");

    // Unanswered: grouped by the knowledge query (what to search the stores for), most asked first.
    let mut gaps: Vec<Unanswered> = Vec::new();
    let mut gap_at: HashMap<String, usize> = HashMap::new();
    for (a, g) in &verdicts {
        if *g == "grounded" {
            continue;
        }
        let query = a.query.as_deref().unwrap_or(&a.question);
        let key = query.trim().to_lowercase();
        match gap_at.get(&key).copied() {
            Some(ix) => {
                let cur = &mut gaps[ix];
                cur.count += 1;
                if a.at > cur.last_at {
                    cur.last_at = a.at;
                }
            }
            None => {
                gap_at.insert(key, gaps.len());
                gaps.push(Unanswered {
                    query: query.to_string(),
                    example: a.question.clone(),
                    count: 1,
                    last_at: a.at,
                });
            }
        }
    }
    gaps.sort_by(|x, y| {
        y.count
            .cmp(&x.count)
            .then_with(|| y.last_at.cmp(&x.last_at))
    });

    // Citations per document (an answer citing two chunks of one document counts once for it).
    let mut cited: Vec<CitedDocument> = Vec::new();
    let mut cited_at: HashMap<String, usize> = HashMap::new();
    for a in &knowledge {
        let mut docs: Vec<&Source> = Vec::new();
        for s in &a.sources {
            match docs.iter().position(|d| d.document_id == s.document_id) {
                Some(p) => docs[p] = s,
                None => docs.push(s),
            }
        }
        for s in docs {
            match cited_at.get(&s.document_id).copied() {
                Some(ix) => {
                    let cur = &mut cited[ix];
                    cur.citations += 1;
                    // the most recent citation naming a pack layer names it
                    if s.pack_layer.is_some()
                        && (cur.pack_layer.is_none() || a.at >= cur.last_cited_at)
                    {
                        cur.pack_layer = s.pack_layer.clone();
                    }
                    if a.at > cur.last_cited_at {
                        cur.last_cited_at = a.at;
                    }
                }
                None => {
                    cited_at.insert(s.document_id.clone(), cited.len());
                    cited.push(CitedDocument {
                        document_id: s.document_id.clone(),
                        title: s.title.clone(),
                        store_path: s.cited_path(),
                        pack_layer: s.pack_layer.clone(),
                        citations: 1,
                        last_cited_at: a.at,
                    });
                }
            }
        }
    }
    let mut most_cited = cited;
    most_cited.sort_by(|x, y| {
        y.citations
            .cmp(&x.citations)
            .then_with(|| x.title.cmp(&y.title))
    });

    // Never cited: documents of the stores, smaller (curated) stores first — 200 crawled articles last.
    let by_id: HashMap<&str, &StoreNode> = i.stores.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut size: HashMap<&str, usize> = HashMap::new();
    for d in &i.documents {
        *size.entry(d.store_id.as_str()).or_default() += 1;
    }
    let mut never: Vec<(usize, &DocRow)> = i
        .documents
        .iter()
        .filter(|d| !cited_at.contains_key(&d.id))
        .map(|d| (size.get(d.store_id.as_str()).copied().unwrap_or(0), d))
        .collect();
    never.sort_by(|(sx, x), (sy, y)| sx.cmp(sy).then_with(|| x.title.cmp(&y.title)));

    let fb: Vec<&FeedbackRow> = i.feedback.iter().filter(|f| f.at >= since).collect();
    let mut reasons: Vec<ReasonCount> = REASONS
        .iter()
        .map(|&r| ReasonCount {
            reason: r,
            count: fb
                .iter()
                .filter(|f| f.vote == "down" && f.reason.as_deref() == Some(r))
                .count(),
        })
        .filter(|r| r.count > 0)
        .collect();
    reasons.sort_by_key(|r| Reverse(r.count));

    let never_cited_total = never.len();
    KnowledgeAnalytics {
        job: i.job.clone(),
        days: i.days,
        since,
        as_of: i.as_of,
        answers: answers.len(),
        knowledge_answers: knowledge.len(),
        grounded,
        not_grounded,
        dont_know,
        grounded_rate: rate(grounded, knowledge.len()),
        uncited_rate: rate(not_grounded, knowledge.len()),
        dont_know_rate: rate(dont_know, knowledge.len()),
        feedback: FeedbackSummary {
            up: fb.iter().filter(|f| f.vote == "up").count(),
            down: fb.iter().filter(|f| f.vote == "down").count(),
            reasons,
        },
        unanswered: gaps.into_iter().take(TOP_UNANSWERED).collect(),
        most_cited: most_cited.into_iter().take(TOP_CITED).collect(),
        never_cited: never
            .into_iter()
            .take(TOP_NEVER)
            .map(|(_, d)| NeverCited {
                document_id: d.id.clone(),
                title: d.title.clone(),
                store_path: path_of(&by_id, &d.store_id),
            })
            .collect(),
        never_cited_total,
        documents_total: i.documents.len(),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Loaders

/// What the conversation runtime knows about how one agent reply used knowledge (B4 fills it from the turn's
/// knowledge search; a reply that searched nothing is [`ReplyGrounding::default`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplyGrounding {
    /// The knowledge query the turn ran; `None` when it ran none.
    pub knowledge_query: Option<String>,
    pub used_knowledge: bool,
    /// The chunk ids the reply cites.
    pub citations: Vec<String>,
    /// The cited sources (`AnswerSource` JSON objects, R10 `layer` / `pack` on platform hits).
    pub sources: Vec<Value>,
    pub dont_know: bool,
}

/// Store the grounding of agent reply `message_id` of job `job` (the conversation runtime's hook, B4). Call it in
/// the transaction that stored the reply (`conversations::store::append` with role `agent`, which returns the
/// message id), right after the append: a reply and its grounding commit together. The verdict is derived here
/// ([`grounding_of`]) and pinned by the table's CHECK. 23514 `answer_not_agent_reply` when the message is not an
/// agent reply of this tenant's job; a second record for one reply is 23505.
pub async fn record_grounding(
    c: &mut PgConnection,
    job: Uuid,
    message_id: Uuid,
    g: &ReplyGrounding,
) -> ApiResult<()> {
    let verdict = grounding_of(g.used_knowledge, !g.citations.is_empty(), g.dont_know);
    sqlx::query(
        "INSERT INTO answer_grounding (tenant_id, job_id, message_id, knowledge_query, used_knowledge, citations, \
                                       sources, dont_know, grounding) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(job)
    .bind(message_id)
    .bind(g.knowledge_query.as_deref())
    .bind(g.used_knowledge)
    .bind(g.citations.as_slice())
    .bind(Value::Array(g.sources.clone()))
    .bind(g.dont_know)
    .bind(verdict)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The question an agent reply `m` answered: the text of the last customer message of its conversation before it
/// (lateral `q`; no such message → no row, read as `''`). Columns/identifiers only, for `AssertSqlSafe`.
pub const QUESTION_JOIN: &str = "LEFT JOIN LATERAL (SELECT p.text FROM conversation_message p \
     WHERE p.tenant_id = m.tenant_id AND p.job_id = m.job_id AND p.conversation_id = m.conversation_id \
       AND p.role = 'customer' AND (p.at_ms, p.message_id) < (m.at_ms, m.message_id) \
     ORDER BY p.at_ms DESC, p.message_id DESC LIMIT 1) q ON true";

#[derive(sqlx::FromRow)]
struct TurnRow {
    at: DateTime<Utc>,
    question: String,
    knowledge_query: Option<String>,
    used_knowledge: bool,
    citations: Vec<String>,
    sources: Value,
    dont_know: bool,
}

/// The job's grounded agent replies in `prod` conversations since `since`, oldest first (none before the runtime
/// records any).
pub async fn prod_answers(
    c: &mut PgConnection,
    job: Uuid,
    since: DateTime<Utc>,
) -> ApiResult<Vec<AnswerRow>> {
    let rows: Vec<TurnRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT to_timestamp(m.at_ms::float8 / 1000) AS at, coalesce(q.text, '') AS question, g.knowledge_query, \
                g.used_knowledge, g.citations, g.sources, g.dont_know \
           FROM answer_grounding g \
           JOIN conversation_message m \
             ON m.tenant_id = g.tenant_id AND m.job_id = g.job_id AND m.message_id = g.message_id \
           JOIN conversation c \
             ON c.tenant_id = m.tenant_id AND c.job_id = m.job_id AND c.conversation_id = m.conversation_id \
           {QUESTION_JOIN} \
          WHERE g.job_id = $1 AND c.env = 'prod' AND m.at_ms >= $2 \
          ORDER BY m.at_ms, m.message_id"
    )))
    .bind(job)
    .bind(since.timestamp_millis())
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AnswerRow {
            at: r.at,
            question: r.question,
            query: r.knowledge_query,
            used_knowledge: r.used_knowledge,
            citations: r.citations,
            sources: r
                .sources
                .as_array()
                .map(|a| a.iter().filter_map(Source::from_json).collect())
                .unwrap_or_default(),
            dont_know: r.dont_know,
        })
        .collect())
}

/// Every rating of the job's answers since `since` (all surfaces).
pub async fn feedback_since(
    c: &mut PgConnection,
    job: Uuid,
    since: DateTime<Utc>,
) -> ApiResult<Vec<FeedbackRow>> {
    let rows: Vec<(String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT vote, reason, at FROM answer_feedback WHERE job_id = $1 AND at >= $2 ORDER BY at, id",
    )
    .bind(job)
    .bind(since)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(vote, reason, at)| FeedbackRow { vote, reason, at })
        .collect())
}

/// A tenant leaf a job's kind is bound to: the agent store and its strata store in the job's pinned generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundLeaf {
    /// The agent store (what never-cited documents are labelled by: its path of keys, never a generation's id).
    pub store: Uuid,
    /// Its strata store in the job's pinned generation of design `of` (what is listed).
    pub strata: StrataStoreId,
}

/// The tenant leaves the job's kinds are bound to in design `of` (each kind's [`kinds::leaf_set`] `tenant_store` and
/// `tenant`; packs are left out, R10), deduplicated in kind order.
pub async fn bound_tenant_leaves(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<BoundLeaf>> {
    let places = design::values_of_kind(&mut *c, reg, job, of, kinds::KIND).await?;
    let mut out: Vec<BoundLeaf> = Vec::new();
    for p in places {
        let part = |k: &str| {
            p.r#ref
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (stage, kind) = (part("stage"), part("kind"));
        let set = match kinds::leaf_set(&mut *c, reg, job, &stage, &kind, of).await {
            Ok(s) => s,
            Err(ApiError::Missing("kind_not_found", _)) => continue,
            Err(e) => return Err(e),
        };
        if let (Some(store), Some(strata)) = (set.tenant_store, set.tenant) {
            let leaf = BoundLeaf { store, strata };
            if !out.contains(&leaf) {
                out.push(leaf);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_follow_grounding_of() {
        assert_eq!(grounding_of(true, true, false), "grounded");
        assert_eq!(grounding_of(true, false, false), "not_grounded");
        assert_eq!(grounding_of(true, false, true), "dont_know");
        // a citation wins over a stray dont_know; no knowledge searched → nothing to judge
        assert_eq!(grounding_of(true, true, true), "grounded");
        assert_eq!(grounding_of(false, true, true), "no_knowledge");
    }

    #[test]
    fn platform_sources_are_labelled_by_pack() {
        let v = serde_json::json!({"document_id": "d1", "title": "T", "store_path": ["base"],
                                    "layer": "platform", "pack": "base@2"});
        let s = Source::from_json(&v).unwrap();
        assert_eq!(s.cited_path(), ["platform:base@2"]);
        let t = serde_json::json!({"document_id": "d2", "title": "T", "store_path": ["faq", "leaf"],
                                    "layer": "tenant"});
        assert_eq!(Source::from_json(&t).unwrap().cited_path(), ["faq", "leaf"]);
        assert!(Source::from_json(&serde_json::json!({"title": "no id"})).is_none());
    }

    #[test]
    fn iso_is_whole_seconds() {
        let t = DateTime::parse_from_rfc3339("2026-10-18T12:00:00.123456Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(iso(t), "2026-10-18T12:00:00Z");
    }
}
