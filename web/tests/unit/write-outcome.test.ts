import { describe, expect, it } from 'vitest';
import { ApiError } from '../../src/lib/api/errors';
import { guard, guardOutcome } from '../../src/lib/app.svelte';
import { toastState } from '../../src/lib/ui/toaster.svelte';
import {
	CHECKING_TEXT,
	NOT_SAVED_TEXT,
	UNREACHABLE_TEXT,
	classifyWriteError,
	compareFields,
	fieldFault,
	orNull,
	reconcile,
	reconcileText,
	sameValue,
} from '../../src/lib/write-outcome';

const fail =
	(status: number, code: string, message = 'x') =>
	async () => {
		throw new ApiError(status, code, message);
	};

describe('classifyWriteError', () => {
	it('is uncertain only when no answer from agent-api came back', () => {
		expect(classifyWriteError(new ApiError(0, 'unavailable', 'down'))).toBe('uncertain');
		expect(classifyWriteError(new ApiError(502, 'http_502', 'Bad Gateway'))).toBe('uncertain');
		expect(classifyWriteError(new ApiError(504, 'http_504', 'Gateway Timeout'))).toBe('uncertain');
		expect(classifyWriteError(new ApiError(200, 'bad_response', 'not JSON'))).toBe('uncertain');
	});
	it('is a definite refusal for answered errors, 503 included, and for non-API errors', () => {
		for (const s of [400, 403, 404, 409, 422, 500, 503])
			expect(classifyWriteError(new ApiError(s, 'c', 'm'))).toBe('rejected');
		expect(classifyWriteError(new Error('boom'))).toBe('rejected');
	});
});

describe('guardOutcome', () => {
	it('reports ok, rejected and uncertain', async () => {
		expect(await guardOutcome(async () => 1, 'Saved.')).toEqual({ kind: 'ok' });
		expect(toastState.msg).toBe('Saved.');
		expect((await guardOutcome(fail(422, 'invalid_cap', 'bad cap'), 'Saved.')).kind).toBe('rejected');
		expect(toastState.msg).toBe('Not saved: bad cap');
		expect((await guardOutcome(fail(403, 'forbidden'), 'Saved.')).kind).toBe('rejected');
		const u = await guardOutcome(fail(0, 'unavailable'), 'Saved.');
		expect(u.kind).toBe('uncertain');
		expect(toastState.msg).toMatch(/^The write outcome is uncertain/);
	});
	it('says a check is running when the caller reconciles', async () => {
		await guardOutcome(fail(504, 'http_504'), 'Saved.', 'Not saved', { reconcile: true });
		expect(toastState.msg).toBe(CHECKING_TEXT);
	});
	it('keeps the boolean guard() working', async () => {
		expect(await guard(async () => 1, 'Saved.')).toBe(true);
		expect(await guard(fail(0, 'unavailable'), 'Saved.')).toBe(false);
		expect(await guard(fail(400, 'invalid_body'), 'Saved.')).toBe(false);
	});
});

describe('reconcile', () => {
	const same = () => ({ differs: [], unchecked: [] });
	it('confirms when the stored resource matches, never calling anything but the read', async () => {
		let reads = 0;
		const r = await reconcile(async () => {
			reads++;
			return { a: 1 };
		}, same);
		expect(r).toEqual({ kind: 'confirmed', unchecked: [] });
		expect(reads).toBe(1);
	});
	it('reports not saved when it differs, and unreachable when the read fails', async () => {
		expect(
			await reconcile(
				async () => null,
				() => ({ differs: ['x'], unchecked: [] }),
			),
		).toEqual({
			kind: 'not_saved',
			differs: ['x'],
		});
		expect(await reconcile(fail(0, 'unavailable'), same)).toEqual({ kind: 'unreachable' });
	});
	it('orNull maps only 404 to null', async () => {
		expect(await orNull(fail(404, 'not_found')())).toBeNull();
		await expect(orNull(fail(0, 'unavailable')())).rejects.toThrow();
	});
	it('words each result, naming unchecked secrets', () => {
		expect(reconcileText({ kind: 'confirmed', unchecked: [] })).toBe('Saved — confirmed after reloading.');
		expect(reconcileText({ kind: 'confirmed', unchecked: ['API key'] })).toMatch(
			/Only the non-secret fields could be checked; the API key is never shown back/,
		);
		expect(reconcileText({ kind: 'not_saved', differs: [] })).toBe(NOT_SAVED_TEXT);
		expect(reconcileText({ kind: 'unreachable' })).toBe(UNREACHABLE_TEXT);
	});
});

describe('compareFields and sameValue', () => {
	it('treats blank as null and ignores key order', () => {
		expect(sameValue('', null)).toBe(true);
		expect(sameValue({ a: 1, b: 2 }, { b: 2, a: 1 })).toBe(true);
		expect(sameValue([1, 2], [2, 1])).toBe(false);
	});
	it('lists changed fields, and every field when nothing is stored', () => {
		const fields = ['name', 'base_url'] as const;
		expect(compareFields({ name: 'a', base_url: 'x' }, { name: 'a', base_url: 'y' }, fields)).toEqual([
			'base_url',
		]);
		expect(compareFields({ name: 'a' }, null, fields)).toEqual(['name', 'base_url']);
	});
});

describe('fieldFault', () => {
	const FIELDS = ['base_url', 'secret_ref', 'timeout_ms'] as const;
	const CODES = { invalid_base_url: 'base_url', invalid_timeout: 'timeout_ms' } as const;
	it('maps a validation code onto its field', () => {
		expect(fieldFault(new ApiError(400, 'invalid_timeout', 't'), CODES, FIELDS)).toEqual({
			field: 'timeout_ms',
			code: 'invalid_timeout',
			message: 't',
		});
	});
	it('reads the field from a secret rejection, and leaves unknown codes unplaced', () => {
		const e = new ApiError(
			422,
			'secret_value_rejected',
			'field secret_ref looks like a secret; store a secret_ref instead',
		);
		expect(fieldFault(e, CODES, FIELDS).field).toBe('secret_ref');
		expect(fieldFault(new ApiError(400, 'invalid_body', 'b'), CODES, FIELDS).field).toBeNull();
		expect(fieldFault(new ApiError(403, 'forbidden', 'f'), CODES, FIELDS).code).toBe('forbidden');
		expect(fieldFault(new Error('boom'), CODES, FIELDS)).toEqual({
			field: null,
			code: 'error',
			message: 'boom',
		});
	});
});
