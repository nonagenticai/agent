// Channel settings the tenant admin owns (owner 2026-10-28): photos and PDFs are a per-binding switch pair
// (`settings.attachments {customer, staff}`), a web chat's origins stay editable after create, the tenant's storage is
// shown read-only, and the staff paperclip follows the switch.
import { describe, expect, it } from 'vitest';
import { attachmentSwitches } from '../../src/lib/api/types/conversations';
import { noFilesReason, refusalText } from '../../src/lib/support/attachments';

describe('the switches as the console reads them', () => {
	it('absent means on for web chat and Telegram, off for every other kind', () => {
		expect(attachmentSwitches('web_chat', {})).toEqual({ customer: true, staff: true });
		expect(attachmentSwitches('telegram', undefined)).toEqual({ customer: true, staff: true });
		expect(attachmentSwitches('telegram', { attachments: { customer: false } })).toEqual({
			customer: false,
			staff: true,
		});
		expect(attachmentSwitches('helpdesk', { attachments: { customer: true } })).toEqual({
			customer: false,
			staff: false,
		});
	});

	it('the staff reason says it is a setting, and the refusal has plain words', () => {
		expect(noFilesReason('web_chat')).toBeNull();
		expect(noFilesReason('web_chat', true)).toBeNull();
		expect(noFilesReason('web_chat', false)).toMatch(/turned off for this channel/);
		expect(noFilesReason('helpdesk', true)).toMatch(/cannot receive/);
		expect(refusalText({ code: 'attachments_off' })).toMatch(/turned off/);
	});
});
