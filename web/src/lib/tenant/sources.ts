// Source editor model (T9) and the store tree (T7). Adapted from care-bus-agents web/src/lib/sources.ts@45ee128
// onto the U1 fixture types: mapping targets come from the job's effective ontology (`MappingTarget`).
import type {
	KnowledgeStore,
	MappingTarget,
	RunStatus,
	Source,
	SourceInput,
	SourceMapping,
	SourceRun,
	SourceType,
} from '$lib/api/types/tenant';

export const SOURCE_TYPES: [SourceType, string, string][] = [
	['website', 'Website', 'Pages fetched from public URLs'],
	['api', 'API', 'Records from a REST endpoint, mapped to the ontology'],
	['file', 'File', 'Documents you upload with each re-sync'],
	['datatable', 'Datatable', 'A CSV or XLSX table, mapped to the ontology'],
];

export type WebsiteMode = 'urls' | 'crawl' | 'sitemap';
export const WEBSITE_MODES: [WebsiteMode, string, string][] = [
	['urls', 'Listed pages', 'Fetch the URLs you list, one per line'],
	['crawl', 'Crawl', 'Start at one page and follow its links'],
	['sitemap', 'Sitemap', 'Read the pages a sitemap.xml lists'],
];

/** api and datatable carry a mapping and credentials. */
export const structured = (t: SourceType) => t === 'api' || t === 'datatable';

export interface SourceDraft {
	name: string;
	type: SourceType;
	mode: WebsiteMode;
	urls: string;
	url: string;
	maxDepth: number | null;
	maxPages: number | null;
	allowedDomains: string;
	authType: 'none' | 'bearer' | 'header';
	headerName: string;
	format: 'csv' | 'xlsx';
	/** Datatable: the XLSX sheet to read ('' = the first) and whether the first row names the columns. */
	sheet: string;
	headerRow: boolean;
	/** D68: language of the documents ('' = the job's first language, set by the server). */
	language: string;
	targetStoreId: string;
	schedule: string;
	enabled: boolean;
	mapping: SourceMapping[];
	/** Typed in this session only; credentials are write-only and never prefilled. */
	secret: string;
	clearSecret: boolean;
}

const str = (v: unknown) => (typeof v === 'string' ? v : '');
const num = (v: unknown) => (typeof v === 'number' ? v : null);
const lines = (v: unknown) => (Array.isArray(v) ? v.map(str).join('\n') : '');
const listOf = (t: string) =>
	t
		.split('\n')
		.map((u) => u.trim())
		.filter(Boolean);
const obj = (v: unknown) => (v && typeof v === 'object' ? (v as Record<string, unknown>) : {});

export function draftOf(s: Source | null): SourceDraft {
	const c = (s?.config ?? {}) as Record<string, unknown>;
	const auth = obj(c.auth);
	const crawl = obj(c.crawl);
	return {
		name: s?.name ?? '',
		type: s?.type ?? 'website',
		mode: c.mode === 'crawl' || c.mode === 'sitemap' ? c.mode : 'urls',
		urls: lines(c.urls),
		url: str(c.url),
		maxDepth: num(crawl.max_depth),
		maxPages: num(crawl.max_pages),
		allowedDomains: lines(crawl.allowed_domains),
		authType: auth.type === 'bearer' || auth.type === 'header' ? auth.type : 'none',
		headerName: str(auth.header_name),
		format: c.format === 'xlsx' ? 'xlsx' : 'csv',
		sheet: str(c.sheet),
		headerRow: c.header_row !== false,
		language: s?.language ?? '',
		targetStoreId: s?.target_store_id ?? '',
		schedule: s?.schedule ?? '',
		enabled: s?.enabled ?? true,
		mapping: (s?.mapping ?? []).map((m) => ({ ...m })),
		secret: '',
		clearSecret: false,
	};
}

/** The POST/PUT body. `credentials` is left out (keep) unless a new secret was typed or removal was asked for. */
export function inputOf(d: SourceDraft, hadCredentials: boolean): SourceInput {
	const config: Record<string, unknown> = {};
	if (d.type === 'website') {
		if (d.mode === 'urls') config.urls = listOf(d.urls);
		else {
			config.mode = d.mode;
			config.url = d.url.trim();
			if (d.mode === 'crawl') {
				const crawl: Record<string, unknown> = {};
				if (d.maxDepth != null) crawl.max_depth = d.maxDepth;
				if (d.maxPages != null) crawl.max_pages = d.maxPages;
				if (listOf(d.allowedDomains).length) crawl.allowed_domains = listOf(d.allowedDomains);
				if (Object.keys(crawl).length) config.crawl = crawl;
			}
		}
	}
	if (structured(d.type)) {
		config.url = d.url.trim();
		config.auth =
			d.authType === 'header' ? { type: 'header', header_name: d.headerName.trim() } : { type: d.authType };
	}
	if (d.type === 'datatable') {
		config.format = d.format;
		if (d.format === 'xlsx' && d.sheet.trim()) config.sheet = d.sheet.trim();
		if (!d.headerRow) config.header_row = false;
	}
	const input: SourceInput = {
		name: d.name.trim(),
		type: d.type,
		config,
		mapping: structured(d.type) ? d.mapping.filter((m) => m.source_field.trim() !== '') : [],
		target_store_id: d.targetStoreId,
		schedule: d.schedule.trim() === '' ? null : d.schedule.trim(),
		enabled: d.enabled,
	};
	if (d.language) input.language = d.language;
	const uses = structured(d.type) && d.authType !== 'none';
	if (uses && d.secret !== '') input.credentials = { secret: d.secret };
	else if (hadCredentials && (d.clearSecret || !uses)) input.credentials = null;
	return input;
}

/** True when saving the draft would send something different from what is stored. */
export function draftDirty(d: SourceDraft, saved: Source): boolean {
	const had = saved.has_credentials;
	return JSON.stringify(inputOf(d, had)) !== JSON.stringify(inputOf(draftOf(saved), had));
}

/** Client-side checks before a save; the server repeats them (and also rejects unknown attributes, 422). */
export function draftErrors(d: SourceDraft): string[] {
	const e: string[] = [];
	if (!d.name.trim()) e.push('Give the source a name.');
	if (!d.targetStoreId) e.push('Pick the store the documents go into.');
	if (d.type === 'website') {
		if (d.mode === 'urls' && listOf(d.urls).length === 0) e.push('Add at least one URL, one per line.');
		if (d.mode !== 'urls' && !/^https?:\/\/\S+$/.test(d.url.trim()))
			e.push('The start URL must start with http:// or https://.');
		if (d.maxDepth != null && (d.maxDepth < 1 || d.maxDepth > 5)) e.push('Link depth is 1 to 5.');
		if (d.maxPages != null && (d.maxPages < 1 || d.maxPages > 500)) e.push('Max pages is 1 to 500.');
	}
	if (structured(d.type) && !/^https?:\/\/\S+$/.test(d.url.trim()))
		e.push('The URL must start with http:// or https://.');
	if (structured(d.type) && d.authType === 'header' && !/^[A-Za-z0-9-]{1,64}$/.test(d.headerName.trim()))
		e.push('Name the header, for example X-Api-Key.');
	if (d.schedule.trim() && d.schedule.trim().split(/\s+/).length !== 5)
		e.push('A schedule has five cron fields, for example 0 3 * * *.');
	const fields = d.mapping.map((m) => m.source_field.trim()).filter(Boolean);
	if (new Set(fields).size !== fields.length) e.push('Each source field can be mapped once.');
	if (d.mapping.some((m) => m.source_field.trim() && !m.entity))
		e.push('Pick an ontology attribute for every mapped field.');
	return e;
}

/** A mapping row whose target is not in the job's ontology (the server refuses it with 422 unknown_attribute). */
export const unknownTarget = (m: SourceMapping, targets: MappingTarget[]) =>
	!!m.entity && !targets.some((t) => t.entity === m.entity && t.attribute === m.attribute);

/**
 * Rows for every discovered field not yet mapped; the attribute is filled when exactly one ontology attribute has
 * the same name (case-insensitive), else when one of several sits on an entity the mapping already uses.
 */
export function mapAllFields(
	fields: string[],
	rows: SourceMapping[],
	options: MappingTarget[],
): { rows: SourceMapping[]; added: number; matched: number } {
	const mapped = new Set(rows.map((r) => r.source_field.trim().toLowerCase()).filter(Boolean));
	const used = new Set(rows.map((r) => r.entity).filter(Boolean));
	const out = [...rows];
	let added = 0;
	let matched = 0;
	for (const f of fields) {
		const key = f.trim().toLowerCase();
		if (!key || mapped.has(key)) continue;
		mapped.add(key);
		const hits = options.filter((o) => o.attribute.toLowerCase() === key);
		const pick = hits.length === 1 ? hits[0] : hits.find((o) => used.has(o.entity));
		out.push({ source_field: f, entity: pick?.entity ?? '', attribute: pick?.attribute ?? '' });
		added++;
		if (pick) matched++;
	}
	return { rows: out, added, matched };
}

export const unmappedCount = (fields: string[], rows: SourceMapping[]) => {
	const mapped = new Set(rows.map((r) => r.source_field.trim().toLowerCase()));
	return new Set(fields.map((f) => f.trim().toLowerCase()).filter((f) => f && !mapped.has(f))).size;
};

export const RUN_LABELS: Record<RunStatus, string> = {
	running: 'Running',
	completed: 'Completed',
	failed: 'Failed',
	unsupported: 'Not supported by strata_ingest yet',
};

/** The toast after a re-sync, from the run the server returned: a failed run must not read as "started". */
export function resyncToast(r: Pick<SourceRun, 'status' | 'error' | 'documents'>): string {
	switch (r.status) {
		case 'running':
			return 'Re-sync started.';
		case 'completed':
			return `Re-sync completed: ${r.documents.toLocaleString('en-GB')} documents.`;
		case 'unsupported':
			return `${RUN_LABELS.unsupported}.`;
		default:
			return r.error ? `Re-sync failed: ${r.error.message}` : 'Re-sync failed.';
	}
}

/** "221 pages: 214 completed · 7 skipped". */
export function pagesSummary(
	p: { total: number; by_status: Record<string, number> } | null | undefined,
): string {
	if (!p) return '';
	const parts = Object.entries(p.by_status)
		.filter(([, n]) => n > 0)
		.map(([st, n]) => `${n} ${st}`);
	return `${p.total} page${p.total === 1 ? '' : 's'}${parts.length ? `: ${parts.join(' · ')}` : ''}`;
}

/** Depth-first from the roots in list order; an orphan is shown at depth 0. */
export function storeTree(stores: KnowledgeStore[]): { node: KnowledgeStore; depth: number }[] {
	const out: { node: KnowledgeStore; depth: number }[] = [];
	const seen = new Set<string>();
	const ids = new Set(stores.map((s) => s.id));
	const visit = (n: KnowledgeStore, depth: number) => {
		if (seen.has(n.id)) return;
		seen.add(n.id);
		out.push({ node: n, depth });
		for (const c of stores.filter((s) => s.parent_id === n.id)) visit(c, depth + 1);
	};
	for (const s of stores) if (!s.parent_id || !ids.has(s.parent_id)) visit(s, 0);
	for (const s of stores) visit(s, 0);
	return out;
}

export const leafStores = (stores: KnowledgeStore[]) => stores.filter((s) => s.kind === 'leaf');
