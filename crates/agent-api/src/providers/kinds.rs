//! The provider-slot manifest (plan B3 T4, interim for W5 A1): the six rag/ingest slots, which service holds each,
//! what the UI shows for it, which provider kinds a tenant may pick, and how a UI kind maps to the strata `dialect`.
//! Mirrored from `web/src/lib/fixtures/tenant.ts` `SLOT_DEFS` / `SLOT_ORDER`, `web/src/lib/api/types/tenant.ts`
//! `GPU_PROVIDER_KINDS` and `web/src/lib/api/areas/readiness.ts` `SLOT_USE` (the required slots).
//!
//! Dialects are what strata implements (strata_rag `7c2f1ad` `providers::Capability::implemented_dialect`: embed and
//! chat `openai`, rerank `cohere`; strata_ingest `ce9590e` `config::DIALECTS`: ocr `mistral`, vision and transcribe
//! `openai`). A UI kind with no strata dialect for the slot is not offered (`azure_document_intelligence` for `ocr`:
//! strata_ingest would refuse it 422 `dialect_not_allowed` on every push).
//!
//! The internal kinds (D70) `mlx_api` and `pgx_vllm` serve nonagentic's own tenant only; their endpoints are the
//! platform's deployment configuration (`INTERNAL_MLX_TEXT_URL` / `INTERNAL_MLX_MEDIA_URL` /
//! `INTERNAL_PGX_VLLM_URL`), never tenant input.
//!
//! Speech kinds (B3 T4d, D101/D102): the `asr` / `tts` uses bind an LLM provider record (crate::providers::llm) of
//! kind `w2v` or `openai`; [`speech_contract`] names the crate::voice wire contract each speaks.
use super::llm::Kind;
use crate::config::SpeechProviderKind;
use crate::strata::Service;

/// The region shown for an internal slot.
pub const INTERNAL_REGION: &str = "EU (platform)";
/// strata_rag's embedding size: an embed registration must declare it.
pub const EMBED_DIMS: i64 = 1024;

/// One provider slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    Embed,
    Rerank,
    Chat,
    Ocr,
    Vision,
    Transcribe,
}

impl Slot {
    /// Fixture `SLOT_ORDER`.
    pub const ALL: [Slot; 6] = [
        Slot::Embed,
        Slot::Rerank,
        Slot::Chat,
        Slot::Ocr,
        Slot::Vision,
        Slot::Transcribe,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Embed => "embed",
            Slot::Rerank => "rerank",
            Slot::Chat => "chat",
            Slot::Ocr => "ocr",
            Slot::Vision => "vision",
            Slot::Transcribe => "transcribe",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Slot::ALL.into_iter().find(|x| x.as_str() == s)
    }

    /// The strata service that holds the slot.
    pub fn service(self) -> Service {
        match self {
            Slot::Embed | Slot::Rerank | Slot::Chat => Service::Rag,
            Slot::Ocr | Slot::Vision | Slot::Transcribe => Service::Ingest,
        }
    }

    /// The slots a service holds, in slot order.
    pub fn of(service: Service) -> Vec<Slot> {
        Slot::ALL
            .into_iter()
            .filter(|s| s.service() == service)
            .collect()
    }

    pub fn label(self) -> &'static str {
        match self {
            Slot::Embed => "Embeddings",
            Slot::Rerank => "Reranking",
            Slot::Chat => "Answer drafting",
            Slot::Ocr => "OCR",
            Slot::Vision => "Vision",
            Slot::Transcribe => "Transcription",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Slot::Embed => "Turns passages and questions into vectors for knowledge search.",
            Slot::Rerank => "Re-orders the best passages before they reach the agent.",
            Slot::Chat => {
                "Router and answer model inside strata_rag (laya routing, knowledge.answer)."
            }
            Slot::Ocr => "Reads text from PDFs and images on upload.",
            Slot::Vision => "Reads scanned pages, tables and figures the OCR model cannot.",
            Slot::Transcribe => "Turns audio and video into text.",
        }
    }

    pub fn used_by(self) -> &'static [&'static str] {
        match self {
            Slot::Embed => &["knowledge.search", "ingest indexing"],
            Slot::Rerank => &["knowledge.search"],
            Slot::Chat => &["laya routing", "knowledge.answer"],
            Slot::Ocr => &["file ingest"],
            Slot::Vision => &["file ingest (scanned pages)"],
            Slot::Transcribe => &["audio/video ingest"],
        }
    }

    /// Readiness (`SLOT_USE`): every slot but `transcribe` must be deployed before a release that uses it applies.
    pub fn required(self) -> bool {
        !matches!(self, Slot::Transcribe)
    }

    /// The external kinds a customer tenant may pick (fixture `SLOT_DEFS.kinds`, minus kinds strata cannot speak).
    pub fn external_kinds(self) -> &'static [&'static str] {
        match self {
            Slot::Embed => &["openai_compatible", "voyage"],
            Slot::Rerank => &["cohere", "voyage"],
            Slot::Chat | Slot::Vision | Slot::Transcribe => &["openai_compatible"],
            Slot::Ocr => &["mistral_ocr"],
        }
    }

    /// The internal kind of nonagentic's own tenant (fixture `internalKindFor`).
    pub fn internal_kind(self) -> &'static str {
        match self {
            Slot::Chat => "pgx_vllm",
            _ => "mlx_api",
        }
    }

    /// The internal kind's endpoint for this slot, from the platform's deployment configuration. `None` when the
    /// environment does not set it — the caller shows no default rather than inventing one.
    pub fn internal_base_url(self, ip: &crate::config::InternalProvidersConfig) -> Option<&str> {
        match self {
            Slot::Embed | Slot::Rerank | Slot::Vision => ip.mlx_text_url.as_deref(),
            Slot::Ocr | Slot::Transcribe => ip.mlx_media_url.as_deref(),
            Slot::Chat => ip.pgx_url.as_deref(),
        }
    }

    /// The internal kind's model for this slot (fixture `kindModels(..)[0]`), shown when strata does not say.
    pub fn internal_model(self) -> &'static str {
        match self {
            Slot::Embed => "BAAI/bge-m3",
            Slot::Rerank => "BAAI/bge-reranker-v2-m3",
            Slot::Chat => "local-llm",
            Slot::Ocr => "PaddleOCR-VL",
            Slot::Vision => "Qwen2-VL-2B-Instruct-4bit",
            Slot::Transcribe => "whisper-large-v3-turbo",
        }
    }

    /// The model strata serves a tenant with no registration of this slot (its deployment default; F-08 "Use
    /// default", T4b), `None` when this platform deploys none for the slot (`transcribe`: audio needs a tenant
    /// provider). The defaults are the internal tenant's mlx_api / pgx vLLM models.
    pub fn deployment_default(self) -> Option<&'static str> {
        match self {
            Slot::Transcribe => None,
            s => Some(s.internal_model()),
        }
    }

    /// strata_rag's `stores.embed_model` identity of an embed registration of `model` without a `model_id`
    /// (strata `ProviderSpec::embed_model_id`: `<model>@<dims>`).
    pub fn embed_model_id(model: &str) -> String {
        format!("{model}@{EMBED_DIMS}")
    }

    /// The strata dialect of `kind` in this slot, `None` when the slot does not take that kind.
    pub fn dialect(self, kind: &str) -> Option<&'static str> {
        if !self.external_kinds().contains(&kind) {
            return None;
        }
        Some(match self {
            Slot::Rerank => "cohere",
            Slot::Ocr => "mistral",
            Slot::Embed | Slot::Chat | Slot::Vision | Slot::Transcribe => "openai",
        })
    }

    /// The UI kind shown for a registration strata reports with `dialect` (drift, adopt): the first external kind of
    /// the slot that speaks it, else the dialect itself.
    pub fn kind_of_dialect(self, dialect: &str) -> String {
        self.external_kinds()
            .iter()
            .find(|k| self.dialect(k) == Some(dialect))
            .copied()
            .map_or_else(|| dialect.to_string(), str::to_string)
    }

    /// strata_rag embed registrations declare the vector size.
    pub fn dims(self) -> Option<i64> {
        (self == Slot::Embed).then_some(EMBED_DIMS)
    }
}

/// True for a kind only nonagentic's own tenant binds (fixture `isInternalKind`).
pub fn is_internal_kind(kind: &str) -> bool {
    matches!(kind, "mlx_api" | "pgx_vllm")
}

/// Display name of a kind (fixture `gpuKindLabel`).
pub fn kind_label(kind: &str) -> &str {
    match kind {
        "mlx_api" => "mlx_api (internal)",
        "pgx_vllm" => "pgx vLLM (internal)",
        "openai_compatible" => "OpenAI-compatible",
        "voyage" => "Voyage AI",
        "cohere" => "Cohere",
        "mistral_ocr" => "Mistral OCR",
        "azure_document_intelligence" => "Azure Document Intelligence",
        other => other,
    }
}

/// The crate::voice wire contract a provider of `kind` speaks as an `asr` / `tts` provider: `w2v` the w2v services
/// (`/api/transcribe`, `/api/tts/synthesize`), `openai` the OpenAI audio API (`/audio/transcriptions`,
/// `/audio/speech`); Laya speaks none.
pub fn speech_contract(kind: Kind) -> Option<SpeechProviderKind> {
    match kind {
        Kind::W2v => Some(SpeechProviderKind::W2v),
        Kind::Openai => Some(SpeechProviderKind::OpenaiCompatible),
        Kind::Laya => None,
    }
}

/// The voice a `tts` binding without one speaks in (the `VOICE_TTS_VOICE` defaults, crate::config).
pub fn default_voice(contract: SpeechProviderKind) -> &'static str {
    match contract {
        SpeechProviderKind::W2v => "af_heart",
        SpeechProviderKind::OpenaiCompatible => "alloy",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_kinds_follow_the_uses() {
        use crate::providers::llm::Use;
        for u in [Use::Asr, Use::Tts] {
            for k in u.kinds() {
                assert!(speech_contract(*k).is_some(), "{u:?} {k:?}");
            }
            assert!(u.refusal(Kind::Laya).is_some());
        }
        assert_eq!(speech_contract(Kind::Laya), None);
        assert_eq!(default_voice(SpeechProviderKind::W2v), "af_heart");
        for u in [Use::Qualify, Use::Gates, Use::Generator] {
            assert_eq!(
                u.refusal(Kind::W2v).map(|r| r.0),
                Some("kind_not_valid_for_use"),
                "{u:?}"
            );
        }
        assert_eq!(
            Use::Gates.refusal(Kind::Openai).map(|r| r.0),
            Some("provider_kind_mismatch")
        );
    }

    #[test]
    fn manifest_matches_strata_dialects() {
        let order: Vec<&str> = Slot::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            order,
            ["embed", "rerank", "chat", "ocr", "vision", "transcribe"]
        );
        for s in Slot::ALL {
            assert_eq!(Slot::parse(s.as_str()), Some(s));
            for k in s.external_kinds() {
                assert!(s.dialect(k).is_some(), "{s:?} {k}");
                assert!(!is_internal_kind(k));
            }
            assert!(is_internal_kind(s.internal_kind()));
            assert_eq!(s.dialect(s.internal_kind()), None);
        }
        assert_eq!(Slot::Rerank.dialect("voyage"), Some("cohere"));
        assert_eq!(Slot::Ocr.dialect("azure_document_intelligence"), None);
        assert_eq!(Slot::Embed.kind_of_dialect("openai"), "openai_compatible");
        assert_eq!(Slot::Ocr.kind_of_dialect("mistral"), "mistral_ocr");
        assert_eq!(Slot::Vision.kind_of_dialect("weird"), "weird");
        assert_eq!(Slot::Transcribe.deployment_default(), None);
        assert_eq!(Slot::Embed.deployment_default(), Some("BAAI/bge-m3"));
        assert_eq!(Slot::embed_model_id("e-1"), "e-1@1024");
        assert_eq!(
            Slot::of(Service::Ingest),
            [Slot::Ocr, Slot::Vision, Slot::Transcribe]
        );
        assert_eq!(
            Slot::ALL.iter().filter(|s| s.required()).count(),
            5,
            "SLOT_USE"
        );
    }
}
