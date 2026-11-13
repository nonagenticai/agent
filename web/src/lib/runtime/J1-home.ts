// J1 Agent home helpers. The state of the agent and what needs a person come from `agentStatus()` / `inbox()`; this
// module holds what the page derives from the overview: the case lanes, the release pipeline and the change list. Tested in tests/unit/runtime-J1.test.ts. Owned by the J1 page.
import type { AuditEvent } from '$lib/api/types/tenant';
import type { CaseStatus, JobOverview } from '$lib/api/types/runtime';
import type { OpenCases } from '$lib/api/types/status';

const plural = (n: number, w: string, many = `${w}s`) => `${n} ${n === 1 ? w : many}`;

export interface Lane {
	key: string;
	label: string;
	hint: string;
	count: number;
	/** Case statuses the lane's link filters on. */
	statuses: CaseStatus[];
	tone: 'accent' | 'warn' | 'work' | 'err';
}

/** Open production cases grouped by who is holding them, from the one status (`AgentStatus.open_cases`). */
export function lanesOf(c: OpenCases): Lane[] {
	return [
		{
			key: 'agent',
			label: 'Agent is answering',
			hint: 'the agent is answering',
			statuses: ['open'],
			count: c.agent,
			tone: 'accent',
		},
		{
			key: 'staff',
			label: 'With a person',
			hint: 'handed over to staff',
			statuses: ['handover'],
			count: c.staff,
			tone: 'work',
		},
		{
			key: 'customer',
			label: 'Waiting on customer',
			hint: 'identity or a confirmation',
			statuses: ['identity_required', 'confirmation_required'],
			count: c.waiting,
			tone: 'warn',
		},
		{
			key: 'failed',
			label: 'Stopped on an error',
			hint: 'an outcome is unknown',
			statuses: ['failed'],
			count: c.failed,
			tone: 'err',
		},
	];
}

export type FlowState = 'done' | 'live' | 'pending' | 'blocked' | 'none' | 'behind';

export interface FlowNode {
	key: 'templates' | 'refinements' | 'pending' | 'live';
	title: string;
	value: string;
	note: string;
	state: FlowState;
	sub: string;
}

/**
 * The release pipeline left to right: template layers (inherited, read-only here) + the tenant's refinements compile
 * into a cut; an approved cut waits until readiness allows apply; apply makes it the live release.
 * `draftChanges` = the job's unpublished changes (D54 draft ledger, `draftSummary().count`): the refinements node
 * counts them; with none it shows the live release's refinement rev (or that nothing was refined yet).
 */
export function releaseFlow(o: JobOverview, draftChanges: number): FlowNode[] {
	const r = o.release_status;
	const pins = o.pins ?? [];
	const behind = pins.filter((p) => p.latest != null).length || r.base_changed.length;
	const layers = pins.length;
	const w = r.approved_waiting;
	const cur = o.current_release;
	return [
		{
			key: 'templates',
			title: 'Templates',
			value: layers ? plural(layers, 'layer') : 'pinned layers',
			note: behind ? `${behind} newer tag available` : 'all pins current',
			state: behind ? 'behind' : 'done',
			sub: behind ? '/release#conflicts' : '/refinements',
		},
		{
			key: 'refinements',
			title: 'Your refinements',
			value: draftChanges ? plural(draftChanges, 'draft change') : cur ? 'all published' : 'no changes yet',
			note: draftChanges
				? cur
					? `not published yet · live is ${cur.id}`
					: 'saved to draft · try them in the test chat'
				: cur
					? `live in ${cur.id}`
					: 'template defaults only',
			state: draftChanges ? 'pending' : 'done',
			sub: '/refinements',
		},
		{
			key: 'pending',
			title: 'Approved, waiting',
			value: w ? w.id : 'none',
			note: w
				? r.missing_slots.length
					? `blocked by ${plural(r.missing_slots.length, 'item')} to fix`
					: `approved by ${w.approved_by}`
				: 'cut and approve on Release',
			state: w ? (r.missing_slots.length ? 'blocked' : 'pending') : 'none',
			sub: w && r.missing_slots.length ? '/readiness' : '/release',
		},
		{
			key: 'live',
			title: 'Live for customers',
			value: cur ? cur.id : 'nothing live',
			note: cur ? `#${cur.hash}` : 'customers get “unavailable”',
			state: cur ? 'live' : 'none',
			sub: '/release',
		},
	];
}

/** Audit actions that change the job's design: saved to draft, they reach customers only when a release is
 *  published (D54 — nothing a tenant changes is live on save). */
const DESIGN_ACTION =
	/^(refinement|rule|gate|procedure|ontology|toolset|journey|knowledge_kind)\.|^job\.(target|settings)$/;
/** Audit actions that are not changes to the job (test chats, agent sends). */
const NOT_A_CHANGE = new Set(['chat.test', 'interaction.send', 'execute']);

const ACTION_LABEL: Record<string, string> = {
	'release.approve': 'approved release',
	'release.apply': 'applied release',
	'refinement.save': 'saved a refinement',
	'job.target': 'changed the job target',
	'connection.update': 'updated connection',
	'connection.test': 'tested connection',
	'licence.gate_refused': 'refused a conversation (licence) on',
	'provision.succeeded': 'provisioned',
	'provision.drift': 'found a change made elsewhere on',
	'provider.bind': 'bound provider',
	'provider.update': 'updated provider',
	'ingest.upload': 'uploaded to',
	'ingest.failed': 'ingest failed for',
};

export interface Change {
	id: string;
	at: string;
	who: string;
	what: string;
	object: string;
	failed: boolean;
	/** A design change saved after the live release: draft (or in an approved, unapplied release) — not yet seen
	 *  by customers. */
	unreleased: boolean;
	section: 'Design' | 'Release' | 'Runtime' | 'Tenant';
}

/** Job-scoped audit rows as "who changed what", newest first, with the not-yet-released design edits marked. */
export function changesOf(rows: AuditEvent[], releasedAt: string | null): Change[] {
	return rows
		.filter((r) => !NOT_A_CHANGE.has(r.action))
		.map((r) => {
			const design = DESIGN_ACTION.test(r.action);
			const fam = r.action.split('.')[0];
			const section: Change['section'] = design
				? 'Design'
				: fam === 'release'
					? 'Release'
					: fam === 'connection' || fam === 'licence'
						? 'Runtime'
						: 'Tenant';
			return {
				id: r.id,
				at: r.at,
				who: r.actor_name ?? r.actor_id,
				what: ACTION_LABEL[r.action] ?? r.action.replace(/[._]/g, ' '),
				object: r.resource_id,
				failed: r.status !== 'success',
				unreleased: design && r.status === 'success' && (releasedAt == null || r.at > releasedAt),
				section,
			};
		})
		.sort((a, b) => b.at.localeCompare(a.at));
}
