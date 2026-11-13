//! The voice connector: one spoken turn with a job. The recording is transcribed (ASR), the transcript is answered
//! through the interaction API in process (channel `voice`, crate::interaction), and the answer is spoken (TTS).
//!
//! - [`SpeechServices`]: what transcribes and speaks; [`VoiceClient`] calls a speech gateway over HTTP in one of two
//!   wire contracts (crate::config::SpeechProviderKind: `w2v` or `openai_compatible`; [`asr`], [`tts`]).
//! - [`SpeechResolver`]: which speech services serve one job (async, per [`Scope`]). [`ProviderSpeech`] (B3 T4d,
//!   D101/D102) first tries [`PlatformSpeech`], which lends the env-configured gateway to the tenants in
//!   `VOICE_PLATFORM_TENANTS` only (owner test tenants); every other tenant brings its own gateways as B3 provider
//!   records bound to the uses `asr` and `tts` (crate::providers::llm, `w2v` or OpenAI-compatible), read from the
//!   job's LIVE design (D62/D69: a binding is a draft until the job publishes it). Both bound → one [`VoiceClient`]
//!   per use for the turn, each pinned to its provider's host (crate::netguard::pinned_client); either missing → 409
//!   `provider_not_configured`. Tests set a fake.
//! - [`routes`]: `POST /api/jobs/{job}/voice/turns` (multipart).
//!
//! The recording, the transcript and the answer are customer data: never in a log line, an audit detail or a metric
//! label. Only statuses and sizes are.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use uuid::Uuid;

use crate::config::{Config, SpeechProviderKind, VoiceConfig};
use crate::connectors::wire::OpError;
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::interaction::Scope;
use crate::netguard;
use crate::providers::kinds;
use crate::providers::llm::{self, ResolvedLlm, Use};
use crate::state::AppState;
use crate::tenancy::begin_scoped;

pub mod asr;
pub mod routes;
pub mod tts;

/// The channel every voice turn is recorded under.
pub const CHANNEL: &str = "voice";

/// One recording to transcribe.
#[derive(Clone, Debug)]
pub struct Clip {
    pub audio: Vec<u8>,
    /// The file extension the ASR service reads the container from (`webm`, `ogg`, `wav`, `m4a`).
    pub ext: &'static str,
    /// A language hint (BCP-47 primary tag), else the service detects it.
    pub language: Option<String>,
}

/// Which upstream failed (named in the client-facing message; never the user's text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Asr,
    Tts,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Asr => "asr",
            Stage::Tts => "tts",
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a speech upstream could not answer. The detail is operator-facing (a status), never customer text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VoiceError {
    /// 503 `voice_unavailable`: unconfigured, unreachable or timed out.
    #[error("{0} unavailable: {1}")]
    Unavailable(Stage, String),
    /// 502 `voice_upstream_error`: the service answered, but not with something usable.
    #[error("{0} upstream error: {1}")]
    Upstream(Stage, String),
}

impl VoiceError {
    pub fn code(&self) -> &'static str {
        match self {
            VoiceError::Unavailable(..) => "voice_unavailable",
            VoiceError::Upstream(..) => "voice_upstream_error",
        }
    }
}

impl From<VoiceError> for ApiError {
    fn from(e: VoiceError) -> Self {
        match &e {
            VoiceError::Unavailable(s, _) => ApiError::Unavailable(
                e.code(),
                format!(
                    "the voice service ({}) is not available right now; try again shortly",
                    s.as_str()
                ),
                None,
            ),
            VoiceError::Upstream(s, _) => ApiError::BadGateway(
                e.code(),
                format!(
                    "the voice service ({}) could not process this turn",
                    s.as_str()
                ),
            ),
        }
    }
}

/// What transcribes a recording and speaks an answer.
pub trait SpeechServices: Send + Sync {
    /// The transcript (possibly blank: no speech).
    fn transcribe(&self, clip: Clip) -> BoxFuture<'static, Result<String, VoiceError>>;
    /// WAV bytes of `text` spoken in `voice` (else the configured default).
    fn synthesize(
        &self,
        text: String,
        voice: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<u8>, VoiceError>>;
}

/// Which speech services serve one job (`None`: 409 `provider_not_configured`). Async and per job (B3 T4d): a
/// tenant's own gateways are the job's live `asr` / `tts` bindings.
pub trait SpeechResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        st: &'a AppState,
        scope: &'a Scope,
    ) -> BoxFuture<'a, ApiResult<Option<Arc<dyn SpeechServices>>>>;
}

/// The interim resolver: one platform gateway, lent to the allow-listed tenants only.
pub struct PlatformSpeech {
    services: Option<Arc<dyn SpeechServices>>,
    tenants: HashSet<uuid::Uuid>,
}

impl PlatformSpeech {
    pub fn new(
        services: Option<Arc<dyn SpeechServices>>,
        tenants: impl IntoIterator<Item = uuid::Uuid>,
    ) -> Self {
        Self {
            services,
            tenants: tenants.into_iter().collect(),
        }
    }
}

impl PlatformSpeech {
    /// The platform gateway when `tenant` is allow-listed and the gateway is configured.
    pub fn serves(&self, tenant: Uuid) -> Option<Arc<dyn SpeechServices>> {
        if self.tenants.contains(&tenant) {
            self.services.clone()
        } else {
            None
        }
    }
}

impl SpeechResolver for PlatformSpeech {
    fn resolve<'a>(
        &'a self,
        _st: &'a AppState,
        scope: &'a Scope,
    ) -> BoxFuture<'a, ApiResult<Option<Arc<dyn SpeechServices>>>> {
        let s = self.serves(scope.tenant);
        Box::pin(async move { Ok(s) })
    }
}

/// The bound on one whole call to a tenant's speech gateway when `VOICE_TIMEOUT_S` is not configured.
pub const PROVIDER_TIMEOUT_S: u64 = 60;

/// A voice name a `tts` binding or a turn may carry: 1..=[`routes::MAX_VOICE`] characters of `[A-Za-z0-9_-]`.
pub fn valid_voice(v: &str) -> bool {
    !v.is_empty()
        && v.chars().count() <= routes::MAX_VOICE
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The answer as plain text for TTS, so markdown symbols are not spoken: emphasis markers (`*`, `_` at a word edge,
/// `~~`) and backticks are dropped (their content kept), code fences are dropped (their lines kept), a link or image
/// keeps its text and loses its URL, headings, list items (`-`, `*`, `+`, `1.`, and a stray ` * ` mid-line) and
/// fence lines become sentences of their own, rules (`---`) vanish, and whitespace collapses. A paragraph's
/// wrapped lines stay one sentence, and plain text comes back as it went in. The text answer itself is never changed.
pub fn speakable(text: &str) -> String {
    let mut blocks: Vec<String> = Vec::new();
    // the last block is a paragraph that a following plain line continues
    let mut open = false;
    let mut fenced = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            open = false;
            continue;
        }
        if line.is_empty() || (!fenced && is_rule(line)) {
            open = false;
            continue;
        }
        if fenced {
            blocks.push(line.to_string());
            continue;
        }
        let (body, item) = block_body(line);
        for (n, seg) in body.split(" * ").enumerate() {
            let seg = inline_plain(seg);
            if seg.trim().is_empty() {
                continue;
            }
            let item = item || n > 0;
            match blocks.last_mut() {
                Some(last) if open && !item => {
                    last.push(' ');
                    last.push_str(&seg);
                }
                _ => blocks.push(seg),
            }
            open = !item;
        }
    }
    let mut out = String::new();
    for b in blocks {
        let b = b.split_whitespace().collect::<Vec<_>>().join(" ");
        if b.is_empty() {
            continue;
        }
        if !out.is_empty() {
            // a block that does not end a sentence is ended here, so the voice pauses between blocks
            if !out.ends_with(['.', '!', '?', ':', ';', ',']) {
                out.push('.');
            }
            out.push(' ');
        }
        out.push_str(&b);
    }
    out
}

/// `---`, `***`, `* * *`, `___`: a thematic break.
fn is_rule(line: &str) -> bool {
    let marks: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3 && ['-', '*', '_'].iter().any(|m| marks.iter().all(|c| c == m))
}

/// A line without its block marker (heading, quote, bullet, number) and whether it stands alone as a sentence.
fn block_body(line: &str) -> (&str, bool) {
    let h = line.trim_start_matches('#');
    if h.len() < line.len() && (h.is_empty() || h.starts_with(' ')) {
        return (h.trim(), true);
    }
    let line = line.trim_start_matches(['>', ' ']);
    for b in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = line.strip_prefix(b) {
            return (rest.trim(), true);
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return (rest.trim(), true);
        }
    }
    (line, false)
}

/// One line's inline markdown as plain text.
fn inline_plain(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' if c.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                out.push(c[i + 1]);
                i += 2;
                continue;
            }
            '[' => match link_at(&c, i) {
                Some((label, end)) => {
                    if i > 0 && c[i - 1] == '!' && out.ends_with('!') {
                        out.pop();
                    }
                    out.push_str(&inline_plain(&label));
                    i = end;
                    continue;
                }
                None => out.push('['),
            },
            '*' | '`' => {}
            '~' if c.get(i + 1) == Some(&'~') => {
                i += 2;
                continue;
            }
            '_' => {
                let inner = i > 0
                    && c[i - 1].is_alphanumeric()
                    && c.get(i + 1).is_some_and(|n| n.is_alphanumeric());
                if inner {
                    out.push('_');
                }
            }
            ch => out.push(ch),
        }
        i += 1;
    }
    out
}

/// `[label](target)` opening at `c[i] == '['`: the label and the index after the `)`.
fn link_at(c: &[char], i: usize) -> Option<(String, usize)> {
    let close = i + 1 + c[i + 1..].iter().position(|&x| x == ']')?;
    if c.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = close + 2 + c[close + 2..].iter().position(|&x| x == ')')?;
    Some((c[i + 1..close].iter().collect(), end + 1))
}

/// The B3 T4d resolver (D101/D102): the platform gateway for `VOICE_PLATFORM_TENANTS` (tried first), else the job's
/// own live `asr` + `tts` providers ([`provider_clients`]).
pub struct ProviderSpeech {
    platform: PlatformSpeech,
}

impl ProviderSpeech {
    pub fn new(platform: PlatformSpeech) -> Self {
        Self { platform }
    }
}

impl SpeechResolver for ProviderSpeech {
    fn resolve<'a>(
        &'a self,
        st: &'a AppState,
        scope: &'a Scope,
    ) -> BoxFuture<'a, ApiResult<Option<Arc<dyn SpeechServices>>>> {
        Box::pin(async move {
            if let Some(s) = self.platform.serves(scope.tenant) {
                return Ok(Some(s));
            }
            Ok(provider_clients(st, scope)
                .await?
                .map(|(asr, tts)| Arc::new(ProviderVoice { asr, tts }) as Arc<dyn SpeechServices>))
        })
    }
}

/// A tenant's speech for one turn: ASR from the `asr` provider, TTS from the `tts` provider (each its own host, key
/// and contract).
#[derive(Debug)]
struct ProviderVoice {
    asr: VoiceClient,
    tts: VoiceClient,
}

impl SpeechServices for ProviderVoice {
    fn transcribe(&self, clip: Clip) -> BoxFuture<'static, Result<String, VoiceError>> {
        self.asr.transcribe(clip)
    }

    fn synthesize(
        &self,
        text: String,
        voice: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<u8>, VoiceError>> {
        self.tts.synthesize(text, voice)
    }
}

/// The job's own speech gateways: its LIVE `asr` and `tts` bindings (crate::providers::llm::resolve,
/// `DesignOf::Live`), as `(asr client, tts client)`. `None` when either use is unbound, its provider is gone, has no
/// stored key or is not a speech kind (the caller answers 409 `provider_not_configured`). Each client is pinned to
/// its provider's host by crate::netguard (one lookup, every answer public, no redirects); a host that no longer
/// passes is 503 `voice_unavailable` (only the netguard code is logged, never the host, key or URL).
pub async fn provider_clients(
    st: &AppState,
    scope: &Scope,
) -> ApiResult<Option<(VoiceClient, VoiceClient)>> {
    let job = JobRef {
        id: scope.job_id,
        key: scope.job_key.clone(),
    };
    let (asr, tts) = {
        let mut tx = begin_scoped(&st.db, scope.tenant).await?;
        let asr = llm::resolve(&mut tx, st, scope.tenant, &job, Use::Asr, &DesignOf::Live).await?;
        let tts = llm::resolve(&mut tx, st, scope.tenant, &job, Use::Tts, &DesignOf::Live).await?;
        tx.commit().await?;
        (asr, tts)
    };
    let (Some(asr), Some(tts)) = (asr, tts) else {
        return Ok(None);
    };
    let Some(asr) = provider_client(st, scope.tenant, &asr, Stage::Asr).await? else {
        return Ok(None);
    };
    let Some(tts) = provider_client(st, scope.tenant, &tts, Stage::Tts).await? else {
        return Ok(None);
    };
    Ok(Some((asr, tts)))
}

/// A client for one resolved speech provider (`None`: not usable as one — see [`provider_clients`]).
async fn provider_client(
    st: &AppState,
    tenant: Uuid,
    r: &ResolvedLlm,
    stage: Stage,
) -> ApiResult<Option<VoiceClient>> {
    let Some(kind) = kinds::speech_contract(r.kind) else {
        return Ok(None);
    };
    // the platform tenant's speech is the platform gateway (D102): no internal speech provider resolves here
    if r.provider_id.is_none() {
        return Ok(None);
    }
    let Some(key) = r.key.as_ref() else {
        return Ok(None);
    };
    let timeout_s = st
        .cfg
        .voice
        .as_ref()
        .map_or(PROVIDER_TIMEOUT_S, |v| v.timeout_s);
    let policy = st.connectors.policy();
    let base = r.base_url.trim_end_matches('/').to_string();
    let url = netguard::check_url(&base, policy, tenant).map_err(|e| refused(stage, &e))?;
    let http = netguard::pinned_client(&url, policy, tenant, Duration::from_secs(timeout_s))
        .await
        .map_err(|e| refused(stage, &e))?;
    Ok(Some(VoiceClient {
        http,
        kind,
        asr_url: base.clone(),
        tts_url: base,
        token: key.expose().to_string(),
        asr_model: r.model.clone(),
        tts_model: r.model.clone(),
        tts_voice: r
            .voice
            .clone()
            .unwrap_or_else(|| kinds::default_voice(kind).to_string()),
    }))
}

/// A tenant's speech host that netguard refuses now (or does not resolve): 503 `voice_unavailable`.
fn refused(stage: Stage, e: &OpError) -> ApiError {
    tracing::warn!(
        stage = stage.as_str(),
        code = e.code,
        "voice_provider_refused"
    );
    VoiceError::Unavailable(stage, "unreachable".into()).into()
}

/// A speech gateway over HTTP: ASR and TTS in the contract of `kind`.
#[derive(Clone)]
pub struct VoiceClient {
    /// A client of its own: the shared one's 10 s timeout would cut a long transcription.
    pub http: reqwest::Client,
    pub kind: SpeechProviderKind,
    pub asr_url: String,
    pub tts_url: String,
    pub token: String,
    /// openai_compatible only.
    pub asr_model: String,
    /// openai_compatible only.
    pub tts_model: String,
    pub tts_voice: String,
}

impl std::fmt::Debug for VoiceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceClient")
            .field("kind", &self.kind)
            .field("asr_url", &self.asr_url)
            .field("tts_url", &self.tts_url)
            .field("token", &"[redacted]")
            .field("tts_voice", &self.tts_voice)
            .finish()
    }
}

impl VoiceClient {
    /// From the config: a client bounded by `VOICE_TIMEOUT_S` (connect: 5 s).
    pub fn from_config(c: &VoiceConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(c.timeout_s))
            .build()?;
        Ok(Self {
            http,
            kind: c.kind,
            asr_url: c.asr_url.clone(),
            tts_url: c.tts_url.clone(),
            token: c.token.clone(),
            asr_model: c.asr_model.clone(),
            tts_model: c.tts_model.clone(),
            tts_voice: c.tts_voice.clone(),
        })
    }
}

impl SpeechServices for VoiceClient {
    fn transcribe(&self, clip: Clip) -> BoxFuture<'static, Result<String, VoiceError>> {
        let me = self.clone();
        Box::pin(async move { asr::transcribe(&me, clip).await })
    }

    fn synthesize(
        &self,
        text: String,
        voice: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<u8>, VoiceError>> {
        let me = self.clone();
        Box::pin(async move { tts::synthesize(&me, &text, voice.as_deref()).await })
    }
}

/// The resolver the config names: [`ProviderSpeech`] over [`PlatformSpeech`] — a [`VoiceClient`] when the voice
/// variables are all set (serving `VOICE_PLATFORM_TENANTS` only, tried first), else nothing (the missing names are
/// logged, never a value) — and, for every other tenant, the job's own `asr` / `tts` providers.
pub fn from_config(cfg: &Config) -> anyhow::Result<Arc<dyn SpeechResolver>> {
    let platform = match &cfg.voice {
        Some(c) => {
            tracing::info!(kind = ?c.kind, timeout_s = c.timeout_s, tenants = c.platform_tenants.len(), "voice_platform_gateway_on");
            let client: Arc<dyn SpeechServices> = Arc::new(VoiceClient::from_config(c)?);
            PlatformSpeech::new(Some(client), c.platform_tenants.iter().copied())
        }
        None => {
            tracing::info!(missing = ?cfg.voice_missing, "voice_platform_gateway_unconfigured");
            PlatformSpeech::new(None, Vec::new())
        }
    };
    Ok(Arc::new(ProviderSpeech::new(platform)))
}

/// A transport failure: a timeout or a refused connection is `Unavailable`, anything else after the request went
/// out is `Upstream`.
pub(crate) fn transport(stage: Stage, e: &reqwest::Error) -> VoiceError {
    tracing::warn!(stage = stage.as_str(), error = %e, "voice_upstream_unreachable");
    if e.is_timeout() || e.is_connect() || e.is_request() {
        VoiceError::Unavailable(stage, "unreachable".into())
    } else {
        VoiceError::Upstream(stage, "transport".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Never;
    impl SpeechServices for Never {
        fn transcribe(&self, _clip: Clip) -> BoxFuture<'static, Result<String, VoiceError>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn synthesize(
            &self,
            _text: String,
            _voice: Option<String>,
        ) -> BoxFuture<'static, Result<Vec<u8>, VoiceError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[test]
    fn the_platform_gateway_serves_allow_listed_tenants_only() {
        let t = uuid::Uuid::new_v4();
        let p = PlatformSpeech::new(Some(Arc::new(Never)), [t]);
        assert!(p.serves(t).is_some());
        assert!(p.serves(uuid::Uuid::new_v4()).is_none());
        assert!(PlatformSpeech::new(None, [t]).serves(t).is_none());
    }

    #[test]
    fn voices() {
        for ok in ["af_heart", "alloy", "en-GB_1"] {
            assert!(valid_voice(ok), "{ok}");
        }
        let long = "v".repeat(routes::MAX_VOICE + 1);
        for bad in ["", "a b", "x\"", long.as_str()] {
            assert!(!valid_voice(bad), "{bad}");
        }
    }

    #[test]
    fn speakable_drops_emphasis_and_code_marks() {
        assert_eq!(
            speakable("Your **order** is *on its way*."),
            "Your order is on its way."
        );
        assert_eq!(
            speakable("__Done__ and ~~gone~~ _now_"),
            "Done and gone now"
        );
        assert_eq!(
            speakable("Keep snake_case and 3_000"),
            "Keep snake_case and 3_000"
        );
        assert_eq!(speakable("Run `make test` now"), "Run make test now");
        assert_eq!(speakable("A literal \\*star\\*"), "A literal *star*");
    }

    #[test]
    fn speakable_keeps_link_text_without_the_url() {
        assert_eq!(
            speakable("See [our FAQ](https://x.example/faq?a=1) or ![a map](m.png)."),
            "See our FAQ or a map."
        );
        assert_eq!(speakable("[**Track it**](https://t.example)"), "Track it");
        assert_eq!(speakable("an [unclosed bracket"), "an [unclosed bracket");
    }

    #[test]
    fn speakable_turns_headings_and_lists_into_sentences() {
        assert_eq!(
            speakable("## Next steps\n- first thing\n* second thing\n+ third"),
            "Next steps. first thing. second thing. third"
        );
        assert_eq!(
            speakable("Do this:\n1. Open the app\n2) Tap **Orders**\n10. Done!"),
            "Do this: Open the app. Tap Orders. Done!"
        );
        assert_eq!(speakable("> quoted text"), "quoted text");
        assert_eq!(speakable("Need: * name * email"), "Need: name. email");
        assert_eq!(speakable("above\n\n---\n\nbelow"), "above. below");
    }

    #[test]
    fn speakable_drops_fences_and_collapses_whitespace() {
        assert_eq!(
            speakable("Try:\n```bash\ncurl   example\n```\nthen   wait"),
            "Try: curl example. then wait"
        );
        assert_eq!(
            speakable("a line that\nwraps   here\n\nnew\tparagraph"),
            "a line that wraps here. new paragraph"
        );
        assert_eq!(speakable("  \n***\n``\n"), "");
        assert_eq!(speakable("Hello"), "Hello", "plain text is unchanged");
    }

    #[test]
    fn speakable_reads_a_realistic_reply() {
        let reply = "Hello! Your rug order is on its way.\n\n\
                     Here is where it stands:\n\n\
                     * **Status** (e.g. `shipped`)\n\
                     * delivery date\n\n\
                     ### What happens next\n\
                     1. I check the status\n\
                     2. You get an update via [your inbox](https://mail.example/inbox)";
        assert_eq!(
            speakable(reply),
            "Hello! Your rug order is on its way. Here is where it stands: \
             Status (e.g. shipped). delivery date. What happens next. I check the status. \
             You get an update via your inbox"
        );
    }

    #[test]
    fn errors_map_to_coded_api_errors_naming_the_stage() {
        let e: ApiError = VoiceError::Unavailable(Stage::Asr, "x".into()).into();
        assert_eq!(e.code(), "voice_unavailable");
        assert_eq!(e.status_and_code().0.as_u16(), 503);
        assert!(e.to_string().contains("asr"), "{e}");
        let e: ApiError = VoiceError::Upstream(Stage::Tts, "status 500".into()).into();
        assert_eq!(e.code(), "voice_upstream_error");
        assert_eq!(e.status_and_code().0.as_u16(), 502);
        assert!(e.to_string().contains("tts"), "{e}");
        assert!(!e.to_string().contains("500"), "operator detail stays out");
    }
}
