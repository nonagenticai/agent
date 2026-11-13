import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';

const goto = vi.hoisted(() => vi.fn(async () => {}));
vi.mock('$app/navigation', () => ({ goto }));

import { paletteRows, searchPages } from '../../src/lib/shell/search';
import CommandPalette from '../../src/lib/shell/CommandPalette.svelte';

const JOB = 'customer_care_agent';
const admin = { superAdmin: false, member: true };
const superAdmin = { superAdmin: true, member: true };

afterEach(() => {
	cleanup();
	goto.mockClear();
});

describe('searchPages', () => {
	it('"readiness" finds Go-live checklist (the old term)', () => {
		expect(searchPages('readiness', admin, JOB).map((i) => i.label)).toContain('Go-live checklist');
	});
	it('"cases" finds Cases first', () => {
		expect(searchPages('cases', admin, JOB)[0]?.label).toBe('Cases');
	});
	it('prefix matches rank above substring matches, at most 8 results', () => {
		const labels = searchPages('know', admin, JOB).map((i) => i.label);
		expect(labels.length).toBeLessThanOrEqual(8);
		expect(labels).toContain('Platform knowledge');
		expect(labels[0]!.toLowerCase().startsWith('know')).toBe(true);
		expect(labels.indexOf('Platform knowledge')).toBeGreaterThan(
			labels.findLastIndex((l) => l.toLowerCase().startsWith('know')),
		);
		expect(searchPages('e', admin, JOB).length).toBeLessThanOrEqual(8);
	});
	it('finds Advanced pages too', () => {
		expect(searchPages('ontology', admin, JOB).map((i) => i.label)).toContain('Ontology');
	});
	it('studio pages are hidden from non-super-admins', () => {
		expect(searchPages('tenants', admin, JOB).some((i) => i.href.startsWith('/platform'))).toBe(false);
		expect(searchPages('tenants', superAdmin, JOB).some((i) => i.href.startsWith('/platform'))).toBe(true);
	});
	it('agent pages carry the current job and are absent without one', () => {
		expect(searchPages('cases', admin, JOB)[0]?.href).toContain(JOB);
		expect(searchPages('cases', admin, '').some((i) => i.label === 'Cases')).toBe(false);
	});
	it('empty query gives nothing', () => {
		expect(searchPages('  ', admin, JOB)).toEqual([]);
	});
});

describe('CommandPalette', () => {
	it('Ctrl+K opens, Esc closes, Enter navigates to the first hit', async () => {
		render(CommandPalette, { session: admin, job: JOB });
		expect(screen.queryByRole('dialog')).toBeNull();
		await fireEvent.keyDown(window, { key: 'k', ctrlKey: true });
		await tick();
		const input = screen.getByRole('combobox');
		expect(screen.getByRole('dialog')).toBeTruthy();
		await fireEvent.keyDown(input, { key: 'Escape' });
		await tick();
		expect(screen.queryByRole('dialog')).toBeNull();

		await fireEvent.keyDown(window, { key: 'k', metaKey: true });
		await tick();
		const box = screen.getByRole('combobox') as HTMLInputElement;
		await fireEvent.input(box, { target: { value: 'readiness' } });
		await tick();
		await fireEvent.keyDown(box, { key: 'Enter' });
		await tick();
		expect(goto).toHaveBeenCalledTimes(1);
		expect(String((goto.mock.calls[0] as unknown[])[0])).toContain('/readiness');
		expect(screen.queryByRole('dialog')).toBeNull();
	});
	it('the trigger button is labelled "Search pages"', () => {
		render(CommandPalette, { session: admin, job: JOB });
		expect(screen.getByRole('button', { name: /search pages/i })).toBeTruthy();
	});
});

describe('paletteRows / settings words', () => {
	const hit = (job: string, id: string, title = 'Parcel late') => ({
		job,
		job_name: `Agent ${job}`,
		case_id: id,
		title,
		customer_name: 'Femke',
		status: 'identity_required' as const,
		href: `/admin/jobs/${job}/cases/${id}`,
	});
	it('pages first, then cases in the given order, with the job named', () => {
		const pages = searchPages('cases', admin, JOB);
		const rows = paletteRows(pages, [hit('j2', 'CS-1'), hit('j1', 'CS-1')]);
		const kinds = rows.map((r) => r.kind);
		expect(kinds.indexOf('case')).toBe(pages.length);
		expect(kinds.slice(pages.length)).toEqual(['case', 'case']);
		const c = rows[pages.length];
		expect(c).toMatchObject({ href: '/admin/jobs/j2/cases/CS-1', label: 'CS-1 · Parcel late' });
		expect(c && 'detail' in c ? c.detail : '').toBe('Femke · Agent j2 · identity required');
	});
	it('the same case id in two jobs stays two rows; an identical href is dropped', () => {
		const rows = paletteRows([], [hit('j1', 'CS-1'), hit('j2', 'CS-1'), hit('j1', 'CS-1')]);
		expect(rows).toHaveLength(2);
	});
	it('no cases leaves the pages untouched', () => {
		const pages = searchPages('cases', admin, JOB);
		expect(paletteRows(pages, []).map((r) => r.href)).toEqual(pages.map((p) => p.href));
	});
	it('"settings" and "preferences" find the settings-like pages', () => {
		const labels = searchPages('settings', admin, JOB).map((i) => i.label);
		expect(labels).toEqual(expect.arrayContaining(['Team & access', 'Providers', 'Connections']));
		expect(searchPages('preferences', admin, JOB).length).toBeGreaterThan(0);
	});
});
