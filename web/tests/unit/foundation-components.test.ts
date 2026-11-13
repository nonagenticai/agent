import { cleanup, render, screen } from '@testing-library/svelte';
import { createRawSnippet, flushSync } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import StateDiagram, { layoutStates } from '../../src/lib/ui/StateDiagram.svelte';
import MatrixTable from '../../src/lib/ui/MatrixTable.svelte';
import StepFlow from '../../src/lib/ui/StepFlow.svelte';
import MilestoneChip from '../../src/lib/ui/MilestoneChip.svelte';
import PagePlaceholder from '../../src/lib/ui/PagePlaceholder.svelte';
import PageHead from '../../src/lib/ui/PageHead.svelte';
import LoadState from '../../src/lib/ui/LoadState.svelte';
import { loadPrefs, prefs, setShowMilestones } from '../../src/lib/ui/prefs.svelte';
import { Resource } from '../../src/lib/load.svelte';
import { ApiError } from '../../src/lib/api/errors';

// These component tests check the chip itself, so they run with the build-team toggle on.
beforeEach(() => {
	prefs.showMilestones = true;
});
afterEach(() => {
	cleanup();
	prefs.showMilestones = false;
	try {
		localStorage.removeItem('agent-show-milestones');
	} catch {
		/* no storage */
	}
});

describe('StateDiagram', () => {
	const states = [
		{ id: 'quoted', initial: true },
		{ id: 'processing' },
		{ id: 'in_transit' },
		{ id: 'cancelled', terminal: true },
	];
	const transitions = [
		{ from: 'quoted', to: 'processing', label: 'order.place' },
		{ from: 'processing', to: 'in_transit', label: 'delivery.ship' },
		{ from: 'quoted', to: 'cancelled', label: 'order.cancel' },
		{ from: 'processing', to: 'cancelled', label: 'order.cancel' },
	];
	it('renders a box per state, the verbs, and a transition list', () => {
		const { container } = render(StateDiagram, {
			states,
			transitions,
			current: 'processing',
			title: 'Order states',
		});
		expect(container.querySelectorAll('g.node')).toHaveLength(4);
		expect(container.querySelector('g.node.current text')?.textContent).toBe('processing');
		expect(container.querySelectorAll('g.node.terminal')).toHaveLength(1);
		expect(screen.getAllByText('order.cancel').length).toBeGreaterThanOrEqual(2);
		expect(screen.getByText('quoted → cancelled (order.cancel)')).toBeTruthy();
		expect(screen.getByRole('img', { name: 'Order states' })).toBeTruthy();
	});
	it('lays states out by distance from the initial state', () => {
		const { pos } = layoutStates(states, transitions);
		expect(pos.get('quoted')!.x).toBeLessThan(pos.get('processing')!.x);
		expect(pos.get('processing')!.x).toBeLessThan(pos.get('in_transit')!.x);
	});
});

describe('MatrixTable', () => {
	it('renders rows × columns through the cell snippet', () => {
		const rows = [{ key: 'order.read' }, { key: 'order.cancel' }];
		const columns = ['prod', 'test'];
		render(MatrixTable<{ key: string }, string>, {
			rows,
			columns,
			rowKey: (r: { key: string }) => r.key,
			colKey: (c: string) => c,
			caption: 'Routes',
			rowHeader: createRawSnippet((r: () => { key: string }) => ({
				render: () => `<span>${r().key}</span>`,
			})),
			cell: createRawSnippet((r: () => { key: string }, c: () => string) => ({
				render: () => `<span>${r().key}@${c()}</span>`,
			})),
		});
		expect(screen.getAllByRole('columnheader').map((h) => h.textContent)).toEqual(['', 'prod', 'test']);
		expect(screen.getByText('order.cancel@test')).toBeTruthy();
		expect(screen.getAllByRole('row')).toHaveLength(3);
	});
	it('shows the empty text with no rows', () => {
		render(MatrixTable<string, string>, {
			rows: [],
			columns: ['prod'],
			rowKey: (r: string) => r,
			colKey: (c: string) => c,
			empty: 'No operations.',
			rowHeader: createRawSnippet(() => ({ render: () => '<span></span>' })),
			cell: createRawSnippet(() => ({ render: () => '<span></span>' })),
		});
		expect(screen.getByText('No operations.')).toBeTruthy();
	});
});

describe('StepFlow', () => {
	it('numbers the steps and spells out each status', () => {
		render(StepFlow, {
			label: 'Readiness',
			steps: [
				{ key: 'a', label: 'Job template chosen', status: 'done' },
				{ key: 'b', label: 'Routes', status: 'missing', detail: 'case.read on test', href: '/x' },
				{ key: 'c', label: 'Rules', status: 'placeholder' },
			],
		});
		const list = screen.getByRole('list', { name: 'Readiness' });
		expect(list.querySelectorAll('li')).toHaveLength(3);
		expect(screen.getByText('Missing')).toBeTruthy();
		expect(screen.getByText('Placeholder')).toBeTruthy();
		expect(screen.getByRole('link', { name: 'Routes' }).getAttribute('href')).toBe('/x');
		expect(screen.getByText('case.read on test')).toBeTruthy();
	});
});

describe('MilestoneChip', () => {
	it('shows the milestone (or its note) and hides when the toggle is off', () => {
		render(MilestoneChip, { milestone: 'M4', note: 'M4 (concept)' });
		expect(screen.getByTestId('milestone').textContent).toBe('M4 (concept)');
		prefs.showMilestones = false;
		flushSync();
		expect(screen.queryByTestId('milestone')).toBeNull();
	});
});

describe('Show build milestones preference', () => {
	it('is OFF by default: no stored choice renders no milestone chip', () => {
		localStorage.removeItem('agent-show-milestones');
		loadPrefs();
		expect(prefs.showMilestones).toBe(false);
		render(PageHead, { id: 'J14' });
		expect(screen.queryByTestId('milestone')).toBeNull();
		expect(document.body.textContent).not.toMatch(/\bM[1-4]\b|merged app/);
	});
	it('the toggle turns it on and is remembered; "off" is remembered too', () => {
		setShowMilestones(true);
		loadPrefs();
		expect(prefs.showMilestones).toBe(true);
		render(PageHead, { id: 'J14' });
		expect(screen.getByTestId('milestone').textContent).toBe('M1');
		setShowMilestones(false);
		loadPrefs();
		expect(prefs.showMilestones).toBe(false);
	});
});

describe('PageHead and PagePlaceholder', () => {
	it('PageHead takes the title and milestone from the registry', () => {
		render(PageHead, { id: 'J14', sub: 'Selection, approve/apply' });
		expect(screen.getByRole('heading', { level: 1 }).textContent).toContain('Release');
		expect(screen.getByTestId('milestone').textContent).toBe('M1');
		expect(screen.getByText('Selection, approve/apply')).toBeTruthy();
	});
	it('PagePlaceholder shows id, area and "Not built yet"', () => {
		render(PagePlaceholder, { id: 'P8' });
		expect(screen.getByRole('heading', { level: 1 }).textContent).toContain('Journey designer');
		expect(screen.getByTestId('page-placeholder').textContent).toContain('P8');
		expect(screen.getByTestId('page-placeholder').textContent).toContain('journey');
		expect(screen.getByText('Not built yet')).toBeTruthy();
	});
});

describe('LoadState', () => {
	const kids = createRawSnippet((d: () => string[]) => ({ render: () => `<p>rows: ${d().join(',')}</p>` }));
	it('walks loading → data', async () => {
		const res = new Resource(async () => ['a', 'b']);
		render(LoadState<string[]>, { resource: res, children: kids });
		expect(screen.getByTestId('load-loading')).toBeTruthy();
		await res.refresh();
		flushSync();
		expect(screen.getByText('rows: a,b')).toBeTruthy();
	});
	it('shows empty and unavailable states', async () => {
		const empty = new Resource(async () => [] as string[]);
		await empty.refresh();
		render(LoadState<string[]>, { resource: empty, children: kids, empty: 'No cases yet.' });
		expect(screen.getByText('No cases yet.')).toBeTruthy();
		cleanup();
		const down = new Resource<string[]>(async () => {
			throw new ApiError(503, 'unavailable', 'Cases are unavailable');
		});
		await down.refresh();
		render(LoadState<string[]>, { resource: down, children: kids });
		expect(screen.getByText('Cases are unavailable')).toBeTruthy();
		expect(screen.getByRole('button', { name: 'Try again' })).toBeTruthy();
	});
});
