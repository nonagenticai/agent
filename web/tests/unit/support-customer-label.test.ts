// A customer line stored under the channel's party id (web chat `wc:<session>`, Telegram `tg:<id>`) never shows that
// id: it is labelled with the conversation's customer as the header names them, or plain "Customer".
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/svelte';
import MessageItem from '../../src/lib/support/ui/MessageItem.svelte';
import type { Row } from '../../src/lib/support/rows';

afterEach(cleanup);

const line = (author_name: string | null): Row => ({
	message_id: 'm-1',
	kind: 'customer',
	event: null,
	text: 'Where is my parcel?',
	author_name,
	at: '2026-10-28T07:00:00Z',
	case_id: null,
	client_msg_id: null,
	delivery: null,
	mirror: null,
});

describe('customer line label', () => {
	it('shows the conversation customer instead of a web chat party id', () => {
		render(MessageItem, {
			message: line('wc:9c2f469d-2315-44b3-960d-12b667a0beb4'),
			customerName: 'Sanne de Vries',
		});
		expect(screen.getByText('Sanne de Vries')).toBeTruthy();
		expect(screen.queryByText(/wc:/)).toBeNull();
	});

	it('falls back to "Customer" when the conversation has no name', () => {
		render(MessageItem, { message: line('tg:4815162342') });
		expect(screen.getByText('Customer')).toBeTruthy();
		expect(screen.queryByText(/tg:/)).toBeNull();
	});

	it('keeps a real author name', () => {
		render(MessageItem, { message: line('Noor Bakker'), customerName: 'Someone else' });
		expect(screen.getByText('Noor Bakker')).toBeTruthy();
	});

	it('shows "Customer" when the line has no author', () => {
		render(MessageItem, { message: line(null) });
		expect(screen.getByText('Customer')).toBeTruthy();
	});
});
