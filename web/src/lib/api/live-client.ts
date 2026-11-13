// The client every caller runs on: each method (`http/routes.ts`) over HTTP against the real agent-api, no fallback.
// Later plans spread their `httpBn(req)` here.
import type { Api } from './client';
import type { Req } from './http-client';
import { httpB1 } from './http/b1';
import { httpB2 } from './http/b2';
import { httpB3 } from './http/b3';
import { httpB7 } from './http/b7';
import { httpInteraction } from './http/interaction';
import { httpB12 } from './http/b12';
import { httpB4 } from './http/b4';
import { httpB4Journey } from './http/b4-journey';
import { httpPk } from './http/pk';
import { httpB5 } from './http/b5';
import { httpB6 } from './http/b6';
import { httpB8 } from './http/b8';
import { httpSupport } from './http/support';
import { httpIam } from './http/iam';
import { httpMcpTasks } from './http/mcp-tasks';

/** `job` is the app's active job: the job the job-scoped B2, B3, B4, B5, B8 and B12 routes act in, and the key B7 `systemCatalogue`
 *  sends as `X-Job` (none when empty). */
export function createLiveClient(req: Req, job: () => string): Api {
	return {
		...httpB1(req),
		...httpInteraction(req),
		...httpB2(req, job),
		...httpB3(req, job),
		...httpB7(req, () => job() || null),
		...httpB12(req, job),
		...httpB4(req, job),
		...httpB4Journey(req, job),
		...httpPk(req),
		...httpB5(req, job),
		...httpB6(req),
		...httpB8(req, job),
		...httpSupport(req),
		...httpIam(req, () => job() || null),
		...httpMcpTasks(req, () => job() || null),
	} as Api;
}
