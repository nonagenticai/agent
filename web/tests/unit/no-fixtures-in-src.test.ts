// Owner ruling: no demo data or seed content in the platform, and no mocked tests: the UI runs against the real
// agent-api only. The fixture world and in-memory API were deleted (tests/support); this guard keeps production code
// (everything under src/) from ever importing test support or fixtures again.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, sep } from 'node:path';
import { describe, expect, it } from 'vitest';

function walk(dir: string): string[] {
	return readdirSync(dir).flatMap((name) => {
		const p = join(dir, name);
		return statSync(p).isDirectory() ? walk(p) : [p];
	});
}

/** Every module specifier in `from '…'`, `import '…'`, `import('…')` and `vi.mock('…')` position. */
const SPECIFIER = /(?:\bfrom|\bimport|\bmock)\s*\(?\s*['"]([^'"]+)['"]/g;

describe('production code does not reach test support', () => {
	const files = walk('src')
		.filter((f) => /\.(ts|js|svelte)$/.test(f))
		.map((f) => f.split(sep).join('/'));

	it('scans the source tree', () => {
		expect(files.length).toBeGreaterThan(200);
	});

	it('no file under src/ imports from tests/support ($support or a relative tests/ path)', () => {
		const hits: string[] = [];
		for (const f of files)
			for (const m of readFileSync(f, 'utf8').matchAll(SPECIFIER))
				if (/^\$support\b|tests\/support|(^|\/)tests\//.test(m[1]!)) hits.push(`${f} imports ${m[1]}`);
		expect(hits).toEqual([]);
	});

	it("no file under src/ contains 'fixtures/' (the fixture world's directory)", () => {
		const hits = files.filter((f) => readFileSync(f, 'utf8').includes('fixtures/'));
		expect(hits).toEqual([]);
	});

	it('no file under src/ imports a fixture client or fixture helpers', () => {
		const hits: string[] = [];
		for (const f of files)
			for (const m of readFileSync(f, 'utf8').matchAll(SPECIFIER))
				if (/fixture-client|fixture-helpers/.test(m[1]!)) hits.push(`${f} imports ${m[1]}`);
		expect(hits).toEqual([]);
	});
});
