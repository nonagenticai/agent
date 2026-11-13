// A2/A3 over HTTP: tenant IAM procedures and a job's declared tool inputs. Op keys (`case.create{return}`) travel as
// one percent-encoded path segment. Job-scoped methods act in the page's job.
import type { Api } from '../client';
import { ApiError } from '../errors';
import { seg, type Req } from '../http-client';
import type { LiveMethod } from './routes';

export const IAM_METHODS = [
	'iamProcedures',
	'iamProcedure',
	'createIamProcedure',
	'updateIamProcedure',
	'deleteIamProcedure',
	'toolPolicy',
	'saveToolPolicy',
	'clearToolPolicy',
	'jobIamProcedures',
	'attachIamProcedure',
	'detachIamProcedure',
] as const satisfies readonly LiveMethod[];

export type IamMethod = (typeof IAM_METHODS)[number];

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

export function httpIam(req: Req, job: () => string | null = () => null): Pick<Api, IamMethod> {
	const jobSeg = () => {
		const j = job();
		if (!j) throw new ApiError(404, 'job_not_found', 'Pick a job first.');
		return seg(j);
	};
	return {
		iamProcedures: () => req<R<'iamProcedures'>>('GET', '/api/iam-procedures'),
		iamProcedure: (id) => req<R<'iamProcedure'>>('GET', `/api/iam-procedures/${seg(id)}`),
		createIamProcedure: (input) =>
			req<R<'createIamProcedure'>>('POST', '/api/iam-procedures', { body: input }),
		updateIamProcedure: (id, input) =>
			req<R<'updateIamProcedure'>>('PUT', `/api/iam-procedures/${seg(id)}`, { body: input }),
		deleteIamProcedure: async (id) => {
			await req<void>('DELETE', `/api/iam-procedures/${seg(id)}`);
		},
		toolPolicy: async (op) => req<R<'toolPolicy'>>('GET', `/api/jobs/${jobSeg()}/tools/${seg(op)}/policy`),
		saveToolPolicy: async (op, policy) =>
			req<R<'saveToolPolicy'>>('PUT', `/api/jobs/${jobSeg()}/tools/${seg(op)}/policy`, { body: policy }),
		clearToolPolicy: async (op) =>
			req<R<'clearToolPolicy'>>('DELETE', `/api/jobs/${jobSeg()}/tools/${seg(op)}/policy`),
		jobIamProcedures: async () => req<R<'jobIamProcedures'>>('GET', `/api/jobs/${jobSeg()}/iam-procedures`),
		attachIamProcedure: async (id) =>
			req<R<'attachIamProcedure'>>('PUT', `/api/jobs/${jobSeg()}/iam-procedures/${seg(id)}`),
		detachIamProcedure: async (id) =>
			req<R<'detachIamProcedure'>>('DELETE', `/api/jobs/${jobSeg()}/iam-procedures/${seg(id)}`),
	};
}
