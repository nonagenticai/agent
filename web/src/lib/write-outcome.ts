// What happened to a write, and how a form finds out when the answer was lost.
//
// A write either succeeded (`ok`), was refused (`rejected`: the server answered with an error, nothing was
// stored), or its outcome is unknown (`uncertain`: no usable answer came back, so the server may or may not have
// stored it). An uncertain write is never resubmitted automatically: the form re-reads the resource and compares
// it with what it sent (`reconcile`). Pure functions only, so they are unit tested directly.
import { ApiError } from './api/errors';
import { ROLE_LABEL, type Role } from './api/types/core';

export type WriteKind = 'ok' | 'rejected' | 'uncertain';
export type WriteOutcome =
	| { kind: 'ok' }
	| { kind: 'rejected'; error: unknown }
	| { kind: 'uncertain'; error: unknown };

/**
 * A failed write is uncertain when no answer from agent-api itself came back: the network failed (status 0),
 * a gateway gave up (502, 504), or the body was not ours (`bad_response`). Everything else, 503 included, is a
 * definite refusal.
 */
export function classifyWriteError(e: unknown): 'rejected' | 'uncertain' {
	return e instanceof ApiError &&
		(e.status === 0 || e.status === 502 || e.status === 504 || e.code === 'bad_response')
		? 'uncertain'
		: 'rejected';
}

/** The result of re-reading a resource after an uncertain write. */
export type Reconciliation =
	| { kind: 'checking' }
	/** Stored as sent. `unchecked` names write-only fields the API never echoes, so they could not be compared. */
	| { kind: 'confirmed'; unchecked: string[] }
	/** The stored resource differs from what was sent (or does not exist): the write did not land. */
	| { kind: 'not_saved'; differs: string[] }
	/** The re-read failed too. */
	| { kind: 'unreachable' };

export interface Comparison {
	/** Fields whose stored value is not the sent one. Empty = the write landed. */
	differs: string[];
	/** Write-only fields that could not be compared. */
	unchecked: string[];
}

export const CHECKING_TEXT = 'No answer came back. Checking whether the change was saved…';
export const NOT_SAVED_TEXT = 'Not saved — your changes are still here; you can save again.';
export const UNREACHABLE_TEXT = "Still can't reach the server; your changes are kept.";

export function reconcileText(r: Reconciliation): string {
	switch (r.kind) {
		case 'checking':
			return CHECKING_TEXT;
		case 'confirmed':
			return r.unchecked.length
				? `Saved — confirmed after reloading. Only the non-secret fields could be checked; the ${r.unchecked.join(' and ')} is never shown back.`
				: 'Saved — confirmed after reloading.';
		case 'not_saved':
			return NOT_SAVED_TEXT;
		case 'unreachable':
			return UNREACHABLE_TEXT;
	}
}

/** Resolves to null when the resource does not exist (404); any other failure propagates. */
export async function orNull<T>(p: Promise<T>): Promise<T | null> {
	try {
		return await p;
	} catch (e) {
		if (e instanceof ApiError && e.status === 404) return null;
		throw e;
	}
}

/** Re-reads the resource and compares it with what was sent. Never writes. */
export async function reconcile<S>(
	fetchStored: () => Promise<S | null>,
	compare: (stored: S | null) => Comparison,
): Promise<Reconciliation> {
	let stored: S | null;
	try {
		stored = await fetchStored();
	} catch {
		return { kind: 'unreachable' };
	}
	const c = compare(stored);
	return c.differs.length
		? { kind: 'not_saved', differs: c.differs }
		: { kind: 'confirmed', unchecked: c.unchecked };
}

/** JSON with object keys sorted, so key order never makes two equal values differ. */
function canon(v: unknown): string {
	if (v === undefined) return 'null';
	if (v === null || typeof v !== 'object') return JSON.stringify(v);
	if (Array.isArray(v)) return `[${v.map(canon).join(',')}]`;
	const o = v as Record<string, unknown>;
	return `{${Object.keys(o)
		.sort()
		.map((k) => `${JSON.stringify(k)}:${canon(o[k])}`)
		.join(',')}}`;
}

/** `''` and `null` are the same "no value" in these forms. */
const blank = (v: unknown) => (v === '' || v === undefined ? null : v);

export function sameValue(a: unknown, b: unknown): boolean {
	return canon(blank(a)) === canon(blank(b));
}

/** The listed fields whose stored value differs from the sent one; every field when nothing is stored. */
export function compareFields<K extends string>(
	sent: Partial<Record<K, unknown>>,
	stored: Partial<Record<K, unknown>> | null,
	fields: readonly K[],
): string[] {
	if (!stored) return [...fields];
	return fields.filter((f) => !sameValue(sent[f], stored[f]));
}

// ---- field-level refusals --------------------------------------------------------------------------------------

export interface FieldFault<F extends string> {
	/** The form field the refusal is about, or null when it names none (shown above the actions). */
	field: F | null;
	code: string;
	message: string;
}

/**
 * The refusal as a field fault. The error envelope has no field name, so the machine code picks the field; a
 * `secret_value_rejected` names it in its message ("field X looks like a secret…").
 */
export function fieldFault<F extends string>(
	e: unknown,
	codes: Partial<Record<string, F>>,
	fields: readonly F[],
): FieldFault<F> {
	if (!(e instanceof ApiError))
		return { field: null, code: 'error', message: (e as Error)?.message ?? String(e) };
	if (e.status === 403) return { field: null, code: 'forbidden', message: forbiddenText(e) };
	let field: F | null = codes[e.code] ?? null;
	if (!field && e.code === 'secret_value_rejected') {
		const named = /field (\w+) looks like a secret/.exec(e.message)?.[1];
		field = fields.find((f) => f === named) ?? null;
	}
	return { field, code: e.code, message: e.message };
}

/** The text of a 403: the role the API named (D88 `missing_role`), else the server's message. */
export function forbiddenText(e: ApiError): string {
	const label = e.missingRole && ROLE_LABEL[e.missingRole as Role];
	return label ? `Needs the ${label} role.` : e.message || 'Not allowed.';
}
