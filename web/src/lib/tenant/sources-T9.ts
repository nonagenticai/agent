// T9 Sources: the feed side of the knowledge chain  source → ingest job → store → knowledge kind → journey stage.
// Joins the job's sources (and hand uploads, i.e. file ingest jobs with no source) with the T7 coverage model, so the
// page can say for every required knowledge kind whether a live feed keeps it fresh, someone uploads it by hand, the
// feed is broken, or nothing feeds it at all. Pure functions: the page and tests/unit/tenant-T9.test.ts share them.
import type { IngestJob, KnowledgeStore, MappingTarget, Source } from '$lib/api/types/tenant';
import { COVERAGE, pathOf, type KindCoverage } from '$lib/tenant/knowledge-T7';
import { structured, unknownTarget } from '$lib/tenant/sources';

/** A completed run older than this is shown as out of date, whatever the schedule says. */
export const SOURCE_STALE_DAYS = 7;

/** One source's health, worst first. */
export type SourceHealth =
	'failing' | 'mapping' | 'unsupported' | 'never' | 'running' | 'stale' | 'paused' | 'ok';

export const HEALTH: Record<
	SourceHealth,
	{ text: string; variant: 'ok' | 'warn' | 'err' | 'muted'; hint: string }
> = {
	failing: {
		text: 'last run failed',
		variant: 'err',
		hint: 'The store keeps the documents of the last good run.',
	},
	mapping: {
		text: 're-sync blocked',
		variant: 'err',
		hint: 'A mapped field points at an attribute the ontology no longer has; re-syncs are refused until it is fixed.',
	},
	unsupported: {
		text: 'not supported yet',
		variant: 'warn',
		hint: 'strata_ingest cannot run this source type yet.',
	},
	never: { text: 'never synced', variant: 'warn', hint: 'Nothing from this source is in the store yet.' },
	running: { text: 'syncing', variant: 'warn', hint: 'A run is in progress.' },
	stale: {
		text: 'out of date',
		variant: 'warn',
		hint: `The last good run is more than ${SOURCE_STALE_DAYS} days old.`,
	},
	paused: { text: 'paused', variant: 'muted', hint: 'Disabled: no scheduled runs.' },
	ok: { text: 'in sync', variant: 'ok', hint: 'The last run completed and is recent.' },
};

const DAY = 86_400_000;

export function sourceHealth(s: Source, targets: MappingTarget[], now: number): SourceHealth {
	if (!s.enabled) return 'paused';
	if (s.last_run?.status === 'failed') return 'failing';
	if (structured(s.type) && targets.length && s.mapping.some((m) => unknownTarget(m, targets)))
		return 'mapping';
	if (!s.last_run) return 'never';
	if (s.last_run.status === 'unsupported') return 'unsupported';
	if (s.last_run.status === 'running') return 'running';
	if (now - Date.parse(s.last_run.started_at) > SOURCE_STALE_DAYS * DAY) return 'stale';
	return 'ok';
}

const pad = (v: string) => v.padStart(2, '0');
const DOW = ['Sundays', 'Mondays', 'Tuesdays', 'Wednesdays', 'Thursdays', 'Fridays', 'Saturdays'];

/** `0 2 * * *` → "Daily 02:00"; `30 6 * * 1` → "Mondays 06:30"; null → "Manual"; anything else stays cron. */
export function scheduleText(cron: string | null): string {
	if (!cron) return 'Manual';
	const f = cron.trim().split(/\s+/);
	if (f.length !== 5 || !/^\d+$/.test(f[0]!) || !/^\d+$/.test(f[1]!) || f[2] !== '*' || f[3] !== '*')
		return cron;
	const at = `${pad(f[1]!)}:${pad(f[0]!)}`;
	if (f[4] === '*') return `Daily ${at}`;
	if (/^[0-6]$/.test(f[4]!)) return `${DOW[Number(f[4])]} ${at}`;
	return cron;
}

/** The ontology objects a source's mapping writes into (`Offering`). */
export const mappedEntities = (s: Source): string[] => [
	...new Set(s.mapping.map((m) => m.entity).filter(Boolean)),
];

/** What feeds one store: its sources and the files uploaded by hand (file ingest jobs with no source). */
export interface StoreFeed {
	store: KnowledgeStore;
	path: string;
	sources: Source[];
	uploads: { jobs: number; last: IngestJob | null };
	/** This job's knowledge kinds the store answers for (from T7 coverage). */
	kinds: KindCoverage[];
	feed: KindFeed;
}

/**
 * How a store (and so its kinds) is kept current, worst first: no usable store (T7 gap) · fed only by broken sources ·
 * filled by hand · a live source keeps it in sync.
 */
export type KindFeed = 'gap' | 'broken' | 'manual' | 'synced';

export const FEED: Record<
	KindFeed,
	{ text: string; variant: 'ok' | 'warn' | 'err' | 'muted'; hint: string }
> = {
	gap: {
		text: 'nothing feeds it',
		variant: 'err',
		hint: 'No usable store: the agent cannot answer from it.',
	},
	broken: {
		text: 'feed broken',
		variant: 'err',
		hint: 'Every source that feeds it is failing or blocked; the store is frozen at its last good run.',
	},
	manual: {
		text: 'by hand',
		variant: 'warn',
		hint: 'Only uploads keep it current: nobody is told when the original changes.',
	},
	synced: { text: 'live feed', variant: 'ok', hint: 'A scheduled source keeps it in sync.' },
};

const GOOD: SourceHealth[] = ['ok', 'running', 'stale'];

export function storeFeeds(
	stores: KnowledgeStore[],
	sources: Source[],
	jobs: IngestJob[],
	cov: KindCoverage[],
	targets: MappingTarget[],
	now: number,
): StoreFeed[] {
	const rows: StoreFeed[] = [];
	for (const store of stores) {
		if (store.kind !== 'leaf') continue;
		const mine = sources.filter((s) => s.target_store_id === store.id);
		// A file source is filled by hand too (someone uploads the files with each re-sync): it never makes a store
		// a live feed, and its failed upload does not make the feed "broken" (it shows under Needs attention).
		const feeding = mine.filter((s) => s.type !== 'file');
		const up = jobs
			.filter((j) => j.store_id === store.id && !j.source_id && j.source_kind === 'file')
			.sort((a, b) => b.created_at.localeCompare(a.created_at));
		const kinds = cov.filter((c) => c.store?.id === store.id);
		if (!mine.length && !up.length && !kinds.length && store.documents === 0) continue;
		const gap = kinds.length > 0 && kinds.every((k) => COVERAGE[k.state].gap);
		const live = feeding.some((s) => GOOD.includes(sourceHealth(s, targets, now)));
		const feed: KindFeed = gap ? 'gap' : live ? 'synced' : feeding.length ? 'broken' : 'manual';
		rows.push({
			store,
			path: pathOf(stores, store),
			sources: mine,
			uploads: { jobs: up.length, last: up[0] ?? null },
			kinds,
			feed,
		});
	}
	const rank: Record<KindFeed, number> = { gap: 0, broken: 1, manual: 2, synced: 3 };
	return rows.sort(
		(a, b) =>
			Number(b.sources.some((s) => s.type !== 'file')) - Number(a.sources.some((s) => s.type !== 'file')) ||
			rank[a.feed] - rank[b.feed] ||
			a.store.key.localeCompare(b.store.key),
	);
}

/** Per required knowledge kind: how it is fed. A kind with no store at all is a gap. */
export interface FeedSummary {
	required: number;
	byFeed: Record<KindFeed, string[]>;
}

export function feedSummary(cov: KindCoverage[], feeds: StoreFeed[]): FeedSummary {
	const byKind = new Map<string, KindFeed>();
	const rank: Record<KindFeed, number> = { gap: 0, broken: 1, manual: 2, synced: 3 };
	for (const c of cov) {
		if (!c.row.required) continue;
		const f: KindFeed = COVERAGE[c.state].gap
			? 'gap'
			: (feeds.find((x) => x.store.id === c.store?.id)?.feed ?? 'manual');
		const prev = byKind.get(c.row.kind);
		byKind.set(c.row.kind, prev && rank[prev] < rank[f] ? prev : f);
	}
	const byFeed: Record<KindFeed, string[]> = { gap: [], broken: [], manual: [], synced: [] };
	for (const [k, f] of byKind) byFeed[f].push(k);
	for (const k of Object.keys(byFeed) as KindFeed[]) byFeed[k].sort();
	return { required: byKind.size, byFeed };
}

/** One thing to fix, most urgent first, with the place to fix it. */
export interface Attention {
	key: string;
	severity: 'err' | 'gap';
	title: string;
	detail: string;
	/** Open this source's editor. */
	source?: string;
	href?: string;
	cta: string;
}

export function attention(
	sources: Source[],
	targets: MappingTarget[],
	sum: FeedSummary,
	now: number,
	kindsHref: string,
): Attention[] {
	const out: Attention[] = [];
	for (const s of sources) {
		const h = sourceHealth(s, targets, now);
		if (h === 'failing')
			out.push({
				key: `fail-${s.id}`,
				severity: 'err',
				title: `${s.name}: last run failed`,
				detail: s.last_run?.error?.message ?? HEALTH.failing.hint,
				source: s.id,
				cta: s.has_credentials ? 'Replace the key and re-sync' : 'Open source',
			});
		if (h === 'mapping') {
			const bad = s.mapping.filter((m) => unknownTarget(m, targets)).map((m) => `${m.entity}.${m.attribute}`);
			out.push({
				key: `map-${s.id}`,
				severity: 'err',
				title: `${s.name}: re-sync blocked by the mapping`,
				detail: `${bad.join(', ')} is not in this job's ontology any more. Remap or remove the field.`,
				source: s.id,
				cta: 'Fix the mapping',
			});
		}
	}
	if (sum.byFeed.gap.length)
		out.push({
			key: 'gap',
			severity: 'gap',
			title: `${sum.byFeed.gap.length} required kind${sum.byFeed.gap.length === 1 ? '' : 's'} with nothing behind it`,
			detail: `${sum.byFeed.gap.join(', ')}: bind a store on Knowledge kinds, then add a source (or upload) for it.`,
			href: kindsHref,
			cta: 'Open Knowledge kinds',
		});
	return out;
}
