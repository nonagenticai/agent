//! The staff draft playground's backend: an OpenAI-compatible `POST {url}/chat/completions` with `stream: true` (the
//! in-cluster `inference-router`). D109 (KH T6): it never answers a customer party — those are B4 runtime turns
//! (crate::interaction::respond); it is constructed only in crate::interaction (`from_config`, `client_for`).
//!
//! - Tenants: a platform-hosted model is not for production. It serves ONLY the allow-listed test tenants
//!   (`INTERACTION_LLM_TENANTS`); any other tenant gets [`BackendError::NotConfigured`] (409 `provider_not_configured`)
//!   before any upstream call, until B3 tenant providers resolve per tenant.
//! - Messages: one `system` message (the request's instructions, else "You are the assistant for job {job_key}."),
//!   then the input in order, each message's parts joined by newlines (data parts as compact JSON).
//! - The answer is read as server-sent events: each `data:` line's `choices[0].delta.content` is a
//!   [`BackendEvent::Delta`]; the `usage` chunk (asked for with `stream_options.include_usage`) and `[DONE]` end it with
//!   [`BackendEvent::Done`]. A body that ends cleanly without `[DONE]` is done too.
//! - A transport failure before any answer is [`BackendError::Unavailable`]; a non-2xx status, an error chunk, a
//!   malformed chunk or a body cut mid-way is [`BackendError::Upstream`].
//!
//! The call is counted as upstream `inference_router` / `chat_completions` (crate::observability::upstream_http). No
//! message text, party or metadata is logged: only statuses.
use std::collections::{HashSet, VecDeque};

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};

use super::backend::InteractionBackend;
use super::model::{BackendError, BackendEvent, InteractionRequest, Usage};
use crate::config::InteractionLlmConfig;

/// The system message when the request carries no instructions; `{job_key}` is replaced.
pub const DEFAULT_INSTRUCTIONS: &str = "You are the assistant for job {job_key}.";

pub struct LlmPassthrough {
    /// A client of its own: the shared one's 10 s timeout would cut every long answer.
    pub http: reqwest::Client,
    /// The base URL, without a trailing slash (`…/v1`); the call is `{url}/chat/completions`.
    pub url: String,
    pub token: String,
    pub model: String,
    /// With `{job_key}` replaced by the job's key.
    pub default_instructions: String,
    /// The only tenants served; everyone else is [`BackendError::NotConfigured`].
    pub tenants: HashSet<uuid::Uuid>,
    /// Send the request's tenant as `X-Effective-Tenant-Id` (crate::providers::llm::ResolvedLlm::effective_tenant),
    /// with the rest of the router identity (crate::providers::llm::router_identity_headers): the platform-hosted
    /// model only, never a tenant's own provider.
    pub effective_tenant: bool,
    /// The `X-Actor-Id` of those calls (agent's service account, `SERVICE_ACTOR_ID`); sent only with
    /// [`Self::effective_tenant`].
    pub actor: Option<uuid::Uuid>,
}

impl std::fmt::Debug for LlmPassthrough {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmPassthrough")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("model", &self.model)
            .field("tenants", &self.tenants.len())
            .field("effective_tenant", &self.effective_tenant)
            .field("actor", &self.actor)
            .finish()
    }
}

impl LlmPassthrough {
    /// From the config: a client bounded by `INTERACTION_LLM_TIMEOUT_S` (connect: 5 s). The configured URL is the
    /// platform-hosted model, so each call names its tenant and the router identity, `actor` (`SERVICE_ACTOR_ID`) as
    /// its `X-Actor-Id`.
    pub fn from_config(
        c: &InteractionLlmConfig,
        actor: Option<uuid::Uuid>,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(c.timeout_s))
            .build()?;
        Ok(Self {
            http,
            url: c.url.clone(),
            token: c.token.clone(),
            model: c.model.clone(),
            default_instructions: DEFAULT_INSTRUCTIONS.into(),
            tenants: c.tenants.iter().copied().collect(),
            effective_tenant: true,
            actor,
        })
    }

    /// The chat-completions body for `req`.
    pub fn body(&self, req: &InteractionRequest) -> Value {
        let system = req
            .instructions
            .clone()
            .unwrap_or_else(|| self.default_instructions.replace("{job_key}", &req.job_key));
        let mut messages = vec![json!({"role": "system", "content": system})];
        messages.extend(
            req.input
                .iter()
                .map(|m| json!({"role": m.role.as_str(), "content": m.joined()})),
        );
        json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        })
    }
}

impl InteractionBackend for LlmPassthrough {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        if !self.tenants.contains(&req.tenant) {
            return stream::once(async {
                Err(BackendError::NotConfigured(
                    "the tenant is not on INTERACTION_LLM_TENANTS".into(),
                ))
            })
            .boxed();
        }
        let mut call = self
            .http
            .post(format!("{}/chat/completions", self.url))
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&self.body(&req));
        if self.effective_tenant {
            for (k, v) in crate::providers::llm::router_identity_headers(req.tenant, self.actor) {
                call = call.header(k, v);
            }
        }
        stream::unfold(Phase::Connect(Box::new(call)), step).boxed()
    }
}

enum Phase {
    Connect(Box<reqwest::RequestBuilder>),
    Read(Box<Reader>),
    End,
}

/// One step of the stream: connect, then read chunks until an event is ready.
async fn step(mut phase: Phase) -> Option<(Result<BackendEvent, BackendError>, Phase)> {
    loop {
        match phase {
            Phase::Connect(call) => {
                let sent = crate::observability::upstream_http(
                    "inference_router",
                    "chat_completions".into(),
                    (*call).send(),
                )
                .await;
                match sent {
                    Err(e) => {
                        tracing::warn!(error = %e, "interaction_llm_unreachable");
                        let e =
                            BackendError::Unavailable("the model endpoint is unreachable".into());
                        return Some((Err(e), Phase::End));
                    }
                    Ok(r) if !r.status().is_success() => {
                        let status = r.status().as_u16();
                        tracing::warn!(status, "interaction_llm_status");
                        return Some((
                            Err(BackendError::Upstream(format!("status {status}"))),
                            Phase::End,
                        ));
                    }
                    Ok(r) => phase = Phase::Read(Box::new(Reader::new(r))),
                }
            }
            Phase::Read(mut rd) => {
                if let Some(ev) = rd.queue.pop_front() {
                    let last = !matches!(ev, Ok(BackendEvent::Delta(_)));
                    return Some((ev, if last { Phase::End } else { Phase::Read(rd) }));
                }
                match rd.resp.chunk().await {
                    Ok(Some(bytes)) => rd.feed(&bytes),
                    Ok(None) => {
                        // a final line without its newline, then done
                        let rest = std::mem::take(&mut rd.buf);
                        rd.line(&rest);
                        rd.finish();
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "interaction_llm_stream_cut");
                        rd.fail("the model stream was cut");
                    }
                }
                phase = Phase::Read(rd);
            }
            Phase::End => return None,
        }
    }
}

/// Server-sent-event lines → events.
struct Reader {
    resp: reqwest::Response,
    buf: Vec<u8>,
    usage: Option<Usage>,
    queue: VecDeque<Result<BackendEvent, BackendError>>,
    /// `Done` or an error is queued: later lines are ignored.
    ended: bool,
}

impl Reader {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            resp,
            buf: Vec::new(),
            usage: None,
            queue: VecDeque::new(),
            ended: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            self.line(&line);
        }
    }

    fn line(&mut self, raw: &[u8]) {
        if self.ended {
            return;
        }
        let Ok(line) = std::str::from_utf8(raw) else {
            return self.fail("the model sent a line that is not UTF-8");
        };
        let Some(data) = line.trim_end_matches(['\r', '\n']).strip_prefix("data:") else {
            return; // `event:`, `id:`, comments and blank separators
        };
        let data = data.trim();
        if data == "[DONE]" {
            return self.finish();
        }
        match parse_chunk(data) {
            Ok(Chunk { delta, usage }) => {
                if let Some(u) = usage {
                    self.usage = Some(u);
                }
                if let Some(d) = delta.filter(|d| !d.is_empty()) {
                    self.queue.push_back(Ok(BackendEvent::Delta(d)));
                }
            }
            Err(why) => self.fail(why),
        }
    }

    fn finish(&mut self) {
        if !self.ended {
            self.ended = true;
            self.queue
                .push_back(Ok(BackendEvent::Done { usage: self.usage }));
        }
    }

    fn fail(&mut self, why: &str) {
        if !self.ended {
            self.ended = true;
            self.queue
                .push_back(Err(BackendError::Upstream(why.into())));
        }
    }
}

/// What one `chat.completion.chunk` carries.
#[derive(Debug, PartialEq)]
struct Chunk {
    delta: Option<String>,
    usage: Option<Usage>,
}

fn parse_chunk(data: &str) -> Result<Chunk, &'static str> {
    let v: Value = serde_json::from_str(data).map_err(|_| "the model sent a malformed chunk")?;
    if !v["error"].is_null() {
        return Err("the model sent an error chunk");
    }
    let delta = v["choices"][0]["delta"]["content"]
        .as_str()
        .map(str::to_string);
    let tokens = |k: &str| {
        v["usage"][k]
            .as_u64()
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    let usage = match (tokens("prompt_tokens"), tokens("completion_tokens")) {
        (Some(input_tokens), Some(output_tokens)) => Some(Usage {
            input_tokens,
            output_tokens,
        }),
        _ => None,
    };
    Ok(Chunk { delta, usage })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;

    use super::*;
    use crate::interaction::backend::collect;
    use crate::interaction::model::{Message, Part, Role, Scope};

    fn llm(url: &str) -> LlmPassthrough {
        LlmPassthrough::from_config(
            &InteractionLlmConfig {
                url: url.into(),
                token: "tok-1".into(),
                model: "local-llm".into(),
                timeout_s: 10,
                tenants: vec![uuid::Uuid::nil()],
            },
            Some(SERVICE_ACTOR),
        )
        .unwrap()
    }

    /// agent's service account (`SERVICE_ACTOR_ID`) in these tests.
    const SERVICE_ACTOR: uuid::Uuid =
        uuid::Uuid::from_u128(0x0195_f48b_d000_7000_8000_0000_0000_00aa);

    fn req() -> InteractionRequest {
        InteractionRequest::new(
            &Scope {
                tenant: uuid::Uuid::nil(),
                job_id: uuid::Uuid::nil(),
                job_key: "faq_agent".into(),
            },
            vec![
                Message::text(Role::User, "hi"),
                Message::text(Role::Assistant, "hello"),
                Message {
                    role: Role::User,
                    parts: vec![Part::Text("order".into()), Part::Data(json!({"id": 7}))],
                },
            ],
        )
    }

    #[test]
    fn body_maps_instructions_and_parts() {
        let l = llm("http://x");
        let b = l.body(&req());
        assert_eq!(b["model"], "local-llm");
        assert_eq!(b["stream"], true);
        assert_eq!(b["stream_options"]["include_usage"], true);
        assert_eq!(
            b["messages"],
            json!([
                {"role": "system", "content": "You are the assistant for job faq_agent."},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "order\n{\"id\":7}"},
            ])
        );
        let mut r = req();
        r.instructions = Some("Be brief.".into());
        assert_eq!(l.body(&r)["messages"][0]["content"], "Be brief.");
    }

    #[test]
    fn chunks() {
        assert_eq!(
            parse_chunk(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).unwrap(),
            Chunk {
                delta: Some("Hi".into()),
                usage: None
            }
        );
        assert_eq!(
            parse_chunk(r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#)
                .unwrap()
                .usage,
            Some(Usage {
                input_tokens: 3,
                output_tokens: 2
            })
        );
        assert!(parse_chunk("{nope").is_err());
        assert!(parse_chunk(r#"{"error":{"message":"x"}}"#).is_err());
    }

    /// What the fake upstream saw.
    #[derive(Default)]
    struct Seen {
        auth: Option<String>,
        effective_tenant: Option<String>,
        app: Option<String>,
        project: Option<String>,
        actor: Option<String>,
        body: Option<Value>,
        requests: usize,
    }

    async fn upstream(status: StatusCode, sse: &'static str) -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let s2 = seen.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |h: HeaderMap, body: axum::Json<Value>| {
                let seen = s2.clone();
                async move {
                    let mut g = seen.lock().unwrap();
                    g.requests += 1;
                    g.auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(String::from);
                    g.effective_tenant = get("x-effective-tenant-id");
                    g.app = get("x-app-id");
                    g.project = get("x-project-id");
                    g.actor = get("x-actor-id");
                    g.body = Some(body.0);
                    (status, [("content-type", "text/event-stream")], sse).into_response()
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}/v1"), seen)
    }

    const SSE: &str = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
        : keep-alive\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\r\n\r\n\
        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n\
        data: [DONE]\n\n";

    #[tokio::test]
    async fn streams_deltas_and_usage_from_an_openai_compatible_upstream() {
        let (url, seen) = upstream(StatusCode::OK, SSE).await;
        let events: Vec<_> = llm(&url).respond(req()).collect().await;
        assert_eq!(
            events,
            vec![
                Ok(BackendEvent::Delta("Hel".into())),
                Ok(BackendEvent::Delta("lo".into())),
                Ok(BackendEvent::Done {
                    usage: Some(Usage {
                        input_tokens: 3,
                        output_tokens: 2
                    })
                }),
            ]
        );
        let g = seen.lock().unwrap();
        assert_eq!(g.auth.as_deref(), Some("Bearer tok-1"));
        let body = g.body.as_ref().unwrap();
        assert_eq!(body["model"], "local-llm");
        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "hi"})
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn only_listed_tenants_reach_the_upstream() {
        let (url, seen) = upstream(StatusCode::OK, SSE).await;
        let mut stranger = req();
        stranger.tenant = uuid::Uuid::new_v4();
        let events: Vec<_> = llm(&url).respond(stranger).collect().await;
        assert!(
            matches!(events.as_slice(), [Err(BackendError::NotConfigured(_))]),
            "{events:?}"
        );
        assert_eq!(seen.lock().unwrap().requests, 0, "no upstream call");
        let r = collect(llm(&url).respond(req())).await.unwrap();
        assert_eq!(r.text, "Hello");
        assert_eq!(seen.lock().unwrap().requests, 1);
    }

    #[tokio::test]
    async fn the_platform_model_is_told_the_tenant_and_a_tenant_provider_is_not() {
        let (url, seen) = upstream(StatusCode::OK, SSE).await;
        collect(llm(&url).respond(req())).await.unwrap();
        assert_eq!(
            seen.lock().unwrap().effective_tenant.as_deref(),
            Some(uuid::Uuid::nil().to_string().as_str())
        );
        let (url, seen) = upstream(StatusCode::OK, SSE).await;
        let mut byo = llm(&url);
        byo.effective_tenant = false;
        collect(byo.respond(req())).await.unwrap();
        let g = seen.lock().unwrap();
        assert_eq!(g.requests, 1);
        assert_eq!(g.effective_tenant, None);
    }

    // Guards `respond`'s `router_identity_headers` loop: the platform model gets the whole caller identity (the
    // inference-router 400s a call without X-App-Id / X-Project-Id once it enforces), a tenant provider none of it.
    #[tokio::test]
    async fn the_platform_model_gets_the_router_identity_and_a_tenant_provider_gets_none() {
        let tenant = uuid::Uuid::nil();
        let (url, seen) = upstream(StatusCode::OK, SSE).await;
        collect(llm(&url).respond(req())).await.unwrap();
        {
            let g = seen.lock().unwrap();
            assert_eq!(g.app.as_deref(), Some("agent"));
            assert_eq!(
                g.project.as_deref(),
                Some(
                    crate::providers::llm::tenant_level_project_id(tenant)
                        .to_string()
                        .as_str()
                )
            );
            assert_eq!(g.actor.as_deref(), Some(SERVICE_ACTOR.to_string().as_str()));
        }
        // no SERVICE_ACTOR_ID: the actor header is omitted, never invented
        let (url, seen) = upstream(StatusCode::OK, SSE).await;
        let mut anonymous = llm(&url);
        anonymous.actor = None;
        collect(anonymous.respond(req())).await.unwrap();
        {
            let g = seen.lock().unwrap();
            assert_eq!(g.app.as_deref(), Some("agent"));
            assert_eq!(g.actor, None);
        }
        let (url, seen) = upstream(StatusCode::OK, SSE).await;
        let mut byo = llm(&url);
        byo.effective_tenant = false;
        collect(byo.respond(req())).await.unwrap();
        let g = seen.lock().unwrap();
        assert_eq!(g.requests, 1);
        assert_eq!(
            (&g.effective_tenant, &g.app, &g.project, &g.actor),
            (&None, &None, &None, &None)
        );
    }

    #[tokio::test]
    async fn a_body_without_done_still_completes() {
        let (url, _) = upstream(
            StatusCode::OK,
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}",
        )
        .await;
        let r = collect(llm(&url).respond(req())).await.unwrap();
        assert_eq!((r.text.as_str(), r.usage), ("ok", None));
    }

    #[tokio::test]
    async fn non_2xx_is_upstream_and_unreachable_is_unavailable() {
        let (url, _) = upstream(StatusCode::INTERNAL_SERVER_ERROR, "boom").await;
        assert_eq!(
            collect(llm(&url).respond(req())).await,
            Err(BackendError::Upstream("status 500".into()))
        );
        let (url, _) = upstream(StatusCode::OK, "data: {nope\n\n").await;
        assert!(matches!(
            collect(llm(&url).respond(req())).await,
            Err(BackendError::Upstream(_))
        ));
        // the discard port: connection refused
        assert!(matches!(
            collect(llm("http://127.0.0.1:9/v1").respond(req())).await,
            Err(BackendError::Unavailable(_))
        ));
    }
}
