//! The qualifier (port of `cb:interaction.rs` + `cb:chat_q.rs`, B4 T9): one customer turn → one of the job's
//! context spaces and the slots it needs, through the job's `qualify` LLM binding (§0 R8:
//! `providers::llm::resolve(.., Use::Qualify, &of)` with the scope's design, `Live` for prod and `Draft` for test).
//!
//! - The model sees the customer's (already redacted) text, the open draft and the context spaces (key, label, slots)
//!   and answers JSON `{"context_space": "<key>" | null, "slots": {"<slot>": "<value>"}}`. Only a space of the job
//!   and only that space's slots count; a blank value is no value.
//! - [`step`] folds the extraction into the conversation's draft: no space → `Unclear`; a missing slot → `Ask` it;
//!   else `Ready` (the drain stores the case and publishes `request.qualified`). **System slots** (`order_id`,
//!   `payment_id`, `delivery_id`, `tracking_ref`; [`crate::runtime::resolve`]) are never asked and never kept: the
//!   orchestrator resolves them from the customer's own records. What the model extracted for one, or for
//!   `item_hint` ("the rug", "the one from 12 October"), is kept only as the transient hint, and only when it is
//!   not an id.
//! - Every model call is metered: the drain writes one `usage_event` row (0064) per call through
//!   `crate::usage::emit`, in its own transaction.
//!
//! No text is logged: only codes and statuses.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::netguard;
use crate::providers::llm::{Kind, ResolvedLlm};
use crate::release::targets::SETTING_KEYS;
use crate::runtime::domain::Slots;
use crate::runtime::prompt::SystemMessage;
use crate::runtime::resolve::{HINT_SLOT, clean_hint, is_system_slot};
use crate::runtime::spaces::{ContextSpace, SpaceCatalog};
use crate::state::AppState;
use crate::tenants::app_licence::Renewal;

/// One qualifier call's budget (connect + answer). Sized for ~7.5 tok/s decode (owner 2026-11-03): [`MAX_TOKENS`]
/// (2048) take ~273 s, so 600 s is ~2x headroom. [`crate::turn::LOCK_SECS`] must stay above two calls.
pub const LLM_TIMEOUT: Duration = Duration::from_secs(600);

/// Reasoning tokens count against this budget, so it is generous.
const MAX_TOKENS: u64 = 2048;

/// The platform's own model (Qwen3 on the pgx vLLM) answers the extraction with thinking off: the JSON is ~50
/// tokens, but its default thinking spent up to ~1,500 reasoning tokens per turn (~29 s on a follow-up turn, live
/// acceptance 2026-10-22), all counted against [`MAX_TOKENS`].
const INTERNAL_MAX_TOKENS: u64 = 512;

/// The `provider_id` of the platform's own model (a binding with no provider record).
pub const INTERNAL_PROVIDER: &str = "internal";

/// `b` for the platform's own model: Qwen3 thinking off (`chat_template_kwargs`, a vLLM extension a tenant's
/// OpenAI-compatible provider might reject, so only here) and the smaller answer budget.
pub fn for_internal(mut b: Value) -> Value {
    b["chat_template_kwargs"] = json!({"enable_thinking": false});
    b["max_tokens"] = json!(INTERNAL_MAX_TOKENS);
    b
}

/// The qualifier's `response_format` for the platform's own model: a `json_schema` (vLLM xgrammar) whose
/// `context_space` is one of the job's space keys or null. Under plain `json_object` the guided decoder produced
/// `{"": null, "slots": {}}` for prod (an empty key, so no space and the generic menu, 12/12 reproduced).
pub fn internal_response_format(spaces: &SpaceCatalog) -> Value {
    let mut keys: Vec<Value> = spaces.spaces().iter().map(|s| json!(s.key)).collect();
    keys.push(Value::Null);
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "qualify",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "context_space": {"enum": keys},
                    "slots": {"type": "object", "additionalProperties": {"type": "string"}},
                },
                "required": ["context_space", "slots"],
                "additionalProperties": false,
            },
        },
    })
}

/// [`for_internal`] for a qualifier body: also constrains the answer to `spaces` ([`internal_response_format`]).
pub fn for_internal_qualify(b: Value, spaces: &SpaceCatalog) -> Value {
    let mut b = for_internal(b);
    b["response_format"] = internal_response_format(spaces);
    b
}

/// The qualifier's task (`[task qualify]`), after the job's assembled layer sections (D108, KH T5).
const PREAMBLE: &str = "You are the intake step of a customer-care desk. From the customer's latest message and \
the draft request so far, pick the one context space (from `context_spaces`, by its `key`) the customer needs and \
extract any of that space's `slots` the message gives, each under its slot name. Never invent a value: leave a slot \
out when the message does not give it. Answer with JSON only: {\"context_space\": \"<key>\" or null, \"slots\": \
{\"<slot>\": \"<value>\"}}. Slots such as the order, payment or parcel are never asked of the customer and are not \
listed: when the customer points at one of their purchases by product or date (\"the rug\", \"my headphones\", \"the \
one from 12 October\", \"the first one\"), put those words under the slot name `item_hint`.";

/// What the qualifier read out of one turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extraction {
    /// A context-space key of the job, or `None`.
    pub context_space: Option<String>,
    pub slots: Slots,
}

/// A request still missing slots, kept between turns (Redis `chat:{conv}`, 7 d).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub context_space: String,
    pub slots: Slots,
    /// The slot the last turn asked for.
    #[serde(default)]
    pub awaiting: Option<String>,
    /// The customer message that started the request (its id, never its text).
    #[serde(default)]
    pub message_id: Option<String>,
}

/// Where a turn stands after qualifying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No context space of the job recognised.
    Unclear,
    /// The draft's space needs this slot next.
    Ask(String),
    /// Complete: publish this space with these slots.
    Ready(Draft),
}

/// Fold `ex` into `draft` against the job's `spaces`. Returns the outcome and the draft to keep (`None` = clear it).
///
/// A system slot is never asked ([`is_system_slot`]) and never kept; the words the model extracted for one, or for
/// [`HINT_SLOT`], are folded into the transient hint ([`clean_hint`]: an id typed anyway is dropped).
pub fn step(
    draft: Option<Draft>,
    ex: Extraction,
    spaces: &SpaceCatalog,
    message_id: &str,
) -> (Outcome, Option<Draft>) {
    let named = ex
        .context_space
        .as_deref()
        .and_then(|k| spaces.by_key(k))
        .map(|s| s.key.clone());
    let keep = draft.as_ref().filter(|d| {
        spaces.by_key(&d.context_space).is_some()
            && named.as_ref().is_none_or(|n| *n == d.context_space)
    });
    let mut d = match (keep, named) {
        (Some(d), _) => d.clone(),
        (None, Some(key)) => Draft {
            context_space: key,
            slots: Slots::new(),
            awaiting: None,
            message_id: Some(message_id.to_owned()),
        },
        (None, None) => return (Outcome::Unclear, None),
    };
    let Some(space) = spaces.by_key(&d.context_space) else {
        return (Outcome::Unclear, None);
    };
    fold(space, &mut d, ex.slots);
    match space
        .slots
        .iter()
        .find(|s| !never_asked(s) && !d.slots.contains_key(*s))
    {
        Some(missing) => {
            d.awaiting = Some(missing.clone());
            (Outcome::Ask(missing.clone()), Some(d))
        }
        None => (Outcome::Ready(d), None),
    }
}

/// Fold the extraction's `slots` into `draft` under `space`.
///
/// A slot keeps its extracted value only when it is a declared slot of `space` ([`ContextSpace::slots`]), is not a
/// system or lookup key ([`never_asked`]), is non-blank, and the draft does not hold a value for it yet: the earlier
/// turn's value wins. Everything that points at one thing in plain words ([`HINT_SLOT`], a system slot, a declared
/// lookup key) folds into the transient hint ([`clean_hint`]: an id typed anyway is dropped). Any other key is
/// dropped.
pub fn fold(space: &ContextSpace, draft: &mut Draft, slots: Slots) {
    let mut hints: Vec<String> = Vec::new();
    for (k, v) in slots {
        if k == HINT_SLOT
            || is_system_slot(&k)
            // a lookup key the space declares but no record kind resolves: the words still point, the key is not kept
            || (space.slots.contains(&k) && is_lookup_key(&k))
        {
            if let Some(h) = clean_hint(&v) {
                hints.push(h);
            }
        } else if space.slots.contains(&k) && !draft.slots.contains_key(&k) && !v.trim().is_empty()
        {
            draft.slots.insert(k, v.trim().to_owned());
        }
    }
    if !hints.is_empty() {
        draft.slots.insert(HINT_SLOT.to_owned(), hints.join(" "));
    }
}

/// Is `slot` never asked of a known customer: a system slot (retail, or a registered record kind) or any other lookup
/// key ([`is_lookup_key`]). An unregistered lookup key is not resolvable here: the orchestrator's `slots` step hands the
/// case over (`unresolvable_slot`) rather than asking.
pub fn never_asked(slot: &str) -> bool {
    is_system_slot(slot) || is_lookup_key(slot)
}

/// The job's ONLY context space, when it has exactly one. A message that names no space cannot be asking for a
/// different one, so in a one-space job it belongs to that space rather than the generic menu. `None` for a job
/// with zero or several spaces: there the menu is the honest answer.
///
/// Found live 2026-11-07: the devops job has one space and "please close the release readiness project you just set
/// up for us" was answered with "I can help with: project work. What do you need?" on one turn and routed correctly
/// on another — the qualifier model is a classifier, and a one-space job must not depend on it to find its only
/// space. The fallback is deterministic and cannot misroute: there is nothing else to choose.
pub fn sole_space(spaces: &SpaceCatalog) -> Option<&ContextSpace> {
    match spaces.spaces() {
        [only] => Some(only),
        _ => None,
    }
}

/// What the customer reads when no context space was recognised.
pub fn unclear(spaces: &SpaceCatalog) -> String {
    let names: Vec<String> = spaces
        .spaces()
        .iter()
        .map(|s| {
            if s.label.trim().is_empty() {
                s.space().replace(['_', '-'], " ")
            } else {
                s.label.to_lowercase()
            }
        })
        .collect();
    if names.is_empty() {
        "I can't take requests here yet. A colleague will follow up here.".into()
    } else {
        format!("I can help with: {}. What do you need?", names.join(", "))
    }
}

/// Is `slot` a lookup key of the customer (an email, phone, postcode, date of birth, or any `*_id` / `*_number` /
/// `*_ref` / `*_reference`; a promo or discount code is the customer's to give, not a lookup key)? The customer is
/// known (owner ruling 2026-10-26): [`question`] never asks for its value, only which thing it is, in plain words.
pub fn is_lookup_key(slot: &str) -> bool {
    let s = slot.trim().to_lowercase().replace('-', "_");
    const KEYS: [&str; 20] = [
        "email",
        "e_mail",
        "phone",
        "phone_number",
        "mobile",
        "postcode",
        "post_code",
        "zip",
        "zip_code",
        "date_of_birth",
        "dob",
        "birth_date",
        "msisdn",
        "iban",
        "imei",
        "imsi",
        "sim",
        "iccid",
        "sort_code",
        "mobile_number",
    ];
    KEYS.contains(&s.as_str())
        || ["_id", "_number", "_ref", "_reference"]
            .iter()
            .any(|suf| s.ends_with(suf))
        || s == "id"
}

/// The thing a lookup-key slot names (`policy_id` → "policy", `claim_number` → "claim"); `None` for a bare key
/// (`email`, `postcode`, `id`), which names no thing.
fn lookup_noun(slot: &str) -> Option<String> {
    let s = slot.trim().to_lowercase().replace('-', "_");
    let stem = ["_reference", "_number", "_ref", "_id"]
        .iter()
        .find_map(|suf| s.strip_suffix(suf))?;
    let noun = stem.replace('_', " ");
    (!noun.trim().is_empty()).then(|| noun.trim().to_owned())
}

/// The `reason` question for the request `space_key` names (by the key's last segment); neutral when unknown.
fn reason_question(space_key: &str) -> &'static str {
    let last = space_key
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        .replace('-', "_");
    match last.as_str() {
        "return" | "returns" => "What's the reason you'd like to return it?",
        "cancel" | "cancellation" | "policy_cancellation" | "contract_cancellation" => {
            "What's the reason you'd like to cancel?"
        }
        "change" | "modify" | "plan_change" | "policy_change" => {
            "What would you like to change, and why?"
        }
        "complaint" => "What happened?",
        "refund" => "What's the reason for the refund?",
        _ => "What's the reason?",
    }
}

/// The question asking for `slot` in the request `space_key` (a context-space key; empty/unknown = neutral wording).
pub fn question(slot: &str, space_key: &str) -> String {
    match slot {
        // a system slot is resolved from the customer's own records and never asked (`step` cannot produce it); the
        // line is only a defence, and it names no id
        s if is_system_slot(s) => match crate::runtime::resolve::record_kind(s) {
            Some(k) => format!("Which {} is this about?", k.noun),
            None => "Which item is this about?".into(),
        },
        // the `reason` slot is shared by returns, cancellations, HR and billing: the space key picks the wording
        "reason" => reason_question(space_key).into(),
        "defect" => "What exactly is wrong with it?".into(),
        // a lookup key a space still declares (`policy_id`, `claim_number`, `prescription_ref`): asked as which
        // thing, never for its number
        other if is_lookup_key(other) => match lookup_noun(other) {
            Some(noun) => format!("Which {noun} is this about?"),
            None => "Which item is this about?".into(),
        },
        other => format!("Could you tell me the {}?", other.replace(['_', '-'], " ")),
    }
}

// ---- the model call ---------------------------------------------------------------------------------------------

/// What one model call spent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmUsage {
    /// The provider row id, or `internal` for the platform's own model (D70).
    pub provider_id: String,
    pub model: String,
    /// `local_gpu` | `tenant_byo` (from the resolved binding).
    pub funding: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub latency_ms: i64,
}

/// A resolved `qualify` binding, ready to call. `Debug` never prints the key.
pub struct Model {
    http: reqwest::Client,
    url: String,
    token: String,
    model: String,
    provider_id: String,
    funding: String,
    /// The tenant the platform model bills (`X-Effective-Tenant-Id`); `None` for a tenant's own provider.
    effective_tenant: Option<Uuid>,
    /// Set only when `token` is the tenant's own minted bundle token ([`ResolvedLlm::tenant_licence`]) and the model
    /// was built with state ([`model`]): how a 401 re-mints it.
    renew: Option<Renewal>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("model", &self.model)
            .field("provider_id", &self.provider_id)
            .field("token", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// `r` as a callable model (`None`: not OpenAI-compatible, or a tenant provider without a key). The platform's own
/// model (config URL, not tenant input) is called directly; a tenant provider only through the netguard.
///
/// A tenant-licence key ([`ResolvedLlm::tenant_licence`]) is renewable here: [`extract`] re-mints it once on a 401.
/// [`model_in`] has no state, so its models never renew.
pub async fn model(st: &AppState, tenant: Uuid, r: ResolvedLlm) -> ApiResult<Option<Model>> {
    let app = r.tenant_licence;
    let mut m = model_in(&st.cfg.connector_policy, tenant, r).await?;
    if let (Some(m), Some(app)) = (m.as_mut(), app) {
        m.renew = Some(Renewal::new(st, tenant, app));
    }
    Ok(m)
}

/// [`model`] under the outbound `policy` (`CONNECTOR_PRIVATE_TARGETS`) a tenant provider is reached through.
pub async fn model_in(
    policy: &netguard::Policy,
    tenant: Uuid,
    r: ResolvedLlm,
) -> ApiResult<Option<Model>> {
    if r.kind != Kind::Openai {
        return Ok(None);
    }
    let url = r.base_url.trim_end_matches('/').to_string();
    let refused = |code: &'static str| {
        tracing::warn!(code, "qualify_provider_refused");
        ApiError::Unavailable(
            "llm_unavailable",
            "The qualifier model is unreachable.".into(),
            None,
        )
    };
    let (http, token, provider_id, effective_tenant) = match (r.provider_id, r.key) {
        (None, key) => {
            // the router identity (X-App-Id, X-Project-Id, X-Actor-Id) rides on every request of this client
            let http =
                crate::providers::llm::platform_client(LLM_TIMEOUT, r.effective_tenant, r.actor)
                    .map_err(|_| refused("client_build"))?;
            let token = key.map(|k| k.expose().to_string()).unwrap_or_default();
            (
                http,
                token,
                INTERNAL_PROVIDER.to_string(),
                r.effective_tenant,
            )
        }
        (Some(_), None) => return Ok(None),
        (Some(id), Some(key)) => {
            let checked = netguard::check_url(&url, policy, tenant).map_err(|e| refused(e.code))?;
            let http = netguard::pinned_client(&checked, policy, tenant, LLM_TIMEOUT)
                .await
                .map_err(|e| refused(e.code))?;
            (http, key.expose().to_string(), id.to_string(), None)
        }
    };
    Ok(Some(Model {
        http,
        url,
        token,
        model: r.model,
        provider_id,
        funding: r.funding.to_string(),
        effective_tenant,
        renew: None,
    }))
}

/// The prompt's view of the job's context spaces.
fn spaces_prompt(spaces: &SpaceCatalog) -> Value {
    spaces
        .spaces()
        .iter()
        .map(|s: &ContextSpace| {
            let asked: Vec<&String> = s.slots.iter().filter(|x| !never_asked(x)).collect();
            json!({"key": s.key, "label": s.label, "slots": asked})
        })
        .collect()
}

/// The job's own words for the qualifier beyond its prompt layers: its settings in force (`actor`, `stance` —
/// template base plus the job's override, crate::runtime::design::RuntimeDesign::settings, so a test-env draft edit
/// reaches the next turn), appended to the qualifier's task (see [`body`]); `None` when the job sets neither. The
/// job's `preamble` is not repeated here: it is the job layer's `identity` instruction (D108), which reaches the
/// message as that layer's section (crate::runtime::prompt::assemble).
pub fn job_context(settings: &BTreeMap<String, Value>) -> Option<String> {
    let lines: Vec<String> = SETTING_KEYS
        .iter()
        .filter_map(|k| {
            settings
                .get(*k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| format!("{k}: {v}"))
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "About this job (context only; the answer format above still applies):\n{}",
        lines.join("\n")
    ))
}

/// What each context space is for, in the qualifier's task (KH-F1): one line per space, `- <key>: <text>` with the
/// space's description, else its label ([`ContextSpace::qualifier_text`]), on one line; `None` without spaces.
pub fn spaces_context(spaces: &SpaceCatalog) -> Option<String> {
    let lines: Vec<String> = spaces
        .spaces()
        .iter()
        .map(|s| {
            let text = s
                .qualifier_text()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            format!("- {}: {text}", s.key)
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "When each context space applies (pick by these, not by the key's words):\n{}",
        lines.join("\n")
    ))
}

/// The qualifier's one system message: `prompt` (the job's assembled layers, crate::runtime::prompt::assemble) with
/// the task `[task qualify]` — [`PREAMBLE`], then the [`spaces_context`], then the [`job_context`].
pub fn system(
    prompt: &SystemMessage,
    spaces: &SpaceCatalog,
    settings: &BTreeMap<String, Value>,
) -> SystemMessage {
    let task = [
        Some(PREAMBLE.to_owned()),
        spaces_context(spaces),
        job_context(settings),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n");
    prompt.clone().with_task("qualify", &task)
}

/// The chat-completions body of one qualifier call.
pub fn body(
    model: &str,
    text: &str,
    draft: Option<&Draft>,
    spaces: &SpaceCatalog,
    settings: &BTreeMap<String, Value>,
    prompt: &SystemMessage,
) -> Value {
    let user = json!({
        "message": text,
        "draft": draft.map(|d| json!({"context_space": d.context_space, "slots": d.slots, "awaiting": d.awaiting})),
        "context_spaces": spaces_prompt(spaces),
    });
    // ONE system message, first: the internal model's chat template (Qwen3 on the pgx vLLM) rejects a system
    // message anywhere but at index 0 with 400 "System message must be at the beginning." — so the job's layers,
    // the task and the job context are sections of one message, never a second system message.
    let messages = [
        system(prompt, spaces, settings).message(),
        json!({"role": "user", "content": user.to_string()}),
    ];
    json!({
        "model": model,
        "messages": messages,
        "temperature": 0,
        "max_tokens": MAX_TOKENS,
        "response_format": {"type": "json_object"},
    })
}

/// The extraction in a model's answer text: the outermost JSON object in it (a fenced block is fine).
pub fn parse_answer(content: &str) -> Option<Extraction> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    let v: Value = serde_json::from_str(content.get(start..=end)?).ok()?;
    let context_space = v
        .get("context_space")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mut slots = BTreeMap::new();
    if let Some(m) = v.get("slots").and_then(Value::as_object) {
        for (k, val) in m {
            let s = match val {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            if !s.is_empty() {
                slots.insert(k.clone(), s);
            }
        }
    }
    Some(Extraction {
        context_space,
        slots,
    })
}

/// Whether a chat-completions response `resp` stopped at its token budget (`choices[0].finish_reason == "length"`):
/// its answer is cut off, so it reads as no context space unless this is logged.
pub fn truncated(resp: &Value) -> bool {
    resp["choices"][0]["finish_reason"].as_str() == Some("length")
}

/// What one chat-completions response `v` answered: the extraction, with `usage`'s token counts filled in (its
/// `provider_id`/`model` are the call's). A truncated answer is logged here and reported as `true`.
fn read_answer(
    v: &Value,
    usage: &mut LlmUsage,
    max_tokens: u64,
    message_id: &str,
) -> (Extraction, bool) {
    usage.tokens_in = v["usage"]["prompt_tokens"].as_i64().unwrap_or(0);
    usage.tokens_out = v["usage"]["completion_tokens"].as_i64().unwrap_or(0);
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    let cut = truncated(v);
    if cut {
        // ids and codes only (K50): never the customer's text or the model's answer
        tracing::warn!(
            max_tokens,
            provider_id = %usage.provider_id,
            model = %usage.model,
            message_id,
            tokens_out = usage.tokens_out,
            "qualify_truncated"
        );
    }
    // an unreadable answer is no space, not an outage: the customer is asked what they need
    let parsed = parse_answer(content);
    if !cut && parsed.as_ref().is_none_or(|e| e.context_space.is_none()) && !is_null_answer(content)
    {
        // model output, never the customer's message
        tracing::warn!(
            provider_id = %usage.provider_id,
            model = %usage.model,
            tokens_out = usage.tokens_out,
            raw_answer = %content.chars().take(200).collect::<String>(),
            "qualify_unclear"
        );
    }
    (parsed.unwrap_or_default(), cut)
}

/// Whether `content` is exactly a well-formed "no space" answer: `{"context_space": null, "slots": {..}}`.
fn is_null_answer(content: &str) -> bool {
    serde_json::from_str::<Value>(content.trim()).is_ok_and(|v| {
        v.get("context_space").is_some_and(Value::is_null)
            && v.get("slots").is_some_and(Value::is_object)
    })
}

/// One qualifier call. The usage is returned whether or not the answer was usable (the call happened); `Err` only
/// when the model could not be reached at all.
pub async fn extract(
    m: &Model,
    text: &str,
    draft: Option<&Draft>,
    spaces: &SpaceCatalog,
    settings: &BTreeMap<String, Value>,
    prompt: &SystemMessage,
) -> (ApiResult<Extraction>, LlmUsage) {
    let started = Instant::now();
    let mut usage = LlmUsage {
        provider_id: m.provider_id.clone(),
        model: m.model.clone(),
        funding: m.funding.clone(),
        ..LlmUsage::default()
    };
    let max_tokens = if m.provider_id == INTERNAL_PROVIDER {
        INTERNAL_MAX_TOKENS
    } else {
        MAX_TOKENS
    };
    // built once: a 401 on a tenant bundle token resends the same body under a fresh token
    let payload = {
        let b = body(&m.model, text, draft, spaces, settings, prompt);
        if m.provider_id == INTERNAL_PROVIDER {
            for_internal_qualify(b, spaces)
        } else {
            b
        }
    };
    let send = |token: &str| {
        let mut call = m
            .http
            .post(format!("{}/chat/completions", m.url))
            .json(&payload);
        if !token.is_empty() {
            call = call.bearer_auth(token);
        }
        if let Some(t) = m.effective_tenant {
            call = call.header(
                crate::providers::llm::EFFECTIVE_TENANT_HEADER,
                t.to_string(),
            );
        }
        crate::observability::upstream_http("llm", "qualify".into(), call.send())
    };
    let mut sent = send(&m.token).await;
    // The router refused the tenant's cached bundle token (rotated, revoked, expired early): drop it, mint once,
    // resend once. A second 401, a failed mint, or any other key (platform, BYO) takes the error path below.
    if let (Some(renew), Ok(r)) = (&m.renew, &sent)
        && r.status() == reqwest::StatusCode::UNAUTHORIZED
    {
        tracing::warn!(tenant = %renew.tenant(), "qualify_tenant_token_refused");
        if let Some(fresh) = renew.fresh().await {
            sent = send(&fresh).await;
        }
    }
    let failed = |what: &'static str| {
        tracing::warn!(what, "qualify_call_failed");
        ApiError::Unavailable(
            "llm_unavailable",
            "The qualifier model did not answer.".into(),
            None,
        )
    };
    let out = match sent {
        Err(_) => Err(failed("unreachable")),
        Ok(r) if !r.status().is_success() => {
            tracing::warn!(status = r.status().as_u16(), "qualify_status");
            Err(failed("status"))
        }
        Ok(r) => match r.json::<Value>().await {
            Err(_) => Err(failed("body")),
            Ok(v) => {
                let message_id = draft.and_then(|d| d.message_id.as_deref()).unwrap_or("");
                Ok(read_answer(&v, &mut usage, max_tokens, message_id).0)
            }
        },
    };
    usage.latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    (out, usage)
}

// ---- the visitor gate (B4 plan 4.2) -----------------------------------------------------------------------------

#[cfg(test)]
#[path = "qualify_effective_tenant_tests.rs"]
mod effective_tenant_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::resolve::{SYSTEM_SLOTS as SYSTEM_SLOT_NAMES, contains_id_pattern};

    fn spaces() -> SpaceCatalog {
        SpaceCatalog::new(vec![
            ContextSpace {
                key: "retail:order-support:returns".into(),
                label: "Returns".into(),
                description: String::new(),
                area: "order-support".into(),
                knowledge_kinds: vec![],
                slots: vec!["order_id".into(), "reason".into()],
                activity: String::new(),
            },
            ContextSpace {
                key: "retail:faq:shipping".into(),
                label: "Shipping questions".into(),
                description: "General questions about shipping,\n not about one order.".into(),
                area: "faq".into(),
                knowledge_kinds: vec![],
                slots: vec![],
                activity: String::new(),
            },
        ])
    }

    fn ex(space: Option<&str>, slots: &[(&str, &str)]) -> Extraction {
        Extraction {
            context_space: space.map(str::to_owned),
            slots: slots
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn unknown_space_is_unclear() {
        let (o, d) = step(None, ex(Some("retail:nope:x"), &[]), &spaces(), "m1");
        assert_eq!(o, Outcome::Unclear);
        assert!(d.is_none());
        assert!(unclear(&spaces()).contains("returns"));
    }

    #[test]
    fn a_one_space_job_has_a_deterministic_fallback_space() {
        // two spaces: the model must choose, so there is no fallback
        assert!(sole_space(&spaces()).is_none());
        assert!(sole_space(&SpaceCatalog::new(vec![])).is_none());
        let one = SpaceCatalog::new(vec![ContextSpace {
            key: "devops:delivery:project_work".into(),
            label: "Project work".into(),
            description: "started, moved forward, or closed".into(),
            area: "delivery".into(),
            knowledge_kinds: vec![],
            slots: vec![],
            activity: "delivery:project_work".into(),
        }]);
        assert_eq!(
            sole_space(&one).map(|s| s.key.as_str()),
            Some("devops:delivery:project_work")
        );
    }

    #[test]
    fn missing_slot_is_asked_then_ready() {
        let s = spaces();
        // the model extracted an id for the system slot: it is dropped, only `reason` is asked
        let (o, d) = step(
            None,
            ex(
                Some("retail:order-support:returns"),
                &[("order_id", "ORD-1")],
            ),
            &s,
            "m1",
        );
        assert_eq!(o, Outcome::Ask("reason".into()));
        let d = d.expect("a draft is kept");
        assert_eq!(d.message_id.as_deref(), Some("m1"));
        assert!(d.slots.is_empty(), "an id is no hint: {:?}", d.slots);
        // the next turn answers the question; a slot of another space is ignored
        let (o, d2) = step(
            Some(d),
            ex(None, &[("reason", "too big"), ("x", "y")]),
            &s,
            "m2",
        );
        let Outcome::Ready(r) = o else {
            panic!("ready expected")
        };
        assert_eq!(r.slots.len(), 1);
        assert_eq!(r.slots.get("reason").map(String::as_str), Some("too big"));
        assert_eq!(r.message_id.as_deref(), Some("m1"));
        assert!(d2.is_none());
    }

    /// [`fold`]'s rule on its own: a declared slot is kept, a system slot's words become the hint (the key itself is
    /// never kept), and a key the space does not declare is dropped.
    #[test]
    fn fold_keeps_declared_slots_and_hints_the_rest() {
        let s = spaces();
        let space = s
            .by_key("retail:order-support:returns")
            .expect("the returns space");
        let mut d = Draft {
            context_space: space.key.clone(),
            slots: Slots::new(),
            awaiting: None,
            message_id: Some("m1".into()),
        };
        fold(
            space,
            &mut d,
            ex(
                None,
                &[
                    ("reason", "too big"),
                    ("order_id", "the one from 12 October"),
                    ("x", "y"),
                ],
            )
            .slots,
        );
        assert_eq!(d.slots.get("reason").map(String::as_str), Some("too big"));
        assert_eq!(
            d.slots.get(HINT_SLOT).map(String::as_str),
            Some("the one from 12 October"),
            "the system slot's words are the hint: {:?}",
            d.slots
        );
        assert!(!d.slots.contains_key("order_id"), "{:?}", d.slots);
        assert!(!d.slots.contains_key("x"), "{:?}", d.slots);
        assert_eq!(d.message_id.as_deref(), Some("m1"), "nothing else moves");
    }

    /// A value the draft already holds (an earlier turn's answer) is never overwritten by a later extraction.
    #[test]
    fn fold_never_overwrites_a_kept_slot() {
        let s = spaces();
        let space = s
            .by_key("retail:order-support:returns")
            .expect("the returns space");
        let mut d = Draft {
            context_space: space.key.clone(),
            slots: Slots::from([("reason".to_owned(), "too small".to_owned())]),
            awaiting: Some("reason".into()),
            message_id: Some("m1".into()),
        };
        fold(space, &mut d, ex(None, &[("reason", "too big")]).slots);
        assert_eq!(
            d.slots.get("reason").map(String::as_str),
            Some("too small"),
            "the kept value wins: {:?}",
            d.slots
        );
    }

    #[test]
    fn a_space_without_slots_is_ready_at_once() {
        let (o, _) = step(None, ex(Some("retail:faq:shipping"), &[]), &spaces(), "m1");
        assert!(matches!(o, Outcome::Ready(_)));
    }

    const TRACK: &str = "retail:logistics:track_parcel";
    const STATUS: &str = "retail:order-support:order_status";

    fn retail() -> SpaceCatalog {
        let space = |key: &str, area: &str, slot: &str| ContextSpace {
            key: key.into(),
            label: String::new(),
            description: String::new(),
            area: area.into(),
            knowledge_kinds: vec![],
            slots: vec![slot.into()],
            activity: String::new(),
        };
        SpaceCatalog::new(vec![
            space(TRACK, "logistics", "tracking_ref"),
            space(STATUS, "order-support", "order_id"),
            ContextSpace {
                key: "retail:faq:shipping".into(),
                label: String::new(),
                description: String::new(),
                area: "faq".into(),
                knowledge_kinds: vec![],
                slots: vec![],
                activity: String::new(),
            },
        ])
    }

    /// Owner ruling 2026-10-26: a system slot is never asked, whatever the model extracted (or did not).
    #[test]
    fn a_system_slot_is_never_asked() {
        for (space, extracted) in [
            (TRACK, vec![]),
            (TRACK, vec![("tracking_ref", " ")]),
            (TRACK, vec![("order_id", "ORD-LATE-01")]),
            (STATUS, vec![]),
            (STATUS, vec![("order_id", "")]),
            (STATUS, vec![("order_id", "the mugs")]),
            (STATUS, vec![("payment_id", "PAY-1")]),
        ] {
            let (o, d) = step(None, ex(Some(space), &extracted), &retail(), "m1");
            assert!(
                matches!(o, Outcome::Ready(_)),
                "{space} {extracted:?}: {o:?}"
            );
            assert!(d.is_none());
        }
        // every slot of every space is either asked or a system slot, and `question` of a system slot names no id
        for slot in SYSTEM_SLOT_NAMES {
            assert!(!contains_id_pattern(&question(slot, "")), "{slot}");
            assert!(
                !question(slot, "").to_lowercase().contains("number"),
                "{slot}"
            );
        }
    }

    /// The words that point at an order are kept as the transient hint, an id typed anyway is not.
    #[test]
    fn what_points_at_an_order_is_kept_as_the_hint_and_an_id_is_not() {
        let hinted = |slots: &[(&str, &str)]| {
            let (o, _) = step(None, ex(Some(STATUS), slots), &retail(), "m1");
            let Outcome::Ready(r) = o else {
                panic!("ready expected: {o:?}")
            };
            r.slots
        };
        let h = hinted(&[("item_hint", "the rug")]);
        assert_eq!(h.get(HINT_SLOT).map(String::as_str), Some("the rug"));
        assert!(!h.contains_key("order_id"));
        let h = hinted(&[("order_id", "the one from 12 October")]);
        assert_eq!(
            h.get(HINT_SLOT).map(String::as_str),
            Some("the one from 12 October")
        );
        assert!(!h.contains_key("order_id"));
        assert!(hinted(&[("order_id", "RET-01")]).is_empty());
        assert!(hinted(&[("item_hint", "ORD-LATE-01 please")]).contains_key(HINT_SLOT));
        assert_eq!(
            hinted(&[("item_hint", "ORD-LATE-01 please")])[HINT_SLOT],
            "please"
        );
    }

    /// Owner ruling 2026-10-26: a lookup-key slot a space still declares is asked as which thing it is, in plain
    /// words; never for its id, number or reference.
    #[test]
    fn a_lookup_key_slot_is_asked_as_which_thing_never_for_its_number() {
        for k in [
            "email",
            "postcode",
            "date_of_birth",
            "account_number",
            "customer_id",
            "policy_ref",
            "zip-code",
            "msisdn",
            "iban",
            "imei",
            "sim",
            "iccid",
            "sort_code",
            "mobile_number",
        ] {
            assert!(is_lookup_key(k), "{k}");
        }
        for k in [
            "reason",
            "defect",
            "problem",
            "refund_type",
            "return_method",
            "promo_code",
        ] {
            assert!(!is_lookup_key(k), "{k}");
        }
        assert_eq!(question("policy_id", ""), "Which policy is this about?");
        assert_eq!(question("claim_number", ""), "Which claim is this about?");
        assert_eq!(
            question("prescription_ref", ""),
            "Which prescription is this about?"
        );
        assert_eq!(question("email", ""), "Which item is this about?");
        for k in [
            "account_id",
            "payment_ref",
            "case_id",
            "payslip_id",
            "appointment_ref",
            "email",
            "postcode",
        ] {
            let q = question(k, "").to_lowercase();
            for bad in [" id", "number", "reference", " ref", "email", "postcode"] {
                assert!(!q.contains(bad), "{k}: {q}");
            }
            assert!(!contains_id_pattern(&q), "{k}: {q}");
        }
    }

    fn space_with(slots: &[&str]) -> SpaceCatalog {
        SpaceCatalog::new(vec![ContextSpace {
            key: "ins:claims:status".into(),
            label: String::new(),
            description: String::new(),
            area: "claims".into(),
            knowledge_kinds: vec![],
            slots: slots.iter().map(|s| (*s).to_owned()).collect(),
            activity: String::new(),
        }])
    }

    /// Owner ruling 2026-10-26 (generic known customer): the lookup key of every registered record kind is a system
    /// slot, never asked, whatever the model extracted; the customer's words are kept as the hint.
    #[test]
    fn a_registered_record_slot_is_never_asked() {
        for k in crate::runtime::resolve::RECORD_KINDS {
            assert!(is_system_slot(k.slot), "{}", k.slot);
            assert!(never_asked(k.slot), "{}", k.slot);
            for extracted in [vec![], vec![(k.slot, " ")], vec![(k.slot, "my old one")]] {
                let (o, d) = step(
                    None,
                    ex(Some("ins:claims:status"), &extracted),
                    &space_with(&[k.slot]),
                    "m1",
                );
                let Outcome::Ready(r) = o else {
                    panic!("{}: ready expected, got {o:?}", k.slot)
                };
                assert!(d.is_none());
                assert!(!r.slots.contains_key(k.slot), "{}: kept", k.slot);
            }
            let q = question(k.slot, "");
            assert!(!contains_id_pattern(&q), "{q}");
            assert!(q.starts_with("Which ") && q.contains(k.noun), "{q}");
        }
        for slot in [
            "policy_id",
            "claim_number",
            "prescription_ref",
            "payslip_id",
            "account_ref",
            "campaign_ref",
        ] {
            let (o, _) = step(
                None,
                ex(Some("ins:claims:status"), &[]),
                &space_with(&[slot, "reason"]),
                "m1",
            );
            assert_eq!(
                o,
                Outcome::Ask("reason".into()),
                "{slot} is skipped, reason asked"
            );
        }
        let (o, _) = step(
            None,
            ex(
                Some("ins:claims:status"),
                &[("policy_id", "the car one, POL-0042")],
            ),
            &space_with(&["policy_id"]),
            "m1",
        );
        let Outcome::Ready(r) = o else {
            panic!("ready")
        };
        assert_eq!(r.slots[HINT_SLOT], "the car one,");
    }

    /// A lookup key no record kind resolves is still never asked (the slots step hands the case over).
    #[test]
    fn an_unregistered_lookup_slot_is_never_asked() {
        for slot in [
            "member_number",
            "customer_id",
            "email",
            "postcode",
            "badge_ref",
            "iban",
            "imei",
        ] {
            assert!(is_lookup_key(slot) && !is_system_slot(slot), "{slot}");
            let (o, d) = step(
                None,
                ex(Some("ins:claims:status"), &[(slot, "whatever")]),
                &space_with(&[slot]),
                "m1",
            );
            let Outcome::Ready(r) = o else {
                panic!("{slot}: ready expected, got {o:?}")
            };
            assert!(d.is_none());
            assert!(!r.slots.contains_key(slot), "{slot} must not be kept");
        }
        // an ordinary slot is still asked
        let (o, _) = step(
            None,
            ex(Some("ins:claims:status"), &[]),
            &space_with(&["member_number", "reason"]),
            "m1",
        );
        assert_eq!(o, Outcome::Ask("reason".into()));
        // the extraction prompt does not offer them
        let p = spaces_prompt(&space_with(&["policy_id", "member_number", "reason"]));
        assert_eq!(p[0]["slots"], json!(["reason"]));
    }

    /// Nothing the qualifier says to a customer carries an id (unclear menu, questions).
    #[test]
    fn qualifier_texts_carry_no_id() {
        for slot in [
            "reason",
            "defect",
            "problem",
            "refund_type",
            "return_method",
            "x_y",
        ] {
            let q = question(slot, "");
            assert!(!contains_id_pattern(&q), "{q}");
        }
        assert_eq!(question("reason", ""), "What's the reason?");
        for slot in ["msisdn", "iban", "imei", "reason", "defect", "x_y"] {
            assert!(
                !question(slot, "").to_lowercase().contains("email"),
                "{slot}"
            );
        }
        assert!(!question("msisdn", "").to_lowercase().contains("msisdn"));
    }

    /// The `reason` question follows the request; unknown spaces stay neutral; no wording carries an id or "email".
    #[test]
    fn reason_question_depends_on_the_request() {
        let cases = [
            (
                "retail:order-support:returns",
                "What's the reason you'd like to return it?",
            ),
            (
                "retail:order-support:return",
                "What's the reason you'd like to return it?",
            ),
            (
                "insurance:policy:cancel",
                "What's the reason you'd like to cancel?",
            ),
            (
                "insurance:policy:policy_cancellation",
                "What's the reason you'd like to cancel?",
            ),
            (
                "telecom:contract:contract-cancellation",
                "What's the reason you'd like to cancel?",
            ),
            (
                "telecom:plan:plan_change",
                "What would you like to change, and why?",
            ),
            (
                "insurance:policy:modify",
                "What would you like to change, and why?",
            ),
            (
                "insurance:policy:policy_change",
                "What would you like to change, and why?",
            ),
            ("retail:support:complaint", "What happened?"),
            ("retail:billing:refund", "What's the reason for the refund?"),
            ("retail:support:something_else", "What's the reason?"),
            ("", "What's the reason?"),
        ];
        for (key, want) in cases {
            let q = question("reason", key);
            assert_eq!(q, want, "{key}");
            assert!(!contains_id_pattern(&q), "{key}");
            assert!(!q.to_lowercase().contains("email"), "{key}");
            assert!(!q.to_lowercase().contains("number"), "{key}");
        }
        // other slots ignore the space
        assert_eq!(
            question("defect", "retail:order-support:returns"),
            "What exactly is wrong with it?"
        );
        assert!(
            !unclear(&SpaceCatalog::default())
                .to_lowercase()
                .contains("email")
        );
        assert!(!unclear(&spaces()).to_lowercase().contains("email"));
        assert!(!contains_id_pattern(&unclear(&spaces())));
        assert!(!contains_id_pattern(&unclear(&retail())));
    }

    #[test]
    fn a_partly_given_named_space_stays_and_asks_its_next_slot() {
        let mut spaces = retail().spaces().to_vec();
        spaces.push(ContextSpace {
            key: "retail:order-support:returns".into(),
            label: String::new(),
            description: String::new(),
            area: "order-support".into(),
            knowledge_kinds: vec![],
            slots: vec!["order_id".into(), "reason".into()],
            activity: String::new(),
        });
        let (o, d) = step(
            None,
            ex(
                Some("retail:order-support:returns"),
                &[("order_id", "ORD-1")],
            ),
            &SpaceCatalog::new(spaces),
            "m1",
        );
        assert_eq!(o, Outcome::Ask("reason".into()));
        assert_eq!(
            d.map(|d| d.context_space).as_deref(),
            Some("retail:order-support:returns")
        );
    }

    #[test]
    fn a_kept_draft_is_not_switched() {
        let draft = Draft {
            context_space: "retail:order-support:returns".into(),
            slots: Slots::new(),
            awaiting: Some("reason".into()),
            message_id: Some("m1".into()),
        };
        let mut spaces = retail().spaces().to_vec();
        spaces.push(ContextSpace {
            key: "retail:order-support:returns".into(),
            label: String::new(),
            description: String::new(),
            area: "order-support".into(),
            knowledge_kinds: vec![],
            slots: vec!["order_id".into(), "reason".into()],
            activity: String::new(),
        });
        let (o, d) = step(
            Some(draft),
            ex(None, &[("order_id", "the rug")]),
            &SpaceCatalog::new(spaces),
            "m2",
        );
        assert_eq!(o, Outcome::Ask("reason".into()));
        let d = d.expect("a draft is kept");
        assert_eq!(d.slots.get(HINT_SLOT).map(String::as_str), Some("the rug"));
        assert_eq!(d.context_space, "retail:order-support:returns");
        assert_eq!(d.message_id.as_deref(), Some("m1"));
    }

    #[test]
    fn the_internal_model_answers_with_thinking_off_and_a_small_budget() {
        let b = body(
            "m",
            "hi",
            None,
            &spaces(),
            &BTreeMap::new(),
            &SystemMessage::default(),
        );
        assert!(
            b.get("chat_template_kwargs").is_none(),
            "a tenant provider gets no vLLM extension"
        );
        assert_eq!(b["max_tokens"], MAX_TOKENS);
        let b = for_internal(b);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(b["max_tokens"], INTERNAL_MAX_TOKENS);
        assert_eq!(b["messages"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn the_internal_qualifier_body_is_constrained_to_the_spaces_and_a_byo_body_is_not() {
        let byo = body(
            "m",
            "hi",
            None,
            &spaces(),
            &BTreeMap::new(),
            &SystemMessage::default(),
        );
        assert_eq!(byo["response_format"], json!({"type": "json_object"}));
        let b = for_internal_qualify(byo, &spaces());
        let rf = &b["response_format"];
        assert_eq!(rf["type"], "json_schema");
        assert_eq!(rf["json_schema"]["name"], "qualify");
        assert_eq!(rf["json_schema"]["strict"], true);
        let schema = &rf["json_schema"]["schema"];
        let mut want: Vec<Value> = spaces().spaces().iter().map(|s| json!(s.key)).collect();
        want.push(Value::Null);
        assert_eq!(schema["properties"]["context_space"]["enum"], json!(want));
        assert_eq!(want.len(), spaces().spaces().len() + 1);
        assert_eq!(schema["required"], json!(["context_space", "slots"]));
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn an_empty_key_answer_is_unclear_and_logged_but_valid_answers_are_not() {
        let buf = Captured::default();
        let w = buf.clone();
        let sub = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || w.clone())
            .finish();
        let resp = |content: &str| {
            json!({"choices": [{"finish_reason": "stop", "message": {"content": content}}],
                   "usage": {"prompt_tokens": 7, "completion_tokens": 9}})
        };
        let mut usage = LlmUsage {
            provider_id: INTERNAL_PROVIDER.into(),
            model: "qwen".into(),
            ..LlmUsage::default()
        };
        let lines = |b: &Captured| String::from_utf8(b.0.lock().unwrap().clone()).unwrap();
        let (bad, after_bad, ok, none, after_all) = tracing::subscriber::with_default(sub, || {
            let bad = read_answer(
                &resp("{\"\": null, \"slots\": {}}"),
                &mut usage,
                INTERNAL_MAX_TOKENS,
                "m1",
            );
            let after_bad = lines(&buf);
            let ok = read_answer(
                &resp("{\"context_space\": \"retail:faq:shipping\", \"slots\": {}}"),
                &mut usage,
                INTERNAL_MAX_TOKENS,
                "m2",
            );
            let none = read_answer(
                &resp("{\"context_space\": null, \"slots\": {}}"),
                &mut usage,
                INTERNAL_MAX_TOKENS,
                "m3",
            );
            (bad, after_bad, ok, none, lines(&buf))
        });
        assert_eq!(bad.0, Extraction::default());
        assert!(after_bad.contains("qualify_unclear"), "{after_bad}");
        assert!(after_bad.contains("raw_answer"), "{after_bad}");
        assert_eq!(ok.0.context_space.as_deref(), Some("retail:faq:shipping"));
        assert_eq!(none.0, Extraction::default());
        assert_eq!(
            after_all.matches("qualify_unclear").count(),
            1,
            "a valid answer and a well-formed null answer are not logged: {after_all}"
        );
    }

    #[test]
    fn the_job_layers_task_and_settings_reach_one_system_message() {
        use crate::runtime::prompt::{Section, SectionKind};
        let layers = SystemMessage {
            sections: vec![Section {
                kind: SectionKind::Layer,
                marker: "[layer job/customer_care_agent@v4]".into(),
                text: "You help Demo Retail customers.".into(),
            }],
            warning: None,
        };
        let mut settings = BTreeMap::new();
        assert_eq!(job_context(&settings), None);
        let b = body("m", "hi", None, &spaces(), &settings, &layers);
        assert_eq!(b["messages"].as_array().map(Vec::len), Some(2));
        // the preamble is the identity layer's section, never a job-context line again
        settings.insert(
            "preamble".to_owned(),
            json!("You help Demo Retail customers."),
        );
        settings.insert("actor".to_owned(), json!("customer care"));
        settings.insert("stance".to_owned(), json!("  "));
        settings.insert("other".to_owned(), json!("not a job setting"));
        let b = body("m", "hi", None, &spaces(), &settings, &layers);
        let msgs = b["messages"].as_array().unwrap();
        // one system message, at index 0 (a later one is a 400 from the internal model's chat template)
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        let content = msgs[0]["content"].as_str().unwrap();
        let task = format!("[task qualify]\n{PREAMBLE}");
        assert!(
            content.starts_with(
                "[layer job/customer_care_agent@v4]\nYou help Demo Retail customers.\n\n[task qualify]\n"
            ),
            "{content}"
        );
        let job = &content[content.find(&task).expect("the task section") + task.len()..];
        assert!(job.contains("actor: customer care"), "{job}");
        assert!(
            !job.contains("preamble")
                && !job.contains("stance")
                && !job.contains("not a job setting"),
            "{job}"
        );
    }

    #[test]
    fn the_prompt_lists_no_system_slot_and_names_the_hint() {
        let b = body(
            "m",
            "hi",
            None,
            &spaces(),
            &BTreeMap::new(),
            &SystemMessage::default(),
        );
        let user: Value =
            serde_json::from_str(b["messages"][1]["content"].as_str().unwrap()).unwrap();
        let returns = user["context_spaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == "retail:order-support:returns")
            .expect("the returns space");
        assert_eq!(returns["slots"], json!(["reason"]));
        let content = b["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("item_hint"), "{content}");
    }

    #[test]
    fn the_task_shows_a_space_description_else_its_label() {
        // KH-F1: `retail:faq:shipping` has a description (shown, on one line), `returns` has none (its label)
        let b = body(
            "m",
            "hi",
            None,
            &spaces(),
            &BTreeMap::new(),
            &SystemMessage::default(),
        );
        let content = b["messages"][0]["content"].as_str().unwrap();
        let task = &content[content.find("[task qualify]\n").expect("the task section")..];
        assert!(
            task.contains(
                "\n- retail:faq:shipping: General questions about shipping, not about one order.\n"
            ),
            "{task}"
        );
        assert!(
            task.ends_with("\n- retail:order-support:returns: Returns"),
            "{task}"
        );
        assert!(
            !task.contains("Shipping questions"),
            "the description replaces the label: {task}"
        );
        // no spaces: no section
        assert_eq!(spaces_context(&SpaceCatalog::default()), None);
    }

    #[test]
    fn answers_parse_from_fenced_json_and_drop_blanks() {
        let e = parse_answer(
            "```json\n{\"context_space\": \"retail:faq:shipping\", \"slots\": {\"a\": \" \", \"b\": 5}}\n```",
        )
        .unwrap();
        assert_eq!(e.context_space.as_deref(), Some("retail:faq:shipping"));
        assert_eq!(e.slots.get("b").map(String::as_str), Some("5"));
        assert!(!e.slots.contains_key("a"));
        assert!(parse_answer("no json").is_none());
    }

    /// A log sink for one test's scoped subscriber.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_call_site_logs_a_truncated_answer_and_not_a_complete_one() {
        // Mutation: deleting the `if cut { tracing::warn!(.., "qualify_truncated") }` block of read_answer (the
        // place extract() reads every answer) fails the log assertions; ignoring finish_reason there (`cut = false`)
        // also fails the `truncated` flag assertion.
        let buf = Captured::default();
        let w = buf.clone();
        let sub = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || w.clone())
            .finish();
        let resp = |finish: &str, content: &str| {
            json!({"choices": [{"finish_reason": finish, "message": {"content": content}}],
                   "usage": {"prompt_tokens": 7, "completion_tokens": 512}})
        };
        let mut usage = LlmUsage {
            provider_id: INTERNAL_PROVIDER.into(),
            model: "qwen".into(),
            ..LlmUsage::default()
        };
        let lines = |b: &Captured| String::from_utf8(b.0.lock().unwrap().clone()).unwrap();
        let ((cut_e, cut), (full_e, full), after_cut) =
            tracing::subscriber::with_default(sub, || {
                let a = read_answer(
                    &resp("length", "{\"context_space\": \"retail:faq:shipp"),
                    &mut usage,
                    INTERNAL_MAX_TOKENS,
                    "msg-1",
                );
                let after_cut = lines(&buf);
                let b = read_answer(
                    &resp("stop", "{\"context_space\": \"retail:faq:shipping\"}"),
                    &mut usage,
                    INTERNAL_MAX_TOKENS,
                    "msg-2",
                );
                (a, b, after_cut)
            });
        assert!(cut);
        assert_eq!(
            cut_e,
            Extraction::default(),
            "a cut answer reads as no space"
        );
        assert!(after_cut.contains("qualify_truncated"), "{after_cut}");
        assert!(
            after_cut.contains("msg-1") && after_cut.contains("512"),
            "{after_cut}"
        );
        assert!(
            !after_cut.contains("retail:faq"),
            "never the model's answer: {after_cut}"
        );
        assert!(!full);
        assert_eq!(full_e.context_space.as_deref(), Some("retail:faq:shipping"));
        let all = lines(&buf);
        assert_eq!(all.matches("qualify_truncated").count(), 1, "{all}");
        assert!(!all.contains("msg-2"), "{all}");
        assert_eq!(usage.tokens_in, 7);
        assert_eq!(usage.tokens_out, 512);
    }

    #[test]
    fn a_completion_cut_at_its_token_budget_is_truncated() {
        let resp = |finish: Value| json!({"choices": [{"finish_reason": finish, "message": {}}]});
        assert!(truncated(&resp(json!("length"))));
        assert!(!truncated(&resp(json!("stop"))));
        assert!(!truncated(&resp(Value::Null)));
        assert!(!truncated(
            &json!({"choices": [{"message": {"content": "{}"}}]})
        ));
        assert!(!truncated(&json!({"choices": []})));
        assert!(!truncated(&json!({})));
    }
}
