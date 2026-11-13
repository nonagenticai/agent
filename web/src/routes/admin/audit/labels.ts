// T4 status + actor-type vocabularies (04 §9b, 12 §9). Sentences, families and links live in $lib/tenant/auditT4.
export const ACTOR_TYPES: [string, string][] = [
	['human', 'Person'],
	['ai_agent', 'AI agent'],
	['system', 'System'],
];
export const STATUSES: [string, string][] = [
	['success', 'Success'],
	['failure', 'Failure'],
	['denied', 'Denied'],
	['error', 'Error'],
	['fallback', 'Fallback'],
];
const find = (list: [string, string][], k: string) =>
	list.find(([x]) => x === k)?.[1] ?? k.replaceAll('_', ' ');
export const actorTypeLabel = (t: string) => find(ACTOR_TYPES, t);
export const statusLabel = (t: string) => find(STATUSES, t);
export const statusVariant = (s: string): 'ok' | 'warn' | 'err' | 'muted' =>
	s === 'success'
		? 'ok'
		: s === 'fallback'
			? 'warn'
			: s === 'denied' || s === 'failure' || s === 'error'
				? 'err'
				: 'muted';
