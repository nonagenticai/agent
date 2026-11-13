// The `InteractionApi` contract.
import type { JobKey } from '$lib/api/types/core';
import type {
	InteractOptions,
	Interaction,
	VoiceTurn,
	VoiceTurnInput,
	WebhookSecret,
} from '$lib/api/types/interaction';

export type InteractionApi = {
	/**
	 * `POST /api/jobs/{job}/interactions` — one turn: `input` is a user message or the whole history (the API keeps
	 * none). `stream: true` reads the SSE answer (`output_text.delta` → `onDelta`) and resolves with the
	 * `interaction.completed` object. 503 `interaction_backend_unavailable` when the job's agent is down. D109: a
	 * member's call without a `party` is the staff draft playground; a customer party (an API key, or a body `party`)
	 * is a runtime turn — 409 `job_not_live` while the job's live release has no runtime design, 503 `unavailable`
	 * while no runtime serves it. For a customer the earlier `input` turns seed its conversation, and `instructions`
	 * are NOT applied (D108: the system message comes from the release's layers only); `channel` labels the request,
	 * the conversation stays `api`. Streamed, `interaction.created` arrives at once and keep-alives until the reply.
	 */
	interact(job: JobKey, opts: InteractOptions): Promise<Interaction>;
	/**
	 * `POST /api/jobs/{job}/voice/turns` (multipart: `audio`, `history` JSON, `language`) — transcribes the
	 * recording, answers it and speaks the answer (`audio_b64`, WAV). 422 `no_speech`, 503 `voice_unavailable`.
	 * D109: for a customer (an API key) the `history` seeds the turn's `voice` conversation on the runtime; no
	 * instructions apply (D108).
	 */
	voiceTurn(job: JobKey, input: VoiceTurnInput): Promise<VoiceTurn>;
	/**
	 * `GET /api/jobs/{job}/interactions/webhook-secret` (integrations_manager) — the job's Standard Webhooks signing
	 * secret for async interactions (`callback_url`). 409 `webhooks_not_configured` when the platform has no signing
	 * key; 403 for other roles.
	 */
	webhookSecret(job: JobKey): Promise<WebhookSecret>;
};
