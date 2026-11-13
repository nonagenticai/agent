// Before any spec: the real stack answers and the smoke's journey exists. A missing stack fails here, once, with how
// to start it, instead of as hundreds of element-not-found failures.
import { existsSync, readFileSync } from 'node:fs';

export default async function globalSetup(): Promise<void> {
	const missing = ['PS_E2E_WEB', 'PS_E2E_IDP', 'PS_E2E_EDGE', 'PS_E2E_JOURNEY'].filter(
		(k) => !process.env[k],
	);
	if (missing.length)
		throw new Error(
			`${missing.join(', ')} not set: run the e2e through scripts/local/web-e2e.sh (it starts the stack)`,
		);
	const journey = process.env.PS_E2E_JOURNEY!;
	if (!existsSync(journey))
		throw new Error(`${journey} does not exist: the platform-stack smoke did not run`);
	const j = JSON.parse(readFileSync(journey, 'utf8'));
	for (const k of ['tenant', 'job', 'widget_key', 'binding', 'id_secret', 'customers', 'admin', 'agent'])
		if (!j[k]) throw new Error(`${journey} has no ${k}: the smoke did not finish`);
	const probes: [string, string][] = [
		['web', process.env.PS_E2E_WEB!],
		['idp', `${process.env.PS_E2E_IDP}/realms/portal/.well-known/openid-configuration`],
		['edge', `${process.env.PS_E2E_EDGE}/health`],
	];
	for (const [name, url] of probes) {
		const status = await fetch(url).then(
			(r) => (r.ok ? 0 : r.status),
			(e: Error) => e.message,
		);
		if (status !== 0) throw new Error(`${name} (${url}) does not answer (${status})`);
	}
}
