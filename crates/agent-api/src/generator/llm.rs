//! B8 T8: the generator's LLM drafter (D89, D70). A [`Drafter`] makes one drafting call; [`draft`] runs a step
//! (the journey step = three samples in parallel, then one merge call, from P8 T4) under the step budget
//! (`GENERATOR_DRAFT_TIMEOUT_SECS`, B8-5). Every call returns its usage so the caller meters each one
//! (crate::generator::usage). A step that fails, times out or answers malformed JSON is 503 `unavailable` and
//! nothing is stored.
//!
//! Providers: a platform run calls the platform's provider record (`GENERATOR_PROVIDER_ID`, a B3 record of the
//! platform-owned tenant; funding follows the record: `local_gpu` for a platform-internal / private endpoint, else
//! `platform_external`). A tenant run calls the tenant's `generator` use (funding `tenant_byo`) through the netguard,
//! its key resolved per step and held in memory only.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::future::{BoxFuture, join_all};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::GeneratorConfig;
use super::prompts::{self, Call};
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::journey::model::GeneratorStepKey;
use crate::netguard;
use crate::providers::llm::{self, Kind, ResolvedLlm, Use};
use crate::state::AppState;
use crate::usage::LlmUsage;

/// Answer budget of one drafting call.
const MAX_TOKENS: u64 = 8192;
/// The journey step's parallel samples (P8 T4).
pub const JOURNEY_SAMPLES: usize = 3;
/// A title longer than this is a malformed draft.
const MAX_TITLE: usize = 200;
/// A hint longer than this is cut (F-54).
pub const MAX_HINT: usize = 2000;

/// What a step drafts from (the user message of every call of the step).
#[derive(Clone, Debug, Default, Serialize)]
pub struct StepContext {
    pub industry: String,
    pub department: String,
    pub job: String,
    pub description: String,
    /// The person's hint for this draft (F-54), at most [`MAX_HINT`] characters.
    pub hint: Option<String>,
    pub canonical_objects: Vec<String>,
    pub canonical_operations: Vec<String>,
    /// The stages in force (B8-10: an open set).
    pub stages: Vec<String>,
    /// What earlier steps accepted (typed drafts), for this step to build on.
    pub accepted: Value,
}

/// One drafted item (becomes a `generator_item` row).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DraftItem {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub example: Option<Value>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub new_object: Option<String>,
    #[serde(default)]
    pub confidence: f32,
}

/// One metered call of a step.
#[derive(Clone, Debug, PartialEq)]
pub struct Metered {
    pub call: Call,
    pub usage: LlmUsage,
}

/// A step's draft: its items and every call it took.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub items: Vec<DraftItem>,
    pub calls: Vec<Metered>,
}

impl Draft {
    pub fn tokens_in(&self) -> i64 {
        self.calls.iter().map(|c| c.usage.tokens_in).sum()
    }
    pub fn tokens_out(&self) -> i64 {
        self.calls.iter().map(|c| c.usage.tokens_out).sum()
    }
}

/// Why a drafting call produced nothing usable.
#[derive(Debug, PartialEq, Eq)]
pub enum DraftError {
    /// Unreachable, a non-2xx status, or an unreadable body.
    Unreachable(&'static str),
    /// The answer is not `{"items": [..]}` with valid items.
    Malformed(String),
    /// Over `GENERATOR_DRAFT_TIMEOUT_SECS`.
    Timeout,
}

impl From<DraftError> for ApiError {
    fn from(e: DraftError) -> Self {
        let msg = match e {
            DraftError::Unreachable(_) => "The drafting model did not answer. Try again.",
            DraftError::Malformed(_) => {
                "The drafting model answered something unusable. Try again."
            }
            DraftError::Timeout => "The drafting model took too long. Try again.",
        };
        ApiError::Unavailable("unavailable", msg.into(), None)
    }
}

/// One drafting call.
pub trait Drafter: Send + Sync {
    /// Call `call` with `input` as the user message; `sample` > 0 asks for a varied sample (temperature).
    fn call<'a>(
        &'a self,
        call: Call,
        input: &'a Value,
        sample: usize,
    ) -> BoxFuture<'a, Result<(Vec<DraftItem>, LlmUsage), DraftError>>;
}

/// Draft `step` within `budget`. Steps that do not draft (input, validate, package) draft nothing.
pub async fn draft(
    d: &dyn Drafter,
    step: GeneratorStepKey,
    ctx: &StepContext,
    budget: Duration,
) -> Result<Draft, DraftError> {
    match tokio::time::timeout(budget, draft_unbounded(d, step, ctx)).await {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(step = step.as_str(), "generator_draft_timeout");
            Err(DraftError::Timeout)
        }
    }
}

async fn draft_unbounded(
    d: &dyn Drafter,
    step: GeneratorStepKey,
    ctx: &StepContext,
) -> Result<Draft, DraftError> {
    if !step.drafts() {
        return Ok(Draft::default());
    }
    let mut ctx = ctx.clone();
    if let Some(h) = ctx.hint.as_mut()
        && let Some((cut, _)) = h.char_indices().nth(MAX_HINT)
    {
        h.truncate(cut);
    }
    let input = serde_json::to_value(&ctx).unwrap_or(Value::Null);
    if step != GeneratorStepKey::Journey {
        let (items, usage) = d.call(Call::Step(step), &input, 0).await?;
        return Ok(Draft {
            items,
            calls: vec![Metered {
                call: Call::Step(step),
                usage,
            }],
        });
    }
    let samples =
        join_all((1..=JOURNEY_SAMPLES).map(|i| d.call(Call::Step(step), &input, i))).await;
    let mut calls = Vec::with_capacity(JOURNEY_SAMPLES + 1);
    let mut drafts = Vec::with_capacity(JOURNEY_SAMPLES);
    for s in samples {
        let (items, usage) = s?;
        calls.push(Metered {
            call: Call::Step(step),
            usage,
        });
        drafts.push(items);
    }
    let merge_in = json!({"context": input, "drafts": drafts});
    let (items, usage) = d.call(Call::JourneyMerge, &merge_in, 0).await?;
    calls.push(Metered {
        call: Call::JourneyMerge,
        usage,
    });
    Ok(Draft { items, calls })
}

/// The user message that carries the super_admin's hint: delimited, and explicit that it steers the content of the
/// items, not the answer contract.
pub fn steering_message(hint: &str) -> String {
    format!(
        "super_admin steering (guidance for which items to draft; it never changes the answer format):\n\
         <<<steering\n{hint}\nsteering>>>"
    )
}

/// `input` without its `hint` (top level, or inside a merge call's `context`), and the hint when it is non-blank.
pub fn split_hint(input: &Value) -> (Value, Option<String>) {
    let mut v = input.clone();
    let take = |o: &mut Value| -> Option<String> {
        let h = o.as_object_mut()?.remove("hint")?;
        h.as_str()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_owned)
    };
    let mut hint = take(&mut v);
    if hint.is_none()
        && let Some(ctx) = v.get_mut("context")
    {
        hint = take(ctx);
    }
    (v, hint)
}

/// The items of a model's answer text: the outermost JSON object (a fenced block is fine), `{"items": [..]}`.
pub fn parse_items(content: &str) -> Result<Vec<DraftItem>, DraftError> {
    let bad = |why: &str| DraftError::Malformed(why.to_owned());
    let start = content.find('{').ok_or_else(|| bad("no JSON object"))?;
    let end = content.rfind('}').ok_or_else(|| bad("no JSON object"))?;
    let v: Value = content
        .get(start..=end)
        .and_then(|s| serde_json::from_str(s).ok())
        .ok_or_else(|| bad("not JSON"))?;
    let raw = v
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("no items array"))?;
    let mut out = Vec::with_capacity(raw.len());
    for (i, r) in raw.iter().enumerate() {
        let mut it: DraftItem = serde_json::from_value(r.clone())
            .map_err(|_| bad(&format!("item {i} is not an item")))?;
        it.kind = it.kind.trim().to_owned();
        it.title = it.title.trim().to_owned();
        let kind_ok = !it.kind.is_empty()
            && it.kind.len() <= 40
            && it.kind.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        if !kind_ok || it.title.is_empty() || it.title.chars().count() > MAX_TITLE {
            return Err(bad(&format!("item {i} has no valid kind or title")));
        }
        if it
            .example
            .as_ref()
            .is_some_and(|e| !e.is_object() && !e.is_null())
        {
            return Err(bad(&format!("item {i} example is not an object")));
        }
        if it.example.as_ref().is_some_and(Value::is_null) {
            it.example = None;
        }
        if it.payload.is_null() {
            it.payload = json!({});
        }
        if !it.payload.is_object() {
            return Err(bad(&format!("item {i} payload is not an object")));
        }
        it.new_object = it
            .new_object
            .map(|o| o.trim().to_owned())
            .filter(|o| !o.is_empty());
        it.confidence = if it.confidence.is_finite() {
            it.confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        out.push(it);
    }
    Ok(out)
}

// ---- the OpenAI-compatible drafter -------------------------------------------------------------------------------

/// Which provider a run drafts with, snapshotted at start (a tenant run: `funding = tenant_byo`). Never the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub provider_id: String,
    pub model: String,
    pub funding: String,
}

/// A chat-completions drafter. `Debug` never prints the key.
pub struct OpenAiDrafter {
    http: reqwest::Client,
    url: String,
    token: String,
    model: String,
    provider_id: String,
    funding: String,
}

impl std::fmt::Debug for OpenAiDrafter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiDrafter")
            .field("model", &self.model)
            .field("provider_id", &self.provider_id)
            .field("funding", &self.funding)
            .field("token", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// The funding lane of a platform provider record: its own GPU for an internal name, private or loopback endpoint,
/// else a commercial endpoint the platform pays for.
pub fn platform_funding(base_url: &str) -> &'static str {
    let host = reqwest::Url::parse(base_url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let private_ip = host
        .trim_matches(|c| c == '[' || c == ']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| match ip {
            std::net::IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.octets()[0] == 100,
            std::net::IpAddr::V6(v) => v.is_loopback(),
        });
    if host.ends_with(".internal") || host == "localhost" || private_ip {
        "local_gpu"
    } else {
        "platform_external"
    }
}

impl OpenAiDrafter {
    /// A drafter on a fixed endpoint (platform config, tests). `token` may be empty.
    pub fn direct(
        base_url: &str,
        token: &str,
        model: &str,
        provider_id: &str,
        funding: &str,
        timeout: Duration,
    ) -> ApiResult<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .map_err(|_| DraftError::Unreachable("client_build"))?;
        Ok(Self {
            http,
            url: base_url.trim().trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            model: model.to_owned(),
            provider_id: provider_id.to_owned(),
            funding: funding.to_owned(),
        })
    }

    /// What this drafter is, for the run's snapshot.
    pub fn snapshot(&self) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: self.provider_id.clone(),
            model: self.model.clone(),
            funding: self.funding.clone(),
        }
    }

    /// The platform's drafter (platform runs, discovery cluster drafting): `GENERATOR_PROVIDER_ID`, a provider
    /// record of the platform-owned tenant, read on `c` (scoped to that tenant). Not configured, or the record is
    /// gone: 503 `unavailable` "generator provider not configured".
    pub async fn platform(
        c: &mut PgConnection,
        st: &AppState,
        cfg: &GeneratorConfig,
    ) -> ApiResult<Self> {
        let missing = || {
            ApiError::Unavailable(
                "unavailable",
                "generator provider not configured".into(),
                None,
            )
        };
        let (Some(pid), Some(tenant)) = (cfg.provider_id, st.cfg.platform_tenant_id) else {
            return Err(missing());
        };
        let Some(r) = llm::resolve_by_id(c, st.provider_keyring.as_deref(), tenant, pid).await?
        else {
            tracing::warn!(provider = %pid, "generator_provider_missing: GENERATOR_PROVIDER_ID names no provider");
            return Err(missing());
        };
        if r.kind != Kind::Openai {
            return Err(missing());
        }
        let model = cfg.model.clone().unwrap_or_else(|| r.model.clone());
        let token = r
            .key
            .as_ref()
            .map(|k| k.expose().to_owned())
            .unwrap_or_default();
        Self::direct(
            &r.base_url,
            &token,
            &model,
            &pid.to_string(),
            platform_funding(&r.base_url),
            cfg.draft_timeout,
        )
    }

    /// A tenant run's drafter: the tenant's `generator` use of `job` (draft design, D89), through the netguard.
    /// Unbound, or bound to a provider without a key: 409 `generator_provider_missing`. Run on `c` scoped to `tenant`.
    pub async fn tenant(
        c: &mut PgConnection,
        st: &AppState,
        cfg: &GeneratorConfig,
        tenant: Uuid,
        job: &JobRef,
    ) -> ApiResult<Self> {
        let r = llm::resolve(c, st, tenant, job, Use::Generator, &DesignOf::Draft).await?;
        Self::tenant_from(st, cfg, tenant, r).await
    }

    async fn tenant_from(
        st: &AppState,
        cfg: &GeneratorConfig,
        tenant: Uuid,
        r: Option<ResolvedLlm>,
    ) -> ApiResult<Self> {
        let missing = || {
            ApiError::Conflict(
                "generator_provider_missing",
                "No provider is bound to the generator use: bind one on Providers.".into(),
            )
        };
        let Some(r) = r else { return Err(missing()) };
        let (Some(id), Some(key), Kind::Openai) = (r.provider_id, r.key.as_ref(), r.kind) else {
            return Err(missing());
        };
        let refused = |code: &'static str| {
            tracing::warn!(code, "generator_provider_refused");
            ApiError::Unavailable(
                "unavailable",
                "The drafting model is unreachable.".into(),
                None,
            )
        };
        let policy = &st.cfg.connector_policy;
        let checked =
            netguard::check_url(&r.base_url, policy, tenant).map_err(|e| refused(e.code))?;
        let http = netguard::pinned_client(&checked, policy, tenant, cfg.draft_timeout)
            .await
            .map_err(|e| refused(e.code))?;
        Ok(Self {
            http,
            url: r.base_url.trim().trim_end_matches('/').to_owned(),
            token: key.expose().to_owned(),
            model: r.model.clone(),
            provider_id: id.to_string(),
            funding: r.funding.to_owned(),
        })
    }

    /// The chat-completions body of one call. A super_admin hint (F-54) travels as its own delimited user message
    /// after the step context, never inside the system prompt or the context message.
    pub fn body(&self, call: Call, input: &Value, sample: usize) -> Value {
        let system = prompts::system(call).unwrap_or_default();
        let temperature = if sample == 0 { 0.2 } else { 0.8 };
        let (input, hint) = split_hint(input);
        let mut messages = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": input.to_string()}),
        ];
        if let Some(h) = hint {
            messages.push(json!({"role": "user", "content": steering_message(&h)}));
        }
        let mut b = json!({
            "model": self.model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": MAX_TOKENS,
            "response_format": {"type": "json_object"},
        });
        if self.funding == "local_gpu" {
            // the platform's own vLLM (Qwen3): thinking off, a vLLM extension a commercial endpoint may reject
            b["chat_template_kwargs"] = json!({"enable_thinking": false});
        }
        b
    }

    async fn call_once(
        &self,
        call: Call,
        input: &Value,
        sample: usize,
    ) -> Result<(Vec<DraftItem>, LlmUsage), DraftError> {
        let started = Instant::now();
        let mut usage = LlmUsage {
            provider_id: self.provider_id.clone(),
            model: self.model.clone(),
            funding: self.funding.clone(),
            ..LlmUsage::default()
        };
        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.url))
            .json(&self.body(call, input, sample));
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        let op = format!("generator.{}", call.use_label());
        let r = crate::observability::upstream_http("llm", op, req.send())
            .await
            .map_err(|_| DraftError::Unreachable("unreachable"))?;
        if !r.status().is_success() {
            tracing::warn!(
                status = r.status().as_u16(),
                call = call.use_label(),
                "generator_call_status"
            );
            return Err(DraftError::Unreachable("status"));
        }
        let v: Value = r
            .json()
            .await
            .map_err(|_| DraftError::Unreachable("body"))?;
        usage.tokens_in = v["usage"]["prompt_tokens"].as_i64().unwrap_or(0);
        usage.tokens_out = v["usage"]["completion_tokens"].as_i64().unwrap_or(0);
        usage.latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default();
        match parse_items(content) {
            Ok(items) => Ok((items, usage)),
            Err(e) => {
                // the call happened: say what it spent, though nothing is stored for a malformed step
                tracing::warn!(
                    call = call.use_label(), tokens_in = usage.tokens_in, tokens_out = usage.tokens_out,
                    error = ?e, "generator_draft_malformed"
                );
                Err(e)
            }
        }
    }
}

impl Drafter for OpenAiDrafter {
    fn call<'a>(
        &'a self,
        call: Call,
        input: &'a Value,
        sample: usize,
    ) -> BoxFuture<'a, Result<(Vec<DraftItem>, LlmUsage), DraftError>> {
        Box::pin(self.call_once(call, input, sample))
    }
}

// ---- the fixture drafter (tests, demos) ----------------------------------------------------------------------------

/// Answers every call from `<dir>/<use_label>.json` (`{"items": [..], "tokens_in"?, "tokens_out"?}`), with no
/// network. A missing or malformed file is the matching [`DraftError`].
#[derive(Clone, Debug)]
pub struct FixtureDrafter {
    pub dir: PathBuf,
    pub model: String,
    pub provider_id: String,
    pub funding: String,
}

impl FixtureDrafter {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            model: "fixture".into(),
            provider_id: "fixture".into(),
            funding: "local_gpu".into(),
        }
    }
}

impl Drafter for FixtureDrafter {
    fn call<'a>(
        &'a self,
        call: Call,
        _input: &'a Value,
        _sample: usize,
    ) -> BoxFuture<'a, Result<(Vec<DraftItem>, LlmUsage), DraftError>> {
        Box::pin(async move {
            let path = self.dir.join(format!("{}.json", call.use_label()));
            let text = tokio::fs::read_to_string(&path)
                .await
                .map_err(|_| DraftError::Unreachable("fixture_missing"))?;
            let items = parse_items(&text)?;
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let usage = LlmUsage {
                provider_id: self.provider_id.clone(),
                model: self.model.clone(),
                funding: self.funding.clone(),
                tokens_in: v["tokens_in"].as_i64().unwrap_or(0),
                tokens_out: v["tokens_out"].as_i64().unwrap_or(0),
                latency_ms: 0,
            };
            Ok((items, usage))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_items_accepts_a_fenced_answer() {
        let s = "```json\n{\"items\":[{\"kind\":\"activity\",\"title\":\" Track order \",\"payload\":null,\
                 \"example\":null,\"new_object\":\"\",\"confidence\":1.7}]}\n```";
        let items = parse_items(s).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Track order");
        assert_eq!(items[0].payload, json!({}));
        assert_eq!(items[0].new_object, None);
        assert!((items[0].confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_items_refuses_malformed() {
        for s in [
            "sorry, no",
            "{\"answer\": 1}",
            "{\"items\": [{\"kind\": \"activity\"}]}",
            "{\"items\": [{\"kind\": \"Bad Kind\", \"title\": \"x\"}]}",
            "{\"items\": [{\"kind\": \"activity\", \"title\": \"x\", \"example\": [1]}]}",
        ] {
            assert!(
                matches!(parse_items(s), Err(DraftError::Malformed(_))),
                "{s}"
            );
        }
    }

    #[test]
    fn platform_funding_follows_the_endpoint() {
        assert_eq!(
            platform_funding("http://vllm.internal:8001/v1"),
            "local_gpu"
        );
        assert_eq!(platform_funding("http://100.64.0.9:8001/v1"), "local_gpu");
        assert_eq!(
            platform_funding("https://api.openai.com/v1"),
            "platform_external"
        );
    }

    #[test]
    fn draft_error_is_503_unavailable() {
        for e in [
            DraftError::Timeout,
            DraftError::Unreachable("status"),
            DraftError::Malformed("x".into()),
        ] {
            assert!(matches!(
                ApiError::from(e),
                ApiError::Unavailable("unavailable", _, None)
            ));
        }
    }
}
