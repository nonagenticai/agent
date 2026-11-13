// SW-11: a staff note whose copy to the helpdesk ticket failed carries a quiet chip; every other state shows nothing.
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/svelte';
import MessageItem from '../../src/lib/support/ui/MessageItem.svelte';
import type { MirrorState } from '../../src/lib/api/types/support';
import type { Row } from '../../src/lib/support/rows';

afterEach(cleanup);

const note = (mirror: MirrorState | null, kind: Row['kind'] = 'note'): Row => ({
	message_id: 'n-1',
	kind,
	event: null,
	text: 'Check the PSP ledger first',
	author_name: 'Sanne Jansen',
	at: '2026-10-27T10:00:00Z',
	case_id: null,
	client_msg_id: null,
	delivery: null,
	mirror,
});

describe('note mirror chip', () => {
	it('shows "Not copied to ticket" with an explanatory title only when mirror is failed', () => {
		render(MessageItem, { message: note('failed') });
		const chip = screen.getByText('Not copied to ticket');
		expect(chip.getAttribute('title')).toBe(
			'The helpdesk ticket refused this note or could not be reached; it is kept here.',
		);
	});

	for (const m of ['pending', 'sent', 'skipped', null] as const) {
		it(`shows no chip when mirror is ${m}`, () => {
			render(MessageItem, { message: note(m) });
			expect(screen.queryByTestId('mirror-chip')).toBeNull();
			expect(screen.queryByText('Not copied to ticket')).toBeNull();
		});
	}
});
