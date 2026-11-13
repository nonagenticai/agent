// B4 over HTTP (T21b): the J21 handover policy with its business hours and the feedback verdict (T16/T16a,
// crates/agent-api/src/routes/handover.rs), the tenant holiday calendar (T16a, routes/tenant_calendar.rs; tenant
// scoped), the /chat rating (T16b, conversations/routes.rs `submit_csat`), and the saved test cases (T18,
// routes/test_cases.rs). `draftHandoverProposal` (B8 T7 registered the `activity` draft kind) saves the proposal
// of an "agent should have handled" verdict to the job's draft.
import type { Api } from '../client';
import { seg, type Req } from '../http-client';
import type { Paged } from '../types/core';
import type { HandoverItem, HandoverPolicyView } from '../types/journey';
import type { CsatRequest, TestCase } from '../types/live';
import type { TenantCalendarView } from '../types/tenant';

export type B4JourneyMethod =
	// T16 / T16a: the J21 SLA policy (+ business hours) and the feedback verdict
	| 'handoverPolicy'
	| 'saveHandoverPolicy'
	| 'handoverFeedback'
	| 'draftHandoverProposal'
	// T16a: the tenant holiday calendar
	| 'tenantCalendar'
	| 'saveTenantCalendar'
	// T16b: the /chat end-of-conversation rating
	| 'submitCsat'
	// T18: saved test cases (D54 TEST step)
	| 'testCases'
	| 'saveTestCase';

/** A list answer as a page: `[…]` or `{items, next}`. */
function pageOf<T>(raw: unknown): Paged<T> {
	if (Array.isArray(raw)) return { items: raw as T[], next: null };
	const r = (raw ?? {}) as { items?: T[]; next?: string | null };
	return { items: r.items ?? [], next: r.next ?? null };
}

/** `handoverPolicy` as the page reads it: an answer without `hours` (an older server) reads as always staffed. */
function policyOf(raw: HandoverPolicyView): HandoverPolicyView {
	return { ...raw, hours: raw.hours ?? { live: null, draft: null } };
}

export function httpB4Journey(req: Req, job: () => string): Pick<Api, B4JourneyMethod> {
	const j = () => seg(job());
	return {
		// ---- T16 / T16a handover policy + feedback
		handoverPolicy: async () =>
			policyOf(await req<HandoverPolicyView>('GET', `/api/jobs/${j()}/handover/policy`)),
		// `hours` absent keeps the stored hours, null = always staffed (agent-api `PolicyBody`)
		saveHandoverPolicy: async (targets, hours) =>
			policyOf(
				await req<HandoverPolicyView>('PUT', `/api/jobs/${j()}/handover/policy`, {
					body: hours === undefined ? { targets } : { targets, hours },
				}),
			),
		handoverFeedback: (id, verdict, note) =>
			req<HandoverItem>('POST', `/api/jobs/${j()}/handover/${seg(id)}/feedback`, {
				body: { verdict, note },
			}),
		draftHandoverProposal: (id) =>
			req<HandoverItem>('POST', `/api/jobs/${j()}/handover/${seg(id)}/feedback/draft`, { body: {} }),
		// ---- T16a tenant calendar (not job scoped)
		tenantCalendar: () => req<TenantCalendarView>('GET', '/api/tenant/calendar'),
		saveTenantCalendar: (input) =>
			req<TenantCalendarView>('PUT', '/api/tenant/calendar', {
				body: { timezone: input.timezone, holidays: input.holidays },
			}),
		// ---- T16b rating
		submitCsat: async (conversationId, input) =>
			(await req<CsatRequest | null>('POST', `/api/jobs/${j()}/conversations/${seg(conversationId)}/csat`, {
				body: { case_id: input.case_id, rating: input.rating },
			})) ?? null,
		// ---- T18 saved test cases
		testCases: async () => pageOf<TestCase>(await req<unknown>('GET', `/api/jobs/${j()}/test-cases`)),
		saveTestCase: (input) =>
			req<TestCase>('POST', `/api/jobs/${j()}/test-cases`, {
				body: { conversation_id: input.conversation_id, title: input.title },
			}),
	};
}
