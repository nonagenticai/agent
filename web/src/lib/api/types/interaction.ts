// The interaction API (agent-api): talk to a job in plain text or by voice. Stateless — the caller keeps the
// conversation and sends the history with every turn. OpenAI-shaped siblings (`/responses`, `/chat/completions`)
// exist under the same `/api/jobs/{job}` base for SDK users; the web only shows them as snippets.

export type InteractionRole = 'user' | 'assistant' | 'system';

/** One turn of the history the caller sends. */
export interface InteractionMessage {
	role: InteractionRole;
	content: string;
}

/** `POST /api/jobs/{job}/interactions` body. */
export interface InteractionInput {
	/** One user message, or the whole history ending with the new user message. */
	input: string | InteractionMessage[];
	channel?: string;
	/** true → the answer is an SSE stream (`interaction.created`, `output_text.delta`, `interaction.completed`). */
	stream?: boolean;
	metadata?: Record<string, unknown>;
}

/** The web's call options: the body plus where streamed text goes and how to stop it. */
export interface InteractOptions extends InteractionInput {
	/** Called with each `output_text.delta` while streaming (ignored without `stream`). */
	onDelta?: (delta: string) => void;
	/** Aborts a streamed reply. */
	signal?: AbortSignal;
}

export interface InteractionOutputText {
	type: 'text';
	text: string;
}

export interface InteractionOutputItem {
	role: 'assistant';
	content: InteractionOutputText[];
}

export interface InteractionUsage {
	input_tokens?: number;
	output_tokens?: number;
	total_tokens?: number;
	[k: string]: unknown;
}

/** The sync answer and the `interaction.completed` event's data. */
export interface Interaction {
	id: string;
	object: 'interaction';
	status: string;
	job: string;
	channel: string | null;
	created_at: number | string;
	output: InteractionOutputItem[];
	output_text: string;
	usage: InteractionUsage | null;
}

/** `POST /api/jobs/{job}/voice/turns` multipart fields. */
export interface VoiceTurnInput {
	/** The recording (browser MediaRecorder: webm/opus where supported). */
	audio: Blob;
	/** The conversation so far (sent as a JSON string field). */
	history?: InteractionMessage[];
	/** BCP-47 primary tag; omitted = the backend detects it. */
	language?: string;
}

export interface VoiceTurn {
	transcript: string;
	reply_text: string;
	/** The spoken reply, base64. */
	audio_b64: string;
	audio_mime: string;
}

/**
 * `GET /api/jobs/{job}/interactions/webhook-secret` — the job's Standard Webhooks signing secret: async interactions
 * (`callback_url`) POST their result signed with it (`webhook-id`, `webhook-timestamp`, `webhook-signature: v1,…`).
 */
export interface WebhookSecret {
	/** `whsec_` + base64 key, as the standardwebhooks libraries take it. */
	secret: string;
	scheme: string;
	signature_header: string;
}
