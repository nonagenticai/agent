// P6 Templates — pure view rules over the template catalogue (D15, D26, 11 §4.1):
//   - a job's lineage: the one tag it pins per layer, stacked base → industry → department → job;
//   - a layer's tag timeline: immutable tags oldest first, the jobs sitting on each, the untagged draft after them;
//   - the rollout waves (customer care → IT service desk → other job areas) and how far each has got.
// Owned by the P6 page agent.
import type {
	RolloutWave,
	TemplateCatalog,
	TemplateLayerSummary,
	TemplatePinRow,
	TemplateScope,
	TemplateTag,
} from '$lib/api/types/model';
import type { JobKey, TenantId, TenantStatus } from '$lib/api/types/core';

/** Composition order: later layers override earlier ones. */
export const STACK_ORDER: TemplateScope[] = ['base', 'industry', 'base_dept', 'industry_dept', 'job'];

export interface StackSlot {
	scope: TemplateScope;
	/** The layer this job pins at this scope, or null when the job has no layer of that scope. */
	layer: TemplateLayerSummary | null;
	pin: TemplatePinRow | null;
}

export interface JobStack {
	tenant_id: TenantId;
	tenant_name: string;
	tenant_status: TenantStatus;
	job: JobKey;
	job_label: string;
	/** One slot per scope, in STACK_ORDER. */
	slots: StackSlot[];
	/** Layers where a newer tag exists than the pin (drives the tenant's K5 base-changed badge). */
	behind: number;
}

/** Every pinning job with its layer stack, behind-latest jobs first, archived tenants last. */
export function jobStacks(c: TemplateCatalog): JobStack[] {
	const byJob = new Map<string, JobStack>();
	for (const l of c.layers)
		for (const p of l.pins) {
			const id = `${p.tenant_id}\u0000${p.job}`;
			let s = byJob.get(id);
			if (!s) {
				s = {
					tenant_id: p.tenant_id,
					tenant_name: p.tenant_name,
					tenant_status: p.tenant_status,
					job: p.job,
					job_label: p.job_label,
					slots: STACK_ORDER.map((scope) => ({ scope, layer: null, pin: null })),
					behind: 0,
				};
				byJob.set(id, s);
			}
			const slot = s.slots[STACK_ORDER.indexOf(l.scope)]!;
			slot.layer = l;
			slot.pin = p;
			if (p.behind > 0) s.behind++;
		}
	const archived = (s: JobStack) => (s.tenant_status === 'archived' ? 1 : 0);
	return [...byJob.values()].sort(
		(a, b) => archived(a) - archived(b) || b.behind - a.behind || a.tenant_name.localeCompare(b.tenant_name),
	);
}

export interface TimelineNode {
	tag: TemplateTag;
	latest: boolean;
	/** Jobs pinned exactly on this tag. */
	pins: TemplatePinRow[];
}

export interface LayerTimeline {
	nodes: TimelineNode[];
	/** The untagged revisions that the next Package step turns into `@v{next}`. */
	draft: { next: number; count: number } | null;
}

/** A layer's tags oldest first, each with the jobs pinned on it, then the pending draft. */
export function layerTimeline(l: TemplateLayerSummary): LayerTimeline {
	const tags = [...l.tags].sort((a, b) => a.version - b.version);
	const latest = l.latest?.version ?? null;
	return {
		nodes: tags.map((tag) => ({
			tag,
			latest: tag.version === latest,
			pins: l.pins.filter((p) => p.version === tag.version),
		})),
		draft: l.draft_revisions.length ? { next: (latest ?? 0) + 1, count: l.draft_revisions.length } : null,
	};
}

export type WaveStatus = 'done' | 'current' | 'todo';

export interface Wave {
	wave: RolloutWave | null;
	label: string;
	status: WaveStatus;
	layers: TemplateLayerSummary[];
	/** Tenants with a job pinned on a layer of this wave (its non-base layers when it has any). */
	tenants: string[];
}

/**
 * The D26 rollout waves. A wave is settled once every layer has a real (non-provisional) tag, no generator run is
 * open on it, and at least one job pins a real tag of it (the wave reached a tenant). Later drafts are maintenance,
 * not rollout. Waves complete in order: a settled wave is `done` only when every earlier wave is done, otherwise it
 * is `current` (started early). A wave without layers is `todo`.
 */
export function rolloutWaves(c: TemplateCatalog, labels: Record<RolloutWave, string>): Wave[] {
	let earlierDone = true;
	const waves: Wave[] = ([1, 2, 3] as RolloutWave[]).map((wave) => {
		const layers = c.layers.filter((l) => l.rollout_wave === wave);
		const settled = layers.every(
			(l) =>
				l.latest !== null &&
				!l.latest.provisional &&
				l.generator === null &&
				l.pins.some((p) => !l.tags.find((t) => t.version === p.version)?.provisional),
		);
		const status: WaveStatus = layers.length === 0 ? 'todo' : settled && earlierDone ? 'done' : 'current';
		earlierDone = status === 'done';
		// base is pinned by every job; name a wave's tenants by its specific layers when it has any
		const specific = layers.some((l) => l.scope !== 'base')
			? layers.filter((l) => l.scope !== 'base')
			: layers;
		const tenants = [...new Set(specific.flatMap((l) => l.pins.map((p) => p.tenant_name)))].sort();
		return { wave, label: layers[0]?.rollout_label ?? labels[wave], status, layers, tenants };
	});
	// Freshly authored families need no rollout metadata. Keep their draft/tag controls reachable.
	const layers = c.layers.filter((l) => l.rollout_wave == null);
	if (layers.length)
		waves.unshift({
			wave: null,
			label: 'Template layers',
			status: 'current',
			layers,
			tenants: [...new Set(layers.flatMap((l) => l.pins.map((p) => p.tenant_name)))].sort(),
		});
	return waves;
}

/** Counts for the Revise → Tag → Pin → Re-pin pipeline band. */
export function pipelineCounts(c: TemplateCatalog) {
	const stacks = jobStacks(c);
	return {
		drafts: c.layers.reduce((n, l) => n + l.draft_revisions.length, 0),
		tags: c.layers.reduce((n, l) => n + l.tags.length, 0),
		pins: c.layers.reduce((n, l) => n + l.pins.length, 0),
		jobs: stacks.length,
		behindPins: c.layers.reduce((n, l) => n + l.behind_jobs, 0),
		behindJobs: stacks.filter((s) => s.behind > 0).length,
	};
}
