// Owner frames can arrive out of order (each is published after its own commit): a client keeps the newest by `since`.
import { describe, expect, it } from 'vitest';
import { newerOwner } from '../../src/lib/support/owner';

const at = (since: string, owner: string) => ({ owner, since });

describe('newerOwner', () => {
	it('takes the first owner', () => {
		expect(newerOwner(null, at('2026-10-27T05:02:34.237Z', 'user:u-agent')).owner).toBe('user:u-agent');
	});
	it('keeps the newer owner when an older frame arrives late (a Take frame after its Resolve)', () => {
		const resolved = at('2026-10-27T05:02:34.277Z', 'agent');
		expect(newerOwner(resolved, at('2026-10-27T05:02:34.237Z', 'user:u-agent'))).toBe(resolved);
	});
	it('moves to a newer owner, and an equal time takes the later arrival', () => {
		const taken = at('2026-10-27T05:02:34.237Z', 'user:u-agent');
		expect(newerOwner(taken, at('2026-10-27T05:02:34.277Z', 'agent')).owner).toBe('agent');
		expect(newerOwner(taken, at('2026-10-27T05:02:34.237Z', 'queue')).owner).toBe('queue');
	});
});
