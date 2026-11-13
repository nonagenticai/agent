// Customer display-name fallback (staff workspace): a known name wins; otherwise a short, readable handle — never a
// raw `wc:…` or `tg-…` id. Web chat reads `Web visitor · 4f2a`, every other channel `{Channel} user · 3456`.
import { CHANNEL_LABEL } from '$lib/conversations/labels';

export function displayName(
	name: string | null | undefined,
	channel: string,
	conversationId: string,
): string {
	const n = name?.trim();
	if (n) return n;
	const tail = conversationId.slice(-4);
	if (channel === 'web_chat') return `Web visitor · ${tail}`;
	const label = (CHANNEL_LABEL as Record<string, string>)[channel] ?? channel;
	return `${label} user · ${tail}`;
}
