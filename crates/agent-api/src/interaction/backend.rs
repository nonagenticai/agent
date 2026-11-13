//! What answers an interaction. B4's runtime implements [`InteractionBackend`] for customer channels
//! (crate::runtime::backend::RuntimeBackend, T9c); [`crate::interaction::llm::LlmPassthrough`] calls an
//! OpenAI-compatible model directly, and [`Unconfigured`] answers 409 `provider_not_configured` when no backend is
//! set.
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};

use super::model::{BackendError, BackendEvent, InteractionRequest, InteractionResult};

/// One stream per request: zero or more [`BackendEvent::Delta`], then one [`BackendEvent::Done`]; or an error, after
/// which nothing follows. Dropping the stream cancels the work (a client that disconnects stops the upstream call).
pub trait InteractionBackend: Send + Sync {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>>;

    /// The whole answer (sync mode), with the grounding the backend reports. The default drains [`Self::respond`]
    /// ([`collect`]) and reports none; a backend that knows how its answer used knowledge overrides it.
    fn answer(
        &self,
        req: InteractionRequest,
    ) -> BoxFuture<'static, Result<InteractionResult, BackendError>> {
        Box::pin(collect(self.respond(req)))
    }
}

/// Drain a stream into one answer (sync mode). A stream that ends without `Done` is an upstream error.
pub async fn collect(
    mut s: BoxStream<'static, Result<BackendEvent, BackendError>>,
) -> Result<InteractionResult, BackendError> {
    let mut text = String::new();
    while let Some(ev) = s.next().await {
        match ev? {
            BackendEvent::Delta(d) => text.push_str(&d),
            BackendEvent::Done { usage } => {
                return Ok(InteractionResult {
                    text,
                    usage,
                    grounding: None,
                });
            }
        }
    }
    Err(BackendError::Upstream(
        "the stream ended without completing".into(),
    ))
}

/// No backend configured (`INTERACTION_LLM_URL` / `INTERACTION_LLM_TOKEN` unset): every request is 409
/// `provider_not_configured` (a missing tenant provider, not an outage).
pub struct Unconfigured;

impl InteractionBackend for Unconfigured {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        stream::once(async {
            Err(BackendError::NotConfigured(
                "no interaction backend is configured".into(),
            ))
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::model::{Message, Role, Scope, Usage};

    fn req() -> InteractionRequest {
        InteractionRequest::new(
            &Scope {
                tenant: uuid::Uuid::nil(),
                job_id: uuid::Uuid::nil(),
                job_key: "faq".into(),
            },
            vec![Message::text(Role::User, "hi")],
        )
    }

    #[tokio::test]
    async fn collect_drains_deltas_and_usage() {
        let usage = Some(Usage {
            input_tokens: 3,
            output_tokens: 2,
        });
        let s = stream::iter(vec![
            Ok(BackendEvent::Delta("Hel".into())),
            Ok(BackendEvent::Delta("lo".into())),
            Ok(BackendEvent::Done { usage }),
        ])
        .boxed();
        assert_eq!(
            collect(s).await.unwrap(),
            InteractionResult {
                text: "Hello".into(),
                usage,
                grounding: None,
            }
        );
        let cut = stream::iter(vec![Ok(BackendEvent::Delta("x".into()))]).boxed();
        assert!(matches!(collect(cut).await, Err(BackendError::Upstream(_))));
        assert!(matches!(
            collect(Unconfigured.respond(req())).await,
            Err(BackendError::NotConfigured(_))
        ));
    }
}
