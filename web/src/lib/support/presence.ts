// Presence in the staff workspace: "Sam is viewing / typing". The viewer sends `viewing` every 5 s while a
// conversation is open and `typing` at most once a second while they type; a colleague's state is shown until 10 s
// after their last beat. Not stored server-side (`SupportApi.presence`), carried by the tenant stream's `presence`
// frame `{conversation_id, job, sub, name, state}`.
import type { PresenceState } from '$lib/api/types/support';

export const VIEWING_EVERY_MS = 5_000;
export const TYPING_DEBOUNCE_MS = 1_000;
export const PRESENCE_TTL_MS = 10_000;

/** Sends `viewing` now and every `every` ms; returns the stop function. Errors are dropped (presence is best effort). */
export function startViewing(send: () => Promise<unknown> | void, every = VIEWING_EVERY_MS): () => void {
	const beat = () => {
		try {
			void Promise.resolve(send()).catch(() => {});
		} catch {
			/* best effort */
		}
	};
	beat();
	const t = setInterval(beat, every);
	return () => clearInterval(t);
}

/** `input()` on every keystroke; `send` fires on the first one and then at most once per `ms`. */
export function typingNotifier(send: () => Promise<unknown> | void, ms = TYPING_DEBOUNCE_MS, now = Date.now) {
	let last = Number.NEGATIVE_INFINITY;
	return {
		input() {
			const t = now();
			if (t - last < ms) return;
			last = t;
			try {
				void Promise.resolve(send()).catch(() => {});
			} catch {
				/* best effort */
			}
		},
	};
}

export interface PresenceEntry {
	sub: string;
	name: string;
	state: PresenceState;
	at: number;
}

/** Colleagues' presence per conversation, each entry expiring `ttl` ms after its last frame. */
export function presenceBoard(ttl = PRESENCE_TTL_MS) {
	const by = new Map<string, Map<string, PresenceEntry>>();
	return {
		/** A `presence` frame. */
		seen(
			d: { conversation_id: string; job?: string; sub: string; name?: string | null; state: PresenceState },
			at = Date.now(),
		) {
			const key = `${d.job ?? ''}|${d.conversation_id}`;
			let m = by.get(key);
			if (!m) by.set(key, (m = new Map()));
			m.set(d.sub, { sub: d.sub, name: d.name ?? 'A colleague', state: d.state, at });
		},
		/** Who else is here now (never `me`); typing before viewing. */
		of(job: string, conversationId: string, me: string | null, now = Date.now()): PresenceEntry[] {
			const m = by.get(`${job}|${conversationId}`);
			if (!m) return [];
			return [...m.values()]
				.filter((e) => e.sub !== me && now - e.at < ttl)
				.sort((a, b) => (a.state === b.state ? a.name.localeCompare(b.name) : a.state === 'typing' ? -1 : 1));
		},
	};
}

/** "Sam is typing…" / "Sam is viewing" / "Sam and Daan are viewing". Empty when nobody is here. */
export function presenceText(list: PresenceEntry[]): string {
	if (!list.length) return '';
	const first = (n: string) => n.split(' ')[0] ?? n;
	const typing = list.filter((e) => e.state === 'typing').map((e) => first(e.name));
	if (typing.length) return `${typing.join(' and ')} ${typing.length > 1 ? 'are' : 'is'} typing…`;
	const names = list.map((e) => first(e.name));
	return `${names.join(' and ')} ${names.length > 1 ? 'are' : 'is'} viewing`;
}
