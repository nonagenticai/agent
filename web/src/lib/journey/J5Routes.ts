// J5 Activity detail — how a route and a side effect read on the page (shared by the flow and the operations table).
import type { OperationRouteInfo } from '$lib/api/types/journey';

export const ROUTE_CHIP: Record<OperationRouteInfo['kind'], string> = {
	instance: 'ok',
	builtin: 'tool',
	fallback: 'accent',
	deep_link: 'muted',
	not_offered: 'muted',
	unrouted: 'err',
};

export function routeText(r: OperationRouteInfo): string {
	if (r.kind === 'unrouted') return 'no route';
	if (r.kind === 'not_offered') return 'not offered';
	if (r.kind === 'deep_link') return 'deep link';
	if (r.kind === 'builtin') return 'built-in';
	if (r.kind === 'fallback') return `${r.target} (fallback)`;
	return r.target ?? r.kind;
}

export const EFFECT_TEXT: Record<'R' | 'W' | 'X' | 'P', string> = {
	R: 'reads',
	W: 'writes',
	X: 'irreversible',
	P: 'payment',
};

/** `88 %` vs `≥ 85 %` → progress 0–1 towards the target (null when either side is not a number). */
export function towardsTarget(current: string | null, target: string): number | null {
	const c = current ? parseFloat(current.replace(/[^\d.]/g, '')) : NaN;
	const t = parseFloat(target.replace(/[^\d.]/g, ''));
	if (Number.isNaN(c) || Number.isNaN(t) || t === 0) return null;
	return Math.min(1, c / t);
}

export const pct = (n: number, of: number) => (of ? `${Math.round((n / of) * 100)} %` : '—');
