// J23 Agents: the health/verdict logic (pure).
import { describe, expect, it } from 'vitest';
import type { AgentFleet, BusAgent } from '../../src/lib/api/types/runtime';
import { configState, healthOf, stepOf, totalsOf, verdictOf } from '../../src/lib/runtime/J23-agents';

describe('J23 logic', () => {
	const fleet: AgentFleet = {
		as_of: '2026-10-18T10:30:00Z',
		production: { release: { id: 'cfg-b', hash: 'x' }, released_at: null },
		test: { rev: 7, unpublished: 2 },
		approved_waiting: null,
		follow_s: 35,
	};
	const base: BusAgent = {
		id: 'w',
		name: 'w',
		kind: 'worker',
		role: 'procedure',
		group: 'w',
		consumers: 1,
		pending: 0,
		lag: 0,
		streams: [],
		status: 'ok',
		model_routes: [],
		last_heartbeat: '2026-10-18T10:29:50Z',
		version: '1',
		prod: { release: 'cfg-b', rev: null, read_at: null, consumers: 1, pending: 0, lag: 0 },
	};

	it('keeping up, behind (lag/pending), old config, silent, stopped', () => {
		expect(healthOf(base, fleet).word).toBe('ok');
		const behind = healthOf({ ...base, status: 'warn', lag: 7, pending: 4, oldest_pending_s: 48 }, fleet);
		expect(behind.word).toBe('warn');
		expect(behind.kinds).toEqual(['waiting', 'unacked']);
		expect(behind.reasons.join(' ')).toContain('oldest 48 s');
		const old = healthOf({ ...base, prod: { ...base.prod!, release: 'cfg-a' } }, fleet);
		expect(old.kinds).toEqual(['config']);
		const silent = healthOf({ ...base, last_heartbeat: '2026-10-18T10:27:00Z' }, fleet);
		expect(silent.word).toBe('err');
		expect(healthOf({ ...base, consumers: 0 }, fleet).word).toBe('idle');
	});

	it('steps follow the role and kind', () => {
		expect(stepOf({ ...base, kind: 'gate', role: 'post_gate' })).toBe('check');
		expect(stepOf({ ...base, kind: 'gate', role: 'qualifier' })).toBe('understand');
		expect(stepOf({ ...base, kind: 'coordination' })).toBe('plan');
		expect(stepOf({ ...base, kind: 'runner' })).toBe('act');
	});

	it('verdict names who is behind, totals add up', () => {
		const items = [
			base,
			{ ...base, id: 'c', name: 'case worker', status: 'warn' as const, lag: 7, pending: 4 },
		];
		const v = verdictOf(items, fleet);
		expect(v.tone).toBe('warn');
		expect(v.title).toBe('1 of 2 agents is behind: case worker');
		expect(totalsOf(items, fleet)).toMatchObject({ agents: 2, keeping_up: 1, waiting: 7, unacked: 4 });
		expect(verdictOf([base], fleet).title).toBe('All 1 agents are keeping up');
	});
});
