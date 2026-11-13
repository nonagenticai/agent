//! Draft validation (J17 "Test the draft", D54; B8 T13): the scenarios of the job's draft design, run against what
//! Production runs and against the draft.
//!
//! - **Scenarios are compiled, never stored** (B8-4). They are the `use_case_card.scenarios` of the activities that are
//!   on in the job's draft journey (a hidden activity has none), the cards read over the pinned stack
//!   ([`crate::journey::job::enabled_cards`]; a tenant refines a card's text, never its scenarios). A scenario's id is
//!   `<activity key>/<card scenario id>`; its `place` is the design place (B2 ledger key) its outcome depends on.
//! - **Run** ([`run`]): every scenario is probed twice through B4's [`probe_turn`] on env test: `Live` (what Production
//!   runs) and `Draft` (the working rows, at the revision read at the start). The replies are judged against
//!   `expected` by the job's `gates` Laya binding ([`LayaJudge`]; the same judge a validation cut uses): a pass is a
//!   `same` answer the judge is sure of ([`MIN_CONFIDENCE`]); `partly`, `different` or an unsure judge is a fail.
//!   `diff` is `regression` (live passes, draft fails), `fixed` (the other way) or `same`; a changed result carries as
//!   `cause` the draft ledger row at the scenario's `place`. A judge or a probe that cannot answer, or a run over the
//!   budget ([`REQUEST_BUDGET`]: Cloudflare cuts a request at ~100 s, B8-5), is 503 `unavailable` and nothing is
//!   stored; a draft that moved during the run is 409 `design_changed`. Probe calls are metered by the probe
//!   (`validation` / `qualify`), judge calls here (`validation` / `judge`).
//! - **Stored** in `draft_test_run` (`tr-<n>`, env test, the draft revision and fingerprint, the change keys the run
//!   included, the release Production ran): D69 live data, audited `validation.draft_run`, never with scenario text.
//! - **Read** ([`read`]): the scenarios, the latest runs, whether the latest run still describes the draft (`never`,
//!   `current`, `stale`), what changed since, and per draft change what the latest run says it did.
//!
//! Within one scenario the judge reads the live reply first, then the draft's.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use agent_model::kinds::journey::ScenarioStyle;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

use super::judge::{
    Judge, Judged, LayaJudge, MIN_CONFIDENCE, NoJudge, Question, REQUEST_BUDGET, Says,
};
use crate::audit::{self, Event};
use crate::design::{self, CaseDesign, DesignOf, Env, JobRef, LedgerRow, ledger};
use crate::error::{ApiError, ApiResult};
use crate::generator::run::iso;
use crate::journey::job as journey;
use crate::providers::llm::{self, Use};
use crate::release::summary;
use crate::state::AppState;
use crate::tenancy::JobCtx;
use crate::turn::probe;
use crate::usage::{self, LlmUsage, UsageEvent};

/// Scenarios probed at once.
pub const CONCURRENCY: usize = 8;
/// One scenario's budget: both probes run together (20 s each), then two judge calls (20 s each).
pub const PER_SCENARIO: Duration = Duration::from_secs(60);
/// Runs the read returns, newest first.
pub const RUNS_SHOWN: i64 = 20;

/// `base_release` of a job never released (the web shows "no release").
const NO_RELEASE: &str = "\u{2014}";

/// One test scenario (web `TestScenario`, with `place`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scenario {
    /// `<activity key>/<card scenario id>`.
    pub id: String,
    pub activity: String,
    pub activity_label: String,
    pub stage: String,
    pub style: ScenarioStyle,
    /// What the customer types.
    pub phrasing: String,
    /// What a correct reply does.
    pub expected: String,
    /// The design place (ledger key) the outcome depends on.
    pub place: Option<String>,
    pub cites: Vec<String>,
}

impl Scenario {
    /// The wire shape. The live probe the fixture graded with is not a server concept: `probe` is the static
    /// placeholder the web type still carries, and the replies are the run's (`ScenarioResult`), not the scenario's.
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "activity": self.activity,
            "activity_label": self.activity_label,
            "stage": self.stage,
            "style": self.style,
            "phrasing": self.phrasing,
            "expected": self.expected,
            "place": self.place,
            "probe": {"type": "static", "pass": true},
            "reply_pass": "",
            "reply_fail": "",
            "cites": self.cites,
            "fail_reason": format!("The reply does not do what a correct one does: {}", self.expected),
        })
    }

    fn question(&self) -> Question {
        Question {
            id: Uuid::new_v4(),
            question: self.phrasing.clone(),
            expected: self.expected.clone(),
            reference: String::new(),
            activity: self.activity.clone(),
        }
    }
}

/// The scenarios of the job's draft design (see the module doc), in journey order.
pub async fn scenarios(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
) -> ApiResult<Vec<Scenario>> {
    let mut out = Vec::new();
    for (a, card) in journey::enabled_cards(&mut *c, st, job, &DesignOf::Draft).await? {
        let Some(card) = card else { continue };
        for s in card.scenarios.unwrap_or_default() {
            out.push(Scenario {
                id: format!("{}/{}", a.activity.key, s.id),
                activity: a.activity.key.clone(),
                activity_label: a.activity.label.clone(),
                stage: a.activity.stage.as_str().to_owned(),
                style: s.style,
                phrasing: s.phrasing,
                expected: s.expected,
                place: s.place,
                cites: s.cites,
            });
        }
    }
    Ok(out)
}

// ---- grading one scenario -------------------------------------------------------------------------------------------

/// One side (live or draft) of a graded scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Side {
    pub pass: bool,
    pub reply: String,
    pub cited: Vec<String>,
    /// The judge call's usage, metered as `validation` / `judge`.
    pub usage: Option<LlmUsage>,
}

/// A graded scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Graded {
    pub live: Side,
    pub draft: Side,
}

impl Graded {
    /// `same` | `regression` | `fixed`.
    pub fn diff(&self) -> &'static str {
        match (self.live.pass, self.draft.pass) {
            (true, false) => "regression",
            (false, true) => "fixed",
            _ => "same",
        }
    }
}

fn verdict(pass: bool) -> &'static str {
    if pass { "pass" } else { "fail" }
}

fn unavailable(what: &str) -> ApiError {
    ApiError::Unavailable(
        "unavailable",
        format!("{what}; try again shortly."),
        Some(5),
    )
}

/// The judge's verdict on one reply: a pass only for a `same` answer it is sure of. A judge that gave no answer is an
/// error (a run never turns that into a pass or a fail).
pub async fn judge_reply(
    judge: &dyn Judge,
    q: &Question,
    reply: &str,
) -> ApiResult<(bool, Option<LlmUsage>)> {
    match judge.judge(q, reply).await {
        Ok(Judged {
            says,
            confidence,
            usage,
        }) => Ok((says == Says::Same && confidence >= MIN_CONFIDENCE, usage)),
        Err(f) => {
            tracing::warn!(code = f.code, "draft_validation_judge_failed");
            Err(unavailable("The judge did not answer"))
        }
    }
}

/// Where a scenario's two probes come from (the live ones: [`Probes`]).
pub trait Prober: Send + Sync {
    fn probe<'a>(
        &'a self,
        design: CaseDesign,
        text: &'a str,
    ) -> futures_util::future::BoxFuture<'a, ApiResult<probe::Probe>>;
}

/// B4's [`probe_turn`](probe::probe_turn) as `acting` on `job`.
pub struct Probes {
    pub st: AppState,
    pub acting: crate::tenancy::TenantCtx,
    pub job: JobRef,
}

impl Prober for Probes {
    fn probe<'a>(
        &'a self,
        design: CaseDesign,
        text: &'a str,
    ) -> futures_util::future::BoxFuture<'a, ApiResult<probe::Probe>> {
        Box::pin(probe::probe_turn(
            &self.st,
            &self.acting,
            &self.job,
            design,
            text,
        ))
    }
}

/// Probe `sc` on both designs (together), then judge the live reply and the draft reply (in that order).
pub async fn grade_scenario(
    prober: &dyn Prober,
    judge: &dyn Judge,
    live: &CaseDesign,
    draft: &CaseDesign,
    sc: &Scenario,
) -> ApiResult<Graded> {
    let (l, d) = tokio::join!(
        prober.probe(live.clone(), &sc.phrasing),
        prober.probe(draft.clone(), &sc.phrasing)
    );
    let (l, d) = (l?, d?);
    let q = sc.question();
    let (live_pass, live_usage) = judge_reply(judge, &q, &l.reply).await?;
    let (draft_pass, draft_usage) = judge_reply(judge, &q, &d.reply).await?;
    Ok(Graded {
        live: Side {
            pass: live_pass,
            reply: l.reply,
            cited: l.cited,
            usage: live_usage,
        },
        draft: Side {
            pass: draft_pass,
            reply: d.reply,
            cited: d.cited,
            usage: draft_usage,
        },
    })
}

/// Grade `scenarios` (concurrency [`CONCURRENCY`], [`PER_SCENARIO`] each, [`REQUEST_BUDGET`] in all), in input
/// order. Any scenario that fails, times out or is not reached inside the budget fails the whole run.
pub async fn grade_all(
    prober: Arc<dyn Prober>,
    judge: Arc<dyn Judge>,
    live: &CaseDesign,
    draft: &CaseDesign,
    scenarios: &[Scenario],
    budget: Duration,
) -> ApiResult<Vec<Graded>> {
    let deadline = tokio::time::Instant::now() + budget;
    let sem = Arc::new(Semaphore::new(CONCURRENCY));
    let mut set: JoinSet<(usize, ApiResult<Graded>)> = JoinSet::new();
    for (i, sc) in scenarios.iter().enumerate() {
        let permit = tokio::time::timeout_at(deadline, sem.clone().acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| unavailable("The run did not finish in time"))?;
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let (p, j, l, d, sc) = (
            prober.clone(),
            judge.clone(),
            live.clone(),
            draft.clone(),
            sc.clone(),
        );
        set.spawn(async move {
            let _permit = permit;
            let out = match tokio::time::timeout(
                PER_SCENARIO.min(left),
                grade_scenario(p.as_ref(), j.as_ref(), &l, &d, &sc),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(unavailable("A scenario did not finish in time")),
            };
            (i, out)
        });
    }
    let mut out: Vec<Option<Graded>> = vec![None; scenarios.len()];
    while let Some(done) = set.join_next().await {
        let Ok((i, r)) = done else {
            return Err(unavailable("A scenario run stopped"));
        };
        out[i] = Some(r?);
    }
    out.into_iter()
        .map(|g| g.ok_or_else(|| unavailable("The run did not finish in time")))
        .collect()
}

// ---- results ------------------------------------------------------------------------------------------------------------

/// `ScenarioResult` of one graded scenario; `cause` = the draft ledger row at the scenario's place when the result
/// changed.
pub fn result_json(sc: &Scenario, g: &Graded, rows: &[LedgerRow]) -> Value {
    let diff = g.diff();
    let cause = if diff == "same" {
        None
    } else {
        sc.place
            .as_deref()
            .and_then(|p| rows.iter().find(|r| r.key == p))
            .map(|r| {
                json!({"change_id": format!("d-{}", r.id), "key": r.key, "label": r.label, "page": r.page})
            })
    };
    json!({
        "scenario": sc.id,
        "live": verdict(g.live.pass),
        "draft": verdict(g.draft.pass),
        "diff": diff,
        "reply_live": g.live.reply,
        "reply_draft": g.draft.reply,
        "cited_draft": g.draft.cited,
        "cause": cause,
    })
}

/// `DraftTestRun.summary`.
pub fn summary_json(graded: &[Graded]) -> Value {
    let count = |f: &dyn Fn(&Graded) -> bool| graded.iter().filter(|g| f(g)).count();
    json!({
        "total": graded.len(),
        "pass": count(&|g| g.draft.pass),
        "fail": count(&|g| !g.draft.pass),
        "regressions": count(&|g| g.diff() == "regression"),
        "fixed": count(&|g| g.diff() == "fixed"),
        "live_pass": count(&|g| g.live.pass),
    })
}

// ---- the run (runDraftValidation) ----------------------------------------------------------------------------------------

/// 409 `no_scenarios`.
pub fn no_scenarios() -> ApiError {
    ApiError::Conflict(
        "no_scenarios",
        "This job has no test scenarios yet: try the draft in the test chat.".into(),
    )
}

fn design_changed() -> ApiError {
    ApiError::Conflict(
        "design_changed",
        "The draft changed while the scenarios ran; run them again.".into(),
    )
}

/// The `gates` judge of the job's draft: Laya on its binding, [`NoJudge`] when unbound or unreachable.
async fn judge_of(
    st: &AppState,
    j: &JobCtx,
    resolved: Option<crate::providers::llm::ResolvedLlm>,
) -> Arc<dyn Judge> {
    match resolved {
        Some(r) => match LayaJudge::for_resolved(&r, &st.cfg.connector_policy, j.t.tenant).await {
            Some(l) => Arc::new(l),
            None => Arc::new(NoJudge),
        },
        None => Arc::new(NoJudge),
    }
}

/// `runDraftValidation`: the module doc. Returns the new [`read`].
pub async fn run(st: &AppState, j: &JobCtx) -> ApiResult<Value> {
    run_with(st, j, None, REQUEST_BUDGET).await
}

/// [`run`] with a prober (`None` = B4's probe) and a budget (a test's).
pub async fn run_with(
    st: &AppState,
    j: &JobCtx,
    prober: Option<Arc<dyn Prober>>,
    budget: Duration,
) -> ApiResult<Value> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    let scenarios = scenarios(&mut tx, st, &job).await?;
    if scenarios.is_empty() {
        return Err(no_scenarios());
    }
    let live = design::case_design(&mut tx, &job, Env::Production).await?;
    let draft = design::case_design(&mut tx, &job, Env::Test).await?;
    let base = summary::current_ref(&mut tx, &job).await?.map(|r| r.id);
    let resolved =
        match llm::resolve(&mut tx, st, j.t.tenant, &job, Use::Gates, &DesignOf::Draft).await {
            Ok(r) => r,
            // a binding that cannot serve is a judge that cannot answer
            Err(ApiError::Conflict(..)) => None,
            Err(e) => return Err(e),
        };
    tx.commit().await?;
    let judge = judge_of(st, j, resolved).await;
    let prober = prober.unwrap_or_else(|| {
        Arc::new(Probes {
            st: st.clone(),
            acting: j.t.clone(),
            job: job.clone(),
        })
    });
    let graded = grade_all(prober, judge, &live, &draft, &scenarios, budget).await?;

    let mut tx = j.begin(&st.db).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('draft_test_run:' || $1::text, 0))")
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
    // the draft the scenarios ran on must still be the draft: otherwise the run describes nothing
    let now_draft = design::case_design(&mut tx, &job, Env::Test).await?;
    if now_draft != draft {
        return Err(design_changed());
    }
    let rows_now = ledger::of_job(&mut tx, &job).await?;
    let keys: Vec<String> = rows_now
        .iter()
        .map(|r| ledger::change_key(&r.key, &r.after))
        .collect();
    let CaseDesign::Draft { draft_rev } = draft else {
        return Err(design_changed());
    };
    let n: i64 = sqlx::query_scalar(
        "SELECT coalesce(max(substr(id, 4)::bigint), 0) + 1 FROM draft_test_run WHERE job_id = $1",
    )
    .bind(job.id)
    .fetch_one(&mut *tx)
    .await?;
    let id = format!("tr-{n}");
    let results: Vec<Value> = scenarios
        .iter()
        .zip(&graded)
        .map(|(s, g)| result_json(s, g, &rows_now))
        .collect();
    let summary = summary_json(&graded);
    sqlx::query(
        "INSERT INTO draft_test_run \
             (id, job_id, env, draft_rev, draft_fp, change_keys, base_release, by, results, summary) \
         VALUES ($1, $2, 'test', $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(&id)
    .bind(job.id)
    .bind(draft_rev)
    .bind(design::fingerprint(&keys))
    .bind(&keys)
    .bind(&base)
    .bind(&j.t.caller.sub)
    .bind(Value::Array(results))
    .bind(&summary)
    .execute(&mut *tx)
    .await?;
    for (s, g) in scenarios.iter().zip(&graded) {
        for u in [&g.live.usage, &g.draft.usage].into_iter().flatten() {
            usage::emit(
                &mut tx,
                &st.cfg.metering,
                &UsageEvent {
                    job_id: Some(job.id),
                    source: "validation",
                    use_: "judge",
                    activity: Some(s.activity.as_str()),
                    conversation_id: None,
                    case_id: None,
                    actor: &j.t.caller.sub,
                    usage: u,
                },
            )
            .await?;
        }
    }
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("validation.draft_run", "draft_test_run", id.clone())
            .job(&j.job_key)
            .detail("draft_rev", draft_rev)
            .detail("scenarios", scenarios.len())
            .detail("regressions", summary["regressions"].clone())
            .detail("fixed", summary["fixed"].clone()),
    )
    .await?;
    let v = read_in(&mut tx, st, &job).await?;
    tx.commit().await?;
    Ok(v)
}

// ---- the read (draftValidation) ---------------------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct RunRow {
    id: String,
    draft_rev: i64,
    draft_fp: String,
    change_keys: Vec<String>,
    base_release: Option<String>,
    at: DateTime<Utc>,
    by: String,
    results: Value,
    summary: Value,
}

/// `draftValidation` for the job of `j` (its own transaction).
pub async fn read(st: &AppState, j: &JobCtx) -> ApiResult<Value> {
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    let v = read_in(&mut tx, st, &job).await?;
    tx.commit().await?;
    Ok(v)
}

/// The change key's design key (`setting:actor@{…}` → `setting:actor`).
fn key_of(change_key: &str) -> &str {
    change_key.split_once('@').map_or(change_key, |(k, _)| k)
}

/// `DraftValidation` (see the module doc).
pub async fn read_in(c: &mut PgConnection, st: &AppState, job: &JobRef) -> ApiResult<Value> {
    let scenarios = scenarios(&mut *c, st, job).await?;
    let rows = ledger::of_job(&mut *c, job).await?;
    let keys: Vec<String> = rows
        .iter()
        .map(|r| ledger::change_key(&r.key, &r.after))
        .collect();
    let draft_rev = design::draft_rev(&mut *c, job).await?;
    let base = summary::current_ref(&mut *c, job).await?.map(|r| r.id);
    let runs: Vec<RunRow> = sqlx::query_as(
        "SELECT id, draft_rev, draft_fp, change_keys, base_release, at, by, results, summary \
           FROM draft_test_run WHERE job_id = $1 \
          ORDER BY substr(id, 4)::bigint DESC LIMIT $2",
    )
    .bind(job.id)
    .bind(RUNS_SHOWN)
    .fetch_all(&mut *c)
    .await?;
    let mut subs: Vec<String> = runs.iter().map(|r| r.by.clone()).collect();
    subs.extend(rows.iter().map(|r| r.by_sub.clone()));
    subs.sort();
    subs.dedup();
    let names = crate::members::names::lookup(&mut *c, &subs).await?;
    let name = |s: &str| names.get(s).cloned().unwrap_or_else(|| s.to_owned());

    let latest = runs.first();
    let tested: BTreeSet<&str> = latest
        .map(|r| r.change_keys.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let now: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let mut changed_since: Vec<String> = Vec::new();
    if let Some(l) = latest {
        for (r, k) in rows.iter().zip(&keys) {
            if !tested.contains(k.as_str()) {
                changed_since.push(r.label.clone());
            }
        }
        for k in &l.change_keys {
            if !now.contains(k.as_str()) {
                changed_since.push(format!("discarded or changed again: {}", key_of(k)));
            }
        }
    }
    let state = match latest {
        None => "never",
        Some(_) if changed_since.is_empty() => "current",
        Some(_) => "stale",
    };

    // per draft change: the scenarios its place covers and what the latest run says it did
    let place_of: BTreeMap<&str, Option<&str>> = scenarios
        .iter()
        .map(|s| (s.id.as_str(), s.place.as_deref()))
        .collect();
    let results: Vec<&Value> = latest
        .and_then(|l| l.results.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let impacts: Vec<Value> = rows
        .iter()
        .zip(&keys)
        .map(|(r, k)| {
            let covered = scenarios
                .iter()
                .filter(|s| s.place.as_deref() == Some(r.key.as_str()))
                .count();
            let at_place = |diff: &str| {
                results
                    .iter()
                    .filter(|x| {
                        x["diff"] == diff
                            && x["scenario"]
                                .as_str()
                                .and_then(|id| place_of.get(id).copied().flatten())
                                == Some(r.key.as_str())
                    })
                    .count()
            };
            json!({
                "change_id": format!("d-{}", r.id),
                "key": r.key,
                "label": r.label,
                "page": r.page,
                "by_name": name(&r.by_sub),
                "at": iso(r.at),
                "covered": covered,
                "fixed": at_place("fixed"),
                "regressions": at_place("regression"),
                "untested": !tested.contains(k.as_str()),
            })
        })
        .collect();
    let runs_json: Vec<Value> = runs
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "env": "test",
                "draft_rev": r.draft_rev,
                "draft_fp": r.draft_fp,
                "change_keys": r.change_keys,
                "base_release": r.base_release.as_deref().unwrap_or(NO_RELEASE),
                "at": iso(r.at),
                "by": r.by,
                "by_name": name(&r.by),
                "results": r.results,
                "summary": r.summary,
            })
        })
        .collect();
    Ok(json!({
        "scenarios": scenarios.iter().map(Scenario::json).collect::<Vec<_>>(),
        "runs": runs_json,
        "current": {
            "draft_rev": draft_rev,
            "draft_fp": design::fingerprint(&keys),
            "changes": rows.len(),
            "base_release": base.as_deref().unwrap_or(NO_RELEASE),
        },
        "state": state,
        "changed_since": changed_since,
        "impacts": impacts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::judge::JudgeFailed;
    use futures_util::future::BoxFuture;

    fn sc(id: &str, place: Option<&str>) -> Scenario {
        Scenario {
            id: id.into(),
            activity: "support:start_return".into(),
            activity_label: "Start a return".into(),
            stage: "support".into(),
            style: ScenarioStyle::Plain,
            phrasing: format!("phrasing {id}"),
            expected: format!("expected {id}"),
            place: place.map(str::to_owned),
            cites: vec![],
        }
    }

    fn side(pass: bool) -> Side {
        Side {
            pass,
            reply: String::new(),
            cited: vec![],
            usage: None,
        }
    }

    #[test]
    fn diff_is_regression_fixed_or_same() {
        let g = |l, d| Graded {
            live: side(l),
            draft: side(d),
        };
        assert_eq!(g(true, false).diff(), "regression");
        assert_eq!(g(false, true).diff(), "fixed");
        assert_eq!(g(true, true).diff(), "same");
        assert_eq!(g(false, false).diff(), "same");
    }

    #[test]
    fn summary_counts_the_draft_side() {
        let g = |l, d| Graded {
            live: side(l),
            draft: side(d),
        };
        let s = summary_json(&[
            g(true, false),
            g(false, true),
            g(true, true),
            g(false, false),
        ]);
        assert_eq!(
            s,
            json!({"total": 4, "pass": 2, "fail": 2, "regressions": 1, "fixed": 1, "live_pass": 2})
        );
    }

    /// A prober whose reply is the design's name; the judge passes a reply that says `draft`.
    struct Echo;
    impl Prober for Echo {
        fn probe<'a>(
            &'a self,
            design: CaseDesign,
            _text: &'a str,
        ) -> BoxFuture<'a, ApiResult<probe::Probe>> {
            Box::pin(async move {
                Ok(probe::Probe {
                    reply: match design {
                        CaseDesign::Live { .. } => "live".into(),
                        CaseDesign::Draft { .. } => "draft".into(),
                    },
                    ..probe::Probe::default()
                })
            })
        }
    }
    struct Says_(&'static str, f64);
    impl Judge for Says_ {
        fn judge<'a>(
            &'a self,
            _q: &'a Question,
            response: &'a str,
        ) -> BoxFuture<'a, Result<Judged, JudgeFailed>> {
            let (want, confidence) = (self.0, self.1);
            Box::pin(async move {
                Ok(Judged {
                    says: if response == want {
                        Says::Same
                    } else {
                        Says::Different
                    },
                    confidence,
                    usage: None,
                })
            })
        }
    }
    struct Silent;
    impl Judge for Silent {
        fn judge<'a>(
            &'a self,
            _q: &'a Question,
            _response: &'a str,
        ) -> BoxFuture<'a, Result<Judged, JudgeFailed>> {
            Box::pin(async {
                Err(JudgeFailed {
                    code: "down",
                    usage: None,
                })
            })
        }
    }

    const LIVE: CaseDesign = CaseDesign::Live { release_id: None };
    const DRAFT: CaseDesign = CaseDesign::Draft { draft_rev: 3 };

    #[tokio::test]
    async fn a_draft_that_answers_better_is_fixed_and_one_that_answers_worse_a_regression() {
        let scs = [sc("a", Some("setting:actor")), sc("b", None)];
        let fixed = grade_all(
            Arc::new(Echo),
            Arc::new(Says_("draft", 0.9)),
            &LIVE,
            &DRAFT,
            &scs,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(fixed.iter().all(|g| g.diff() == "fixed"));
        let worse = grade_all(
            Arc::new(Echo),
            Arc::new(Says_("live", 0.9)),
            &LIVE,
            &DRAFT,
            &scs,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(worse.iter().all(|g| g.diff() == "regression"));
        // an unsure judge never passes
        let unsure = grade_all(
            Arc::new(Echo),
            Arc::new(Says_("draft", 0.1)),
            &LIVE,
            &DRAFT,
            &scs,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(unsure.iter().all(|g| !g.draft.pass && !g.live.pass));
    }

    #[tokio::test]
    async fn a_judge_that_gives_no_answer_fails_the_run_not_the_scenario() {
        let r = grade_all(
            Arc::new(Echo),
            Arc::new(Silent),
            &LIVE,
            &DRAFT,
            &[sc("a", None)],
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(r, Err(ApiError::Unavailable("unavailable", ..))),
            "{r:?}"
        );
    }

    #[test]
    fn a_changed_result_names_the_ledger_row_at_its_place() {
        let row = LedgerRow {
            id: 7,
            job_id: Uuid::nil(),
            key: "setting:actor".into(),
            kind: "setting".into(),
            r#ref: design::DraftRef::new(),
            area: "a".into(),
            page: "J7".into(),
            label: "changed the actor".into(),
            before: None,
            after: None,
            at: Utc::now(),
            by_sub: "u".into(),
            live_moved: None,
        };
        let g = Graded {
            live: side(true),
            draft: side(false),
        };
        let r = result_json(
            &sc("a", Some("setting:actor")),
            &g,
            std::slice::from_ref(&row),
        );
        assert_eq!(
            r["cause"],
            json!({"change_id": "d-7", "key": "setting:actor", "label": "changed the actor", "page": "J7"})
        );
        assert_eq!(r["diff"], "regression");
        // no place, another place or an unchanged result: no cause
        assert_eq!(
            result_json(&sc("a", None), &g, std::slice::from_ref(&row))["cause"],
            Value::Null
        );
        assert_eq!(
            result_json(
                &sc("a", Some("rule:RET-01")),
                &g,
                std::slice::from_ref(&row)
            )["cause"],
            Value::Null
        );
        let same = Graded {
            live: side(true),
            draft: side(true),
        };
        assert_eq!(
            result_json(&sc("a", Some("setting:actor")), &same, &[row])["cause"],
            Value::Null
        );
    }
}
