// The `GroundingApi` contract.
import type { AnswerFeedback, AnswerFeedbackInput, KnowledgeAnalytics } from '$lib/api/types/grounding';
import type { JobKey } from '$lib/api/types/core';

export type GroundingApi = {
	/**
	 * `POST /api/jobs/{job}/answer-feedback` — rate one agent answer (thumbs up/down, a reason and a note on a down
	 * vote). One rating per person per answer: rating again replaces it. 422 `invalid_feedback` on a bad body.
	 */
	submitAnswerFeedback(input: AnswerFeedbackInput): Promise<AnswerFeedback>;
	/** `GET /api/jobs/{job}/answer-feedback?conversation_id=&case_id=` — ratings of the job's answers, newest first. */
	answerFeedback(opts?: { conversation_id?: string; case_id?: string }): Promise<AnswerFeedback[]>;
	/**
	 * `GET /api/jobs/{job}/analytics/knowledge?days=` — grounded-answer rate, uncited and “I don't know” rates,
	 * feedback with its top reasons, the questions knowledge did not answer, and the most- and never-cited documents.
	 * Production answers only; ratings from every surface.
	 */
	knowledgeAnalytics(job: JobKey, days: number): Promise<KnowledgeAnalytics>;
};
