//! Grading one validation question (B8 T12, from P8 T12; B8-2, B8-5, R13):
//!
//! 1. **Answer** through the release's knowledge ([`Answerer`]; live: [`KnowledgeAnswerer`] = B3
//!    `knowledge::kinds::search` with `DesignOf::Release(id)` over the kinds of the question's activity stage, every
//!    kind when the question names no activity or its stage has none) → `response` + `cited`.
//! 2. **Citation check first**: no citation → `rejected / missing_citation`; citations but none of the reference's
//!    document → `rejected / wrong_citation`. The judge is not asked then.
//! 3. **Judge** ([`Judge`]; live: [`LayaJudge`] on the job's `gates` binding, one choice question "same answer?"):
//!    `yes` → `pass`, `no` → `rejected / incorrect`; `partly`, a confidence below [`MIN_CONFIDENCE`], an unbound
//!    `gates` use or any Laya error → `pending` (needs a person), never `pass`.
//!
//! [`grade`] runs the questions with a concurrency, a per-question timeout and an optional total budget: a question
//! not started inside the budget is [`Outcome::NotReached`] (its row stays as it was; a re-run fills it).
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::design::{self, DesignOf, JobRef};
use crate::error::ApiResult;
use crate::knowledge::kinds;
use crate::netguard;
use crate::providers::llm::ResolvedLlm;
use crate::state::AppState;
use crate::strata::rag::QueryReq;
use crate::usage::LlmUsage;

/// A cut run's concurrency (B8-5).
pub const CONCURRENCY: usize = 8;
/// One question's budget: answer + judge (B8-5).
pub const PER_QUESTION: Duration = Duration::from_secs(20);
/// A request's total budget (B8-5: Cloudflare cuts at ~100 s). The approve-triggered grid has none (no request
/// waits for it, T12a).
pub const REQUEST_BUDGET: Duration = Duration::from_secs(85);
/// Below this the judge's answer is not trusted: `pending`.
pub const MIN_CONFIDENCE: f64 = 0.6;
/// Hits an answer is built from.
const TOP_HITS: usize = 3;
/// A stored response's length cap (0172 CHECK is 8000).
const MAX_RESPONSE: usize = 4000;

/// One question as graded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub id: Uuid,
    pub question: String,
    pub expected: String,
    pub reference: String,
    /// `stage:activity`, or empty.
    pub activity: String,
}

/// What the release's knowledge answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub response: String,
    pub cited: Vec<String>,
}

/// What the judge said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Says {
    Same,
    Partly,
    Different,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Judged {
    pub says: Says,
    pub confidence: f64,
    /// The call's usage (metered as `validation` / `judge`).
    pub usage: Option<LlmUsage>,
}

/// Why a judge gave no answer (the verdict stays `pending`); the usage of a call that was made.
#[derive(Clone, Debug, PartialEq)]
pub struct JudgeFailed {
    pub code: &'static str,
    pub usage: Option<LlmUsage>,
}

pub trait Answerer: Send + Sync {
    fn answer<'a>(&'a self, q: &'a Question) -> BoxFuture<'a, Result<Answer, &'static str>>;
}

pub trait Judge: Send + Sync {
    fn judge<'a>(
        &'a self,
        q: &'a Question,
        response: &'a str,
    ) -> BoxFuture<'a, Result<Judged, JudgeFailed>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Rejected,
    Pending,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Rejected => "rejected",
            Self::Pending => "pending",
        }
    }
}

/// A graded question.
#[derive(Clone, Debug, PartialEq)]
pub struct Graded {
    pub response: String,
    pub cited: Vec<String>,
    pub verdict: Verdict,
    /// `missing_citation` | `wrong_citation` | `incorrect` (rejected only).
    pub reason: Option<&'static str>,
    pub usage: Option<LlmUsage>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Graded(Graded),
    /// The answer failed (code) or the question timed out: nothing stored for it.
    Failed(&'static str),
    /// Not started inside the total budget.
    NotReached,
}

/// How a run is bounded.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub concurrency: usize,
    pub per_question: Duration,
    pub budget: Option<Duration>,
}

impl Limits {
    /// A request (`runValidationCut`): B8-5.
    pub fn request() -> Self {
        Self {
            concurrency: CONCURRENCY,
            per_question: PER_QUESTION,
            budget: Some(REQUEST_BUDGET),
        }
    }

    /// The approve-triggered grid: no total budget.
    pub fn grid() -> Self {
        Self {
            concurrency: CONCURRENCY,
            per_question: PER_QUESTION,
            budget: None,
        }
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The document part of a reference: `policy.returns §2` → `policy.returns`.
pub fn reference_doc(reference: &str) -> String {
    norm(reference.split('§').next().unwrap_or(reference))
}

/// The citation check: `None` = the reference's document is cited.
pub fn citation_problem(reference: &str, cited: &[String]) -> Option<&'static str> {
    if cited.iter().all(|c| c.trim().is_empty()) {
        return Some("missing_citation");
    }
    let doc = reference_doc(reference);
    let hit = cited
        .iter()
        .map(|c| norm(c))
        .any(|c| !c.is_empty() && !doc.is_empty() && (c.contains(&doc) || doc.contains(&c)));
    if hit { None } else { Some("wrong_citation") }
}

/// Grade one question: answer, citation check, then the judge.
pub async fn grade_one(q: &Question, a: &dyn Answerer, j: &dyn Judge) -> Outcome {
    let answer = match a.answer(q).await {
        Ok(x) => x,
        Err(code) => return Outcome::Failed(code),
    };
    if let Some(reason) = citation_problem(&q.reference, &answer.cited) {
        return Outcome::Graded(Graded {
            response: answer.response,
            cited: answer.cited,
            verdict: Verdict::Rejected,
            reason: Some(reason),
            usage: None,
        });
    }
    let (verdict, reason, usage) = match j.judge(q, &answer.response).await {
        Ok(Judged {
            says,
            confidence,
            usage,
        }) => match says {
            _ if confidence < MIN_CONFIDENCE => (Verdict::Pending, None, usage),
            Says::Same => (Verdict::Pass, None, usage),
            Says::Different => (Verdict::Rejected, Some("incorrect"), usage),
            Says::Partly => (Verdict::Pending, None, usage),
        },
        Err(f) => (Verdict::Pending, None, f.usage),
    };
    Outcome::Graded(Graded {
        response: answer.response,
        cited: answer.cited,
        verdict,
        reason,
        usage,
    })
}

/// Grade `questions` within `limits`; one outcome per question, in input order. Each question runs on its own task
/// owning clones of the answerer, the judge and the question (a semaphore bounds the concurrency): no future here
/// borrows across a task boundary, so the caller's future stays `Send` (a spawned grid run, an axum handler).
pub async fn grade(
    questions: &[Question],
    a: Arc<dyn Answerer>,
    j: Arc<dyn Judge>,
    limits: Limits,
) -> Vec<Outcome> {
    let deadline = limits.budget.map(|b| tokio::time::Instant::now() + b);
    let sem = Arc::new(Semaphore::new(limits.concurrency.max(1)));
    let mut out: Vec<Outcome> = vec![Outcome::NotReached; questions.len()];
    let mut set: JoinSet<(usize, Outcome)> = JoinSet::new();
    for (i, q) in questions.iter().enumerate() {
        let permit = match deadline {
            None => sem.clone().acquire_owned().await.ok(),
            Some(d) => tokio::time::timeout_at(d, sem.clone().acquire_owned())
                .await
                .ok()
                .and_then(Result::ok),
        };
        let Some(permit) = permit else {
            break;
        };
        let left = match deadline {
            None => limits.per_question,
            Some(d) => {
                let now = tokio::time::Instant::now();
                if now >= d {
                    break;
                }
                limits.per_question.min(d - now)
            }
        };
        // what a task that panics leaves (its result never arrives)
        out[i] = Outcome::Failed("panic");
        let (a, j, q) = (a.clone(), j.clone(), q.clone());
        set.spawn(async move {
            let _permit = permit;
            let o = match tokio::time::timeout(left, grade_one(&q, a.as_ref(), j.as_ref())).await {
                Ok(o) => o,
                Err(_) => Outcome::Failed("timeout"),
            };
            (i, o)
        });
    }
    while let Some(done) = set.join_next().await {
        if let Ok((i, o)) = done {
            out[i] = o;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Live answerer and judge

/// R13: answers through the release's knowledge kinds (B3 `kinds::search`, `DesignOf::Release`).
pub struct KnowledgeAnswerer {
    pub st: AppState,
    pub tenant: Uuid,
    pub job: JobRef,
    pub release_id: String,
    /// `(stage, kind)` of every knowledge kind of the release.
    pub kinds: Vec<(String, String)>,
}

impl KnowledgeAnswerer {
    /// The release's knowledge kinds, read in `c` (scoped to the job's tenant).
    pub async fn kinds_of(
        c: &mut sqlx::PgConnection,
        st: &AppState,
        job: &JobRef,
        release_id: &str,
    ) -> ApiResult<Vec<(String, String)>> {
        let rows = design::values_of_kind(
            &mut *c,
            &st.design,
            job,
            &DesignOf::Release(release_id.to_string()),
            kinds::KIND,
        )
        .await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.snap.is_some())
            .filter_map(|r| {
                let stage = r.r#ref.get("stage").and_then(Value::as_str)?.to_string();
                let kind = r.r#ref.get("kind").and_then(Value::as_str)?.to_string();
                Some((stage, kind))
            })
            .collect())
    }

    /// The kinds a question is answered from: its activity's stage (and `*`), else all.
    pub fn kinds_for(&self, q: &Question) -> Vec<(String, String)> {
        let stage = q.activity.split(':').next().unwrap_or("").trim();
        if !stage.is_empty() {
            let narrowed: Vec<(String, String)> = self
                .kinds
                .iter()
                .filter(|(s, _)| s == stage || s == "*")
                .cloned()
                .collect();
            if !narrowed.is_empty() {
                return narrowed;
            }
        }
        self.kinds.clone()
    }
}

impl Answerer for KnowledgeAnswerer {
    fn answer<'a>(&'a self, q: &'a Question) -> BoxFuture<'a, Result<Answer, &'static str>> {
        Box::pin(async move {
            let kinds = self.kinds_for(q);
            if kinds.is_empty() {
                return Err("no_knowledge");
            }
            let of = DesignOf::Release(self.release_id.clone());
            let req = QueryReq {
                query: q.question.clone(),
                top_k: Some(TOP_HITS),
                ..QueryReq::default()
            };
            let mut texts = Vec::new();
            let mut cited: Vec<String> = Vec::new();
            let mut failed = None;
            for (stage, kind) in &kinds {
                match kinds::search(&self.st, self.tenant, &self.job, stage, kind, &of, &req).await
                {
                    Ok(a) => {
                        for h in a.resp.results.iter().take(TOP_HITS) {
                            texts.push(h.text.trim().to_string());
                            let c = h.title.clone().unwrap_or_else(|| h.source.clone());
                            if !c.trim().is_empty() && !cited.contains(&c) {
                                cited.push(c);
                            }
                        }
                    }
                    Err(e) => failed = Some(e.code()),
                }
            }
            if texts.is_empty()
                && let Some(code) = failed
            {
                return Err(code);
            }
            let mut response: String = texts
                .into_iter()
                .take(TOP_HITS)
                .collect::<Vec<_>>()
                .join("\n\n");
            if response.chars().count() > MAX_RESPONSE {
                response = response.chars().take(MAX_RESPONSE).collect();
            }
            Ok(Answer { response, cited })
        })
    }
}

/// The judge's one Laya question (B8-2: "same answer?", yes / partly / no).
pub const JUDGE_KEY: &str = "same_answer";

/// The `POST /v1/systemone` body of one judgement.
pub fn judge_request(model: &str, q: &Question, response: &str) -> Value {
    json!({
        "model": model,
        "state": {"question": q.question, "expected": q.expected, "response": response},
        "questions": {
            JUDGE_KEY: {
                "type": "choice",
                "instructions": "Does the response give the same answer as the expected answer to the question?",
                "criteria": {
                    "yes": "The response gives the expected answer.",
                    "partly": "The response gives part of the expected answer, or adds something that contradicts it.",
                    "no": "The response does not give the expected answer.",
                },
            },
        },
    })
}

/// Decode a judge reply: `(says, confidence, input_tokens, output_tokens, model)`.
pub fn decode_judge(v: &Value) -> Option<(Says, f64, u64, u64, Option<String>)> {
    let a = v.get("answers")?.get(JUDGE_KEY)?;
    let says = match a.get("choice")?.as_str()? {
        "yes" => Says::Same,
        "partly" => Says::Partly,
        "no" => Says::Different,
        _ => return None,
    };
    let confidence = a
        .get("confidence")
        .and_then(Value::as_f64)
        .filter(|c| c.is_finite() && (0.0..=1.0).contains(c))?;
    let tok = |k: &str| {
        v.get("usage")
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Some((
        says,
        confidence,
        tok("input_tokens"),
        tok("output_tokens"),
        v.get("model").and_then(Value::as_str).map(str::to_string),
    ))
}

/// The Laya judge on a resolved `gates` provider (B5's network rules: the internal provider on a plain client
/// without redirects, a tenant's own only through netguard). No URL, key or body is ever logged or returned.
pub struct LayaJudge {
    http: reqwest::Client,
    url: String,
    model: String,
    key: Option<String>,
    provider_id: String,
    funding: &'static str,
}

impl LayaJudge {
    /// `None` when the client cannot be built (the judge is then unavailable: `pending`).
    pub async fn for_resolved(
        r: &ResolvedLlm,
        policy: &netguard::Policy,
        tenant: Uuid,
    ) -> Option<Self> {
        let url = format!(
            "{}{}",
            r.base_url.trim_end_matches('/'),
            crate::job_design::gates::laya::SYSTEMONE_PATH
        );
        let http = if r.provider_id.is_none() {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(PER_QUESTION)
                .build()
                .ok()?
        } else {
            let checked = netguard::check_url(&url, policy, tenant).ok()?;
            netguard::pinned_client(&checked, policy, tenant, PER_QUESTION)
                .await
                .ok()?
        };
        Some(Self {
            http,
            url,
            model: r.model.clone(),
            key: r.key.as_ref().map(|k| k.expose().to_owned()),
            provider_id: r.provider_id.map_or_else(
                || crate::turn::qualify::INTERNAL_PROVIDER.to_owned(),
                |p| p.to_string(),
            ),
            funding: r.funding,
        })
    }

    fn usage(&self, model: Option<String>, tin: u64, tout: u64, ms: u128) -> LlmUsage {
        LlmUsage {
            provider_id: self.provider_id.clone(),
            model: model.unwrap_or_else(|| self.model.clone()),
            funding: self.funding.to_owned(),
            tokens_in: i64::try_from(tin).unwrap_or(i64::MAX),
            tokens_out: i64::try_from(tout).unwrap_or(i64::MAX),
            latency_ms: i64::try_from(ms).unwrap_or(i64::MAX),
        }
    }
}

impl Judge for LayaJudge {
    fn judge<'a>(
        &'a self,
        q: &'a Question,
        response: &'a str,
    ) -> BoxFuture<'a, Result<Judged, JudgeFailed>> {
        Box::pin(async move {
            let started = Instant::now();
            let mut req = self
                .http
                .post(&self.url)
                .timeout(PER_QUESTION)
                .json(&judge_request(&self.model, q, response));
            if let Some(k) = &self.key {
                req = req.bearer_auth(k);
            }
            let fail = |code: &'static str| JudgeFailed { code, usage: None };
            let resp = req.send().await.map_err(|e| {
                fail(if e.is_timeout() {
                    "timeout"
                } else {
                    "unreachable"
                })
            })?;
            if !resp.status().is_success() {
                return Err(fail("status"));
            }
            let v: Value = resp.json().await.map_err(|_| fail("bad_body"))?;
            let ms = started.elapsed().as_millis();
            match decode_judge(&v) {
                Some((says, confidence, tin, tout, model)) => Ok(Judged {
                    says,
                    confidence,
                    usage: Some(self.usage(model, tin, tout, ms)),
                }),
                None => Err(JudgeFailed {
                    code: "bad_body",
                    usage: Some(self.usage(None, 0, 0, ms)),
                }),
            }
        })
    }
}

/// No `gates` binding (or one that cannot serve): every judgement is `pending` (B8-2).
pub struct NoJudge;

impl Judge for NoJudge {
    fn judge<'a>(
        &'a self,
        _q: &'a Question,
        _response: &'a str,
    ) -> BoxFuture<'a, Result<Judged, JudgeFailed>> {
        Box::pin(async {
            Err(JudgeFailed {
                code: "gates_provider_missing",
                usage: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn q(n: u128) -> Question {
        Question {
            id: Uuid::from_u128(n),
            question: "How long do I have to return?".into(),
            expected: "30 days".into(),
            reference: "policy.returns §2".into(),
            activity: "after_sales:return_item".into(),
        }
    }

    struct FixedAnswer(Vec<String>, Duration);
    impl Answerer for FixedAnswer {
        fn answer<'a>(&'a self, _q: &'a Question) -> BoxFuture<'a, Result<Answer, &'static str>> {
            Box::pin(async move {
                tokio::time::sleep(self.1).await;
                Ok(Answer {
                    response: "You have 30 days.".into(),
                    cited: self.0.clone(),
                })
            })
        }
    }

    struct Counting(AtomicUsize, Result<(Says, f64), &'static str>);
    impl Judge for Counting {
        fn judge<'a>(
            &'a self,
            _q: &'a Question,
            _r: &'a str,
        ) -> BoxFuture<'a, Result<Judged, JudgeFailed>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let r = self.1;
            Box::pin(async move {
                match r {
                    Ok((says, confidence)) => Ok(Judged {
                        says,
                        confidence,
                        usage: None,
                    }),
                    Err(code) => Err(JudgeFailed { code, usage: None }),
                }
            })
        }
    }

    fn verdict(o: &Outcome) -> (Verdict, Option<&'static str>) {
        match o {
            Outcome::Graded(g) => (g.verdict, g.reason),
            other => panic!("not graded: {other:?}"),
        }
    }

    #[tokio::test]
    async fn citation_check_runs_before_judge() {
        let j = Counting(AtomicUsize::new(0), Ok((Says::Same, 0.99)));
        let none = FixedAnswer(vec![], Duration::ZERO);
        let o = grade_one(&q(1), &none, &j).await;
        assert_eq!(verdict(&o), (Verdict::Rejected, Some("missing_citation")));
        let wrong = FixedAnswer(vec!["policy.shipping".into()], Duration::ZERO);
        let o = grade_one(&q(1), &wrong, &j).await;
        assert_eq!(verdict(&o), (Verdict::Rejected, Some("wrong_citation")));
        assert_eq!(j.0.load(Ordering::SeqCst), 0, "the judge is never asked");
        let right = FixedAnswer(vec!["Policy.Returns".into()], Duration::ZERO);
        let o = grade_one(&q(1), &right, &j).await;
        assert_eq!(verdict(&o), (Verdict::Pass, None));
        assert_eq!(j.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn laya_down_is_pending_not_pass() {
        let a = FixedAnswer(vec!["policy.returns".into()], Duration::ZERO);
        for j in [
            Counting(AtomicUsize::new(0), Err("unreachable")),
            Counting(AtomicUsize::new(0), Ok((Says::Partly, 0.9))),
            Counting(AtomicUsize::new(0), Ok((Says::Same, 0.3))),
        ] {
            assert_eq!(
                verdict(&grade_one(&q(1), &a, &j).await),
                (Verdict::Pending, None)
            );
        }
        assert_eq!(
            verdict(&grade_one(&q(1), &a, &NoJudge).await),
            (Verdict::Pending, None)
        );
        let no = Counting(AtomicUsize::new(0), Ok((Says::Different, 0.9)));
        assert_eq!(
            verdict(&grade_one(&q(1), &a, &no).await),
            (Verdict::Rejected, Some("incorrect"))
        );
    }

    #[tokio::test]
    async fn budget_leaves_rest_pending() {
        let a = Arc::new(FixedAnswer(
            vec!["policy.returns".into()],
            Duration::from_millis(150),
        ));
        let j = Arc::new(Counting(AtomicUsize::new(0), Ok((Says::Same, 0.9))));
        let qs: Vec<Question> = (0..6).map(q).collect();
        let limits = Limits {
            concurrency: 2,
            per_question: Duration::from_secs(5),
            budget: Some(Duration::from_millis(200)),
        };
        let out = grade(&qs, a, j, limits).await;
        assert_eq!(out.len(), 6);
        let graded = out
            .iter()
            .filter(|o| matches!(o, Outcome::Graded(_)))
            .count();
        let unreached = out
            .iter()
            .filter(|o| matches!(o, Outcome::NotReached))
            .count();
        assert!(graded >= 2 && unreached >= 1, "{out:?}");
    }

    #[test]
    fn judge_reply_decodes() {
        let v = json!({"model": "laya-x-1", "answers": {JUDGE_KEY: {"choice": "yes", "confidence": 0.8}},
                       "usage": {"input_tokens": 12, "output_tokens": 3}});
        let (says, c, i, o, m) = decode_judge(&v).unwrap();
        assert_eq!((says, i, o), (Says::Same, 12, 3));
        assert!((c - 0.8).abs() < 1e-9);
        assert_eq!(m.as_deref(), Some("laya-x-1"));
        assert!(decode_judge(&json!({"answers": {}})).is_none());
        let body = judge_request("m", &q(1), "r");
        assert_eq!(body["questions"][JUDGE_KEY]["type"], "choice");
    }
}
