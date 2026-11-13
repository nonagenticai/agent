// SW-9: attachments in the staff workspace — the plain-word refusals (pure).
import { describe, expect, it } from 'vitest';
import { refusalText } from '../../src/lib/support/attachments';

describe('refusal words', () => {
	const reasons = [
		'unsupported_type',
		'too_large',
		'too_many_pixels',
		'animated',
		'malformed_image',
		'malformed_pdf',
		'active_pdf',
		'encrypted_pdf',
		'attachment_quota',
		'malware',
		'scan_unavailable',
		'storage_unavailable',
		'attachments_on_note',
		'too_many_attachments',
	];
	it('every refusal reason has its own plain sentence, never the code', () => {
		const seen = new Set<string>();
		for (const r of reasons) {
			for (const e of [{ code: 'attachment_refused', details: { reason: r } }, { code: r }]) {
				const t = refusalText(e);
				expect(t).not.toContain('_');
				expect(t).not.toMatch(/could not be attached/);
				seen.add(t);
			}
		}
		expect(seen.size).toBe(reasons.length);
		expect(refusalText(new Error('boom'))).toMatch(/could not be attached/);
	});
});
