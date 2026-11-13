// One time format for the staff workspace: always the viewer's local time. Pages show the zone ONCE in their header
// (`zoneLabel()`); a time next to a time never repeats it.
import { clock } from '$lib/conversations/labels';

/** The viewer's IANA zone, e.g. `Europe/Amsterdam`. */
export function zoneLabel(): string {
	try {
		return new Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
	} catch {
		return 'UTC';
	}
}

/** `14:05` today, else `18 Oct 14:05` (local time, no zone suffix). `—` for none. */
export function clockOf(iso: string | number | null | undefined, now = Date.now()): string {
	return clock(iso, now);
}

/** `just now`, `5 min ago`, `3 h ago`, `2 d ago` — or `in 30 min` for a time ahead (an SLA due time). */
export function relative(iso: string | number | null | undefined, now = Date.now()): string {
	if (iso == null || iso === '') return '—';
	const ms = typeof iso === 'number' ? iso : Date.parse(iso);
	if (!Number.isFinite(ms)) return String(iso);
	const diff = now - ms;
	const abs = Math.abs(diff);
	const min = Math.round(abs / 60_000);
	if (abs < 45_000) return 'just now';
	const unit =
		min < 60
			? `${Math.max(min, 1)} min`
			: min < 60 * 24
				? `${Math.round(min / 60)} h`
				: `${Math.round(min / 1440)} d`;
	return diff >= 0 ? `${unit} ago` : `in ${unit}`;
}
