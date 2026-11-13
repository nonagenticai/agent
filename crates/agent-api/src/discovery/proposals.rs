//! J18 proposals (B8 T15, second half): what discovery proposes for the job, and accept / reject (D54).
//!
//! **Where proposals come from** (plan B8-3): the clusters of the latest classified import (LLM-drafted at import time,
//! `drafted.tokens` = their call), and three templated signals with no model call (`drafted.tokens` = 0), read over the
//! last [`WINDOW_DAYS`] days of Production traffic:
//!
//! | kind | source | rule | accept writes (D54 draft) |
//! |---|---|---|---|
//! | `new_activity` / `enable_activity` | import cluster | `tenant_extension` / `enable_activity` suggestion | J4 `activity` row through [`refine`]: extension / un-hide |
//! | `new_activity` / `enable_activity` | live hand-overs | a person's `agent_should_handle` verdict with a draftable proposal, not drafted yet | B4's `handover::feedback::draft` (the same J4 write) |
//! | `knowledge_gap` | knowledge searches | >= [`MIN_SIGNAL`] "I don't know" replies on a stage that has an unbound kind with a tenant store named like it | J10 `knowledge` row: the kind is bound to that store |
//! | `operation_route` | unrouted calls | an expected operation called >= [`MIN_SIGNAL`] times with no route; a routed sibling of its object exists | J6 `route` rows, test + prod, copying the sibling's instance |
//!
//! **Ids** are stable: `p-cl-<import>-<cluster>`, `p-ho-<kind>-<activity>`, `p-kg-<stage>-<kind>`, `p-rt-<op>`.
//!
//! **Status.** No decision = `open`. `discovery_decision` `rejected` = `rejected`; `accepted` = B2's
//! [`lifecycle`] of the keys it wrote: `draft` / `in_release` (+release) / `live` (+release) / `discarded`. The
//! decision keeps the proposal as it was shown (`proposal`, 0193): a signal proposal disappears from the live read once
//! its cause is fixed, and the decided one must stay visible.
//!
//! **Accept** is one job-scoped transaction: the writes go through `design::track` (or the J4 / handover functions
//! that do), then the decision row. Live is never touched. Refusals: 404 `proposal_not_found`; 409 `already_decided`,
//! `already_in_job` (the activity is already there / enabled / the kind already bound / the routes exist),
//! `kind_not_in_job` (the knowledge kind is gone), `no_routing` (the object has no routed sibling), `no_route_row`
//! (an env has none to copy); 422 `unknown_store`, `invalid_key` (a cluster without a stage). **Reject** needs a
//! reason (422 `reason_required`).
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::cases::signals as case_signals;
use crate::connectors::model::OpRef;
use crate::connectors::opkey::{op_key, parse_op_key};
use crate::connectors::places;
use crate::connectors::resolve::{OperationRoute, RouteData, RouteTarget};
use crate::connectors::routes_store::{self, DesignOf};
use crate::design::{
    self, DraftRef, DraftWrite, JobRef, KeysLifecycle, Label, Registry, lifecycle,
};
use crate::error::{ApiError, ApiResult};
use crate::handover::{feedback, signals as handover_signals};
use crate::journey::model::Actor;
use crate::journey::refine::{self, NewExtension};
use crate::knowledge::{kinds, stores};
use crate::state::AppState;
use crate::tenancy::JobCtx;

/// The traffic window signals are read over.
pub const WINDOW_DAYS: i32 = 30;
/// How often a signal must repeat before it is proposed.
pub const MIN_SIGNAL: i64 = 3;
/// Ledger labels of discovery's own writes.
const PREFIX: &str = "Discovery: ";

/// What accepting a proposal does.
#[derive(Clone, Debug)]
pub enum Action {
    /// Un-hide a template activity (J4).
    EnableActivity { key: String },
    /// Add the tenant's own activity (J4).
    Extension {
        stage: Option<String>,
        slug: Option<String>,
        label: String,
        trigger: String,
    },
    /// Draft the hand-over item's proposal (B4), `item` = the representative item.
    Handover { item: String, activity: String },
    /// Bind knowledge kind `kind` of `stage` to the tenant store at `store`.
    Knowledge {
        stage: String,
        kind: String,
        store: String,
    },
    /// Route `op` like its sibling on each env (`None` = no sibling there).
    Route {
        op: OpRef,
        copies: Vec<(&'static str, Option<RouteTarget>)>,
    },
}

/// A proposal as the web shows it (status fields left for [`status_of`]) and what accepting it does.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub view: Value,
    pub action: Action,
}

impl Candidate {
    fn id(&self) -> &str {
        self.view["id"].as_str().unwrap_or("")
    }
}

// ---- building ---------------------------------------------------------------------------------------------------

struct Parts<'a> {
    id: String,
    kind: &'a str,
    title: String,
    why: String,
    activity: Option<String>,
    stage: Option<String>,
    evidence: Value,
    change: Value,
    estimate: Option<String>,
    cluster: Option<String>,
    import_id: Option<String>,
    tokens: i64,
}

fn proposal(p: Parts<'_>) -> Value {
    json!({
        "id": p.id, "kind": p.kind, "title": p.title, "why": p.why,
        "activity": p.activity, "stage": p.stage, "evidence": p.evidence, "change": p.change,
        "estimate": p.estimate, "cluster": p.cluster, "import_id": p.import_id,
        "status": "open", "decided_at": null, "decided_by": null, "reason": null, "release": null,
        "drafted": {"tokens": p.tokens},
    })
}

fn evidence(
    source: &str,
    count: i64,
    unit: &str,
    window: &str,
    share: Option<f64>,
    examples: Vec<Value>,
    metric: Option<String>,
) -> Value {
    json!({
        "source": source, "count": count, "unit": unit, "window": window, "share": share,
        "examples": examples, "metric": metric,
    })
}

fn window_text() -> String {
    format!("last {WINDOW_DAYS} days \u{b7} prod")
}

/// A fragment of an id: lowercase ascii, digits, `_`; everything else `-`.
fn frag(s: &str) -> String {
    s.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn text(v: &Value, f: &str) -> Option<String> {
    v.get(f)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn activity_keys(reg: &Registry, key: &str) -> ApiResult<Vec<String>> {
    let mut r = DraftRef::new();
    r.insert("key".into(), Value::String(key.to_owned()));
    Ok(vec![reg.target("activity")?.key(&r)])
}

// ---- candidates -------------------------------------------------------------------------------------------------

type ClusterRow = (String, String, i64, String, String, i64, Value, Value, i64);

/// The clusters of the latest classified import.
async fn cluster_candidates(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobCtx,
) -> ApiResult<Vec<Candidate>> {
    let rows: Vec<ClusterRow> = sqlx::query_as(
        "SELECT i.id, i.file, i.rows::bigint, k.id, k.label, k.tickets::bigint, k.examples, k.suggestion, \
                k.drafted_tokens::bigint \
           FROM discovery_import i \
           JOIN discovery_cluster k \
             ON k.tenant_id = i.tenant_id AND k.job_id = i.job_id AND k.import_id = i.id \
          WHERE i.tenant_id = $1 AND i.job_id = $2 \
            AND i.id = (SELECT x.id FROM discovery_import x \
                         WHERE x.tenant_id = $1 AND x.job_id = $2 AND x.status = 'classified' \
                         ORDER BY x.seq DESC LIMIT 1) \
          ORDER BY k.tickets DESC, k.id",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .fetch_all(&mut *c)
    .await?;
    let mut out = Vec::new();
    for (import, file, total, cid, label, tickets, examples, sug, tokens) in rows {
        let ex: Vec<Value> = examples
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|t| json!({"text": t, "ref": null, "outcome": null}))
            .collect();
        let share = (total > 0).then(|| {
            #[allow(clippy::cast_precision_loss)]
            let s = tickets as f64 / total as f64;
            (s * 10_000.0).round() / 10_000.0
        });
        let ev = evidence(
            "import",
            tickets,
            "tickets",
            &file,
            share,
            ex,
            Some(format!("{tickets} of {total} tickets")),
        );
        let id = format!("p-cl-{}-{}", frag(&import), frag(&cid));
        let stage = text(&sug, "stage");
        if sug["kind"] == "enable_activity" {
            let Some(key) = text(&sug, "activity") else {
                continue;
            };
            let name = text(&sug, "label").unwrap_or_else(|| label.clone());
            out.push(Candidate {
                view: proposal(Parts {
                    id,
                    kind: "enable_activity",
                    title: format!("Enable {name}"),
                    why: format!(
                        "{tickets} tickets in {file} ask for “{label}”; the activity is hidden."
                    ),
                    activity: Some(key.clone()),
                    stage,
                    evidence: ev,
                    change: json!({
                        "page": "J4", "kind": "activity", "keys": activity_keys(reg, &key)?,
                        "target": name, "live": "hidden", "draft": "enabled",
                    }),
                    estimate: Some(format!("~{tickets} tickets in this export")),
                    cluster: Some(cid),
                    import_id: Some(import),
                    tokens,
                }),
                action: Action::EnableActivity { key },
            });
        } else {
            let slug = text(&sug, "slug");
            let name = text(&sug, "label").unwrap_or_else(|| label.clone());
            let key = match (&stage, &slug) {
                (Some(s), Some(l)) => Some(format!("{s}:{l}")),
                _ => None,
            };
            let keys = match &key {
                Some(k) => activity_keys(reg, k)?,
                None => Vec::new(),
            };
            out.push(Candidate {
                view: proposal(Parts {
                    id,
                    kind: "new_activity",
                    title: format!("Add {name}"),
                    why: format!("{tickets} tickets in {file} fit none of the job's activities (“{label}”)."),
                    activity: key.clone(),
                    stage: stage.clone(),
                    evidence: ev,
                    change: json!({
                        "page": "J4", "kind": "activity", "keys": keys,
                        "target": format!("{} \u{b7} {name}", stage.as_deref().unwrap_or("no stage")),
                        "live": "not in the job", "draft": format!("your activity “{name}”"),
                    }),
                    estimate: Some(format!("~{tickets} tickets in this export")),
                    cluster: Some(cid),
                    import_id: Some(import),
                    tokens,
                }),
                action: Action::Extension {
                    stage,
                    slug,
                    label: name,
                    trigger: format!("discovery cluster “{label}” ({tickets} tickets)"),
                },
            });
        }
    }
    Ok(out)
}

/// Live hand-overs a person marked "agent should have handled" with a draftable proposal, not drafted yet; one
/// proposal per (kind, activity).
async fn handover_candidates(
    c: &mut PgConnection,
    reg: &Registry,
    since: DateTime<Utc>,
) -> ApiResult<Vec<Candidate>> {
    let sigs = handover_signals::discovery_signals(&mut *c, since).await?;
    let mut groups: BTreeMap<(String, String), (Vec<String>, Value)> = BTreeMap::new();
    for s in sigs {
        if s.verdict.as_deref() != Some("agent_should_handle") || s.drafted {
            continue;
        }
        let Some(p) = s.proposal else { continue };
        let kind = text(&p, "kind").unwrap_or_default();
        let Some(activity) = text(&p, "activity") else {
            continue;
        };
        if kind != "enable_activity" && kind != "tenant_extension" {
            continue;
        }
        let g = groups
            .entry((kind, activity))
            .or_insert_with(|| (Vec::new(), p.clone()));
        g.0.push(s.item_id);
    }
    let mut out = Vec::new();
    for ((kind, activity), (items, p)) in groups {
        let n = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let enable = kind == "enable_activity";
        let label = text(&p, "label").unwrap_or_else(|| activity.clone());
        out.push(Candidate {
            view: proposal(Parts {
                id: format!("p-ho-{}-{}", if enable { "enable" } else { "new" }, frag(&activity)),
                kind: if enable { "enable_activity" } else { "new_activity" },
                title: if enable {
                    format!("Enable {label}")
                } else {
                    format!("Add {label}")
                },
                why: format!("{n} hand-overs were marked “the agent should have handled this”."),
                activity: Some(activity.clone()),
                stage: text(&p, "stage"),
                evidence: evidence(
                    "live",
                    n,
                    "conversations",
                    &window_text(),
                    None,
                    Vec::new(),
                    Some(format!("{n} handed over, then judged the agent's to handle")),
                ),
                change: json!({
                    "page": "J4", "kind": "activity", "keys": activity_keys(reg, &activity)?,
                    "target": label,
                    "live": if enable { "hidden" } else { "not in the job" },
                    "draft": if enable { "enabled".to_owned() } else { format!("your activity “{label}”") },
                }),
                estimate: Some(format!("~{n} hand-overs a month answered without staff")),
                cluster: None,
                import_id: None,
                tokens: 0,
            }),
            action: Action::Handover {
                item: items[0].clone(),
                activity,
            },
        });
    }
    Ok(out)
}

/// Unbound knowledge kinds of the stages whose replies missed.
async fn knowledge_candidates(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Vec<Candidate>> {
    let misses = case_signals::knowledge_misses(&mut *c, job, WINDOW_DAYS).await?;
    let mut per_stage: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for m in misses {
        let stage = m
            .request_type
            .split(':')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if stage.is_empty() {
            continue;
        }
        let e = per_stage.entry(stage).or_default();
        e.0 += m.misses;
        e.1 += m.replies;
    }
    per_stage.retain(|_, (miss, _)| *miss >= MIN_SIGNAL);
    if per_stage.is_empty() {
        return Ok(Vec::new());
    }
    let rows = kinds::rows(&mut *c, job.id).await?;
    let all = stores::all(&mut *c).await?;
    let leaves: Vec<(String, String)> = all
        .iter()
        .filter(|s| s.is_leaf())
        .map(|s| (stores::path_of(&all, s.id).join("/"), s.key.clone()))
        .collect();
    let mut out = Vec::new();
    for k in rows {
        let Some(&(miss, replies)) = per_stage.get(&k.stage) else {
            continue;
        };
        if k.store_path.is_some() || !k.packs.is_empty() {
            continue;
        }
        let Some((path, _)) = leaves
            .iter()
            .find(|(p, key)| key == &k.kind || p.rsplit('/').next() == Some(k.kind.as_str()))
        else {
            continue;
        };
        let r = kinds::kind_ref(&k.stage, &k.kind);
        out.push(Candidate {
            view: proposal(Parts {
                id: format!("p-kg-{}-{}", frag(&k.stage), frag(&k.kind)),
                kind: "knowledge_gap",
                title: format!("Bind {} to {path}", k.kind),
                why: format!(
                    "{miss} of {replies} replies on {} found no passage, and {} has no knowledge bound.",
                    k.stage, k.kind
                ),
                activity: None,
                stage: Some(k.stage.clone()),
                evidence: evidence(
                    "knowledge_search",
                    miss,
                    "searches",
                    &window_text(),
                    #[allow(clippy::cast_precision_loss)]
                    (replies > 0).then(|| (miss as f64 / replies as f64 * 10_000.0).round() / 10_000.0),
                    Vec::new(),
                    Some(format!("{miss} of {replies} replies had no passage")),
                ),
                change: json!({
                    "page": "J10", "kind": kinds::KIND, "keys": [reg.target(kinds::KIND)?.key(&r)],
                    "target": format!("{} \u{b7} {}", k.stage, k.kind),
                    "live": "unbound", "draft": format!("bound to {path}"),
                }),
                estimate: None,
                cluster: None,
                import_id: None,
                tokens: 0,
            }),
            action: Action::Knowledge {
                stage: k.stage,
                kind: k.kind,
                store: path.clone(),
            },
        });
    }
    Ok(out)
}

fn same_variant(a: &str, b: &str) -> bool {
    let n = |v: &'_ str| if v.is_empty() { "*" } else { v }.to_owned();
    n(a) == n(b)
}

/// Is `op` routed on `env` in `data` (its own row or its verb's `*` row)?
fn routed(data: &RouteData, op: &OpRef, env: &str) -> bool {
    data.routes.iter().any(|r| {
        r.env == env
            && r.object == op.object
            && r.verb == op.verb
            && (same_variant(&r.variant, &op.variant) || r.variant == "*")
    })
}

/// The sibling route `op` is copied from on `env`: a route of the same object to an instance that serves `env` and
/// implements `op`; the same verb first, then by (verb, variant).
fn like(data: &RouteData, op: &OpRef, env: &str) -> Option<RouteTarget> {
    let own = op_key(op);
    let star = op_key(&OpRef {
        variant: "*".into(),
        ..op.clone()
    });
    let mut sib: Vec<&OperationRoute> = data
        .routes
        .iter()
        .filter(|r| r.env == env && r.object == op.object)
        .collect();
    sib.sort_by(|a, b| {
        (a.verb != op.verb, &a.verb, &a.variant).cmp(&(b.verb != op.verb, &b.verb, &b.variant))
    });
    sib.into_iter().find_map(|r| {
        let RouteTarget::Instance { instance_id, .. } = &r.target else {
            return None;
        };
        let inst = data.instances.iter().find(|i| &i.id == instance_id)?;
        let serves = inst.env == "all" || inst.env == env;
        let prod_ok = env != "prod" || (!inst.stub && !data.draft_instances.contains(instance_id));
        let implements = data
            .implements
            .get(instance_id)
            .is_some_and(|ops| ops.iter().any(|o| *o == own || *o == star));
        (serves && prod_ok && implements).then(|| r.target.clone())
    })
}

async fn route_candidates(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Vec<Candidate>> {
    let calls = case_signals::unrouted_calls(&mut *c, job, WINDOW_DAYS).await?;
    let mut per_op: BTreeMap<OpRef, i64> = BTreeMap::new();
    for u in calls {
        let op = parse_op_key(&u.operation);
        if op.object.is_empty() {
            continue;
        }
        *per_op.entry(op).or_default() += u.calls;
    }
    per_op.retain(|_, n| *n >= MIN_SIGNAL);
    if per_op.is_empty() {
        return Ok(Vec::new());
    }
    let view = routes_store::route_data(&mut *c, reg, job.id, DesignOf::Draft).await?;
    let data = &view.data;
    let mut out = Vec::new();
    for (op, n) in per_op {
        let expected = data.ops.iter().any(|o| {
            o.object == op.object
                && o.verb == op.verb
                && (same_variant(&o.variant, &op.variant) || o.variant == "*")
        });
        if !expected {
            continue;
        }
        let copies: Vec<(&'static str, Option<RouteTarget>)> = ["test", "prod"]
            .into_iter()
            .filter(|env| !routed(data, &op, env))
            .map(|env| (env, like(data, &op, env)))
            .collect();
        if copies.is_empty() {
            continue;
        }
        let key = op_key(&op);
        let mut keys = Vec::new();
        for (env, _) in &copies {
            keys.push(reg.target("route")?.key(&places::route_ref(&op, env)));
        }
        let envs: Vec<&str> = copies.iter().map(|(e, _)| *e).collect();
        out.push(Candidate {
            view: proposal(Parts {
                id: format!("p-rt-{}", frag(&key)),
                kind: "operation_route",
                title: format!("Route {key}"),
                why: format!(
                    "The agent asked for {key} {n} times and nothing is routed for it on {}.",
                    envs.join(" and ")
                ),
                activity: None,
                stage: None,
                evidence: evidence(
                    "live",
                    n,
                    "conversations",
                    &window_text(),
                    None,
                    Vec::new(),
                    Some(format!("{n} calls to {key} found no route")),
                ),
                change: json!({
                    "page": "J6", "kind": "route", "keys": keys,
                    "target": key, "live": "not offered",
                    "draft": format!("routed like its sibling on {}", envs.join(" + ")),
                }),
                estimate: None,
                cluster: None,
                import_id: None,
                tokens: 0,
            }),
            action: Action::Route { op, copies },
        });
    }
    Ok(out)
}

/// Every proposal the job's evidence yields now, decided or not, in display order (clusters, hand-overs, knowledge,
/// routes).
pub async fn candidates(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
) -> ApiResult<Vec<Candidate>> {
    let reg = &st.design;
    let jr = JobRef {
        id: job.job_id,
        key: job.job_key.clone(),
    };
    let mut out = cluster_candidates(&mut *c, reg, job).await?;
    let since = Utc::now() - Duration::days(i64::from(WINDOW_DAYS));
    out.extend(handover_candidates(&mut *c, reg, since).await?);
    out.extend(knowledge_candidates(&mut *c, reg, &jr).await?);
    out.extend(route_candidates(&mut *c, reg, &jr).await?);
    Ok(out)
}

// ---- decisions + the read ---------------------------------------------------------------------------------------

struct Decision {
    id: String,
    state: String,
    at: DateTime<Utc>,
    by: String,
    reason: Option<String>,
    keys: Vec<String>,
    proposal: Option<Value>,
}

/// One `discovery_decision` row: proposal id, state, at, by, reason, keys, proposal snapshot.
type DecisionRow = (
    String,
    String,
    DateTime<Utc>,
    String,
    Option<String>,
    Vec<String>,
    Option<Value>,
);

async fn decisions(c: &mut PgConnection, job: &JobCtx) -> ApiResult<Vec<Decision>> {
    let rows: Vec<DecisionRow> = sqlx::query_as(
        "SELECT proposal_id, state, at, \"by\", reason, keys, proposal FROM discovery_decision \
              WHERE tenant_id = $1 AND job_id = $2 ORDER BY at DESC, proposal_id",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, state, at, by, reason, keys, proposal)| Decision {
            id,
            state,
            at,
            by,
            reason,
            keys,
            proposal,
        })
        .collect())
}

async fn decision(c: &mut PgConnection, job: &JobCtx, id: &str) -> ApiResult<Option<Decision>> {
    Ok(decisions(c, job).await?.into_iter().find(|d| d.id == id))
}

/// `(status, release)` of a decision.
async fn status_of(
    c: &mut PgConnection,
    job: &JobRef,
    d: &Decision,
) -> ApiResult<(&'static str, Option<String>)> {
    if d.state == "rejected" {
        return Ok(("rejected", None));
    }
    Ok(match lifecycle(&mut *c, job, &d.keys).await? {
        KeysLifecycle::Draft => ("draft", None),
        KeysLifecycle::InRelease(r) => ("in_release", Some(r)),
        KeysLifecycle::Live(r) => ("live", Some(r)),
        KeysLifecycle::Discarded => ("discarded", None),
    })
}

/// `discoveryState.proposals`: the open ones (display order), then the decided ones, newest decision first.
pub async fn view(c: &mut PgConnection, st: &AppState, job: &JobCtx) -> ApiResult<Vec<Value>> {
    let jr = JobRef {
        id: job.job_id,
        key: job.job_key.clone(),
    };
    let decided = decisions(&mut *c, job).await?;
    let ids: BTreeSet<&str> = decided.iter().map(|d| d.id.as_str()).collect();
    let mut out: Vec<Value> = candidates(&mut *c, st, job)
        .await?
        .into_iter()
        .filter(|p| !ids.contains(p.id()))
        .map(|p| p.view)
        .collect();
    for d in &decided {
        let Some(mut v) = d.proposal.clone() else {
            continue;
        };
        let (status, release) = status_of(&mut *c, &jr, d).await?;
        v["status"] = json!(status);
        v["decided_at"] = json!(d.at);
        v["decided_by"] = json!(d.by);
        v["reason"] = json!(d.reason);
        v["release"] = json!(release);
        out.push(v);
    }
    Ok(out)
}

/// The ids of the accepted proposals of clusters of `import`: `(cluster id, decided at, by)` for the import list.
pub fn decided_not_found(id: &str) -> ApiError {
    ApiError::Missing(
        "proposal_not_found",
        format!("Discovery has no proposal {id} for this job."),
    )
}

fn already_decided(id: &str) -> ApiError {
    ApiError::Conflict(
        "already_decided",
        format!("The proposal {id} was already decided."),
    )
}

async fn record(
    c: &mut PgConnection,
    job: &JobCtx,
    p: &Candidate,
    state: &str,
    reason: Option<&str>,
    keys: &[String],
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO discovery_decision (tenant_id, job_id, proposal_id, state, \"by\", reason, keys, proposal) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .bind(p.id())
    .bind(state)
    .bind(&job.t.caller.sub)
    .bind(reason)
    .bind(keys.to_vec())
    .bind(&p.view)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The undecided candidate `id`: 409 `already_decided` when a decision exists, else 404 `proposal_not_found`.
async fn open_candidate(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
    id: &str,
) -> ApiResult<Candidate> {
    if decision(&mut *c, job, id).await?.is_some() {
        return Err(already_decided(id));
    }
    candidates(&mut *c, st, job)
        .await?
        .into_iter()
        .find(|p| p.id() == id)
        .ok_or_else(|| decided_not_found(id))
}

fn already_in_job(e: ApiError) -> ApiError {
    match e {
        ApiError::Conflict(
            "activity_exists" | "already_enabled" | "already_extended" | "not_refined"
            | "already_drafted",
            m,
        ) => ApiError::Conflict("already_in_job", m),
        other => other,
    }
}

/// `acceptDiscoveryProposal`. The caller holds the job-scoped transaction (`ensure_job` done) and the job row lock.
/// Returns the written ledger keys.
pub async fn accept(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
    id: &str,
) -> ApiResult<(String, Vec<String>)> {
    let reg = &st.design;
    let p = open_candidate(&mut *c, st, job, id).await?;
    let kind = p.view["kind"].as_str().unwrap_or("").to_owned();
    let keys: Vec<String> = match p.action.clone() {
        Action::EnableActivity { key } => {
            refine::restore(&mut *c, reg, st.compiler.as_ref(), job, &key)
                .await
                .map_err(already_in_job)?;
            activity_keys(reg, &key)?
        }
        Action::Extension {
            stage,
            slug,
            label,
            trigger,
        } => {
            let (stage, slug) = (stage.unwrap_or_default(), slug.unwrap_or_default());
            refine::add_extension(
                &mut *c,
                reg,
                st.compiler.as_ref(),
                job,
                NewExtension {
                    stage: stage.clone(),
                    slug: slug.clone(),
                    label,
                    actor: Actor::Customer,
                    trigger,
                },
            )
            .await
            .map_err(already_in_job)?;
            activity_keys(reg, &format!("{}:{}", stage.trim(), slug.trim()))?
        }
        Action::Handover { item, activity } => {
            sqlx::query("SELECT 1 FROM agent_handover_items WHERE id = $1 FOR UPDATE")
                .bind(&item)
                .fetch_optional(&mut *c)
                .await?;
            feedback::draft(&mut *c, reg, st.compiler.as_ref(), job, &item)
                .await
                .map_err(already_in_job)?;
            activity_keys(reg, &activity)?
        }
        Action::Knowledge { stage, kind, store } => {
            accept_knowledge(&mut *c, st, job, &stage, &kind, &store).await?
        }
        Action::Route { op, copies } => accept_routes(&mut *c, st, job, &op, &copies).await?,
    };
    record(&mut *c, job, &p, "accepted", None, &keys).await?;
    Ok((kind, keys))
}

async fn accept_knowledge(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
    stage: &str,
    kind: &str,
    store: &str,
) -> ApiResult<Vec<String>> {
    let reg = &st.design;
    let row = kinds::row(&mut *c, job.job_id, stage, kind)
        .await?
        .ok_or_else(|| {
            ApiError::Conflict(
                "kind_not_in_job",
                format!("{kind} ({stage}) is not a knowledge kind of this job."),
            )
        })?;
    if row.store_path.is_some() || !row.packs.is_empty() {
        return Err(ApiError::Conflict(
            "already_in_job",
            format!("{kind} ({stage}) already has knowledge bound."),
        ));
    }
    let all = stores::all(&mut *c).await?;
    if kinds::leaf_by_path(&all, store).is_none() {
        return Err(ApiError::Unprocessable(
            "unknown_store",
            format!("No knowledge store {store}."),
        ));
    }
    let r = kinds::kind_ref(stage, kind);
    let w = DraftWrite {
        area: "knowledge",
        page: "J10",
        kind: kinds::KIND,
        r#ref: r.clone(),
        label: Label::Now(format!("{PREFIX}{stage} \u{b7} {kind} bound to {store}")),
    };
    let (job_id, stg, kd, path, by) = (
        job.job_id,
        stage.to_owned(),
        kind.to_owned(),
        store.to_owned(),
        job.t.caller.sub.clone(),
    );
    design::track::<(), _>(&mut *c, reg, job, w, move |c| {
        Box::pin(
            async move { kinds::bind_row(c, job_id, &stg, &kd, Some(Some(path)), None, &by).await },
        )
    })
    .await?;
    Ok(vec![reg.target(kinds::KIND)?.key(&r)])
}

async fn accept_routes(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
    op: &OpRef,
    copies: &[(&'static str, Option<RouteTarget>)],
) -> ApiResult<Vec<String>> {
    let reg = &st.design;
    let key = op_key(op);
    // the routes may have changed since the proposal was shown: decide from the draft as it is now
    let view = routes_store::route_data(&mut *c, reg, job.job_id, DesignOf::Draft).await?;
    let now: Vec<&'static str> = copies
        .iter()
        .map(|(e, _)| *e)
        .filter(|e| !routed(&view.data, op, e))
        .collect();
    if now.is_empty() {
        return Err(ApiError::Conflict(
            "already_in_job",
            format!("{key} is already routed on every env."),
        ));
    }
    let targets: Vec<(&'static str, Option<RouteTarget>)> =
        now.iter().map(|e| (*e, like(&view.data, op, e))).collect();
    if targets.iter().all(|(_, t)| t.is_none()) {
        return Err(ApiError::Conflict(
            "no_routing",
            format!(
                "No operation of {} is routed yet; route one first.",
                op.object
            ),
        ));
    }
    if let Some((env, _)) = targets.iter().find(|(_, t)| t.is_none()) {
        return Err(ApiError::Conflict(
            "no_route_row",
            format!("{} has no route on {env} to copy for {key}.", op.object),
        ));
    }
    let mut keys = Vec::new();
    for (env, target) in targets {
        let Some(target) = target else { continue };
        let r = places::route_ref(op, env);
        let w = DraftWrite {
            area: places::AREA_ROUTES,
            page: "J6",
            kind: "route",
            r#ref: r.clone(),
            label: Label::Now(format!("{PREFIX}{key} on {env} \u{2192} {}", target.kind())),
        };
        let (job_id, by, row_op, row_target) =
            (job.job_id, job.t.caller.sub.clone(), op.clone(), target);
        design::track::<OperationRoute, _>(&mut *c, reg, job, w, move |c| {
            Box::pin(async move {
                places::write_route(c, job_id, &row_op, env, &row_target, &by, None).await
            })
        })
        .await?;
        keys.push(reg.target("route")?.key(&r));
    }
    Ok(keys)
}

/// `rejectDiscoveryProposal`: 409 `already_decided`, 404 `proposal_not_found`, 422 `reason_required` (in that
/// order). The decision keeps no keys.
pub async fn reject(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
    id: &str,
    reason: &str,
) -> ApiResult<String> {
    let p = open_candidate(&mut *c, st, job, id).await?;
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(ApiError::Unprocessable(
            "reason_required",
            "Say why, so the next discovery run learns from it.".into(),
        ));
    }
    record(&mut *c, job, &p, "rejected", Some(reason), &[]).await?;
    Ok(p.view["kind"].as_str().unwrap_or("").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_url_safe() {
        assert_eq!(frag("case.create{return}"), "case-create-return-");
        assert_eq!(frag("Delivery:Track_Parcel"), "delivery-track_parcel");
    }

    #[test]
    fn variants_compare_star_and_empty_alike() {
        assert!(same_variant("", "*"));
        assert!(!same_variant("return", "*"));
    }
}
