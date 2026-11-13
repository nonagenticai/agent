// Platform knowledge (PK) helpers the pages and the fixture share: the rules the API checks (pack keys, supersedes
// lists, curator documents) mirrored for early feedback, pack key families, and how a citation names its layer.
import type { AnswerSource, Provenance } from '$lib/api/types/grounding';
import type { KnowledgePackPin, PackLayerFamily } from '$lib/api/types/ontology';

/** Pack keys (`kinds::valid_pack_key`): 1–6 `/` segments of a-z 0-9 _ -. */
export const PACK_KEY_RE = /^[a-z0-9_-]+(\/[a-z0-9_-]+){0,5}$/;
/** An exclusion prefix is a pack key of at most 512 characters (T7, 422 `invalid_key_prefix`). */
export const MAX_PREFIX_CHARS = 512;
/** An exclusion reason is 1–500 characters (T7, 422 `invalid_reason`). */
export const MAX_REASON_CHARS = 500;
/** `supersedes` (T6 `normalize_supersedes`): at most 32 keys of 1–128 characters, no control characters. */
export const MAX_SUPERSEDES = 32;
export const MAX_SUPERSEDES_CHARS = 128;
/** A platform document names 1–32 topic keys (T8). */
export const MAX_TOPIC_KEYS = 32;

export function checkPrefix(raw: string): string | null {
	const p = raw.trim();
	if (!p) return 'Name a pack key or a key prefix.';
	if (p.length > MAX_PREFIX_CHARS) return `A prefix is at most ${MAX_PREFIX_CHARS} characters.`;
	if (!PACK_KEY_RE.test(p))
		return `“${p}” is not a pack key: 1–6 segments of a-z, 0-9, _ or - separated by /.`;
	return null;
}

export function checkReason(raw: string): string | null {
	const r = raw.trim();
	if (!r) return 'Say why (the reason is kept with the exclusion).';
	if ([...r].length > MAX_REASON_CHARS) return `A reason is at most ${MAX_REASON_CHARS} characters.`;
	return null;
}

/** A topic key (T6b `knowledge::topic_keys::valid`, strata's rule): 1–128 characters of `[a-z0-9_./-]`. */
export const TOPIC_KEY_RE = /^[a-z0-9_./-]{1,128}$/;

/**
 * A topic list as the API stores it (T6b `topic_keys::normalize`, one validator for `supersedes` and curator
 * `topic_keys`): each key trimmed and checked, then sorted and de-duplicated; at most `max` keys counted after
 * de-duplication. `{keys}` or `{error}` naming the first rule broken.
 */
export function normalizeTopics(
	raw: readonly string[],
	max = MAX_SUPERSEDES,
): { keys: string[]; error: null } | { keys: null; error: string } {
	const trimmed = raw.map((k) => k.trim());
	const bad = trimmed.find((k) => !TOPIC_KEY_RE.test(k));
	if (bad !== undefined)
		return {
			keys: null,
			error: `“${[...bad].slice(0, 80).join('')}” is not a topic key: 1 to ${MAX_SUPERSEDES_CHARS} characters of [a-z0-9_./-].`,
		};
	const out = [...new Set(trimmed)].sort();
	if (out.length > max) return { keys: null, error: `At most ${max} topic keys.` };
	return { keys: out, error: null };
}

/** Comma / newline separated text → topic keys (blank pieces dropped). */
export const splitTopics = (text: string): string[] =>
	text
		.split(/[,\n]/)
		.map((x) => x.trim())
		.filter(Boolean);

/** The layer family a pack key belongs to (its first segment), or `other`. */
export function familyOf(key: string): PackLayerFamily | 'other' {
	const first = key.split('/')[0];
	return first === 'base' ||
		first === 'industry' ||
		first === 'base_dept' ||
		first === 'industry_dept' ||
		first === 'job'
		? first
		: 'other';
}
export const FAMILY_ORDER: readonly (PackLayerFamily | 'other')[] = [
	'base',
	'industry',
	'base_dept',
	'industry_dept',
	'job',
	'other',
];
export const FAMILY_LABEL: Record<PackLayerFamily | 'other', string> = {
	base: 'Base',
	industry: 'Industry',
	base_dept: 'Base department',
	industry_dept: 'Industry department',
	job: 'Job',
	other: 'Other',
};

/** Segment-wise prefix match (T7): `industry/retail` covers `industry/retail/hr`, never `industry/retailx`. */
export const covers = (prefix: string, key: string) => key === prefix || key.startsWith(`${prefix}/`);

/** The shortest exclusion prefix covering `key` (the one that hides it), or null. */
export function excludedBy(key: string, prefixes: readonly string[]): string | null {
	const hit = prefixes.filter((p) => covers(p, key)).sort((a, b) => a.length - b.length);
	return hit[0] ?? null;
}

/** Why a pin is searched without, in words (null = it is searched). Order: hidden, excluded, unreadable, state. */
export function pinBlocked(
	p: KnowledgePackPin,
): { chip: string; title: string; variant: 'warn' | 'err' | 'muted' } | null {
	if (p.tombstoned)
		return {
			chip: 'hidden',
			title: 'Hidden for this job (draft refinement): searches run without it.',
			variant: 'muted',
		};
	if (p.excluded_by)
		return {
			chip: `excluded · ${p.excluded_by}`,
			title: `Your tenant excludes ${p.excluded_by} on Platform knowledge: no job searches it.`,
			variant: 'warn',
		};
	if (p.unreadable)
		return {
			chip: 'unavailable',
			title: 'strata could not read this pack the last time: searches run without it',
			variant: 'err',
		};
	if (p.state === 'missing')
		return {
			chip: 'missing',
			title: 'strata does not know this pack: searches run without it.',
			variant: 'err',
		};
	if (p.state === 'draft')
		return { chip: 'draft', title: 'Not published yet: searches run without it.', variant: 'warn' };
	if (p.state === 'retired' || (!p.listed && !p.state))
		return {
			chip: 'retired',
			title: 'No longer listed for this tenant: stays pinned and readable',
			variant: 'muted',
		};
	return null;
}

/** A citation's layer chip: `platform · <pack>` for platform hits, `tenant` otherwise. */
export function layerChip(s: { layer?: AnswerSource['layer'] | null; pack?: string | null }): string {
	return s.layer === 'platform' ? `platform · ${s.pack ?? 'pack'}` : 'tenant';
}

/** A provenance link only for an http(s) URL (never `javascript:`); null otherwise. */
export function provenanceHref(p: Provenance | null | undefined): string | null {
	if (!p?.source_url) return null;
	try {
		const u = new URL(p.source_url);
		return u.protocol === 'https:' || u.protocol === 'http:' ? u.href : null;
	} catch {
		return null;
	}
}

/** `CC-BY-4.0 · retrieved 2026-10-01` — what a citation shows next to its source link. */
export function provenanceText(p: Provenance | null | undefined): string {
	if (!p) return '';
	const day = p.retrieved_at ? p.retrieved_at.slice(0, 10) : '';
	return [p.license, day ? `retrieved ${day}` : ''].filter(Boolean).join(' · ');
}

/** A curator document check before the API (T8 shape rules; the licence allow-list is strata's). */
export function checkCuratorDocument(d: {
	title: string;
	text: string;
	topic_keys: string[];
	source_url: string;
	license: string;
	retrieved_at: string;
}): string[] {
	const errs: string[] = [];
	if (!d.title.trim()) errs.push('Give the document a title.');
	if (!d.text.trim()) errs.push('Paste the document text.');
	if (!d.topic_keys.length)
		errs.push('Name at least one topic key (tenants replace platform text by topic).');
	else {
		const t = normalizeTopics(d.topic_keys, MAX_TOPIC_KEYS);
		if (t.error) errs.push(t.error);
	}
	try {
		if (new URL(d.source_url.trim()).protocol !== 'https:') errs.push('The source URL must be https.');
	} catch {
		errs.push('The source URL must be an https URL.');
	}
	if (!d.license.trim()) errs.push('Pick the licence the text is published under.');
	if (!d.retrieved_at || Number.isNaN(Date.parse(d.retrieved_at)))
		errs.push('Give the date it was retrieved.');
	return errs;
}

/** `2026-10-01` (a date input) → RFC 3339 midnight UTC; a full timestamp passes through. */
export const rfc3339 = (day: string) => (/^\d{4}-\d{2}-\d{2}$/.test(day) ? `${day}T00:00:00Z` : day);
