// Owner ruling 2026-10-22: a draft change whose kind the LIVE release does not cover (`DraftChange.uncovered`, set
// by agent-api `routes/release/draft.rs` from `design::diff::uncovered_kinds`) shows a warn chip on the J14 release
// page before publishing — publishing makes that draft value live. Covered (null / absent) shows nothing.
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/svelte';
import Chip from '../../src/lib/ui/Chip.svelte';
import { uncoveredChip } from '../../src/lib/release/pending-kinds';

afterEach(() => cleanup());

describe('uncoveredChip', () => {
	it('names the live release and says publishing makes the draft live', () => {
		const c = uncoveredChip('cfg-2026-10-22.1');
		expect(c).toEqual({
			text: 'Not in live cfg-2026-10-22.1',
			title: 'Not part of the live version cfg-2026-10-22.1: publishing makes this draft value live.',
		});
	});

	it('is null when the live release covers the kind (or the backend sent nothing)', () => {
		expect(uncoveredChip(null)).toBeNull();
		expect(uncoveredChip(undefined)).toBeNull();
		expect(uncoveredChip('')).toBeNull();
	});

	it('renders as a warn chip with the explanation as its title', () => {
		const c = uncoveredChip('cfg-2026-10-22.1');
		expect(c).not.toBeNull();
		render(Chip, { text: c!.text, variant: 'warn', title: c!.title });
		const el = screen.getByText('Not in live cfg-2026-10-22.1');
		expect(el.classList.contains('warn')).toBe(true);
		expect(el.getAttribute('title')).toContain('publishing makes this draft value live');
	});
});
