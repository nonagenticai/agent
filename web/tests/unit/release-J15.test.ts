// J15 Release detail: pure helpers (bundle diff, customer impact, pin changes, rollback title).
import { describe, expect, it } from 'vitest';
import {
	bundleDiff,
	customerImpact,
	pinChanges,
	previousOf,
	ptDelta,
	rollbackPreviewTitle,
} from '../../src/lib/release/J15-detail';

describe('J15 helpers', () => {
	const prev = [
		{ key: 'channels', label: 'Channels', items: ['telegram'] },
		{ key: 'activities', label: 'Activities', items: ['a:x — Do X', 'a:y — Do Y'] },
	];
	const cur = [
		{ key: 'channels', label: 'Channels', items: ['telegram', 'web_chat'] },
		{ key: 'activities', label: 'Activities', items: ['a:x — Do X'] },
	];

	it('bundleDiff counts adds, removes and kept items per section', () => {
		expect(bundleDiff(cur, prev)).toEqual([
			{ key: 'channels', label: 'Channels', added: ['web_chat'], removed: [], kept: 1 },
			{ key: 'activities', label: 'Activities', added: [], removed: ['a:y — Do Y'], kept: 1 },
		]);
	});

	it('customerImpact phrases customer-visible changes and is empty without a previous release', () => {
		expect(customerImpact(cur, prev).map((l) => [l.tone, l.text])).toEqual([
			['add', 'Customers can now reach the agent on web chat'],
			['remove', 'No longer handles “Do Y”'],
		]);
		expect(customerImpact(cur, null)).toEqual([]);
	});

	it('pinChanges lists only layers whose version moved; previousOf walks the newest-first list', () => {
		expect(
			pinChanges(
				[
					{ layer: 'base', version: 3 },
					{ layer: 'job/x', version: 3 },
				],
				[
					{ layer: 'base', version: 3 },
					{ layer: 'job/x', version: 2 },
				],
			),
		).toEqual([{ layer: 'job/x', from: 2, to: 3 }]);
		const rows = [{ id: 'c' }, { id: 'b' }, { id: 'a' }] as never[];
		expect((previousOf(rows, 'b') as { id: string } | null)?.id).toBe('a');
		expect(previousOf(rows, 'a')).toBeNull();
		expect(ptDelta(0.7, 0.75)).toBe('+5.0 pt');
		expect(ptDelta(null, 0.75)).toBeNull();
	});
});

describe('rollbackPreviewTitle (live acceptance round 2: a re-apply was titled "Roll back Production?")', () => {
	it('names the action the confirm takes', () => {
		expect(rollbackPreviewTitle('rollback', 'cfg-1')).toBe('Roll back Production to cfg-1?');
		expect(rollbackPreviewTitle('reapply', 'cfg-2')).toBe('Re-apply cfg-2 to Production?');
		expect(rollbackPreviewTitle('apply', 'cfg-3')).toBe('Apply cfg-3 to Production?');
		expect(rollbackPreviewTitle(null, 'cfg-4')).toBe('Put cfg-4 in Production?');
	});
});
