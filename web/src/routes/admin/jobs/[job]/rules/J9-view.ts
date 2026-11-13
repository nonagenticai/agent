// J9 view helpers: outcome colours, the rule statement split into text + value slots, and the template layer link.
import type { BusinessRule, RuleEvaluation } from '$lib/api/types/release';

export type OutcomeVariant = 'ok' | 'err' | 'warn' | 'muted';

export function outcomeVariant(o: RuleEvaluation['outcome'] | string): OutcomeVariant {
	if (o === 'eligible') return 'ok';
	if (o === 'ineligible' || o.startsWith('ineligible:') || o.startsWith('not_eligible')) return 'err';
	if (o === 'escalate' || o === 'placeholder') return 'warn';
	return 'muted';
}

export type StatementPart =
	{ text: string } | { key: string; label: string; value: string | null; unit: string };

/** `A return is accepted up to {window_days} days…` → text and value parts (unknown keys stay literal). */
export function statementParts(r: BusinessRule): StatementPart[] {
	if (!r.statement) return [];
	const parts: StatementPart[] = [];
	const re = /\{([a-z0-9_]+)\}/g;
	let last = 0;
	for (let m = re.exec(r.statement); m; m = re.exec(r.statement)) {
		const v = r.values.find((x) => x.key === m![1]);
		if (!v) continue;
		if (m.index > last) parts.push({ text: r.statement.slice(last, m.index) });
		parts.push({ key: v.key, label: v.label, value: v.value, unit: v.unit });
		last = m.index + m[0].length;
	}
	if (last < r.statement.length) parts.push({ text: r.statement.slice(last) });
	return parts;
}

/** `industry/retail@v2` → `{ layer: 'industry/retail', v: 2 }`. */
export function parseLayer(s: string | undefined): { layer: string; v: number } | null {
	const m = /^(.+)@v(\d+)$/.exec(s ?? '');
	return m ? { layer: m[1]!, v: Number(m[2]) } : null;
}

/** `30` + `days` → `30 days`; list/text units show the value only. */
export function withUnit(value: string, unit: string): string {
	return ['text', 'list'].includes(unit) ? value : unit === 'EUR' ? `€${value}` : `${value} ${unit}`;
}
