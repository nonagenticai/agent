//! T14b: the import classifier (plan J18 / T14, § Revision 2026-10-25 P10). Laya classifies each distinct row into
//! one of the job's effective activities, and one LLM call groups the uncovered rows into clusters.
//!
//! **Classification.** The activities are `Bases.activities` at the job's draft (`job_design::base::at`; they switch
//! to T5's journey once it lands). Each pending row is one Laya call on the tenant's `gates` provider (the way
//! `validation::judge::LayaJudge` resolves it, B5's `LayaClient`): a `choice` question whose options are the
//! activities plus `none`, 8 calls at a time. The answer's bucket:
//!
//! | Laya says | confidence | bucket |
//! |---|---|---|
//! | an activity | >= [`MARGIN`] | `covered` (stage + `stage:activity`) |
//! | `none` | >= [`MARGIN`] | `uncovered` |
//! | anything | < [`MARGIN`] | `unclear` |
//! | no answer, any Laya error (timeout, status, unreadable body, unreachable) | | `unclear` (fail open: never fails the import) |
//!
//! `hidden` and `out_of_scope` are not produced yet: a hidden activity is a refinement (T7 / P8) and B8 is a gate
//! (T15). The margin is a constant until the gate rows (B5 `gates::compiled`) carry a discovery threshold.
//!
//! No `gates` provider (unbound, or a binding that cannot serve): [`Classifier::classify`] answers `None` and the
//! import parks in `classifying` (worker docs); binding one makes the next sweep classify it, with no re-upload.
//! A job whose template yields no activities, or a Laya endpoint that cannot be built, fails open: every row is
//! `unclear`.
//!
//! **Clusters.** [`Classifier::cluster`] drafts `Call::Clusters` (`generator/prompts/clusters.md`) over the heaviest
//! uncovered excerpts through `generator::llm`: the platform drafter (`GENERATOR_PROVIDER_ID`) for a platform-owned
//! tenant, else the tenant's `generator` use. Every cluster is a `tenant_extension` suggestion. Unbound, timed out,
//! unreachable or unusable: a warning and no clusters; the import still finishes classified.
//!
//! **Metering (D56, R7).** Each answered Laya call is one `usage_event` (source `discovery`, use `classify`, activity
//! = the row's qualified activity when it has one) and the drafting call one (use `cluster`), on a connection scoped
//! to the tenant and job, attributed to the person who uploaded the import. A call that fails carries no usage and
//! is not metered.
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::worker::{
    self, Bucket, Classified, Classifier, ClusterDraft, ImportRef, Pending, Uncovered,
};
use crate::design::{self, DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::generator::llm::{DraftItem, Drafter, OpenAiDrafter};
use crate::generator::prompts::Call;
use crate::job_design::base;
use crate::job_design::gates::laya::{LayaAnswer, LayaClient, LayaError};
use crate::job_design::types::EffGateQuestion;
use crate::providers::llm::{self, Use};
use crate::state::AppState;
use crate::usage::{self, LlmUsage, UsageEvent};

/// Laya calls in flight at once.
pub const CONCURRENCY: usize = 8;
/// Below this confidence an answer is `unclear`.
pub const MARGIN: f64 = 0.5;
/// The `usage_event.source` of every discovery call.
pub const SOURCE: &str = "discovery";
/// The `usage_event.use` of a Laya classification.
pub const USE_CLASSIFY: &str = "classify";
/// The question's key in the Laya call, and the option that means "no activity fits".
pub const QUESTION_KEY: &str = "activity";
pub const NONE_KEY: &str = "none";
/// Longest cluster label stored (characters).
const MAX_LABEL: usize = 120;
/// Longest slug of a `tenant_extension` suggestion.
const MAX_SLUG: usize = 40;

/// One activity of the job as Laya is asked about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// `stage:activity`.
    pub key: String,
    pub stage: String,
    pub label: String,
}

/// The Laya-backed [`Classifier`] (module docs).
#[derive(Clone)]
pub struct LlmClassifier {
    st: AppState,
    /// Tests: draft clusters with this instead of the provider records.
    drafter: Option<Arc<dyn Drafter>>,
}

impl LlmClassifier {
    pub fn new(st: AppState) -> Self {
        Self { st, drafter: None }
    }

    /// Draft clusters with `d` instead of the platform / tenant provider records.
    pub fn with_drafter(mut self, d: Arc<dyn Drafter>) -> Self {
        self.drafter = Some(d);
        self
    }
}

/// What the job offers and who pays.
struct JobView {
    job: JobRef,
    activities: Vec<Activity>,
    /// The import's uploader: the actor usage is attributed to.
    actor: String,
}

/// A tenant + job scoped read of the job's draft activities and the import's uploader.
async fn job_view(c: &mut PgConnection, st: &AppState, at: &ImportRef) -> ApiResult<JobView> {
    let key: String = sqlx::query_scalar("SELECT key FROM jobs WHERE id = $1")
        .bind(at.job_id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "That job does not exist.".into()))?;
    let actor: String =
        sqlx::query_scalar("SELECT uploaded_by FROM discovery_import WHERE id = $1")
            .bind(&at.id)
            .fetch_optional(&mut *c)
            .await?
            .ok_or_else(|| {
                ApiError::Missing("import_not_found", "That import does not exist.".into())
            })?;
    let job = JobRef { id: at.job_id, key };
    design::ensure_job(&mut *c, &st.design, &job).await?;
    let bases = base::at(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        &job,
        &DesignOf::Draft,
    )
    .await?;
    let text = |a: &base::BaseActivity, f: &str| {
        a.item
            .get(f)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let activities = bases
        .activities
        .iter()
        .map(|a| Activity {
            key: a.key.clone(),
            stage: text(a, "stage")
                .or_else(|| a.key.split_once(':').map(|(s, _)| s.to_owned()))
                .unwrap_or_default(),
            label: text(a, "label").unwrap_or_else(|| a.key.clone()),
        })
        .collect();
    Ok(JobView {
        job,
        activities,
        actor,
    })
}

/// The one Laya question asked per row: which activity does the message ask for?
fn question(activities: &[Activity]) -> Option<EffGateQuestion> {
    let option = |key: &str, label: &str, description: String| {
        json!({"key": key, "label": label, "description": description, "generated": false,
               "base_label": label, "hidden": false, "refined": false, "verdicts": 0})
    };
    let mut options: Vec<Value> = activities
        .iter()
        .map(|a| option(&a.key, &a.label, format!("{} (stage {})", a.label, a.stage)))
        .collect();
    options.push(option(
        NONE_KEY,
        "None of these",
        "The message asks for something none of the listed activities does.".into(),
    ));
    serde_json::from_value(json!({
        "key": QUESTION_KEY, "group": "base", "layer": "base", "point": "P1", "type": "choice",
        "question": "Which one of the listed activities does this customer message ask for? Answer none when no \
                     activity fits.",
        "options": options, "generated_from": null, "threshold": "", "threshold_value": null,
        "fail": "open", "safety": false, "effect": "",
        "rule": {"fire_when": "in", "threshold": null, "fire_values": [NONE_KEY], "min_confidence": null},
        "action": "annotate", "tools": null, "origin": "platform", "updated_by": null,
        "updated_at": "2026-10-25T00:00:00Z", "base_threshold_value": null, "threshold_refined": false,
        "base_rule": {"fire_when": "in", "threshold": null, "fire_values": [NONE_KEY], "min_confidence": null},
        "rule_refined": false,
        "stats": {"verdicts_7d": 0, "fail_open": 0, "fail_closed": 0, "below_threshold": 0,
                  "latency_p50_ms": null, "latency_p95_ms": null, "timeouts": 0},
    }))
    .ok()
}

/// A row's bucket from Laya's answer (module docs table). `None` = no usable answer.
pub fn verdict_of(
    answer: Option<&LayaAnswer>,
    activities: &[Activity],
) -> (Bucket, Option<Activity>, Option<f32>) {
    let Some(LayaAnswer::Choice {
        choice, confidence, ..
    }) = answer
    else {
        return (Bucket::Unclear, None, None);
    };
    let conf = Some(confidence.clamp(0.0, 1.0) as f32);
    if *confidence < MARGIN {
        return (Bucket::Unclear, None, conf);
    }
    if choice == NONE_KEY {
        return (Bucket::Uncovered, None, conf);
    }
    match activities.iter().find(|a| a.key == *choice) {
        Some(a) => (Bucket::Covered, Some(a.clone()), conf),
        None => (Bucket::Unclear, None, conf),
    }
}

fn unclear(text_hash: &str) -> Classified {
    Classified {
        text_hash: text_hash.to_owned(),
        bucket: Bucket::Unclear,
        stage: None,
        activity: None,
        confidence: None,
        keep_excerpt: true,
    }
}

/// What a batch needs to call Laya.
enum Prepared {
    Ready(Box<Ready>),
    /// No `gates` provider can serve: park.
    NoProvider,
    /// Nothing to ask (no activities, or no Laya client): every row is `unclear`.
    AllUnclear,
}

struct Ready {
    laya: LayaClient,
    question: EffGateQuestion,
    activities: Vec<Activity>,
    provider_id: String,
    funding: String,
    actor: String,
}

impl LlmClassifier {
    async fn prepare(&self, at: &ImportRef) -> ApiResult<Prepared> {
        let mut tx = worker::begin(&self.st.db, at).await?;
        let v = job_view(&mut tx, &self.st, at).await?;
        let resolved = match llm::resolve(
            &mut tx,
            &self.st,
            at.tenant,
            &v.job,
            Use::Gates,
            &DesignOf::Draft,
        )
        .await
        {
            Ok(r) => r,
            // a binding that cannot serve is a provider that is not there
            Err(ApiError::Conflict(..)) => None,
            Err(e) => return Err(e),
        };
        tx.commit().await?;
        let Some(r) = resolved else {
            return Ok(Prepared::NoProvider);
        };
        if v.activities.is_empty() {
            tracing::warn!(import = %at.id, "discovery_classify_no_activities");
            return Ok(Prepared::AllUnclear);
        }
        let laya = match LayaClient::for_resolved(&r, &self.st.cfg.connector_policy, at.tenant)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(import = %at.id, code = e.code(), "discovery_classify_laya_unavailable");
                return Ok(Prepared::AllUnclear);
            }
        };
        let Some(question) = question(&v.activities) else {
            return Ok(Prepared::AllUnclear);
        };
        Ok(Prepared::Ready(Box::new(Ready {
            laya,
            question,
            activities: v.activities,
            provider_id: r.provider_id.map_or_else(
                || crate::turn::qualify::INTERNAL_PROVIDER.to_owned(),
                |p| p.to_string(),
            ),
            funding: r.funding.to_owned(),
            actor: v.actor,
        })))
    }

    /// One `usage_event` per `(activity, usage)`, in one transaction scoped to the import's tenant and job.
    async fn meter(
        &self,
        at: &ImportRef,
        actor: &str,
        use_: &'static str,
        used: &[(Option<String>, LlmUsage)],
    ) -> ApiResult<()> {
        if used.is_empty() {
            return Ok(());
        }
        let mut tx = worker::begin(&self.st.db, at).await?;
        for (activity, u) in used {
            usage::emit(
                &mut tx,
                &self.st.cfg.metering,
                &UsageEvent {
                    job_id: Some(at.job_id),
                    source: SOURCE,
                    use_,
                    activity: activity.as_deref(),
                    conversation_id: None,
                    case_id: None,
                    actor,
                    usage: u,
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn classify_batch(
        &self,
        at: &ImportRef,
        rows: &[Pending],
    ) -> ApiResult<Option<Vec<Classified>>> {
        let ready = match self.prepare(at).await? {
            Prepared::Ready(r) => r,
            Prepared::NoProvider => {
                tracing::debug!(import = %at.id, "discovery_classify_no_gates_provider");
                return Ok(None);
            }
            // rows without an answer are `unclear` (the worker's rule)
            Prepared::AllUnclear => return Ok(Some(Vec::new())),
        };
        let ready: Arc<Ready> = ready.into();
        let asked: Vec<Asked> = stream::iter(rows.to_vec())
            .map(|p| {
                let ready = Arc::clone(&ready);
                async move { ask_row(&ready, &p).await }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        let mut out = Vec::with_capacity(asked.len());
        let mut used = Vec::new();
        for (c, u) in asked {
            out.push(c);
            used.extend(u);
        }
        self.meter(at, &ready.actor, USE_CLASSIFY, &used).await?;
        Ok(Some(out))
    }

    /// The drafter for `at`'s tenant: the test override, the platform's for a platform-owned tenant, else the
    /// tenant's `generator` use.
    async fn drafter(
        &self,
        c: &mut PgConnection,
        at: &ImportRef,
        job: &JobRef,
    ) -> ApiResult<Arc<dyn Drafter>> {
        if let Some(d) = &self.drafter {
            return Ok(Arc::clone(d));
        }
        let cfg = &self.st.cfg.generator;
        if self.st.cfg.platform_tenant_id == Some(at.tenant) {
            Ok(Arc::new(OpenAiDrafter::platform(c, &self.st, cfg).await?))
        } else {
            Ok(Arc::new(
                OpenAiDrafter::tenant(c, &self.st, cfg, at.tenant, job).await?,
            ))
        }
    }

    async fn draft_clusters(
        &self,
        at: &ImportRef,
        rows: &[Uncovered],
    ) -> Result<Vec<ClusterDraft>, &'static str> {
        let mut tx = worker::begin(&self.st.db, at).await.map_err(|_| "db")?;
        let v = job_view(&mut tx, &self.st, at).await.map_err(|_| "db")?;
        let drafter = self.drafter(&mut tx, at, &v.job).await.map_err(|e| {
            tracing::debug!(error = ?e, "discovery_cluster_drafter_unavailable");
            "no_drafter"
        })?;
        tx.commit().await.map_err(|_| "db")?;
        let stages: BTreeSet<&str> = v.activities.iter().map(|a| a.stage.as_str()).collect();
        let input = json!({
            "job": v.job.key,
            "stages": stages,
            "activities": v.activities.iter()
                .map(|a| json!({"key": a.key, "label": a.label, "stage": a.stage}))
                .collect::<Vec<_>>(),
            "messages": rows.iter().enumerate()
                .map(|(i, r)| json!({"id": i, "text": r.excerpt}))
                .collect::<Vec<_>>(),
        });
        let budget = self.st.cfg.generator.draft_timeout;
        let (items, usage) = tokio::time::timeout(budget, drafter.call(Call::Clusters, &input, 0))
            .await
            .map_err(|_| "timeout")?
            .map_err(|e| {
                tracing::debug!(error = ?e, "discovery_cluster_draft_error");
                "draft_failed"
            })?;
        self.meter(
            at,
            &v.actor,
            Call::Clusters.use_label(),
            &[(None, usage.clone())],
        )
        .await
        .map_err(|_| "meter_failed")?;
        let tokens =
            i32::try_from(usage.tokens_in.max(0) + usage.tokens_out.max(0)).unwrap_or(i32::MAX);
        Ok(parse_clusters(&items, rows, &stages, tokens))
    }
}

/// A row's classification and the usage of the call that made it.
type Asked = (Classified, Option<(Option<String>, LlmUsage)>);

/// One Laya call for one row: its [`Classified`] and the call's usage (`None` when it failed or had no text).
async fn ask_row(r: &Ready, p: &Pending) -> Asked {
    let Some(text) = p.excerpt.as_deref().filter(|t| !t.trim().is_empty()) else {
        return (unclear(&p.text_hash), None);
    };
    let started = Instant::now();
    let reply = match r.laya.ask(json!({"message": text}), &[&r.question]).await {
        Ok(reply) => reply,
        Err(e) => {
            log_failure(&e);
            return (unclear(&p.text_hash), None);
        }
    };
    let usage = LlmUsage {
        provider_id: r.provider_id.clone(),
        model: reply
            .model
            .clone()
            .unwrap_or_else(|| r.laya.model().to_owned()),
        funding: r.funding.clone(),
        tokens_in: i64::try_from(reply.input_tokens).unwrap_or(i64::MAX),
        tokens_out: i64::try_from(reply.output_tokens).unwrap_or(i64::MAX),
        latency_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    };
    let (bucket, activity, confidence) = verdict_of(reply.answers.get(QUESTION_KEY), &r.activities);
    let key = activity.as_ref().map(|a| a.key.clone());
    let c = Classified {
        text_hash: p.text_hash.clone(),
        bucket,
        stage: activity.map(|a| a.stage),
        activity: key.clone(),
        confidence,
        keep_excerpt: true,
    };
    (c, Some((key, usage)))
}

/// Never the URL, key, message or upstream body (the error carries none).
fn log_failure(e: &LayaError) {
    tracing::warn!(code = e.code(), "discovery_classify_laya_failed");
}

impl Classifier for LlmClassifier {
    fn classify<'a>(
        &'a self,
        at: &'a ImportRef,
        rows: &'a [Pending],
    ) -> BoxFuture<'a, Option<Vec<Classified>>> {
        Box::pin(async move {
            match self.classify_batch(at, rows).await {
                Ok(r) => r,
                Err(e) => {
                    // a database or metering failure: park, the sweep retries (nothing was written for the batch)
                    tracing::warn!(import = %at.id, error = ?e, "discovery_classify_batch_failed");
                    None
                }
            }
        })
    }

    fn cluster<'a>(
        &'a self,
        at: &'a ImportRef,
        rows: &'a [Uncovered],
    ) -> BoxFuture<'a, Vec<ClusterDraft>> {
        Box::pin(async move {
            match self.draft_clusters(at, rows).await {
                Ok(v) => v,
                Err(why) => {
                    tracing::warn!(import = %at.id, why, "discovery_cluster_failed: the import finishes without clusters");
                    Vec::new()
                }
            }
        })
    }
}

/// A snake_case slug (`^[a-z][a-z0-9_]*$`, at most [`MAX_SLUG`]) of `s`; `None` when nothing is left.
pub fn slug_of(s: &str) -> Option<String> {
    let mut out = String::new();
    for ch in s.trim().to_lowercase().chars() {
        let c = if ch.is_ascii_alphanumeric() { ch } else { '_' };
        if c == '_' && (out.is_empty() || out.ends_with('_')) {
            continue;
        }
        out.push(c);
        if out.len() >= MAX_SLUG {
            break;
        }
    }
    let out = out.trim_end_matches('_').to_owned();
    (out.starts_with(|c: char| c.is_ascii_lowercase())).then_some(out)
}

/// The clusters of a drafting answer. An item needs a label and at least one member id of `rows` that no earlier
/// item took; its suggestion is a `tenant_extension` with a stage the job has (else null), a slug and a label.
/// `tokens` of the call ride on the first cluster.
pub fn parse_clusters(
    items: &[DraftItem],
    rows: &[Uncovered],
    stages: &BTreeSet<&str>,
    tokens: i32,
) -> Vec<ClusterDraft> {
    let mut taken: BTreeSet<usize> = BTreeSet::new();
    let mut out: Vec<ClusterDraft> = Vec::new();
    for it in items {
        let s = |v: &Value| {
            v.as_str()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
        };
        let label = s(&it.payload["label"])
            .or_else(|| s(&Value::String(it.title.clone())))
            .map(|l| l.chars().take(MAX_LABEL).collect::<String>());
        let Some(label) = label else { continue };
        let members: Vec<String> = it.payload["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .filter_map(|i| usize::try_from(i).ok())
            .filter(|i| *i < rows.len() && taken.insert(*i))
            .map(|i| rows[i].text_hash.clone())
            .collect();
        if members.is_empty() {
            continue;
        }
        let sug = &it.payload["suggestion"];
        let stage = s(&sug["stage"]).filter(|st| stages.contains(st.as_str()));
        let slug = s(&sug["slug"])
            .as_deref()
            .and_then(slug_of)
            .or_else(|| slug_of(&label));
        let gap = s(&sug["kind"]).filter(|k| k == "new_activity" || k == "knowledge_gap");
        let sug_label = s(&sug["label"])
            .map(|l| l.chars().take(MAX_LABEL).collect::<String>())
            .unwrap_or_else(|| label.clone());
        out.push(ClusterDraft {
            label,
            suggestion: json!({
                "kind": "tenant_extension", "stage": stage, "slug": slug, "label": sug_label, "gap": gap,
            }),
            members,
            drafted_tokens: 0,
        });
    }
    if let Some(first) = out.first_mut() {
        first.drafted_tokens = tokens.max(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn acts() -> Vec<Activity> {
        vec![Activity {
            key: "delivery:track_parcel".into(),
            stage: "delivery".into(),
            label: "Track a parcel".into(),
        }]
    }

    fn choice(c: &str, confidence: f64) -> LayaAnswer {
        LayaAnswer::Choice {
            choice: c.into(),
            probabilities: BTreeMap::new(),
            confidence,
        }
    }

    #[test]
    fn answers_map_to_buckets() {
        let a = acts();
        let (b, act, _) = verdict_of(Some(&choice("delivery:track_parcel", 0.9)), &a);
        assert_eq!(b, Bucket::Covered);
        assert_eq!(act.unwrap().stage, "delivery");
        assert_eq!(
            verdict_of(Some(&choice("none", 0.9)), &a).0,
            Bucket::Uncovered
        );
        assert_eq!(
            verdict_of(Some(&choice("delivery:track_parcel", 0.2)), &a).0,
            Bucket::Unclear
        );
        assert_eq!(
            verdict_of(Some(&choice("gone", 0.9)), &a).0,
            Bucket::Unclear
        );
        assert_eq!(verdict_of(None, &a).0, Bucket::Unclear);
    }

    #[test]
    fn the_question_offers_every_activity_and_none() {
        let q = question(&acts()).expect("question");
        let keys: Vec<&str> = q.options.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["delivery:track_parcel", NONE_KEY]);
    }

    #[test]
    fn slugs_are_snake_case() {
        assert_eq!(
            slug_of("Garden tools & lawn!").as_deref(),
            Some("garden_tools_lawn")
        );
        assert_eq!(slug_of("  123 go"), None);
        assert_eq!(slug_of("---"), None);
        assert!(slug_of(&"a".repeat(100)).unwrap().len() <= MAX_SLUG);
    }

    fn item(payload: Value) -> DraftItem {
        DraftItem {
            kind: "discovery_cluster".into(),
            title: "t".into(),
            detail: String::new(),
            example: None,
            payload,
            new_object: None,
            confidence: 0.9,
        }
    }

    #[test]
    fn clusters_keep_known_members_once() {
        let rows: Vec<Uncovered> = (0..3)
            .map(|i| Uncovered {
                text_hash: format!("{i:064x}"),
                excerpt: format!("m{i}"),
                weight: 1,
            })
            .collect();
        let stages: BTreeSet<&str> = ["delivery"].into_iter().collect();
        let items = [
            item(json!({"label": "Garden", "members": [0, 1, 9],
                        "suggestion": {"kind": "new_activity", "stage": "delivery", "slug": "garden", "label": "Garden help"}})),
            item(json!({"label": "Again", "members": [1]})),
            item(json!({"label": "Other", "members": [2], "suggestion": {"stage": "nowhere"}})),
        ];
        let got = parse_clusters(&items, &rows, &stages, 120);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].members.len(), 2);
        assert_eq!(got[0].drafted_tokens, 120);
        assert_eq!(got[0].suggestion["stage"], "delivery");
        assert_eq!(got[0].suggestion["kind"], "tenant_extension");
        assert_eq!(got[1].suggestion["stage"], Value::Null);
        assert_eq!(got[1].suggestion["slug"], "other");
        assert_eq!(got[1].drafted_tokens, 0);
    }
}
