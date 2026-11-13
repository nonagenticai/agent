// KH T1 (D106/D111/D115): the selection hierarchy — one industry, one or more ordered departments of it (primary
// first, no duplicates), one optional region; the base-only preview is the only selection without the hierarchy.
import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import {
	emptySelection,
	isBaseOnlyPreview,
	refusalCode,
	selectionIssues,
	selectionValues,
	withSelectionValues,
	type Selection,
} from '../../src/lib/api/types/core';
import { compileModel, selectionRefusal } from '../../src/lib/platform/model';
import DepartmentPicker from '../../src/lib/ui/DepartmentPicker.svelte';

const CS = 'industry_dept/retail/customer_service';
const RETURNS = 'industry_dept/retail/returns';
const retail = (s: Partial<Selection> = {}): Selection => ({
	...emptySelection(),
	industry: 'retail',
	departments: [CS],
	...s,
});
const issues = (s: Selection) => selectionIssues(s).map((i) => [i.code, i.path]);

describe('selectionIssues (Rust Selection::hierarchy_issues)', () => {
	it('one refusal code for a mixed list (Rust compile::issues_refusal)', () => {
		const s = retail({ departments: ['bogus', 'industry_dept/x/y'] });
		expect(refusalCode(selectionIssues(s))).toBe('department_industry_mismatch');
		expect(refusalCode(selectionIssues(retail({ departments: ['bogus'] })))).toBe('invalid_selection');
		expect(refusalCode([])).toBeNull();
	});

	it('no industry / no department → hierarchy_incomplete at its path', () => {
		expect(issues(emptySelection())).toEqual([
			['hierarchy_incomplete', 'selection.industry'],
			['hierarchy_incomplete', 'selection.departments'],
		]);
		expect(issues(retail({ departments: [] }))).toEqual([['hierarchy_incomplete', 'selection.departments']]);
		expect(issues(retail())).toEqual([]);
	});

	it('another industry, a duplicate, a non-department key', () => {
		expect(
			issues(retail({ departments: [CS, 'industry_dept/utilities/billing', CS, 'customer_service'] })),
		).toEqual([
			['department_industry_mismatch', 'selection.departments[1]'],
			['duplicate_department', 'selection.departments[2]'],
			['invalid_selection', 'selection.departments[3]'],
		]);
	});

	it('the base-only preview is allowed only with nothing picked and no job', () => {
		expect(isBaseOnlyPreview(emptySelection())).toBe(true);
		expect(selectionRefusal(emptySelection(), null, true)).toEqual([]);
		// only the explicit preview exempts it: a job-context compile refuses the empty selection
		expect(selectionRefusal(emptySelection(), null).map((e) => e.key)).toEqual([
			'selection.industry',
			'selection.departments',
		]);
		expect(selectionRefusal(emptySelection(), 'customer_care_agent', true).map((e) => e.code)).toEqual([
			'hierarchy_incomplete',
			'hierarchy_incomplete',
		]);
		expect(isBaseOnlyPreview({ ...emptySelection(), profile: 'retail_ecommerce' })).toBe(false);
	});

	it('region is one value (D111)', () => {
		const s = withSelectionValues(retail(), 'region', ['nl', 'be']);
		expect(s.region).toBe('nl');
		expect(selectionValues(s, 'region')).toEqual(['nl']);
		expect(selectionValues(withSelectionValues(s, 'region', []), 'region')).toEqual([]);
	});
});

describe('compileModel keeps the declared department order', () => {
	const layer = (
		id: string,
		scope: 'base' | 'industry' | 'industry_dept',
		industry?: string,
		department?: string,
	) => ({
		id,
		scope,
		industry: industry ?? null,
		department: department ?? null,
		job: null,
		rule: null,
		dimension: null,
		tag: null,
	});
	const layers = [
		layer('L0', 'base'),
		layer('L1', 'industry', 'retail'),
		layer('L2', 'industry_dept', 'retail', 'customer_service'),
		layer('L3', 'industry_dept', 'retail', 'returns'),
	] as never[];
	const run = (s: Selection, preview = true) =>
		compileModel({
			preview,
			layers,
			rows: [],
			selection: s,
			job: null,
			profile: null,
			rules: [],
			departments: [
				{ industry: 'retail', key: 'customer_service' },
				{ industry: 'retail', key: 'returns' },
			],
		});

	it('primary first, in either order', () => {
		for (const order of [
			[RETURNS, CS],
			[CS, RETURNS],
		]) {
			const out = run(retail({ departments: order }));
			expect(out.ok && out.config.layers).toEqual(['base', 'industry/retail', ...order]);
		}
	});

	it('the base-only preview is labelled; a refused selection compiles nothing', () => {
		const base = run(emptySelection());
		expect(base.ok && base.config.warnings.map((w) => w.code)).toEqual(['base_only_preview']);
		const strict = run(emptySelection(), false);
		expect(!strict.ok && strict.errors.map((e) => e.code)).toEqual([
			'hierarchy_incomplete',
			'hierarchy_incomplete',
		]);
		const refused = run(retail({ departments: [] }));
		expect(!refused.ok && refused.errors.map((e) => e.code)).toEqual(['hierarchy_incomplete']);
	});
});

describe('DepartmentPicker (D115: ordered multi-select, primary first)', () => {
	it('adds in order, makes a department primary, removes', async () => {
		let value: string[] = [];
		const options = [
			{ key: CS, label: 'Customer service' },
			{ key: RETURNS, label: 'Returns' },
		];
		const { rerender } = render(DepartmentPicker, {
			props: { options, value, onchange: (next: string[]) => (value = next) },
		});
		expect(screen.getByTestId('department-picker-empty')).toBeTruthy();
		await fireEvent.click(screen.getByRole('button', { name: '+ Customer service' }));
		await rerender({ options, value, onchange: (next: string[]) => (value = next) });
		await fireEvent.click(screen.getByRole('button', { name: '+ Returns' }));
		expect(value).toEqual([CS, RETURNS]);
		await rerender({ options, value, onchange: (next: string[]) => (value = next) });
		const chosen = within(screen.getByTestId('department-picker-chosen'));
		expect(chosen.getAllByRole('listitem').map((li) => li.getAttribute('data-key'))).toEqual([CS, RETURNS]);
		expect(chosen.getAllByRole('listitem')[0]!.textContent).toContain('primary');
		await fireEvent.click(chosen.getByRole('button', { name: 'Make primary' }));
		expect(value).toEqual([RETURNS, CS]);
		await rerender({ options, value, onchange: (next: string[]) => (value = next) });
		await fireEvent.click(screen.getByRole('button', { name: 'Remove Customer service' }));
		expect(value).toEqual([RETURNS]);
	});
});
