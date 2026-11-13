// Declared tool inputs (A2) as a form: rows the editor binds to, and the lossless conversion to and from the wire
// JSON (`ToolPolicy`, `IamStep`). Pure; the component holds no conversion logic.
import type {
	Confirmation,
	IamStep,
	InputDecl,
	OnFailure,
	OutputDecl,
	StateFrom,
	ToolPolicy,
} from '$lib/api/types/iam';

/** Where one input comes from. */
export type InputMode = 'model' | 'state' | 'tool' | 'ask';

export interface InputRow {
	field: string;
	mode: InputMode;
	/** The state key (all modes except `model`). */
	key: string;
	/** `tool` mode: the producing op and its output field. */
	tool: string;
	output: string;
	/** `ask` mode. */
	prompt: string;
	secret: boolean;
	retries: number;
}

export interface OutputRow {
	field: string;
	write: boolean;
	key: string;
	kind: 'value' | 'secret';
	/** Seconds as typed; empty = no expiry. */
	ttl: string;
	persist: boolean;
}

export interface DeclForm {
	inputs: InputRow[];
	outputs: OutputRow[];
}

export interface PolicyForm extends DeclForm {
	/** Comma/space separated state keys. */
	requires: string;
	confirmation: Confirmation;
	onFailure: OnFailure;
}

/** A tool that may produce a value: its op key and output fields. */
export interface ToolChoice {
	key: string;
	output_fields: string[];
}

export const DEFAULT_RETRIES = 2;

export const blankInput = (field: string): InputRow => ({
	field,
	mode: 'model',
	key: '',
	tool: '',
	output: '',
	prompt: '',
	secret: false,
	retries: DEFAULT_RETRIES,
});

export const blankOutput = (field: string): OutputRow => ({
	field,
	write: false,
	key: '',
	kind: 'value',
	ttl: '',
	persist: false,
});

function inputRow(field: string, d: InputDecl | undefined): InputRow {
	const r = blankInput(field);
	if (!d || d.source === 'model') return r;
	r.key = d.key;
	const f = d.from ?? 'state';
	if (f === 'state') r.mode = 'state';
	else if ('tool' in f) {
		r.mode = 'tool';
		r.tool = f.tool;
		r.output = f.output;
	} else {
		r.mode = 'ask';
		r.prompt = f.ask_user;
		r.secret = f.secret ?? false;
		r.retries = f.retries ?? DEFAULT_RETRIES;
	}
	return r;
}

function outputRow(field: string, d: OutputDecl | undefined): OutputRow {
	const r = blankOutput(field);
	if (!d) return r;
	r.write = true;
	r.key = d.key;
	r.kind = d.kind ?? 'value';
	r.ttl = d.ttl_s == null ? '' : String(d.ttl_s);
	r.persist = d.persist_for_subject ?? false;
	return r;
}

/** Rows for the op's fields, plus any declared name the op no longer lists (kept so a save does not drop it). */
export function toDeclForm(
	inputFields: readonly string[],
	outputFields: readonly string[],
	inputs: Record<string, InputDecl> = {},
	outputs: Record<string, OutputDecl> = {},
): DeclForm {
	const extra = (all: readonly string[], d: Record<string, unknown>) =>
		Object.keys(d).filter((k) => !all.includes(k));
	return {
		inputs: [...inputFields, ...extra(inputFields, inputs)].map((f) => inputRow(f, inputs[f])),
		outputs: [...outputFields, ...extra(outputFields, outputs)].map((f) => outputRow(f, outputs[f])),
	};
}

export function inputDecl(r: InputRow): InputDecl {
	if (r.mode === 'model') return { source: 'model' };
	const key = r.key.trim();
	let from: StateFrom;
	if (r.mode === 'state') from = 'state';
	else if (r.mode === 'tool') from = { tool: r.tool.trim(), output: r.output.trim() };
	else from = { ask_user: r.prompt.trim(), secret: r.secret, retries: r.retries };
	return { source: 'state', key, from };
}

/** Only the inputs that leave the model; the model is the default. */
export function toInputs(rows: readonly InputRow[]): Record<string, InputDecl> {
	const out: Record<string, InputDecl> = {};
	for (const r of rows) if (r.mode !== 'model') out[r.field] = inputDecl(r);
	return out;
}

export function toOutputs(rows: readonly OutputRow[]): Record<string, OutputDecl> {
	const out: Record<string, OutputDecl> = {};
	for (const r of rows) {
		if (!r.write) continue;
		const d: OutputDecl = { key: r.key.trim(), kind: r.kind };
		const ttl = r.ttl.trim();
		if (ttl !== '') d.ttl_s = Number(ttl);
		if (r.persist) d.persist_for_subject = true;
		out[r.field] = d;
	}
	return out;
}

export const parseKeys = (text: string): string[] => [
	...new Set(text.split(/[\s,]+/).filter((k) => k !== '')),
];

export function toPolicyForm(
	inputFields: readonly string[],
	outputFields: readonly string[],
	p: ToolPolicy | null,
): PolicyForm {
	return {
		...toDeclForm(inputFields, outputFields, p?.inputs, p?.outputs),
		requires: (p?.requires ?? []).join(', '),
		confirmation: p?.confirmation ?? 'by_user',
		onFailure: p?.on_failure ?? 'handover',
	};
}

export function toPolicy(f: PolicyForm): ToolPolicy {
	return {
		inputs: toInputs(f.inputs),
		outputs: toOutputs(f.outputs),
		requires: parseKeys(f.requires),
		confirmation: f.confirmation,
		on_failure: f.onFailure,
	};
}

export function toStep(op: string, f: DeclForm): IamStep {
	return { op, inputs: toInputs(f.inputs), outputs: toOutputs(f.outputs) };
}

/** Is the policy the same as no declaration at all (every input from the model, nothing written, defaults)? */
export function isEmptyPolicy(p: ToolPolicy): boolean {
	return (
		Object.keys(p.inputs).length === 0 &&
		Object.keys(p.outputs).length === 0 &&
		p.requires.length === 0 &&
		p.confirmation === 'by_user' &&
		p.on_failure === 'handover'
	);
}

/** The first thing wrong with the rows that the server would refuse anyway, in plain words; null when fine. */
export function formProblem(f: DeclForm): string | null {
	for (const r of f.inputs) {
		if (r.mode === 'model') continue;
		if (!r.key.trim()) return `Name the state key for “${r.field}”.`;
		if (r.mode === 'tool' && (!r.tool.trim() || !r.output.trim()))
			return `Choose the tool and output field that provide “${r.field}”.`;
		if (r.mode === 'ask' && !r.prompt.trim())
			return `Write what the agent says when it asks for “${r.field}”.`;
		if (r.mode === 'ask' && (!Number.isInteger(r.retries) || r.retries < 0 || r.retries > 9))
			return `Retries for “${r.field}” must be between 0 and 9.`;
	}
	for (const r of f.outputs) {
		if (!r.write) continue;
		if (!r.key.trim()) return `Name the state key that “${r.field}” is written to.`;
		const ttl = r.ttl.trim();
		if (ttl !== '' && !(Number.isInteger(Number(ttl)) && Number(ttl) > 0))
			return `The expiry of “${r.field}” must be a whole number of seconds.`;
	}
	return null;
}

const PLAIN: Record<string, string> = {
	identity_incomplete:
		'A procedure establishes one person: exactly one step writes the person (subject), and email or name only beside it.',
	identity_not_plain: 'The person’s identity is a plain value without expiry.',
	identity_already_produced:
		'Another procedure this job uses already establishes the person’s identity. Detach it first.',
	reserved_key:
		'That state key belongs to the platform (customer, channel or caller details). Choose another name.',
	invalid_key: 'State keys use lowercase letters, digits, dots and underscores, and start with a letter.',
	unknown_operation: 'One of the tools named here does not exist in this job.',
	operation_not_expected: 'That tool is not part of this job.',
	cycle:
		'These declarations depend on each other in a circle: a tool needs a value that only that same chain can produce.',
	depth_exceeded: 'The chain of tools needed to get a value is too long. Shorten it.',
	no_producer:
		'Nothing provides a value this declaration needs. Add a tool that writes that key, or ask the user.',
	empty_prompt: 'Write what the agent should say when it asks.',
	invalid_retries: 'Retries must be a small whole number.',
	invalid_ttl: 'The expiry must be a whole number of seconds above zero.',
	duplicate_output_key: 'Two outputs write the same state key.',
	empty_input_name: 'An input needs a name.',
	invalid_name: 'Give the procedure a name.',
	name_taken: 'A procedure with this name already exists.',
	procedure_attached: 'A job still uses this procedure. Detach it from every job first.',
	procedure_not_found: 'This procedure no longer exists.',
	policy_not_found: 'There is no saved declaration for this tool.',
	invalid_body: 'The declaration is not valid.',
};

/** A plain-language message for a server refusal code; the server's own text when the code is unknown. */
export function explainCode(code: string | undefined, fallback: string): string {
	return (code && PLAIN[code]) || fallback;
}
