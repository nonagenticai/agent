// B12 console labels: channel names and a short clock for message times.
import type { ConversationChannel } from '$lib/api/types/conversations';

export const CHANNEL_LABEL: Record<ConversationChannel, string> = {
	staff: 'Staff test chat',
	telegram: 'Telegram',
	web_chat: 'Web chat',
	voice: 'Voice',
	api: 'API',
};

// One time format for the conversation, handover and console pages: always the viewer's local time, never with a
// zone name next to it. A page shows its zone ONCE in its header (`zoneLabel()` in `$lib/support/time`).

const msOf = (v: string | number) => (typeof v === 'number' ? v : Date.parse(v));

/** `14:05` today, else `18 Oct 14:05` (local time). Accepts epoch ms or an ISO stamp; `—` for none. */
export function clock(v: number | string | null | undefined, now = Date.now()): string {
	if (v == null || v === '') return '—';
	const ms = msOf(v);
	if (!Number.isFinite(ms)) return String(v);
	const d = new Date(ms);
	const hm = d.toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' });
	if (new Date(now).toDateString() === d.toDateString()) return hm;
	return `${d.toLocaleDateString('en-GB', { day: 'numeric', month: 'short' })} ${hm}`;
}

/** A full timestamp in local time, no zone name: `21 Oct 2026, 14:05`. Epoch ms or ISO; `—` for none. */
export function stamp(v: number | string | null | undefined): string {
	if (v == null || v === '') return '—';
	const ms = msOf(v);
	if (!Number.isFinite(ms)) return String(v);
	return new Date(ms).toLocaleString('en-GB', {
		day: 'numeric',
		month: 'short',
		year: 'numeric',
		hour: '2-digit',
		minute: '2-digit',
	});
}
