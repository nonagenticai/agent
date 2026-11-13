//! The internal, channel-agnostic shape of one interaction: what every connector's request is translated into
//! ([`InteractionRequest`]) and what a backend streams back ([`BackendEvent`] / [`BackendError`]).
//!
//! Message text is customer data: it never reaches a log line, an audit detail or a metric label. Only sizes and
//! codes do.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;

/// Total characters of input (text parts, data parts as compact JSON, and the instructions); more is 413.
pub const MAX_INPUT_CHARS: usize = 32_000;
/// Messages in one request; more is 422 `too_many_messages`.
pub const MAX_MESSAGES: usize = 100;
/// Metadata bounds: keys, key length, value length (characters).
pub const MAX_METADATA_KEYS: usize = 16;
pub const MAX_METADATA_KEY: usize = 64;
pub const MAX_METADATA_VALUE: usize = 512;
/// A party id's length bound (characters).
pub const MAX_PARTY_ID: usize = 256;
/// The longest `conversation.id` (an external thread key, e.g. a card id).
pub const MAX_CONVERSATION_ID: usize = 200;
/// The channel when a connector names none.
pub const DEFAULT_CHANNEL: &str = "api";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One piece of a message. `Data` is structured input or output; until B4 it is forwarded to the LLM as compact JSON
/// text.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    Data(serde_json::Value),
}

impl Part {
    /// The part as the text a model reads (data as compact JSON).
    pub fn as_text(&self) -> String {
        match self {
            Part::Text(t) => t.clone(),
            Part::Data(v) => v.to_string(),
        }
    }

    fn chars(&self) -> usize {
        match self {
            Part::Text(t) => t.chars().count(),
            Part::Data(v) => v.to_string().chars().count(),
        }
    }

    fn is_blank(&self) -> bool {
        match self {
            Part::Text(t) => t.trim().is_empty(),
            Part::Data(v) => v.is_null(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![Part::Text(text.into())],
        }
    }

    /// Every part as text, joined by newlines.
    pub fn joined(&self) -> String {
        self.parts
            .iter()
            .map(Part::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Which side of a conversation a party speaks for (D104). `customer` (the conversation's end user) is the default;
/// `system` is a machine caller of the job (an API key: a board, a service; A4) and, like `customer`, may start a
/// conversation; `staff` (a human service worker answering after a handover) and `agent` address an existing
/// conversation, so they need a [`ConversationRef`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartyRole {
    #[default]
    Customer,
    Agent,
    Staff,
    System,
}

impl PartyRole {
    pub fn as_str(self) -> &'static str {
        match self {
            PartyRole::Customer => "customer",
            PartyRole::Agent => "agent",
            PartyRole::Staff => "staff",
            PartyRole::System => "system",
        }
    }
}

/// The opaque external identity a connector vouches for (a Telegram user, a caller id, an email sender).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Party {
    pub id: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub role: PartyRole,
}

impl Party {
    /// A customer party (the default role).
    pub fn customer(id: impl Into<String>, display_name: Option<String>) -> Self {
        Self {
            id: id.into(),
            display_name,
            role: PartyRole::Customer,
        }
    }

    /// A machine caller of the job (an API key, `api:<key id>`).
    pub fn system(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            role: PartyRole::System,
        }
    }
}

/// Where a conversation lives on its channel, so later messages (an answer delivered after the ack, a staff reply
/// after a handover, D104) reach it: the channel, the channel's own conversation id, and the connector's delivery
/// route (a Telegram chat id, a web chat session).
/// Route values are addresses, never message text. B11 phase B persists it (`0121_channel_conversation_refs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRef {
    pub channel: String,
    pub id: String,
    #[serde(default)]
    pub route: BTreeMap<String, String>,
}

/// The job an interaction is addressed to (from crate::tenancy::JobCtx, or an in-process connector's own lookup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
}

impl From<&crate::tenancy::JobCtx> for Scope {
    fn from(j: &crate::tenancy::JobCtx) -> Self {
        Self {
            tenant: j.t.tenant,
            job_id: j.job_id,
            job_key: j.job_key.clone(),
        }
    }
}

/// One user request, whatever channel it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct InteractionRequest {
    /// uuidv7, minted when the request is translated in.
    pub id: Uuid,
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
    /// `^[a-z][a-z0-9_]{0,31}$`, default `api`.
    pub channel: String,
    pub party: Option<Party>,
    /// The channel conversation this request belongs to (an in-process connector sets it; of the API shapes only
    /// Responses does, from `previous_response_id`).
    /// Absent = stateless.
    pub conversation: Option<ConversationRef>,
    pub instructions: Option<String>,
    pub input: Vec<Message>,
    /// At most 16 keys; a key at most 64 characters, a value at most 512.
    pub metadata: BTreeMap<String, String>,
}

impl InteractionRequest {
    /// A request on `scope` with a fresh v7 id, channel `api` and no party, instructions or metadata.
    pub fn new(scope: &Scope, input: Vec<Message>) -> Self {
        Self {
            id: Uuid::now_v7(),
            tenant: scope.tenant,
            job_id: scope.job_id,
            job_key: scope.job_key.clone(),
            channel: DEFAULT_CHANNEL.into(),
            party: None,
            conversation: None,
            instructions: None,
            input,
            metadata: BTreeMap::new(),
        }
    }

    /// The input limits every shape (and every in-process connector) applies before calling a backend: 422
    /// `empty_input`, `too_many_messages`, `invalid_channel`, `invalid_metadata`, `invalid_party`,
    /// `conversation_required` (a `staff` or `agent` party without a [`ConversationRef`], D104); 413 `input_too_large`.
    pub fn validate(&self) -> Result<(), ApiError> {
        if !valid_channel(&self.channel) {
            return Err(ApiError::Unprocessable(
                "invalid_channel",
                "channel must match ^[a-z][a-z0-9_]{0,31}$".into(),
            ));
        }
        validate_metadata(&self.metadata)?;
        if let Some(p) = &self.party {
            let id = p.id.trim();
            let name_ok = p.display_name.as_deref().is_none_or(|n| {
                n.chars().count() <= MAX_PARTY_ID && !n.chars().any(char::is_control)
            });
            if id.is_empty()
                || p.id.chars().count() > MAX_PARTY_ID
                || p.id.chars().any(char::is_control)
                || !name_ok
            {
                return Err(ApiError::Unprocessable(
                    "invalid_party",
                    format!(
                        "party.id must be 1 to {MAX_PARTY_ID} characters, display_name at most {MAX_PARTY_ID}"
                    ),
                ));
            }
            if matches!(p.role, PartyRole::Staff | PartyRole::Agent) && self.conversation.is_none()
            {
                return Err(ApiError::Unprocessable(
                    "conversation_required",
                    format!(
                        "a {} party answers an existing conversation; only a customer or a system caller may start one",
                        p.role.as_str()
                    ),
                ));
            }
        }
        if let Some(c) = &self.conversation
            && (c.id.trim().is_empty()
                || c.id.chars().count() > MAX_CONVERSATION_ID
                || c.id.chars().any(char::is_control))
        {
            return Err(ApiError::Unprocessable(
                "invalid_conversation",
                format!("conversation.id must be 1 to {MAX_CONVERSATION_ID} characters"),
            ));
        }
        if self.input.len() > MAX_MESSAGES {
            return Err(ApiError::Unprocessable(
                "too_many_messages",
                format!("input carries at most {MAX_MESSAGES} messages"),
            ));
        }
        if self.input.iter().flat_map(|m| &m.parts).all(Part::is_blank) {
            return Err(ApiError::Unprocessable(
                "empty_input",
                "input carries no text or data".into(),
            ));
        }
        let chars: usize = self
            .input
            .iter()
            .flat_map(|m| &m.parts)
            .map(Part::chars)
            .sum::<usize>()
            + self
                .instructions
                .as_deref()
                .map_or(0, |i| i.chars().count());
        if chars > MAX_INPUT_CHARS {
            return Err(ApiError::TooLarge(
                "input_too_large",
                format!("input and instructions carry at most {MAX_INPUT_CHARS} characters"),
            ));
        }
        Ok(())
    }
}

/// `^[a-z][a-z0-9_]{0,31}$`.
pub fn valid_channel(c: &str) -> bool {
    let mut it = c.chars();
    it.next().is_some_and(|f| f.is_ascii_lowercase())
        && c.len() <= 32
        && it.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_')
}

/// 422 `invalid_metadata` beyond 16 keys, an empty or over-64-character key, or an over-512-character value.
pub fn validate_metadata(m: &BTreeMap<String, String>) -> Result<(), ApiError> {
    let bad = m.len() > MAX_METADATA_KEYS
        || m.iter().any(|(k, v)| {
            k.is_empty()
                || k.chars().count() > MAX_METADATA_KEY
                || v.chars().count() > MAX_METADATA_VALUE
        });
    if bad {
        return Err(ApiError::Unprocessable(
            "invalid_metadata",
            format!(
                "metadata carries at most {MAX_METADATA_KEYS} keys of 1 to {MAX_METADATA_KEY} characters, \
                 values of at most {MAX_METADATA_VALUE}"
            ),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// What a backend streams: text deltas, then exactly one `Done` (or one error, and nothing after it).
#[derive(Clone, Debug, PartialEq)]
pub enum BackendEvent {
    Delta(String),
    Done { usage: Option<Usage> },
}

/// Why a backend could not answer. The text is operator-facing (a status, a transport error), never customer text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// 409 `provider_not_configured`: no LLM provider serves this tenant (no backend configured, or the tenant is not
    /// on the platform backend's allow-list).
    #[error("{0}")]
    NotConfigured(String),
    /// 503 `interaction_backend_unavailable`: the backend cannot be reached or timed out.
    #[error("{0}")]
    Unavailable(String),
    /// 502 `interaction_upstream_error`: the backend answered, but not with a usable stream.
    #[error("{0}")]
    Upstream(String),
    /// 409 `job_not_live` (D109, KH T6): a customer party's request for a job with no live release; the runtime
    /// answers customers only from a release, never a generic model reply.
    #[error("{0}")]
    NotLive(String),
}

impl BackendError {
    pub fn code(&self) -> &'static str {
        match self {
            BackendError::NotConfigured(_) => "provider_not_configured",
            BackendError::Unavailable(_) => "interaction_backend_unavailable",
            BackendError::Upstream(_) => "interaction_upstream_error",
            BackendError::NotLive(_) => "job_not_live",
        }
    }

    /// The client-facing message (the operator detail stays in the log line of the backend).
    pub fn public_message(&self) -> &'static str {
        match self {
            BackendError::NotConfigured(_) => "this tenant has no LLM provider configured",
            BackendError::Unavailable(_) => {
                "the assistant is not available right now; try again shortly"
            }
            BackendError::Upstream(_) => "the assistant could not answer this request",
            BackendError::NotLive(_) => {
                "this job has no live release yet; customers reach it once it is released"
            }
        }
    }
}

impl From<BackendError> for ApiError {
    fn from(e: BackendError) -> Self {
        match e {
            BackendError::NotConfigured(_) => {
                ApiError::Conflict(e.code(), e.public_message().into())
            }
            BackendError::Unavailable(_) => {
                ApiError::Unavailable(e.code(), e.public_message().into(), None)
            }
            BackendError::Upstream(_) => ApiError::BadGateway(e.code(), e.public_message().into()),
            // the runtime's own refusal text (it may name the template error for operators; never customer text)
            BackendError::NotLive(why) => ApiError::Conflict("job_not_live", why),
        }
    }
}

/// A drained stream (sync mode).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InteractionResult {
    pub text: String,
    pub usage: Option<Usage>,
    /// How the answer used knowledge, when the backend reports it ([`super::InteractionBackend::answer`]); a
    /// caller that stores the answer as an agent reply records it in that transaction
    /// (crate::knowledge::analytics::record_grounding, §0 R3). `None`: nothing reported, nothing recorded.
    pub grounding: Option<crate::knowledge::analytics::ReplyGrounding>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(input: Vec<Message>) -> InteractionRequest {
        InteractionRequest::new(
            &Scope {
                tenant: Uuid::nil(),
                job_id: Uuid::nil(),
                job_key: "faq".into(),
            },
            input,
        )
    }

    #[test]
    fn channels() {
        let longest = format!("a{}", "b".repeat(31));
        let too_long = format!("a{}", "b".repeat(32));
        for ok in ["api", "telegram", "a", "voice_2", longest.as_str()] {
            assert!(valid_channel(ok), "{ok}");
        }
        for bad in ["", "Api", "1a", "a-b", "_a", too_long.as_str()] {
            assert!(!valid_channel(bad), "{bad}");
        }
    }

    #[test]
    fn limits() {
        let r = req(vec![Message::text(Role::User, "hi")]);
        assert_eq!(r.id.get_version_num(), 7);
        assert!(r.validate().is_ok());
        let code = |r: &InteractionRequest| r.validate().unwrap_err().code();
        assert_eq!(code(&req(vec![])), "empty_input");
        assert_eq!(
            code(&req(vec![Message::text(Role::User, "  ")])),
            "empty_input"
        );
        let data = req(vec![Message {
            role: Role::User,
            parts: vec![Part::Data(serde_json::json!({"a": 1}))],
        }]);
        assert!(data.validate().is_ok(), "a data part is input");
        assert_eq!(
            code(&req(vec![Message::text(Role::User, "x"); 101])),
            "too_many_messages"
        );
        assert_eq!(
            code(&req(vec![Message::text(Role::User, "x".repeat(32_001))])),
            "input_too_large"
        );
        let mut r = req(vec![Message::text(Role::User, "x".repeat(31_999))]);
        r.instructions = Some("yy".into());
        assert_eq!(code(&r), "input_too_large", "instructions count");
        let mut r = req(vec![Message::text(Role::User, "x")]);
        r.channel = "Bad".into();
        assert_eq!(code(&r), "invalid_channel");
        let mut r = req(vec![Message::text(Role::User, "x")]);
        r.metadata = (0..17).map(|i| (format!("k{i}"), "v".into())).collect();
        assert_eq!(code(&r), "invalid_metadata");
        r.metadata = BTreeMap::from([("k".repeat(65), "v".into())]);
        assert_eq!(code(&r), "invalid_metadata");
        r.metadata = BTreeMap::from([("k".into(), "v".repeat(513))]);
        assert_eq!(code(&r), "invalid_metadata");
        r.metadata = BTreeMap::new();
        r.party = Some(Party::customer(" ", None));
        assert_eq!(code(&r), "invalid_party");
    }

    #[test]
    fn staff_and_agent_parties_need_a_conversation() {
        let mut r = req(vec![Message::text(Role::User, "x")]);
        for role in [PartyRole::Staff, PartyRole::Agent] {
            r.party = Some(Party {
                id: "w-1".into(),
                display_name: None,
                role,
            });
            r.conversation = None;
            assert_eq!(r.validate().unwrap_err().code(), "conversation_required");
            r.conversation = Some(ConversationRef {
                channel: "telegram".into(),
                id: "c-1".into(),
                route: BTreeMap::new(),
            });
            assert!(r.validate().is_ok());
        }
    }

    #[test]
    fn party_role_defaults_to_customer_and_rejects_unknown_roles() {
        let p: Party = serde_json::from_str(r#"{"id":"a"}"#).unwrap();
        assert_eq!(p.role, PartyRole::Customer);
        let p: Party = serde_json::from_str(r#"{"id":"a","role":"staff"}"#).unwrap();
        assert_eq!(p.role, PartyRole::Staff);
        assert!(serde_json::from_str::<Party>(r#"{"id":"a","role":"boss"}"#).is_err());
    }

    #[test]
    fn backend_errors_map_to_coded_api_errors() {
        let e: ApiError = BackendError::NotConfigured("x".into()).into();
        assert_eq!(e.code(), "provider_not_configured");
        assert_eq!(e.status_and_code().0.as_u16(), 409);
        let e: ApiError = BackendError::Unavailable("x".into()).into();
        assert_eq!(e.code(), "interaction_backend_unavailable");
        assert_eq!(e.status_and_code().0.as_u16(), 503);
        let e: ApiError = BackendError::Upstream("status 500".into()).into();
        assert_eq!(e.code(), "interaction_upstream_error");
        assert_eq!(e.status_and_code().0.as_u16(), 502);
        assert!(!e.to_string().contains("500"), "operator detail stays out");
        let e: ApiError = BackendError::NotLive("x".into()).into();
        assert_eq!(e.code(), "job_not_live");
        assert_eq!(e.status_and_code().0.as_u16(), 409);
    }
}
