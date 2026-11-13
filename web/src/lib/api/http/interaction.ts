// The interaction API over HTTP: a turn in text (JSON, or SSE when streamed) and a voice turn (multipart). Verbs
// and paths are the ROUTES entries `interact`, `voiceTurn` and `webhookSecret`.
import type { Api } from '../client';
import { ApiError } from '../errors';
import { seg, type Req } from '../http-client';
import type { SseEvent } from '../sse';
import type { Interaction, VoiceTurn, WebhookSecret } from '../types/interaction';

export type InteractionMethod = 'interact' | 'voiceTurn' | 'webhookSecret';

/** A file name for the recording, from its MIME type (the API reads the type; the name is for its logs). */
export function audioFileName(type: string): string {
	const t = type.split(';')[0]!.trim();
	const ext = t === 'audio/ogg' ? 'ogg' : t === 'audio/mp4' ? 'm4a' : t === 'audio/wav' ? 'wav' : 'webm';
	return `turn.${ext}`;
}

/**
 * Folds the interaction stream into its result: `output_text.delta` → `onDelta`, `interaction.completed` → the
 * answer, `error` → `ApiError(502, code, message)`. Unknown events are ignored (forward compatible).
 */
export function interactionStream(onDelta?: (d: string) => void) {
	let completed: Interaction | null = null;
	let text = '';
	const parse = (e: SseEvent): Record<string, unknown> => {
		try {
			return JSON.parse(e.data) as Record<string, unknown>;
		} catch {
			throw new ApiError(502, 'bad_response', `The stream sent an unreadable ${e.event} event.`);
		}
	};
	return {
		onEvent(e: SseEvent) {
			if (e.event === 'output_text.delta') {
				const d = parse(e).delta;
				if (typeof d === 'string' && d) {
					text += d;
					onDelta?.(d);
				}
			} else if (e.event === 'interaction.completed') {
				completed = parse(e) as unknown as Interaction;
			} else if (e.event === 'error') {
				const err = (parse(e).error ?? {}) as { code?: unknown; message?: unknown };
				throw new ApiError(
					502,
					typeof err.code === 'string' ? err.code : 'stream_error',
					typeof err.message === 'string' ? err.message : 'The reply failed while streaming.',
				);
			}
		},
		/** The completed interaction; its `output_text` falls back to the streamed text. */
		result(): Interaction {
			const c = completed as Interaction | null;
			if (!c) throw new ApiError(502, 'stream_incomplete', 'The reply stream ended before it finished.');
			return { ...c, output_text: c.output_text ?? text };
		},
	};
}

export function httpInteraction(req: Req): Pick<Api, InteractionMethod> {
	return {
		interact: async (job, opts) => {
			const { onDelta, signal, ...input } = opts;
			if (!input.stream)
				return req<Interaction>('POST', `/api/jobs/${seg(job)}/interactions`, { body: input });
			const s = interactionStream(onDelta);
			await req.stream('POST', `/api/jobs/${seg(job)}/interactions`, {
				body: { ...input, stream: true },
				onEvent: s.onEvent,
				signal,
			});
			return s.result();
		},
		voiceTurn: (job, input) => {
			const fd = new FormData();
			fd.append('audio', input.audio, audioFileName(input.audio.type));
			if (input.history?.length) fd.append('history', JSON.stringify(input.history));
			if (input.language) fd.append('language', input.language);
			return req.form<VoiceTurn>('POST', `/api/jobs/${seg(job)}/voice/turns`, fd);
		},
		webhookSecret: (job) => req<WebhookSecret>('GET', `/api/jobs/${seg(job)}/interactions/webhook-secret`),
	};
}
