import { describe, expect, it } from 'vitest';
import type { ToolPolicy } from '../../src/lib/api/types/iam';
import {
	blankInput,
	explainCode,
	formProblem,
	isEmptyPolicy,
	parseKeys,
	toDeclForm,
	toPolicy,
	toPolicyForm,
	toStep,
} from '../../src/lib/iam/policy';

const FULL: ToolPolicy = {
	inputs: {
		order_id: { source: 'model' },
		customer_id: { source: 'state', key: 'customer_id', from: 'state' },
		token: { source: 'state', key: 'auth_token', from: { tool: 'auth.login', output: 'token' } },
		code: { source: 'state', key: 'otp', from: { ask_user: 'What is the code?', secret: true, retries: 3 } },
	},
	outputs: { token: { key: 'auth_token', kind: 'secret', ttl_s: 3600, persist_for_subject: true } },
	requires: ['customer_id'],
	confirmation: 'auto',
	on_failure: 'refuse',
};

describe('policy form conversion', () => {
	it('round-trips a full policy (model inputs are the default and are omitted)', () => {
		const f = toPolicyForm(['order_id', 'customer_id', 'token', 'code'], ['token'], FULL);
		const back = toPolicy(f);
		const { order_id: _o, ...rest } = FULL.inputs;
		expect(back).toEqual({ ...FULL, inputs: rest });
		expect(toPolicy(toPolicyForm(['order_id', 'customer_id', 'token', 'code'], ['token'], back))).toEqual(
			back,
		);
	});
	it('no policy gives the defaults and an empty policy', () => {
		const p = toPolicy(toPolicyForm(['a'], ['b'], null));
		expect(p).toEqual({
			inputs: {},
			outputs: {},
			requires: [],
			confirmation: 'by_user',
			on_failure: 'handover',
		});
		expect(isEmptyPolicy(p)).toBe(true);
		expect(isEmptyPolicy(FULL)).toBe(false);
	});
	it('keeps a declared name the op no longer lists', () => {
		const f = toDeclForm(['a'], [], { gone: { source: 'state', key: 'k' } });
		expect(f.inputs.map((r) => r.field)).toEqual(['a', 'gone']);
		expect(toStep('x.y', f).inputs).toEqual({ gone: { source: 'state', key: 'k', from: 'state' } });
	});
	it('omits an empty ttl and an unset persist flag', () => {
		const f = toDeclForm([], ['o']);
		f.outputs[0] = { field: 'o', write: true, key: ' k ', kind: 'value', ttl: '', persist: false };
		expect(toStep('a.b', f).outputs).toEqual({ o: { key: 'k', kind: 'value' } });
	});
	it('parses keys', () => {
		expect(parseKeys('a, b  c,,a')).toEqual(['a', 'b', 'c']);
	});
});

describe('formProblem', () => {
	it('flags missing keys, prompts and bad expiries; passes a good form', () => {
		const f = toDeclForm(['a'], ['o']);
		expect(formProblem(f)).toBeNull();
		f.inputs[0] = { ...blankInput('a'), mode: 'state' };
		expect(formProblem(f)).toMatch(/state key/);
		f.inputs[0] = { ...blankInput('a'), mode: 'ask', key: 'k' };
		expect(formProblem(f)).toMatch(/asks/);
		f.inputs[0] = { ...blankInput('a'), mode: 'tool', key: 'k', tool: 'x.y' };
		expect(formProblem(f)).toMatch(/tool and output/);
		f.inputs[0] = blankInput('a');
		f.outputs[0] = { field: 'o', write: true, key: 'k', kind: 'value', ttl: '1.5', persist: false };
		expect(formProblem(f)).toMatch(/seconds/);
	});
});

describe('explainCode', () => {
	// owner 2026-11-01: a procedure may ask for an id or an email (the never-ask rule was a demo rule), so the
	// lookup-key refusal is gone; the identity refusals arrive with the strength work instead
	it('explains the identity refusals and falls back for unknown codes', () => {
		expect(explainCode('identity_incomplete', 'x')).toMatch(/one person/);
		expect(explainCode('identity_already_produced', 'x')).toMatch(/already establishes/);
		expect(explainCode('weird', 'server said')).toBe('server said');
		expect(explainCode(undefined, 'm')).toBe('m');
	});
});
