// Small helpers the ontology-area pages share (J7, J8, J10, J11, J12, P14, P15). Owned by the ontology area.
import { ApiError } from '$lib/api/errors';
import type { Origin, RouteTarget, SideEffect } from '$lib/api/types/ontology';
import { ROLE_LABEL, type Role } from '$lib/api/types/core';

/** Platform object keys (spec 14 §4): what an ontology entity can be a view of (owner call #8, P10). */
export const OBJECT_KEYS = [
	'knowledge',
	'offering',
	'party',
	'credential',
	'contact_point',
	'consent',
	'order',
	'delivery',
	'payment',
	'rule',
	'case',
	'document',
	'contract',
	'interaction',
	'appointment',
	'access_grant',
	'location',
	'service_event',
	'asset',
	'usage',
	'arrangement',
	'promotion',
] as const;

/** A refused write as the inline message a form shows (the code stays visible for support). */
export function explain(e: unknown, what: string): string {
	if (e instanceof ApiError && e.status === 403) {
		const label = e.missingRole ? ROLE_LABEL[e.missingRole as Role] : null;
		return `You need ${label ? `the ${label} role` : 'another role'} to ${what}.`;
	}
	if (e instanceof ApiError) return `${e.message} (${e.code})`;
	return e instanceof Error ? e.message : String(e);
}

export const ORIGIN_LABEL: Record<Origin, string> = { base: 'Base', tenant: 'Tenant' };

export const SIDE_EFFECT_LABEL: Record<SideEffect, string> = {
	R: 'R · read',
	W: 'W · write',
	X: 'X · irreversible',
	P: 'P · payment',
};

export const SIDE_EFFECT_VARIANT: Record<SideEffect, 'ok' | 'cust' | 'warn' | 'err'> = {
	R: 'ok',
	W: 'cust',
	X: 'warn',
	P: 'err',
};

/** Human text for a route target (N4). */
export function routeText(r: RouteTarget): string {
	switch (r.kind) {
		case 'instance':
			return r.instance;
		case 'builtin':
			return 'builtin (agent)';
		case 'fallback':
			return 'agent_cases (fallback)';
		case 'deep_link':
			return `deep link · ${r.url.replace(/^https:\/\//, '')}`;
		case 'not_offered':
			return 'not offered';
		case 'unrouted':
			return 'no route';
	}
}

/** `2026-10-16 14:02` from an ISO timestamp. */
export const when = (iso: string | null) => (iso ? iso.replace('T', ' ').slice(0, 16) : '—');

/** `Support` from `support`, `Start return` from `start_return`. */
export const title = (v: string) =>
	v
		.split(/[-_\s]+/)
		.filter(Boolean)
		.map((w) => w[0]!.toUpperCase() + w.slice(1))
		.join(' ');
