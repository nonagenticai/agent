// The staff workspace over HTTP (SW plan T2-T7). Every job-scoped call names its job explicitly (the inbox is
// tenant-wide, so the page passes the row's job). The stream is a GET read as SSE through `req.stream`: an
// `EventSource` cannot send the bearer. A 409 `owner_changed` reaches the page as an `ApiError` whose
// `details.owner` is the owner now.
import type { Api } from '../client';
import { seg, type Req } from '../http-client';
import type { LiveMethod } from './routes';
import { frameOf, toHandoverItem, toHandoverQueue } from './b12';
import type { CasePartyAnswer } from '../types/conversations';
import type { HandoverItem, HandoverQueue } from '../types/journey';

export const SUPPORT_METHODS = [
	'supportInbox',
	'events',
	'take',
	'assign',
	'handBack',
	'send',
	'uploadAttachment',
	'attachmentBlob',
	'deleteAttachment',
	'resolve',
	'presence',
	'canned',
	'createCanned',
	'deleteCanned',
	'supportRouting',
	'setSupportRouting',
	'supportCsat',
	'transcript',
	'customerContext',
	'jobHandovers',
	'jobCaseParty',
	'jobLinkCaseParty',
	'jobHandoverFeedback',
] as const satisfies readonly LiveMethod[];

export type SupportMethod = (typeof SUPPORT_METHODS)[number];

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

export function httpSupport(req: Req): Pick<Api, SupportMethod> {
	return {
		supportInbox: (opts = {}) =>
			req<R<'supportInbox'>>('GET', '/api/support/inbox', {
				query: { tab: opts.tab, reason: opts.reason, channel: opts.channel, priority: opts.priority },
			}),
		events: (opts) =>
			req.stream('GET', '/api/support/events', {
				onEvent: (e) => opts.onFrame(frameOf(e)),
				signal: opts.signal,
				headers: opts.lastEventId ? { 'Last-Event-ID': opts.lastEventId } : undefined,
				onOpen: opts.onOpen,
			}),
		take: (job, id, expected) =>
			req<R<'take'>>('POST', `/api/jobs/${seg(job)}/conversations/${seg(id)}/take`, {
				body: { expected_owner: expected },
			}),
		assign: (job, id, user_sub, expected) =>
			req<R<'assign'>>('POST', `/api/jobs/${seg(job)}/conversations/${seg(id)}/assign`, {
				body: { user_sub, expected_owner: expected },
			}),
		handBack: (job, id, note) =>
			req<R<'handBack'>>('POST', `/api/jobs/${seg(job)}/conversations/${seg(id)}/hand-back`, {
				body: { note },
			}),
		send: (job, id, input) =>
			req<R<'send'>>('POST', `/api/jobs/${seg(job)}/conversations/${seg(id)}/messages`, {
				body: {
					kind: input.kind,
					text: input.text,
					client_msg_id: input.client_msg_id,
					...(input.attachments?.length ? { attachments: input.attachments } : {}),
				},
			}),
		uploadAttachment: (job, id, file) => {
			const form = new FormData();
			form.append('file', file, file.name);
			return req.form<R<'uploadAttachment'>>(
				'POST',
				`/api/jobs/${seg(job)}/conversations/${seg(id)}/attachments`,
				form,
			);
		},
		// The bytes are behind auth: fetched with the bearer here, shown from an object URL by the caller.
		attachmentBlob: (job, id, att) =>
			req<Blob>('GET', `/api/jobs/${seg(job)}/conversations/${seg(id)}/attachments/${seg(att)}`, {
				response: 'blob',
			}),
		deleteAttachment: async (job, id, att) => {
			await req<void>('DELETE', `/api/jobs/${seg(job)}/conversations/${seg(id)}/attachments/${seg(att)}`);
		},
		resolve: (job, itemId, input) =>
			req<R<'resolve'>>('POST', `/api/jobs/${seg(job)}/handover/${seg(itemId)}/resolve`, {
				body: input,
			}),
		presence: async (job, id, state) => {
			await req<void>('POST', `/api/jobs/${seg(job)}/conversations/${seg(id)}/presence`, {
				body: { state },
			});
		},
		canned: (job) => req<R<'canned'>>('GET', `/api/jobs/${seg(job)}/canned-replies`),
		createCanned: (job, input) =>
			req<R<'createCanned'>>('POST', `/api/jobs/${seg(job)}/canned-replies`, { body: input }),
		deleteCanned: async (job, id) => {
			await req<void>('DELETE', `/api/jobs/${seg(job)}/canned-replies/${seg(id)}`);
		},
		supportRouting: () => req<R<'supportRouting'>>('GET', '/api/support/routing'),
		setSupportRouting: (input) => req<R<'setSupportRouting'>>('PUT', '/api/support/routing', { body: input }),
		supportCsat: (query = {}) =>
			req<R<'supportCsat'>>('GET', '/api/support/csat', { query: { from: query.from, to: query.to } }),
		transcript: (job, id) => req<R<'transcript'>>('GET', `/api/jobs/${seg(job)}/conversations/${seg(id)}`),
		customerContext: (job, id) =>
			req<R<'customerContext'>>('GET', `/api/jobs/${seg(job)}/conversations/${seg(id)}/customer-context`),
		jobHandovers: async (job, opts = {}) =>
			toHandoverQueue(
				await req<HandoverQueue & Record<string, unknown>>('GET', `/api/jobs/${seg(job)}/handover`, {
					query: { state: opts.state, assignee: opts.assignee },
				}),
			),
		jobCaseParty: async (job, caseId) =>
			(await req<CasePartyAnswer | null>('GET', `/api/jobs/${seg(job)}/cases/${seg(caseId)}/party`))?.party ??
			null,
		jobLinkCaseParty: async (job, caseId, partyId) => {
			const r = await req<CasePartyAnswer>('POST', `/api/jobs/${seg(job)}/cases/${seg(caseId)}/party`, {
				body: { party_id: partyId },
			});
			if (!r?.party) throw new Error('The case was linked but the answer carried no party.');
			return r.party;
		},
		jobHandoverFeedback: async (job, id, verdict, note) =>
			toHandoverItem(
				await req<HandoverItem & Record<string, unknown>>(
					'POST',
					`/api/jobs/${seg(job)}/handover/${seg(id)}/feedback`,
					{ body: { verdict, note } },
				),
			),
	};
}
