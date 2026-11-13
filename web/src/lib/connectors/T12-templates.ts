// T12 Tenant connector templates — pure view logic: group versions into families, diff a version against the one
// before it, and spell out each family's upgrade path (which systems pin what, and what has to happen before they
// can move). Only T12 uses this file.
import type {
	AuthKind,
	TemplateStatus,
	TenantConnectorTemplate,
	TenantTemplateUsage,
} from '$lib/api/types/connectors';
import { opKey } from './ops';

export type T12Pin = TenantTemplateUsage['pins'][number];
export type T12Route = TenantTemplateUsage['routes'][number];

export interface T12Family {
	key: string;
	label: string;
	kind: 'generic' | 'custom';
	description: string;
	/** Oldest first. */
	versions: TenantConnectorTemplate[];
	pins: T12Pin[];
	routes: T12Route[];
	upgrade: T12Upgrade | null;
}

/** What stands between the pinned systems and the newest version of a family. */
export interface T12Upgrade {
	from: string;
	to: string;
	toStatus: TemplateStatus;
	systems: string[];
	/** `ready` = the newer version is active: re-pin on the system page. `blocked` = it is still a draft. */
	state: 'ready' | 'blocked';
	next: string;
}

const num = (v: string) => Number(/(\d+)$/.exec(v)?.[1] ?? 0);

export function familiesT12(
	items: TenantConnectorTemplate[],
	usage: TenantTemplateUsage | null,
): T12Family[] {
	const byKey = new Map<string, TenantConnectorTemplate[]>();
	for (const t of items) byKey.set(t.key, [...(byKey.get(t.key) ?? []), t]);
	const out: T12Family[] = [];
	for (const [key, vs] of byKey) {
		const versions = [...vs].sort((a, b) => num(a.version) - num(b.version));
		const newest = versions[versions.length - 1]!;
		const pins = (usage?.pins ?? []).filter((p) => p.key === key);
		const ids = new Set(pins.map((p) => p.instance_id));
		const routes = (usage?.routes ?? []).filter((r) => ids.has(r.instance_id));
		out.push({
			key,
			label: newest.label,
			kind: newest.kind,
			description: (versions.find((v) => v.status === 'active') ?? newest).description,
			versions,
			pins,
			routes,
			upgrade: upgradeT12(versions, pins),
		});
	}
	// Families in use first, then by key.
	return out.sort((a, b) => b.pins.length - a.pins.length || a.key.localeCompare(b.key));
}

export function upgradeT12(versions: TenantConnectorTemplate[], pins: T12Pin[]): T12Upgrade | null {
	if (!pins.length) return null;
	const newer = versions.filter((v) => v.status !== 'retired');
	const target = newer[newer.length - 1];
	if (!target) return null;
	const behind = pins.filter((p) => num(p.version) < num(target.version));
	if (!behind.length) return null;
	const from = behind[0]!.version;
	const ready = target.status === 'active';
	const valid = target.validation && target.validation.errors.length === 0;
	const next = ready
		? `Re-pin ${behind.map((p) => p.instance_id).join(', ')} to @${target.version} on its system page; T3 re-runs there.`
		: !target.validation
			? `Validate @${target.version}, run its contract tests and activate it — then re-pin.`
			: !valid
				? `Fix ${target.validation.errors.length} validation error(s) on @${target.version} first.`
				: target.contract?.status !== 'green'
					? `Run the contract tests on @${target.version}, then activate and re-pin.`
					: `Activate @${target.version}, then re-pin.`;
	return {
		from,
		to: target.version,
		toStatus: target.status,
		systems: behind.map((p) => p.instance_id),
		state: ready ? 'ready' : 'blocked',
		next,
	};
}

export interface T12Diff {
	base: string | null;
	added: string[];
	removed: string[];
	changed: string[];
	same: number;
}

/** Operations of `cur` compared with the version just before it in the family. */
export function diffT12(versions: TenantConnectorTemplate[], cur: TenantConnectorTemplate): T12Diff {
	const i = versions.findIndex((v) => v.version === cur.version);
	const prev = i > 0 ? versions[i - 1]! : null;
	if (!prev) return { base: null, added: cur.ops.map(opKey), removed: [], changed: [], same: 0 };
	const before = new Map(prev.ops.map((o) => [opKey(o), JSON.stringify(o)]));
	const after = new Map(cur.ops.map((o) => [opKey(o), JSON.stringify(o)]));
	const added = [...after.keys()].filter((k) => !before.has(k));
	const removed = [...before.keys()].filter((k) => !after.has(k));
	const changed = [...after.keys()].filter((k) => before.has(k) && before.get(k) !== after.get(k));
	return {
		base: prev.version,
		added,
		removed,
		changed,
		same: [...after.keys()].filter((k) => before.get(k) === after.get(k)).length,
	};
}

export const AUTH_T12: Record<AuthKind, string> = {
	none: 'No authentication',
	api_key_header: 'API key in a header',
	basic: 'HTTP basic (username + password)',
	bearer_static: 'Static bearer token',
	oauth2_client_credentials: 'OAuth2 client credentials',
	bundle_token: 'Platform licence bundle',
	licence_bearer: 'Platform licence, minted per call',
	mtls: 'Mutual TLS (client certificate + key)',
};

/** Where an operation is routed: `job_label · env` per route, grouped by op key. */
export function routedT12(routes: T12Route[]): Map<string, T12Route[]> {
	const m = new Map<string, T12Route[]>();
	for (const r of routes) m.set(r.op, [...(m.get(r.op) ?? []), r]);
	return m;
}
