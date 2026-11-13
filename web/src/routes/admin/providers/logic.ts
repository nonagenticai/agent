// T6 Providers: pure derivations for the routing map (06 §7, D38; memory usage-funding). Every model call this tenant
// makes is one route: caller use → provider → model. Funding and region are derived, never stored on the route.
// D70: every GPU capability is an external provider (URL + token) — only nonagentic's own tenant runs on mlx_api (LLM: pgx vLLM).
import {
	gpuKindLabel,
	isInternalKind,
	LLM_USE_RULES,
	LLM_USES,
	llmUseProblem,
	type Funding,
	type LlmProvider,
	type LlmProviders,
	type LlmUse,
	type ProviderSlot,
	type UsageReport,
} from '$lib/api/types/tenant';

/** The internal tenant's provider copy (D70): nothing is configured per tenant there. */
export const INTERNAL_COPY =
	'Managed by the platform: pgx vLLM / mlx_api, called with the platform service key; nothing to configure here.';

export type RouteState = 'ok' | 'warn' | 'err';
export type Caller = 'agent' | 'strata_rag' | 'strata_ingest';

export interface Route {
	key: string;
	caller: Caller;
	/** Tab that edits this route. */
	tab: 'llm' | 'knowledge' | 'ingest';
	use: string;
	label: string;
	purpose: string;
	provider: string | null;
	model: string | null;
	funding: Funding | null;
	region: string | null;
	state: RouteState;
	status: string;
	/** What happens when the route has no usable provider. */
	fallback: string;
	/** One line on what to do when the route is not ok; null when healthy. */
	hint: string | null;
	/** 30-day tokens through this route, when the usage ledger attributes them. */
	tokens_30d: number | null;
}

export const PLATFORM_REGION = 'EU (platform)';

export const CALLERS: { key: Caller; label: string; sub: string }[] = [
	{ key: 'agent', label: 'Agent', sub: 'This app: model calls made while handling a turn.' },
	{
		key: 'strata_rag',
		label: 'Knowledge · strata_rag',
		sub: 'Provisioned down: Agent writes these slots into strata_rag for this tenant.',
	},
	{
		key: 'strata_ingest',
		label: 'Ingest · strata_ingest',
		sub: 'Provisioned down: Agent writes these slots into strata_ingest for this tenant.',
	},
];

export const FUNDING: Record<
	Funding,
	{ label: string; short: string; hint: string; variant: 'ok' | 'accent' | 'work' }
> = {
	local_gpu: {
		label: 'Internal GPU',
		short: 'int.',
		hint: 'nonagentic’s own tenant only: the platform’s mlx_api and pgx vLLM, with the platform’s service credential.',
		variant: 'ok',
	},
	platform_external: {
		label: 'Platform external',
		short: 'ext.',
		hint: 'An external API the platform pays for and meters to this tenant.',
		variant: 'accent',
	},
	tenant_byo: {
		label: 'Your key (BYO)',
		short: 'BYO',
		hint: 'Your own provider URL and token; the token is stored sealed and never shown. The provider bills you directly.',
		variant: 'work',
	},
};

/** Display name of a slot kind (D70 provider catalogue). */
export const kindLabel = (k: string): string => gpuKindLabel(k);

export const providerFunding = (p: Pick<LlmProvider, 'token_mode'>): Funding =>
	p.token_mode === 'internal' ? 'local_gpu' : 'tenant_byo';

export const providerRegion = (p: Pick<LlmProvider, 'token_mode' | 'region'>): string | null =>
	p.token_mode === 'internal' ? (p.region ?? PLATFORM_REGION) : (p.region ?? null);

export const slotFunding = (s: Pick<ProviderSlot, 'configured' | 'kind'>): Funding | null =>
	!s.configured ? null : isInternalKind(s.kind) ? 'local_gpu' : 'tenant_byo';

export const slotRegion = (s: Pick<ProviderSlot, 'configured' | 'kind' | 'region'>): string | null =>
	!s.configured ? null : isInternalKind(s.kind) ? (s.region ?? PLATFORM_REGION) : (s.region ?? null);

/** Tokens per provider name over the usage window (the ledger keys rows as "provider · model"). */
export function tokensByProvider(usage: UsageReport | null): Map<string, number> {
	const m = new Map<string, number>();
	for (const r of usage?.rows ?? []) {
		const name = r.key.split(' · ')[0] ?? r.key;
		m.set(name, (m.get(name) ?? 0) + r.tokens);
	}
	return m;
}

/** The whole routing table: Agent's own uses first, then the rag and ingest slots in manifest order. */
export function buildRoutes(
	llm: LlmProviders,
	slots: ProviderSlot[],
	usage: UsageReport | null = null,
): Route[] {
	const tokens = tokensByProvider(usage);
	const out: Route[] = llm.uses.map((u) => {
		const p = u.provider_id ? llm.providers.find((x) => x.id === u.provider_id) : undefined;
		const rule = LLM_USE_RULES[u.use];
		const fallback = rule.fallback;
		const lower = fallback.charAt(0).toLowerCase() + fallback.slice(1);
		let state: RouteState = 'ok';
		let status = 'routed';
		let hint: string | null = null;
		if (!p) {
			state = 'warn';
			status = 'no provider';
			hint = `Falls back to ${lower}. Bind a ${rule.kind === 'laya' ? 'Laya ' : ''}provider.`;
		} else if (p.token_mode === 'tenant_key' && !p.secret.set) {
			state = 'err';
			status = 'key required';
			hint = `Until a key is stored: ${lower}.`;
		} else if (p.last_test && !p.last_test.ok) {
			state = 'err';
			status = 'last test failed';
			hint = p.last_test.detail;
		} else if (!p.last_test) {
			state = 'warn';
			status = 'never tested';
			hint = 'Run Test to prove the token and model answer.';
		}
		return {
			key: `agent.${u.use}`,
			caller: 'agent',
			tab: 'llm',
			use: u.use,
			label: u.label,
			purpose: rule.purpose,
			provider: p?.name ?? null,
			model: p ? u.model : null,
			funding: p ? providerFunding(p) : null,
			region: p ? providerRegion(p) : null,
			state,
			status,
			fallback,
			hint,
			tokens_30d: p ? (tokens.get(p.name) ?? 0) : null,
		};
	});
	for (const s of slots) {
		let state: RouteState = 'ok';
		let status = 'routed';
		let hint: string | null = null;
		const fallback = 'None: work that needs this slot fails';
		if (!s.configured) {
			state = 'err';
			status = 'empty';
			hint = `No fallback: ${s.used_by.join(', ')} fails.`;
		} else if (s.drift) {
			state = 'warn';
			status = `drift: v${s.drift.remote_version} written elsewhere`;
			hint = `${s.drift.changed_by} changed it; Agent never overwrites. Load the current version.`;
		}
		out.push({
			key: `${s.service}.${s.slot}`,
			caller: s.service,
			tab: s.service === 'strata_rag' ? 'knowledge' : 'ingest',
			use: s.slot,
			label: s.label,
			purpose: s.purpose,
			provider: !s.configured || !s.kind ? null : kindLabel(s.kind),
			model: s.model,
			funding: slotFunding(s),
			region: slotRegion(s),
			state,
			status,
			fallback,
			hint,
			tokens_30d: null,
		});
	}
	return out;
}

export interface RouteSummary {
	total: number;
	routed: number;
	attention: number;
	funding: Record<Funding, number>;
}

export function summarize(routes: Route[]): RouteSummary {
	const funding: Record<Funding, number> = { local_gpu: 0, platform_external: 0, tenant_byo: 0 };
	for (const r of routes) if (r.funding) funding[r.funding] += 1;
	return {
		total: routes.length,
		routed: routes.filter((r) => r.provider).length,
		attention: routes.filter((r) => r.state !== 'ok').length,
		funding,
	};
}

/** `1 904 112` → `1.9 M`, `48 200` → `48.2 k`. */
export const compact = (n: number): string =>
	n >= 1e6 ? `${(n / 1e6).toFixed(1)} M` : n >= 1e3 ? `${(n / 1e3).toFixed(1)} k` : String(n);

// ---- "Use for" (drawer) and the Bind call-out ------------------------------------------------------------------

/** One use in the drawer's "Use for" section. */
export interface UseForChoice {
	on: boolean;
	model: string;
}

/** "Use for" as the drawer opens: checked (with its model) where the edited provider is already bound. */
export function useForInitial(
	v: LlmProviders | null,
	editingId: string | null,
): Record<LlmUse, UseForChoice> {
	const out = {} as Record<LlmUse, UseForChoice>;
	for (const use of LLM_USES) {
		const b = v?.uses.find((u) => u.use === use);
		const here = !!editingId && b?.provider_id === editingId;
		out[use] = { on: here, model: here ? (b?.model ?? '') : '' };
	}
	return out;
}

/**
 * The bind to send after the provider `savedId` was saved, or null when nothing changes. Checked binds it (model
 * blank = the provider's first); unchecking a use this provider serves unbinds it; unchecked and bound elsewhere
 * leaves that binding alone. A use the provider's kind cannot serve is never bound.
 */
export function useForPlan(
	v: LlmProviders | null,
	editingId: string | null,
	use: LlmUse,
	choice: UseForChoice,
	saved: Pick<LlmProvider, 'id' | 'kind' | 'models'>,
): { providerId: string | null; model: string | null } | null {
	const b = v?.uses.find((u) => u.use === use);
	const here = !!editingId && b?.provider_id === editingId;
	if (choice.on && !llmUseProblem(use, saved.kind)) {
		const model = saved.models.includes(choice.model.trim())
			? choice.model.trim()
			: (saved.models[0] ?? null);
		return here && (b?.model ?? null) === model ? null : { providerId: saved.id, model };
	}
	return here ? { providerId: null, model: null } : null;
}

/** An unbound use, loudly: what runs instead, and the provider the Bind button would use (first of the right kind). */
export interface UnboundUse {
	use: LlmUse;
	label: string;
	text: string;
	candidate: { id: string; name: string; model: string | null } | null;
	/** The kind a provider needs to serve this use (for "Add a Laya provider"). */
	kind: LlmProvider['kind'];
}

export function unboundUses(v: LlmProviders): UnboundUse[] {
	return v.uses
		.filter((u) => !u.provider_id)
		.map((u) => {
			const rule = LLM_USE_RULES[u.use];
			const p = v.providers.find((x) => !llmUseProblem(u.use, x.kind));
			return {
				use: u.use,
				label: rule.label,
				kind: rule.kind,
				text:
					u.use === 'gates'
						? 'Gates are not bound: Laya gates are off and no gate question runs.'
						: u.use === 'qualify'
							? 'Turn qualification is not bound: customer messages get "the assistant is unavailable" until an LLM provider is bound.'
							: `${rule.label} is not bound: ${rule.fallback.replace(/^None: /, '').toLowerCase()}.`,
				candidate: p ? { id: p.id, name: p.name, model: p.models[0] ?? null } : null,
			};
		});
}
