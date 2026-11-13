// T1 Jobs board: a job's lifecycle stage, its template stack and the one next step the tenant_admin should take.
// Pure functions (tested in tests/unit/tenant-T1.test.ts). Owned by the T1 page.
import type { JobBoardRow } from '$lib/api/types/tenant';
import { pinTag, SELECTION_VALUE_KEYS, selectionValues, type TemplatePin } from '$lib/api/types/core';
import { jobPath } from '$lib/scope';

/** draft → ready → released → serving (05 §4.5, K41): draft = no release cut yet; serving = agents on the bus. */
export type JobStage = 'draft' | 'ready' | 'released' | 'serving';

export const STAGES: { key: JobStage; label: string; hint: string }[] = [
	{ key: 'draft', label: 'Draft', hint: 'Selection and refinements, nothing cut yet' },
	{ key: 'ready', label: 'Ready', hint: 'Readiness checks pass; a release can be applied' },
	{ key: 'released', label: 'Released', hint: 'A release is current' },
	{ key: 'serving', label: 'Serving', hint: 'Agents answer customers with the current release' },
];

export function jobStage(r: JobBoardRow): JobStage {
	if (r.job.current_release) return r.job.on_bus ? 'serving' : 'released';
	return r.readiness.ready ? 'ready' : 'draft';
}

/** Template layers compose base → industry → department → job; later layers override earlier ones. */
export type LayerKind = 'base' | 'industry' | 'department' | 'job';
const LAYER_ORDER: LayerKind[] = ['base', 'industry', 'department', 'job'];
export const LAYER_LABEL: Record<LayerKind, string> = {
	base: 'Base',
	industry: 'Industry',
	department: 'Department',
	job: 'Job',
};

export function layerKind(layer: string): LayerKind {
	if (layer.startsWith('job/')) return 'job';
	if (layer.startsWith('industry_dept/')) return 'department';
	if (layer.startsWith('industry/')) return 'industry';
	return 'base';
}

/** The layer's own name without its kind prefix: `industry_dept/retail/customer_service` → `retail/customer_service`. */
export function layerName(layer: string): string {
	const i = layer.indexOf('/');
	return i < 0 ? layer : layer.slice(i + 1);
}

export interface StackRow {
	kind: LayerKind;
	pin: TemplatePin;
	/** Newer tag the platform has published for this layer, if any. */
	latest: TemplatePin | null;
}

/** The job's pins in compose order (base first), each with the newer tag when one exists. */
export function templateStack(r: JobBoardRow): StackRow[] {
	return [...r.job.pins]
		.map((pin) => ({
			kind: layerKind(pin.layer),
			pin,
			latest: r.newer.find((n) => n.layer === pin.layer) ?? null,
		}))
		.sort((a, b) => LAYER_ORDER.indexOf(a.kind) - LAYER_ORDER.indexOf(b.kind));
}

export interface NextStep {
	tone: 'ok' | 'warn' | 'info';
	text: string;
	href: string;
	cta: string;
}

/** The one decision this job asks of the tenant_admin now, most blocking first. */
export function nextStep(r: JobBoardRow): NextStep {
	const key = r.job.key;
	const gaps = r.readiness.missing.length;
	const waiting = r.approved_waiting ?? null;
	const stage = jobStage(r);
	const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;
	if (waiting && gaps)
		return {
			tone: 'warn',
			text: `${waiting.id} is approved but ${plural(gaps, 'readiness gap')} block applying it.`,
			href: jobPath(key, '/readiness'),
			cta: 'Close the gaps',
		};
	if (waiting)
		return {
			tone: 'info',
			text: `${waiting.id} is approved and ready. Apply it to make it current.`,
			href: jobPath(key, '/release'),
			cta: 'Apply release',
		};
	if (stage === 'draft')
		return {
			tone: 'warn',
			text: `${plural(gaps, 'readiness gap')} before the first release can be applied.`,
			href: jobPath(key, '/readiness'),
			cta: 'Open readiness',
		};
	if (stage === 'ready')
		return {
			tone: 'info',
			text: 'Readiness passes. Approve and release the first cut.',
			href: jobPath(key, '/release'),
			cta: 'Approve & release',
		};
	if (stage === 'released')
		return {
			tone: 'warn',
			text: 'A release is current, but no agent is serving it on the bus.',
			href: jobPath(key, '/agents'),
			cta: 'Check agents',
		};
	if (gaps)
		return {
			tone: 'warn',
			text: `Serving, but the next release would be refused: ${plural(gaps, 'readiness gap')}.`,
			href: jobPath(key, '/readiness'),
			cta: 'Open readiness',
		};
	if (r.newer.length && !r.upgrade_requested_at)
		return {
			tone: 'info',
			text: `Serving. A newer template is published: ${r.newer.map(pinTag).join(', ')}.`,
			href: jobPath(key, '/release'),
			cta: 'Compare releases',
		};
	return {
		tone: 'ok',
		text: 'Serving and ready. Watch cases and containment.',
		href: jobPath(key, '/analytics'),
		cta: 'Open analytics',
	};
}

/** Dimension entries of a selection, for chips (`area: faq, logistics`). */
export function selectionDims(r: JobBoardRow): [string, string[]][] {
	const s = r.selection;
	if (!s) return [];
	return SELECTION_VALUE_KEYS.map((k) => [k, selectionValues(s, k)] as [string, string[]]).filter(
		([, v]) => v.length > 0,
	);
}
