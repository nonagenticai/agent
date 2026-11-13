//! The Laya System-1 client (B5 T9; contract `gitops-production:scripts/host-configs/pgx/laya/bridge.py`).
//!
//! `POST {base_url}/v1/systemone` with `Authorization: Bearer <key>` (license-authz reads the licence token there)
//! and `{model, state, questions}`: one call per gate point carrying every question asked there. Question mapping:
//! noul → `criteria {true, false}` from the options' descriptions; choice → `criteria {key: description}` of the
//! **visible** options; score → `criteria [level descriptions…]` in option order (an empty description falls back
//! to the label). Answers: noul `{noul: f64 | bool}`; choice `{choice, probabilities, confidence}`; score `{score
//! (0-based), probabilities (0-based keys), confidence}`.
//!
//! Errors ([`LayaError`]) name the failure only: never the URL, the key, the state or an upstream body
//! (`reqwest::Error::without_url`). The internal provider (D70, `provider_id: None`) is called on a plain client
//! without redirects; a tenant's own Laya (B3 binding) only through `netguard::pinned_client`.
use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Url;
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::job_design::types::{EffGateQuestion, GateType};
use crate::netguard::{self, Policy};
use crate::providers::llm::ResolvedLlm;

/// One Laya call's budget (`LAYA_TIMEOUT_MS`).
pub const LAYA_TIMEOUT: Duration = Duration::from_millis(1500);
/// A whole gate point's budget (`GATE_BUDGET_SECS`), whatever the client's own timeout.
pub const GATE_BUDGET: Duration = Duration::from_secs(3);
/// Laya's endpoint below the provider's base URL.
pub const SYSTEMONE_PATH: &str = "/v1/systemone";

/// One question as Laya reads it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayaQuestion {
    #[serde(rename = "type")]
    pub kind: GateType,
    pub instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Value>,
}

/// The `POST /v1/systemone` body.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayaRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, LayaQuestion>,
}

fn criterion(o: &crate::job_design::types::EffGateOption) -> String {
    if o.description.trim().is_empty() {
        o.label.clone()
    } else {
        o.description.clone()
    }
}

/// A question's Laya form (criteria per the module doc).
pub fn question(q: &EffGateQuestion) -> LayaQuestion {
    let criteria = match q.kind {
        GateType::Noul => {
            let m: Map<String, Value> = q
                .options
                .iter()
                .filter(|o| {
                    (o.key == "true" || o.key == "false") && !o.description.trim().is_empty()
                })
                .map(|o| (o.key.clone(), Value::String(o.description.clone())))
                .collect();
            (!m.is_empty()).then_some(Value::Object(m))
        }
        GateType::Choice => Some(Value::Object(
            q.options
                .iter()
                .filter(|o| !o.hidden)
                .map(|o| (o.key.clone(), Value::String(criterion(o))))
                .collect(),
        )),
        GateType::Score => Some(Value::Array(
            q.options
                .iter()
                .map(|o| Value::String(criterion(o)))
                .collect(),
        )),
    };
    LayaQuestion {
        kind: q.kind,
        instructions: q.question.clone(),
        criteria,
    }
}

/// The request for `questions` (keyed by question key) on `state`.
pub fn request(model: &str, state: Value, questions: &[&EffGateQuestion]) -> LayaRequest {
    LayaRequest {
        model: model.to_owned(),
        state,
        questions: questions
            .iter()
            .map(|q| (q.key.clone(), question(q)))
            .collect(),
    }
}

/// One decoded answer.
#[derive(Clone, Debug, PartialEq)]
pub enum LayaAnswer {
    /// p(true), 0..=1 (a bool answer is 1.0 / 0.0).
    Noul { p_true: f64 },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    /// `level` is 1-based (`score + 1`); `probabilities` keyed by 1-based level.
    Score {
        level: f64,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

/// A decoded reply: one answer per asked question.
#[derive(Clone, Debug, PartialEq)]
pub struct LayaReply {
    /// The model Laya routed to (`laya-<model>-<version>`), when it said.
    pub model: Option<String>,
    pub answers: BTreeMap<String, LayaAnswer>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Why a call gave no answer. Never carries the URL, the key, the state or an upstream body.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayaError {
    #[error("laya did not answer in time")]
    Timeout,
    #[error("laya answered HTTP {0}")]
    Status(u16),
    #[error("laya answered an unreadable body: {0}")]
    BadBody(String),
    #[error("laya is unreachable: {0}")]
    Unreachable(String),
}

impl LayaError {
    /// `timeout`, `status`, `bad_body`, `unreachable`.
    pub fn code(&self) -> &'static str {
        match self {
            LayaError::Timeout => "timeout",
            LayaError::Status(_) => "status",
            LayaError::BadBody(_) => "bad_body",
            LayaError::Unreachable(_) => "unreachable",
        }
    }
}

fn transport(e: reqwest::Error) -> LayaError {
    if e.is_timeout() {
        LayaError::Timeout
    } else {
        LayaError::Unreachable(e.without_url().to_string())
    }
}

/// A Laya endpoint with its key.
#[derive(Clone)]
pub struct LayaClient {
    http: reqwest::Client,
    url: Url,
    model: String,
    /// The bearer (a licence token or provider key). Never printed.
    key: Option<String>,
    timeout: Duration,
}

impl std::fmt::Debug for LayaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayaClient")
            .field("model", &self.model)
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl LayaClient {
    /// A client on `http` for `{base_url}/v1/systemone`.
    pub fn new(
        http: reqwest::Client,
        base_url: &str,
        model: &str,
        key: Option<String>,
    ) -> Result<Self, LayaError> {
        let url = Url::parse(&format!(
            "{}{SYSTEMONE_PATH}",
            base_url.trim_end_matches('/')
        ))
        .map_err(|_| LayaError::Unreachable("the Laya base URL is not a URL".into()))?;
        Ok(Self {
            http,
            url,
            model: model.to_owned(),
            key,
            timeout: LAYA_TIMEOUT,
        })
    }

    /// The client for a resolved `gates` provider of `tenant`: the internal one (config, `provider_id: None`) on a
    /// plain client without redirects, a tenant's own through netguard (`policy`).
    pub async fn for_resolved(
        r: &ResolvedLlm,
        policy: &Policy,
        tenant: Uuid,
    ) -> Result<Self, LayaError> {
        let target = format!("{}{SYSTEMONE_PATH}", r.base_url.trim_end_matches('/'));
        let http = if r.provider_id.is_none() {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(GATE_BUDGET)
                .build()
                .map_err(|_| LayaError::Unreachable("the HTTP client could not be built".into()))?
        } else {
            let url = netguard::check_url(&target, policy, tenant)
                .map_err(|e| LayaError::Unreachable(e.detail))?;
            netguard::pinned_client(&url, policy, tenant, GATE_BUDGET)
                .await
                .map_err(|e| LayaError::Unreachable(e.detail))?
        };
        Self::new(
            http,
            &r.base_url,
            &r.model,
            r.key.as_ref().map(|k| k.expose().to_owned()),
        )
    }

    /// The per-call timeout (default [`LAYA_TIMEOUT`]).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// The request this client would send.
    pub fn request(&self, state: Value, questions: &[&EffGateQuestion]) -> LayaRequest {
        request(&self.model, state, questions)
    }

    /// Ask `questions` about `state` in one call.
    pub async fn ask(
        &self,
        state: Value,
        questions: &[&EffGateQuestion],
    ) -> Result<LayaReply, LayaError> {
        let body = self.request(state, questions);
        let mut req = self
            .http
            .post(self.url.clone())
            .timeout(self.timeout)
            .json(&body);
        if let Some(k) = &self.key {
            req = req.bearer_auth(k);
        }
        let call = async {
            let resp = req.send().await.map_err(transport)?;
            let status = resp.status();
            if !status.is_success() {
                return Err(LayaError::Status(status.as_u16()));
            }
            resp.bytes().await.map_err(transport)
        };
        let bytes = tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| LayaError::Timeout)??;
        decode(&bytes, questions)
    }
}

fn prob(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
}

fn probabilities(a: &Map<String, Value>, qid: &str) -> Result<BTreeMap<String, f64>, LayaError> {
    match a.get("probabilities") {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                prob(v)
                    .map(|p| (k.clone(), p))
                    .ok_or_else(|| LayaError::BadBody(format!("{qid}: a probability is not 0..1")))
            })
            .collect(),
        Some(_) => Err(LayaError::BadBody(format!(
            "{qid}: probabilities is not an object"
        ))),
    }
}

fn confidence(a: &Map<String, Value>, qid: &str) -> Result<f64, LayaError> {
    a.get("confidence")
        .and_then(prob)
        .ok_or_else(|| LayaError::BadBody(format!("{qid}: confidence is missing or not 0..1")))
}

/// Decode a reply body for the asked `questions`: every asked question must be answered in its own type.
pub fn decode(body: &[u8], questions: &[&EffGateQuestion]) -> Result<LayaReply, LayaError> {
    let v: Value =
        serde_json::from_slice(body).map_err(|_| LayaError::BadBody("not JSON".into()))?;
    let answers = v
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| LayaError::BadBody("no answers object".into()))?;
    let mut out = BTreeMap::new();
    for q in questions {
        let qid = q.key.as_str();
        let a = answers
            .get(qid)
            .and_then(Value::as_object)
            .ok_or_else(|| LayaError::BadBody(format!("no answer for {qid}")))?;
        let answer = match q.kind {
            GateType::Noul => {
                let p_true = match a.get("noul") {
                    Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
                    Some(n) => prob(n),
                    None => None,
                }
                .ok_or_else(|| LayaError::BadBody(format!("{qid}: noul is not a bool or 0..1")))?;
                LayaAnswer::Noul { p_true }
            }
            GateType::Choice => {
                let choice = a
                    .get("choice")
                    .and_then(Value::as_str)
                    .filter(|c| q.options.iter().any(|o| !o.hidden && o.key == *c))
                    .ok_or_else(|| {
                        LayaError::BadBody(format!("{qid}: choice is not an asked option"))
                    })?
                    .to_owned();
                LayaAnswer::Choice {
                    choice,
                    probabilities: probabilities(a, qid)?,
                    confidence: confidence(a, qid)?,
                }
            }
            GateType::Score => {
                let n = q.options.len() as f64;
                let score = a
                    .get("score")
                    .and_then(Value::as_f64)
                    .filter(|s| s.is_finite() && *s >= 0.0 && *s <= (n - 1.0).max(0.0))
                    .ok_or_else(|| LayaError::BadBody(format!("{qid}: score is not a level")))?;
                let mut probs = BTreeMap::new();
                for (k, p) in probabilities(a, qid)? {
                    let i: usize = k.parse().map_err(|_| {
                        LayaError::BadBody(format!("{qid}: a score key is not a level"))
                    })?;
                    probs.insert((i + 1).to_string(), p);
                }
                LayaAnswer::Score {
                    level: score + 1.0,
                    probabilities: probs,
                    confidence: confidence(a, qid)?,
                }
            }
        };
        out.insert(q.key.clone(), answer);
    }
    let usage = v.get("usage");
    let tokens = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Ok(LayaReply {
        model: v.get("model").and_then(Value::as_str).map(str::to_owned),
        answers: out,
        input_tokens: tokens("input_tokens"),
        output_tokens: tokens("output_tokens"),
    })
}
