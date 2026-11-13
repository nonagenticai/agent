// B8 T10a / T10b generator UI: the F-54 draft hint, the F-23 operation proposal drawer, the F-53 evidence upload and
// the D97 tenant run page (409 generator_provider_missing → Providers link, BYO funding line).
import { describe, expect, it } from 'vitest';
import { ApiError } from '../../src/lib/api/errors';

import {
	HINT_MAX,
	PROVIDERS_LINK,
	countRows,
	formFromItem,
	fundingNote,
	hintArg,
	hintState,
	operationErrors,
	operationRefusal,
	providerMissing,
	splitOperationTitle,
	tenantRunHref,
	toOperationDraft,
	uploadRefusal,
} from '../../src/lib/journey/B8Generator';
describe('B8 generator logic', () => {
	it('hint: trims, counts, flags over the limit, sends nothing when empty', () => {
		expect(hintState('  ab  ')).toEqual({ length: 2, left: HINT_MAX - 2, over: false });
		expect(hintState('x'.repeat(HINT_MAX + 1)).over).toBe(true);
		expect(hintArg('   ')).toBeUndefined();
		expect(hintArg(' only onboarding ')).toBe('only onboarding');
	});

	it('operation form: seeded from the draft, validated, sent as the editor takes it', () => {
		expect(splitOperationTitle('case.create{incident} R')).toEqual({
			object: 'case',
			verb: 'create',
			variant: 'incident',
		});
		expect(splitOperationTitle('Not a key')).toBeNull();
		const f = formFromItem('credential.reset O', 'do X 3 · confirm-back · verify_read order.read');
		expect(f).toMatchObject({
			object: 'credential',
			verb: 'reset',
			modality: 'do',
			side_effect: 'X',
			identity_level: '3',
			confirm_back: true,
			verify_read: 'order.read',
		});
		expect(operationErrors(f)).toEqual({});
		expect(operationErrors({ ...f, verb: 'Reset it', confirm_back: false })).toMatchObject({
			verb: expect.any(String),
			confirm_back: expect.any(String),
		});
		expect(toOperationDraft({ ...f, input_fields: 'username, reason', variant: '' })).toMatchObject({
			variant: null,
			input_fields: ['username', 'reason'],
			verify_read: 'order.read',
		});
		expect(operationRefusal(new ApiError(409, 'operation_exists', 'x'))).toMatch(/already exists/);
		expect(operationRefusal(new ApiError(422, 'operation_invalid', 'bad idempotency'))).toMatch(
			/bad idempotency/,
		);
	});

	it('upload refusals: 413 too large, 422 unreadable, 409 no gates model', () => {
		expect(uploadRefusal(new ApiError(413, 'payload_too_large', 'big'))).toMatch(/too large \(413\)/);
		expect(uploadRefusal(new ApiError(422, 'unreadable_export', 'no text column'))).toMatch(
			/422 unreadable_export/,
		);
		expect(uploadRefusal(new ApiError(409, 'gates_provider_missing', 'x'))).toMatch(/gates model/);
		expect(countRows('a,b\n1,2\n3,4\n', 'x.csv')).toBe(2);
		expect(countRows('{"a":1}\n{"a":2}\n', 'x.jsonl')).toBe(2);
	});

	it('tenant run: provider-missing detection, BYO note, route', () => {
		expect(providerMissing(new ApiError(409, 'generator_provider_missing', 'x'))).toBe(true);
		expect(providerMissing(new ApiError(409, 'other', 'x'))).toBe(false);
		expect(PROVIDERS_LINK).toBe('/admin/providers#uses');
		expect(tenantRunHref('customer_care', 12)).toBe('/admin/jobs/customer_care/discovery/runs/12');
		const cost = (funding: 'tenant_byo' | 'local_gpu') => ({
			cost: { tokens: 1, funding, attributed_to: 'tenant' as const },
			tenant_name: 'Demo Retail',
		});
		expect(fundingNote(cost('tenant_byo') as never)).toMatch(/own provider key/);
		expect(fundingNote(cost('local_gpu') as never)).toBeNull();
	});
});
