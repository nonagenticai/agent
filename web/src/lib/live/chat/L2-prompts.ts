// L2 Test chat: how to exercise one unpublished change from the chat. Keyed by the ledger key (or kind) of the
// change, so the "Changes in play" list can offer a one-click message that makes the draft answer it.
import type { DraftChange } from '$lib/api/types/release';

export interface ChangePrompt {
	/** Message to send as the demo customer, or null when no chat turn reaches this change. */
	prompt: string | null;
	/** Short hint shown under the change. */
	hint: string;
}

export function promptFor(c: Pick<DraftChange, 'key' | 'kind'>): ChangePrompt {
	if (c.key.startsWith('rule:RET-01'))
		return {
			prompt: 'I want to return my order',
			hint: 'Return the worktop (O-1015), then give any reason: the return-window rule runs.',
		};
	if (c.key.startsWith('rule:WAR-01'))
		return { prompt: 'My lamp stopped working', hint: 'A warranty claim runs the warranty rule.' };
	if (c.key.startsWith('rule:PAY-04'))
		return {
			prompt: 'I think I was charged twice',
			hint: 'A payment question runs the pending-charge rule.',
		};
	if (c.key.startsWith('gate:GM:'))
		return {
			prompt: 'Can I talk to a person?',
			hint: 'Any “arrange it” request is classified by the modality gate (GM).',
		};
	if (c.key.startsWith('gate:'))
		return {
			prompt: 'Where is my order?',
			hint: 'Every turn passes the gates; the trail shows each verdict.',
		};
	if (c.key.startsWith('route:delivery.redirect'))
		return {
			prompt: 'Can you deliver it to my neighbour?',
			hint: c.key.endsWith(':prod')
				? 'A Production route: Draft shows the test route, Live shows what customers get today.'
				: 'Ask to redirect a parcel that is on its way.',
		};
	if (c.key.startsWith('route:'))
		return { prompt: null, hint: 'A route for an operation no chat prompt calls yet.' };
	return { prompt: null, hint: 'Not reachable from a chat turn; check it on its design page.' };
}
