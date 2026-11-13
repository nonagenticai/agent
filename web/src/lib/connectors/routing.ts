// Route-matrix summary for J6. D67: whether a route is there is the backend's (`jobOperations().cells`).
import type { Env, ExpectedOperation, JobOperations, RouteTarget } from '$lib/api/types/connectors';
import { opKey } from './ops';

/** Required rows with no route on some env — what release apply lists under 409 release_not_ready. */
export function unroutedRequired(
	d: JobOperations,
): { key: string; env: Env; op: ExpectedOperation; suggested: RouteTarget | null }[] {
	const out: { key: string; env: Env; op: ExpectedOperation; suggested: RouteTarget | null }[] = [];
	for (const o of d.ops.filter((x) => x.required))
		for (const env of ['prod', 'test'] as const) {
			const key = opKey(o);
			const c = d.cells[`${key}@${env}`];
			if (!c) throw new Error(`jobOperations() has no cell for ${key}@${env}`);
			if (c.state === 'unrouted') out.push({ key, env, op: o, suggested: c.suggested });
		}
	return out;
}
