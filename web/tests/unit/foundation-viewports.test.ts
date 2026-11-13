// Shared components at phone/tablet width: StateDiagram's top→bottom layout (used when left→right does not fit).
import { describe, expect, it } from 'vitest';
import { layoutStates, shortLabel } from '../../src/lib/ui/StateDiagram.svelte';

const states = [
	{ id: 'invited', initial: true },
	{ id: 'active' },
	{ id: 'suspended' },
	{ id: 'removed', terminal: true },
];
const transitions = [
	{ from: 'invited', to: 'active', label: 'user.accept' },
	{ from: 'active', to: 'suspended', label: 'user.suspend' },
	{ from: 'suspended', to: 'active', label: 'user.reinstate' },
	{ from: 'active', to: 'removed', label: 'user.remove' },
];

describe('StateDiagram layout', () => {
	it('vertical: rows by distance from the initial state, narrower than horizontal', () => {
		const h = layoutStates(states, transitions, 'horizontal');
		const v = layoutStates(states, transitions, 'vertical');
		expect(v.orientation).toBe('vertical');
		expect(v.pos.get('invited')!.y).toBeLessThan(v.pos.get('active')!.y);
		expect(v.pos.get('active')!.y).toBeLessThan(v.pos.get('suspended')!.y);
		expect(v.pos.get('suspended')!.y).toBe(v.pos.get('removed')!.y);
		expect(v.width).toBeLessThan(h.width);
		expect(v.width * 0.7).toBeLessThanOrEqual(358); // fits a 390 px phone (358 px content), scaled at most to 70%
	});
	it('horizontal gap fits the longest verb; long state labels are shortened', () => {
		const h = layoutStates(states, transitions, 'horizontal');
		const gap = h.pos.get('active')!.x - h.pos.get('invited')!.x - h.w;
		expect(gap).toBeGreaterThanOrEqual('user.reinstate'.length * 6);
		expect(shortLabel('a very long state label that goes on')).toHaveLength(24);
	});
});
