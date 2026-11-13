// Rate-limit and contract-probe helpers for the T11 system page / template pages.
import type { RateLimitModel } from '$lib/api/types/connectors';

/** A template with no rate-limit model is stored (and served) as `{}`; `null`/`undefined` mean the same. */
export function hasRateLimit(rl: RateLimitModel | null | undefined): rl is RateLimitModel {
	return !!rl && !!rl.kind && rl.kind !== 'none';
}

export const NO_RATE_LIMIT = 'No rate limit declared';

/** `rps 100 rps live / 25 sandbox`, or the no-limit text. */
export function rateLimitText(rl: RateLimitModel | null | undefined): string {
	if (!hasRateLimit(rl)) return NO_RATE_LIMIT;
	return [rl.kind, rl.value].filter(Boolean).join(' ');
}

/** Short kind label for the chain node (`cost bucket`), or '' when none is declared. */
export function rateLimitKind(rl: RateLimitModel | null | undefined): string {
	return hasRateLimit(rl) ? rl.kind!.replace('_', ' ') : '';
}

/**
 * The env ids a `probe_input_missing` outcome asks for. The server carries the key only inside the note
 * (`Set the env id "probe.<field>" ...`), not as a field of its own, so it is read from there.
 */
export function missingProbeKeys(code: string | null | undefined, note: string | null | undefined): string[] {
	if (code !== 'probe_input_missing' || !note) return [];
	return [...new Set(note.match(/probe\.[A-Za-z0-9_.-]*[A-Za-z0-9_]/g) ?? [])];
}

/** Append `key = ` to the env-ids textarea text unless the key is already a line there. */
export function addEnvIdLine(text: string, key: string): string {
	if (text.split('\n').some((l) => l.trim().startsWith(`${key} =`) || l.trim().startsWith(`${key}=`)))
		return text;
	const base = text.trimEnd();
	return `${base}${base ? '\n' : ''}${key} = `;
}

/** Keys the Test records section shows: those a result asked for plus the probe.* ids already set, in that order. */
export function probeKeys(missing: string[], envIds: Record<string, string>): string[] {
	return [...new Set([...missing, ...Object.keys(envIds).filter((k) => k.startsWith('probe.'))])];
}

/** `probe.case_id` -> `Case ID`; `probe.order_ref` -> `Order ref`. */
export function probeLabel(key: string): string {
	const words = key
		.replace(/^probe\./, '')
		.split(/[_.\-\s]+/)
		.filter(Boolean)
		.map((w) => (/^(id|ids|url|sku|api|uuid)$/i.test(w) ? w.toUpperCase() : w.toLowerCase()));
	const text = words.join(' ');
	return text ? text.charAt(0).toUpperCase() + text.slice(1) : key;
}

/** Write the probe values into the env ids; other ids are kept; a blank value removes that probe id. */
export function mergeProbeIds(
	envIds: Record<string, string>,
	values: Record<string, string>,
): Record<string, string> {
	const out = { ...envIds };
	for (const [k, v] of Object.entries(values)) {
		const t = v.trim();
		if (t) out[k] = t;
		else delete out[k];
	}
	return out;
}
