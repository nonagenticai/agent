// Answer grounding — pure functions shared by the test chat, the console, case transcripts and the knowledge
// analytics: is an answer grounded (a knowledge answer counts only with ≥1 citation), the citations of a seeded case
// reply, and the knowledge analytics of a job computed from answer turns + staff feedback. Unit-tested in
// tests/unit/grounding.test.ts.
import type {
	AnswerFeedback,
	AnswerGrounding,
	AnswerSource,
	CitedDocument,
	FeedbackReason,
	Grounding,
	KnowledgeAnalytics,
	UnansweredQuestion,
} from '$lib/api/types/grounding';
import type { CaseDetail } from '$lib/api/types/runtime';
import type { KnowledgeDocument, KnowledgeStore } from '$lib/api/types/tenant';
import { layerChip, provenanceHref, provenanceText } from '$lib/pk/pk';

/** The grounding verdict of one answer. */
export function groundingOf(a: AnswerGrounding | null | undefined): Grounding {
	if (!a?.used_knowledge) return 'no_knowledge';
	if (a.citations?.length) return 'grounded';
	return a.dont_know ? 'dont_know' : 'not_grounded';
}

export const GROUNDING_LABEL: Record<
	Grounding,
	{ text: string; variant: 'ok' | 'err' | 'warn' | 'muted'; title: string }
> = {
	grounded: { text: 'grounded', variant: 'ok', title: 'Cites at least one knowledge source.' },
	not_grounded: {
		text: 'not grounded',
		variant: 'err',
		title: 'Searched knowledge but cites no source: the answer came from the model alone. Check it.',
	},
	dont_know: {
		text: 'I don’t know',
		variant: 'warn',
		title: 'Searched knowledge, found nothing to cite and said so — a gap in the knowledge stores.',
	},
	no_knowledge: {
		text: 'no knowledge used',
		variant: 'muted',
		title: 'Answered from operations or the conversation.',
	},
};

/** `Returns policy 2026 · p. 2`. */
export function sourceLabel(s: Pick<AnswerSource, 'title' | 'page'>): string {
	return s.page == null ? s.title : `${s.title} · p. ${s.page}`;
}
export const storePath = (p: string[]) => p.join(' / ');

/**
 * PK: a platform citation's chip — `platform · <pack>`, the provenance link (http(s) only) and its licence line — or
 * null for one of the tenant's own sources (no `layer`, or `tenant`).
 */
export function platformCite(
	s: Pick<AnswerSource, 'layer' | 'pack' | 'provenance'>,
): { text: string; href: string | null; licence: string } | null {
	if (s.layer !== 'platform') return null;
	return { text: layerChip(s), href: provenanceHref(s.provenance), licence: provenanceText(s.provenance) };
}

/** The answer id a case transcript reply is rated under. */
export const replyAnswerId = (caseId: string, at: string) => `${caseId}@${at}`;
/** Down-voted answer → the eval page's "add a golden question" form (`?add=`). */
export const promoteHref = (question: string) => `/admin/knowledge/eval?add=${encodeURIComponent(question)}`;
/** An unanswered question → the knowledge stores, searching for it (`?q=`). */
export const storesHref = (query: string) => `/admin/knowledge/stores?q=${encodeURIComponent(query)}`;

/** One agent answer, as the knowledge analytics count it. */
export interface AnswerRow extends AnswerGrounding {
	at: string;
	question: string;
	/** The knowledge query the turn ran; null when it ran none. */
	query: string | null;
}

/** The answers of one case: each reply with the customer message it answered and the turn's knowledge query. */
export function answersOfCase(d: CaseDetail): AnswerRow[] {
	const out: AnswerRow[] = [];
	let question = '';
	let query: string | null = null;
	for (const e of d.timeline) {
		if (e.kind === 'message' && e.from === 'customer') {
			question = e.text;
			query = null;
		} else if (e.kind === 'knowledge') query = e.query;
		else if (e.kind === 'reply')
			out.push({
				at: e.at,
				question,
				query,
				used_knowledge: e.used_knowledge,
				citations: e.citations,
				sources: e.sources,
				dont_know: e.dont_know,
			});
	}
	return out;
}

export interface KnowledgeAnalyticsInput {
	job: string;
	days: number;
	asOf: string;
	answers: AnswerRow[];
	feedback: Pick<AnswerFeedback, 'vote' | 'reason' | 'at'>[];
	documents: KnowledgeDocument[];
	stores: KnowledgeStore[];
}

const REASON_ORDER: FeedbackReason[] = ['wrong', 'incomplete', 'not_grounded', 'tone', 'other'];
const rate = (n: number, of: number) => (of ? n / of : null);

/** The knowledge analytics of one job over `days` up to `asOf` (anything later — a rating made today — counts too). */
export function knowledgeAnalyticsOf(i: KnowledgeAnalyticsInput): KnowledgeAnalytics {
	const since = new Date(Date.parse(i.asOf) - i.days * 86_400_000).toISOString().replace(/\.\d{3}Z$/, 'Z');
	const inWindow = <T extends { at: string }>(xs: T[]) => xs.filter((x) => x.at >= since);
	const answers = inWindow(i.answers);
	const knowledge = answers.filter((a) => a.used_knowledge);
	const verdicts = knowledge.map((a) => ({ a, g: groundingOf(a) }));
	const count = (g: Grounding) => verdicts.filter((v) => v.g === g).length;
	const grounded = count('grounded');
	const not_grounded = count('not_grounded');
	const dont_know = count('dont_know');

	// Unanswered: grouped by the knowledge query (what to search the stores for), most asked first.
	const gaps = new Map<string, UnansweredQuestion>();
	for (const { a, g } of verdicts) {
		if (g === 'grounded') continue;
		const key = (a.query ?? a.question).trim().toLowerCase();
		const cur = gaps.get(key);
		if (cur) {
			cur.count++;
			if (a.at > cur.last_at) cur.last_at = a.at;
		} else gaps.set(key, { query: a.query ?? a.question, example: a.question, count: 1, last_at: a.at });
	}
	const unanswered = [...gaps.values()].sort(
		(x, y) => y.count - x.count || y.last_at.localeCompare(x.last_at),
	);

	// Citations per document (an answer citing two chunks of one document counts once for it).
	const cited = new Map<string, CitedDocument>();
	for (const a of knowledge) {
		const docs = new Map((a.sources ?? []).map((s) => [s.document_id, s]));
		for (const s of docs.values()) {
			const cur = cited.get(s.document_id);
			if (cur) {
				cur.citations++;
				if (a.at > cur.last_cited_at) cur.last_cited_at = a.at;
			} else
				cited.set(s.document_id, {
					document_id: s.document_id,
					title: s.title,
					store_path: s.store_path,
					citations: 1,
					last_cited_at: a.at,
				});
		}
	}
	const most_cited = [...cited.values()].sort(
		(x, y) => y.citations - x.citations || x.title.localeCompare(y.title),
	);

	// Never cited: documents of the stores, smaller (curated) stores first — 200 crawled articles last.
	const byId = new Map(i.stores.map((s) => [s.id, s]));
	const pathOf = (storeId: string) => {
		const out: string[] = [];
		for (let s = byId.get(storeId); s; s = s.parent_id ? byId.get(s.parent_id) : undefined)
			out.unshift(s.key);
		return out;
	};
	const size = new Map<string, number>();
	for (const d of i.documents) size.set(d.store_id, (size.get(d.store_id) ?? 0) + 1);
	const never = i.documents
		.filter((d) => !cited.has(d.id))
		.sort((x, y) => size.get(x.store_id)! - size.get(y.store_id)! || x.title.localeCompare(y.title));

	const fb = inWindow(i.feedback);
	const reasons = REASON_ORDER.map((reason) => ({
		reason,
		count: fb.filter((f) => f.vote === 'down' && f.reason === reason).length,
	}))
		.filter((r) => r.count > 0)
		.sort((x, y) => y.count - x.count);

	return {
		job: i.job,
		days: i.days,
		since,
		as_of: i.asOf,
		answers: answers.length,
		knowledge_answers: knowledge.length,
		grounded,
		not_grounded,
		dont_know,
		grounded_rate: rate(grounded, knowledge.length),
		uncited_rate: rate(not_grounded, knowledge.length),
		dont_know_rate: rate(dont_know, knowledge.length),
		feedback: {
			up: fb.filter((f) => f.vote === 'up').length,
			down: fb.filter((f) => f.vote === 'down').length,
			reasons,
		},
		unanswered: unanswered.slice(0, 8),
		most_cited: most_cited.slice(0, 8),
		never_cited: never
			.slice(0, 10)
			.map((d) => ({ document_id: d.id, title: d.title, store_path: pathOf(d.store_id) })),
		never_cited_total: never.length,
		documents_total: i.documents.length,
	};
}
