// Shapes for answer grounding and answer feedback: which knowledge an agent answer cites, whether it is grounded,
// how staff rate answers, and the job's knowledge analytics. Citations follow strata_rag `/api/ask`, which answers
// `{answer, citations, sources, timings_ms}`; the runtime accepts a knowledge answer only when it cites at least one
// source. Feedback is operational data, not design (D54): it
// never lands in the draft and is not published.
import type { Iso, JobKey, Sub, TenantId } from './core';

/**
 * One chunk an answer drew on: faq_ask `sources[]` (`chunk_id`, `document_id`, `store_path`) plus the document
 * facts staff read (title, page, excerpt).
 */
export interface AnswerSource {
	chunk_id: string;
	document_id: string;
	/** Knowledge store path, root first (`['faq-policies', 'policy.returns']`). */
	store_path: string[];
	title: string;
	/** Page the chunk sits on; null for web pages. */
	page: number | null;
	excerpt: string;
	/** PK (T5 `citations::answer_source`): the tenant's own stores, or a pinned platform pack. Absent = tenant. */
	layer?: KnowledgeLayer;
	/** Platform hits: the pack pin `key@version` (`store_path` is then `['platform:<pin>']`). */
	pack?: string | null;
	/** Ruling R14 (D109 "layer + pack"): the template layer key the cited pack is referenced on (`base`,
	 *  `industry/retail`, `industry_dept/retail/customer_service`, a region sub-layer). Null for a tenant citation
	 *  and for a pack no template layer references; absent outside the knowledge step's citations. */
	pack_layer?: string | null;
	/** Platform hits: where the text came from and under which licence (strata `metadata.provenance`). */
	provenance?: Provenance | null;
}

/** Which layer a knowledge hit or citation came from (PK). */
export type KnowledgeLayer = 'tenant' | 'platform';

/** Where a platform document came from (strata `Provenance`): an https URL, an SPDX-style licence id and the
 *  RFC 3339 time it was retrieved. */
export interface Provenance {
	source_url: string;
	license: string;
	retrieved_at: string;
}

/** The grounding an agent answer carries (on a case `reply` event and on a chat `ChatEvent`). */
export interface AnswerGrounding {
	/** The turn searched knowledge (`knowledge.search` / faq_ask) before answering. */
	used_knowledge?: boolean;
	/** Chunk ids the answer cites (faq_ask `citations`). Used knowledge + none = not grounded. */
	citations?: string[];
	/** The cited chunks, resolved. */
	sources?: AnswerSource[];
	/** The agent said the sources do not cover the question (“I don't know”). */
	dont_know?: boolean;
}

/**
 * `grounded`: used knowledge and cites ≥1 source · `not_grounded`: used knowledge, states an answer, cites nothing ·
 * `dont_know`: used knowledge, found nothing to cite and said so · `no_knowledge`: answered from operations or the
 * conversation alone (nothing to cite).
 */
export type Grounding = 'grounded' | 'not_grounded' | 'dont_know' | 'no_knowledge';

export type FeedbackVote = 'up' | 'down';
export type FeedbackReason = 'wrong' | 'incomplete' | 'not_grounded' | 'tone' | 'other';

export const FEEDBACK_REASONS: { key: FeedbackReason; label: string }[] = [
	{ key: 'wrong', label: 'Wrong' },
	{ key: 'incomplete', label: 'Incomplete' },
	{ key: 'not_grounded', label: 'Not grounded' },
	{ key: 'tone', label: 'Tone' },
	{ key: 'other', label: 'Other' },
];

/** `POST /api/jobs/{job}/answer-feedback` body. */
export interface AnswerFeedbackInput {
	/** Where the answer was rated: the staff test chat or a case transcript. */
	surface: 'test_chat' | 'case';
	conversation_id: string;
	/** The rated answer: the chat reply's `message_id`, or `{case_id}@{at}` for a transcript reply. */
	answer_id: string;
	case_id: string | null;
	/** What the customer asked (the message the answer replied to). */
	question: string;
	answer: string;
	vote: FeedbackVote;
	/** Down votes only; ignored on an up vote. */
	reason?: FeedbackReason | null;
	/** Free text (≤ 500 characters). */
	note?: string | null;
	/** The answer's grounding when rated (the UI computes it from the answer). */
	grounding?: Grounding;
}

/** One stored rating. One per person per answer: rating again replaces it. */
export interface AnswerFeedback {
	id: string;
	tenant_id: TenantId;
	job: JobKey;
	surface: AnswerFeedbackInput['surface'];
	conversation_id: string;
	answer_id: string;
	case_id: string | null;
	question: string;
	answer: string;
	vote: FeedbackVote;
	reason: FeedbackReason | null;
	note: string | null;
	grounding: Grounding;
	by: Sub;
	by_name: string;
	at: Iso;
}

export interface CitedDocument {
	document_id: string;
	title: string;
	store_path: string[];
	/** Ruling R14: the template layer key the most recent citation's pack is referenced on (`industry/retail`).
	 *  Absent for a tenant document and for a pack no template layer references. */
	pack_layer?: string | null;
	/** Answers in the window that cite it. */
	citations: number;
	last_cited_at: Iso;
}

export interface UnansweredQuestion {
	/** The knowledge query the agent ran (what to search the stores for). */
	query: string;
	/** One way a customer asked it. */
	example: string;
	count: number;
	last_at: Iso;
}

/** `GET /api/jobs/{job}/analytics/knowledge?days=` — how well the job's answers are grounded, and what staff think. */
export interface KnowledgeAnalytics {
	job: JobKey;
	days: number;
	since: Iso;
	as_of: Iso;
	/** Agent answers in the window. */
	answers: number;
	/** Answers whose turn searched knowledge — the base of the rates below. */
	knowledge_answers: number;
	grounded: number;
	not_grounded: number;
	dont_know: number;
	/** grounded / knowledge_answers; null without knowledge answers. */
	grounded_rate: number | null;
	/** Answers that state something without citing it: not_grounded / knowledge_answers. */
	uncited_rate: number | null;
	/** dont_know / knowledge_answers. */
	dont_know_rate: number | null;
	feedback: { up: number; down: number; reasons: { reason: FeedbackReason; count: number }[] };
	/** Questions the knowledge did not answer (I don't know, or answered without a source), most asked first. */
	unanswered: UnansweredQuestion[];
	most_cited: CitedDocument[];
	/** Documents of the job's stores no answer in the window cited (curated policy stores first). */
	never_cited: { document_id: string; title: string; store_path: string[] }[];
	never_cited_total: number;
	documents_total: number;
}
