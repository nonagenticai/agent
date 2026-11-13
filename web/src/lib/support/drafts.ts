// SW-6: the composer draft of each conversation, kept in memory while the staff workspace is open. Moving
// inbox -> conversation -> customer pane, resizing, or opening another conversation and coming back must not cost the
// agent what they were typing. Never written to storage: a draft is a customer-facing sentence in progress.
import type { SendKind } from '$lib/api/types/support';

export type Draft = { mode: SendKind; text: string };

const drafts = new Map<string, Draft>();
const keyOf = (job: string, conversationId: string) => `${job}\u0000${conversationId}`;

export function getDraft(job: string, conversationId: string): Draft {
	const d = drafts.get(keyOf(job, conversationId));
	return d ? { ...d } : { mode: 'reply', text: '' };
}

/** An empty reply draft is no draft. An empty note draft keeps the Note tab the agent chose. */
export function setDraft(job: string, conversationId: string, d: Draft): void {
	const k = keyOf(job, conversationId);
	if (d.text === '' && d.mode === 'reply') drafts.delete(k);
	else drafts.set(k, { ...d });
}

export function hasDraft(job: string, conversationId: string): boolean {
	return (drafts.get(keyOf(job, conversationId))?.text ?? '') !== '';
}

export function clearDrafts(): void {
	drafts.clear();
}
