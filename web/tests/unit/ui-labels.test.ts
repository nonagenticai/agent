import { cleanup, render } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { L, STATUS_LABEL, type AgentState } from '../../src/lib/ui/labels';
import TechDetail from '../../src/lib/ui/TechDetail.svelte';
import { loadPrefs, prefs, setShowTech } from '../../src/lib/ui/prefs.svelte';

afterEach(() => {
	cleanup();
	prefs.showTech = false;
	vi.unstubAllGlobals();
});

describe('UI labels glossary', () => {
	it('labels cover every AgentState', () => {
		const states: AgentState[] = ['draft', 'ready', 'live', 'live_with_issues', 'paused'];
		expect(Object.keys(STATUS_LABEL).sort()).toEqual([...states].sort());
		expect(states.map((s) => STATUS_LABEL[s])).toEqual([
			'Draft',
			'Ready',
			'Live',
			'Live with issues',
			'Paused',
		]);
	});

	it('pins the glossary values verbatim', () => {
		expect(L.tenant).toBe('Workspace');
		expect(L.job).toBe('Agent');
		expect(L.jobs).toBe('Agents');
		expect(L.bus_agent).toBe('Specialist');
		expect(L.readiness).toBe('Go-live checklist');
		expect(L.gap).toBe('Item to fix');
		expect(L.cut).toBe('Prepare');
		expect(L.approve).toBe('Approve');
		expect(L.apply).toBe('Go live');
		expect(L.refinements).toBe('Your changes');
		expect(L.template_pin).toBe('Template version');
		expect(L.validation_grid).toBe('Test suite');
		expect(L.contained).toBe('Resolved by the agent');
		expect(L.operation_calls).toBe('Actions on your systems');
		expect(L.dead_letters).toBe('Failed messages');
		expect(L.drift).toBe('Changed elsewhere');
		expect(L.release_not_ready).toBe("This release can't go live yet");
		expect(L.release_model_not_built).toBe('Release not prepared yet');
	});
});

describe('technical-details toggle', () => {
	const children = createRawSnippet(() => ({ render: () => '<b>cfg-123</b>' }));

	it('TechDetail renders nothing by default and its children when showTech is on', async () => {
		const r = render(TechDetail, { children });
		expect(r.container.querySelector('.tech')).toBeNull();
		expect(r.container.textContent).not.toContain('cfg-123');
		prefs.showTech = true;
		await r.rerender({ children });
		expect(r.container.querySelector('span.tech')?.textContent).toBe('cfg-123');
	});

	it('setShowTech persists and survives blocked storage', () => {
		localStorage.removeItem('agent-show-tech');
		setShowTech(true);
		expect(prefs.showTech).toBe(true);
		expect(localStorage.getItem('agent-show-tech')).toBe('on');
		prefs.showTech = false;
		loadPrefs();
		expect(prefs.showTech).toBe(true);
		setShowTech(false);
		expect(localStorage.getItem('agent-show-tech')).toBe('off');

		const blocked = {
			getItem() {
				throw new Error('blocked');
			},
			setItem() {
				throw new Error('blocked');
			},
		};
		vi.stubGlobal('localStorage', blocked);
		expect(() => setShowTech(true)).not.toThrow();
		expect(prefs.showTech).toBe(true);
		expect(() => loadPrefs()).not.toThrow();
		expect(prefs.showTech).toBe(false);
	});
});
