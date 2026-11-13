// T8 Ingest jobs: the strata_ingest pipeline model. Every job walks the same five stages
// (fetch → read → chunk → embed → index); provider slots power `read` (ocr / vision / transcribe) and `embed`.
// Pure functions so the page and its tests share one definition of "where did this job stop".
import type { IngestJob, IngestStep, ProviderSlot, SlotKey } from '$lib/api/types/tenant';
import { durationBetween } from '$lib/format';

export type StageKey = 'fetch' | 'read' | 'chunk' | 'embed' | 'index';
export type StageState = 'done' | 'running' | 'queued' | 'failed' | 'skipped' | 'none';

export interface StageDef {
	key: StageKey;
	label: string;
	what: string;
	/** Provider slots that power this stage (configured on Providers → Ingest / Knowledge slots). */
	slots: SlotKey[];
}

export const STAGES: StageDef[] = [
	{ key: 'fetch', label: 'Fetch', what: 'Download the file, crawl the site or read the table.', slots: [] },
	{
		key: 'read',
		label: 'Read',
		what: 'Text layer via OCR, scanned pages via vision, audio via transcription; table rows are mapped.',
		slots: ['ocr', 'vision', 'transcribe'],
	},
	{ key: 'chunk', label: 'Chunk', what: 'Split the text into passages an agent can quote.', slots: [] },
	{ key: 'embed', label: 'Embed', what: 'Turn every passage into a vector for search.', slots: ['embed'] },
	{ key: 'index', label: 'Index', what: 'Write the passages into the knowledge store.', slots: [] },
];

/** Which pipeline stage a strata_ingest step kind belongs to. */
export function stageOf(kind: string): StageKey {
	const k = kind.toLowerCase();
	if (/^(fetch|crawl|sitemap|download)/.test(k)) return 'fetch';
	if (/^(ocr|vision|transcri|parse|map)/.test(k)) return 'read';
	if (k.startsWith('chunk')) return 'chunk';
	if (k.startsWith('embed')) return 'embed';
	if (k.startsWith('index')) return 'index';
	return 'read';
}

function merge(steps: IngestStep[]): StageState {
	if (!steps.length) return 'none';
	if (steps.some((s) => s.status === 'failed')) return 'failed';
	if (steps.some((s) => s.status === 'running')) return 'running';
	if (steps.every((s) => s.status === 'completed')) return 'done';
	if (steps.every((s) => s.status === 'skipped')) return 'skipped';
	if (steps.some((s) => s.status === 'queued'))
		return steps.some((s) => s.status === 'completed') ? 'running' : 'queued';
	return 'skipped';
}

/** The state of each of the five stages for one job. `index` follows the job's own outcome. */
export function jobStages(job: IngestJob): Record<StageKey, StageState> {
	const out = {} as Record<StageKey, StageState>;
	const waiting = job.status === 'queued' && job.steps.length === 0;
	for (const st of STAGES) {
		if (st.key === 'index') continue;
		out[st.key] = waiting ? 'queued' : merge(job.steps.filter((s) => stageOf(s.kind) === st.key));
	}
	const explicit = job.steps.filter((s) => stageOf(s.kind) === 'index');
	out.index = explicit.length
		? merge(explicit)
		: job.status === 'completed'
			? 'done'
			: job.status === 'queued' || job.status === 'running'
				? 'queued'
				: 'skipped';
	return out;
}

/** The stage a job is at (running/queued) or stopped at (failed); null when finished cleanly or cancelled. */
export function currentStage(job: IngestJob): StageKey | null {
	const st = jobStages(job);
	if (job.status === 'failed') return STAGES.find((s) => st[s.key] === 'failed')?.key ?? null;
	if (job.status === 'running' || job.status === 'queued' || job.status === 'cancelling')
		return STAGES.find((s) => st[s.key] === 'running' || st[s.key] === 'queued')?.key ?? null;
	return null;
}

/** Still holds a worker: queued, running, or cancelling (its in-flight step has not ended yet). */
export const inFlight = (j: IngestJob): boolean =>
	j.status === 'queued' || j.status === 'running' || j.status === 'cancelling';

export type IngestAction = 'cancel' | 'delete' | 'retry';

/** Shown on Retry / Cancel of a job whose delete did not finish (strata_ingest answers both with 409). */
export const DELETING_BLOCK =
	'Being deleted: some of its staged files may already be gone; finish the delete';

/** Why Retry / Cancel / Delete is not available for this job (null = it is). Mirrors strata_ingest's 409s. A
 * `deleting` job (a delete that did not finish) can only be deleted again, which resumes and finishes it. */
export function actionBlock(j: IngestJob, action: IngestAction): string | null {
	if (j.status === 'deleting') return action === 'delete' ? null : DELETING_BLOCK;
	if (action === 'retry') return j.status === 'failed' ? null : 'Only a failed job can be retried';
	if (action === 'cancel') {
		if (j.status === 'queued' || j.status === 'running') return null;
		if (j.status === 'cancelling') return 'Already cancelling: the step in progress finishes first';
		if (j.status === 'cancelled') return 'Already cancelled';
		return `The job has ended (${j.status}); nothing to cancel`;
	}
	if (j.status === 'cancelling') return 'Wait until it is cancelled: the step in progress is still running';
	if (j.status === 'queued' || j.status === 'running')
		return 'Cancel it first; only an ended job can be deleted';
	return null;
}

/** What a cancel will do, in the tenant admin's words: at once, or cooperatively after the running step. */
export function cancelEffect(j: IngestJob): string {
	const running = j.steps.filter((s) => s.status === 'running').map((s) => s.kind);
	if (!running.length) return 'It stops at once; none of its steps run.';
	return `The step in progress (${running.join(', ')}) finishes and its result is kept; nothing after it runs. The job shows "cancelling" until then.`;
}

export interface StageLoad {
	stage: StageDef;
	inFlight: IngestJob[];
	failed: IngestJob[];
	/** Slots of this stage, with their live state (empty / drift). */
	slots: ProviderSlot[];
}

/** Per stage: which jobs are in it now, which stopped there, and the slots that power it. */
export function pipelineLoad(jobs: IngestJob[], slots: ProviderSlot[]): StageLoad[] {
	return STAGES.map((stage) => ({
		stage,
		inFlight: jobs.filter((j) => inFlight(j) && currentStage(j) === stage.key),
		failed: jobs.filter((j) => j.status === 'failed' && currentStage(j) === stage.key),
		slots: stage.slots.map((k) => slots.find((s) => s.slot === k)).filter((s): s is ProviderSlot => !!s),
	}));
}

export interface FailureAdvice {
	stage: StageKey | null;
	step: IngestStep | null;
	/** One sentence in the tenant admin's words. */
	cause: string;
	/** The fix to make before retrying; null = a plain retry may work. */
	fix: { label: string; href: string } | null;
	/** Whether a retry can succeed now (the blocking slot is configured, or nothing blocks). */
	retryReady: boolean;
}

/** What stopped a failed job and what to do before Retry. */
export function failureAdvice(job: IngestJob, slots: ProviderSlot[]): FailureAdvice {
	const step = job.steps.find((s) => s.status === 'failed') ?? null;
	const stage = step ? stageOf(step.kind) : null;
	const tries = step && step.attempt > 1 ? ` after ${step.attempt} automatic attempts` : '';
	if (
		job.error?.code === 'slot_not_configured' ||
		(step?.slot && step.error?.code === 'slot_not_configured')
	) {
		const key = step?.slot ?? null;
		const slot = key ? slots.find((s) => s.slot === key) : undefined;
		const label = slot?.label ?? key ?? 'provider';
		const tab = slot?.service === 'strata_rag' ? 'knowledge' : 'ingest';
		return {
			stage,
			step,
			cause: `${step?.kind ?? 'A step'} needs the ${label} slot, which is empty; it stopped${tries}.`,
			fix: slot?.configured ? null : { label: `Set the ${label} slot`, href: `/admin/providers?tab=${tab}` },
			retryReady: !!slot?.configured,
		};
	}
	return {
		stage,
		step,
		cause: `${step?.kind ?? 'The job'} failed${tries}: ${job.error?.message ?? step?.error?.message ?? 'unknown error'}.`,
		fix: null,
		retryReady: true,
	};
}

export interface DriftImpact {
	slot: ProviderSlot;
	from: number;
	to: number;
	/** Finished jobs that read with the version this tenant still pins. */
	jobs: IngestJob[];
}

/** For each drifted slot: the finished jobs whose steps ran on the older version. */
export function driftImpact(jobs: IngestJob[], slots: ProviderSlot[]): DriftImpact[] {
	return slots
		.filter((s) => s.drift)
		.map((slot) => ({
			slot,
			from: slot.version,
			to: slot.drift!.remote_version,
			jobs: jobs.filter(
				(j) =>
					j.status === 'completed' &&
					j.steps.some(
						(st) => st.slot === slot.slot && (st.slot_version ?? slot.version) < slot.drift!.remote_version,
					),
			),
		}));
}

/** "succeeded on attempt 2" / "failed after 3 attempts" / ''. */
export function attemptNote(step: IngestStep): string {
	if (step.attempt <= 1) return '';
	if (step.status === 'completed') return `succeeded on attempt ${step.attempt}`;
	if (step.status === 'failed') return `failed after ${step.attempt} attempts`;
	return `attempt ${step.attempt}`;
}

/** `€0.86`, `€0.00`, or `—` when not metered. */
export const eur = (v: number | null | undefined): string => (v == null ? '—' : `€${v.toFixed(2)}`);

/** How long a job ran, as the shared human duration (`<1 min`, `14 min`, `1 d 11 h`); `''` while it runs. */
export function took(from: string, to: string | null): string {
	return durationBetween(from, to);
}
