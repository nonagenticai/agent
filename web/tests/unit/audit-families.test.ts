// B1 T16: the API groups audit rows into the same families the pages do. The Rust ports
// (crates/agent-api/src/audit/read.rs TENANT_FAMILIES / PLATFORM_FAMILIES) must list exactly the keys and action
// prefixes of src/lib/tenant/auditT4.ts and src/lib/platform/auditP19.ts `FAMILIES`, in the same order.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { FAMILIES as TENANT } from '$lib/tenant/auditT4';
import { FAMILIES as PLATFORM } from '$lib/platform/auditP19';

const RUST = readFileSync(join('..', 'crates', 'agent-api', 'src', 'audit', 'read.rs'), 'utf8');

/** The `(key, &[prefixes])` entries of one Rust const. */
function rustFamilies(name: string): { key: string; prefixes: string[] }[] {
	const start = RUST.indexOf(`pub const ${name}`);
	expect(start, `${name} in read.rs`).toBeGreaterThan(-1);
	const body = RUST.slice(start, RUST.indexOf('];', start));
	return [...body.matchAll(/\(\s*"([a-z_]+)",\s*&\[([^\]]*)\]/g)].map((m) => ({
		key: m[1] ?? '',
		prefixes: [...(m[2] ?? '').matchAll(/"([^"]+)"/g)].map((p) => p[1] ?? ''),
	}));
}

describe('audit families: API = pages', () => {
	it('tenant (T4)', () => {
		expect(rustFamilies('TENANT_FAMILIES')).toEqual(
			TENANT.map((f) => ({ key: f.key, prefixes: f.prefixes })),
		);
	});
	it('platform (P19)', () => {
		expect(rustFamilies('PLATFORM_FAMILIES')).toEqual(
			PLATFORM.map((f) => ({ key: f.key, prefixes: f.prefixes })),
		);
	});
});
