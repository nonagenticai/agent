// D68 per-language knowledge coverage (closes the D59 open item): documents carry a `language`; a job serves its
// languages (D59 job setting, first = default); coverage is knowledge kind × served language = the documents of
// the kind's bound store in that language. A missing cell is a WARNING (the agent can still answer in another
// language or hand over), never a block on save, test or publish.
import type { KnowledgeKindRow } from '$lib/api/types/ontology';
import type {
	CoverageRow,
	KnowledgeCoverage,
	KnowledgeDocument,
	KnowledgeStore,
} from '$lib/api/types/tenant';
import type { JobKey } from '$lib/api/types/core';
import { pathOf } from './knowledge-T7';

export function coverageOf(
	job: JobKey,
	languages: string[],
	kinds: KnowledgeKindRow[],
	stores: KnowledgeStore[],
	documents: KnowledgeDocument[],
): KnowledgeCoverage {
	const unserved: Record<string, number> = {};
	const counted = new Set<string>();
	const rows: CoverageRow[] = kinds.map((k) => {
		const store = k.store ? stores.find((s) => pathOf(stores, s) === k.store) : undefined;
		const docs = store ? documents.filter((d) => d.store_id === store.id) : [];
		const per: Record<string, number> = Object.fromEntries(languages.map((l) => [l, 0]));
		for (const d of docs) {
			if (d.language in per) per[d.language]!++;
			else if (!counted.has(d.id)) unserved[d.language] = (unserved[d.language] ?? 0) + 1;
			counted.add(d.id);
		}
		return {
			stage: k.stage,
			kind: k.kind,
			required: k.required,
			store: store ? k.store : null,
			documents: per,
			missing: languages.filter((l) => per[l] === 0),
		};
	});
	return {
		job,
		languages: [...languages],
		rows,
		gaps: rows.filter((r) => r.required).reduce((n, r) => n + r.missing.length, 0),
		unserved,
	};
}

/** Per language: how many required kinds have no document in it ("fr: 3 required kinds missing"). */
export function gapsByLanguage(c: KnowledgeCoverage): Record<string, number> {
	return Object.fromEntries(
		c.languages.map((l) => [l, c.rows.filter((r) => r.required && r.missing.includes(l)).length]),
	);
}

/** `full` when every required kind has a document in the language, `none` when none does, else `partial`. */
export function languageLevel(c: KnowledgeCoverage, lang: string): 'full' | 'partial' | 'none' {
	const req = c.rows.filter((r) => r.required);
	const missing = req.filter((r) => r.missing.includes(lang)).length;
	return missing === 0 ? 'full' : missing === req.length ? 'none' : 'partial';
}
