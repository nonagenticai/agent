// J16 Conflicts board: pure helpers (field rows, choice effects, cause text).
import { describe, expect, it } from 'vitest';
import { fieldRows, choiceEffect, causeText } from '../../src/lib/release/conflicts-J16';

describe('J16 helpers', () => {
	it('fieldRows marks where the base moved and where the tenant row differs', () => {
		const rows = fieldRows(
			{ key: 'k', label: 'Returns', required: ['a'] },
			{ key: 'k', label: 'Returns', required: ['a', 'b'] },
			{ key: 'k', label: 'Mine', required: ['a'] },
		);
		const label = rows.find((r) => r.field === 'label')!;
		const req = rows.find((r) => r.field === 'required')!;
		expect(label).toMatchObject({ base_moved: false, mine_differs: true });
		expect(req).toMatchObject({ base_moved: true, mine_differs: true, after: 'a, b' });
		expect(rows.some((r) => r.field === 'key')).toBe(false);
		const hidden = fieldRows({ key: 'k', label: 'X' }, null, null, { mineHidden: true });
		expect(hidden[0]).toMatchObject({
			before: 'X',
			after: '—',
			mine: 'hidden',
			base_moved: true,
			mine_differs: false,
		});
	});
	it('choice copy and cause text', () => {
		expect(choiceEffect({ kind: 'base_removed', tenant_row: 'tombstone' }, 'revert')).toMatch(
			/leftover hide/,
		);
		expect(causeText({ cause: { kind: 'template_upgrade', layer: 'job/x', from: 'v2', to: 'v3' } })).toBe(
			'Template upgrade job/x v2 → v3',
		);
		expect(causeText({ cause: null })).toBeNull();
	});
});

describe('compactRows', () => {
	it('keeps only moved/differing fields, and collapses a removed row to one line', async () => {
		const { compactRows } = await import('../../src/lib/release/conflicts-J16');
		const rows = fieldRows({ key: 'k', a: 1, b: 2 }, { key: 'k', a: 1, b: 3 }, { key: 'k', a: 1, b: 2 });
		const c = compactRows({ key: 'k', a: 1, b: 2 }, { key: 'k', a: 1, b: 3 }, rows, 'x');
		expect(c.shown.map((r) => r.field)).toEqual(['b']);
		expect(c.unchanged).toEqual(['a']);
		const gone = compactRows({ key: 'k', label: 'Gift cards' }, null, [], 'hidden');
		expect(gone.shown).toEqual([
			expect.objectContaining({ before: 'Gift cards', after: 'removed', mine: 'hidden' }),
		]);
	});
});
