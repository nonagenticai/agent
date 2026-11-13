// Number formatters ported from design/admin/ui.js (fmt, lines 5-12).
export const fmt = {
	n: (v: number | null | undefined) => (v == null ? '—' : Number(v).toLocaleString('en-GB')),
	pct: (v: number | null | undefined, d = 1) => (v == null ? '—' : (v * 100).toFixed(d) + '%'),
	ms: (v: number | null | undefined) =>
		v == null ? '—' : v >= 1000 ? (v / 1000).toFixed(v >= 10000 ? 0 : 1) + ' s' : Math.round(v) + ' ms',
	s: (v: number | null | undefined) => (v == null ? '—' : v + ' s'),
	k: (v: number) =>
		v >= 1e6 ? (v / 1e6).toFixed(1) + 'M' : v >= 1e3 ? (v / 1e3).toFixed(1) + 'k' : String(v),
};

/** `p. 2`, `p. 2–3`, or `''` when the chunk has no pages. */
export function pages(start: number | null, end: number | null): string {
	if (start == null) return '';
	return end == null || end === start ? `p. ${start}` : `p. ${start}–${end}`;
}

/**
 * A search hit's citation: `Title · p. 2–3`; the source label stands in for a missing title. With neither (strata's
 * `source` may be empty) only the pages remain, never a dangling ` · `.
 */
export function citation(hit: {
	title: string | null;
	source: string;
	page_start: number | null;
	page_end: number | null;
}): string {
	const p = pages(hit.page_start, hit.page_end);
	const name = hit.title?.trim() || hit.source.trim();
	return [name, p].filter(Boolean).join(' · ');
}

/** A draft revision for display: `r14`, or `r14·3fa9c1` with the draft fingerprint; `—` for none (live). */
export function formatDraftRev(rev: number | null | undefined, fp?: string | null): string {
	if (rev == null) return '—';
	return fp ? `r${rev}·${fp}` : `r${rev}`;
}

/**
 * The one human duration every page shows: `<1 min`, `42 min`, `3 h 4 min`, `19 h`, `1 d 11 h`, `2 d`. Takes
 * minutes; the sign is dropped (the caller says "overdue", "ago", "left"). Under a day: hours and minutes; from a
 * day on: days and hours (the minutes no longer matter). `—` for no value.
 */
export function duration(minutes: number | null | undefined): string {
	if (minutes == null || !Number.isFinite(minutes)) return '—';
	if (Math.abs(minutes) < 1) return '<1 min';
	const m = Math.round(Math.abs(minutes));
	if (m < 60) return `${m} min`;
	if (m < 24 * 60) {
		const h = Math.floor(m / 60);
		const r = m % 60;
		return r ? `${h} h ${r} min` : `${h} h`;
	}
	const d = Math.floor(m / 1440);
	const h = Math.floor((m % 1440) / 60);
	return h ? `${d} d ${h} h` : `${d} d`;
}

/** The duration between two ISO stamps (see `duration`); `''` when either is missing. */
export function durationBetween(from: string | null | undefined, to: string | null | undefined): string {
	if (!from || !to) return '';
	return duration(Math.max(0, Date.parse(to) - Date.parse(from)) / 60_000);
}

/** A message as one sentence: trailing punctuation and space dropped, one full stop added (`files;` → `files.`). */
export function sentence(message: string): string {
	const s = message.trim().replace(/[\s.;:,—-]+$/, '');
	return s ? `${s}.` : '';
}
