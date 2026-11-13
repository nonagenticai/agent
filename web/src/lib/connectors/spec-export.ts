// F-36 (B7 T13a): the integration field-spec export — one row per operation × field × env, so a client's IT can see
// what the job needs and which system and mapping answers it. Pure: the backend (`crates/agent-api/src/connectors/
// spec_export.rs`) loads the same `SpecInput` from its tables and renders the same CSV / Markdown. Never carries a base URL,
// env ids, credential fields, a fingerprint or a sample body: the input has no place for them.
import type { RouteCell } from '$lib/api/types/connectors';
import { opKey } from './ops';

/** One expected operation with its catalogue inputs and outputs. */
export interface SpecOp {
	object: string;
	verb: string;
	variant: string;
	side_effect: string;
	identity_level: string;
	input_fields: string[];
	output_fields: string[];
}

/** One object field as the catalogue declares it (`ObjectField` subset). */
export interface SpecField {
	field: string;
	data_type: string;
	required: boolean;
	description: string;
}

/** One routed system: its pinned template ops (paths only) and the tenant's mapping overrides. */
export interface SpecInstance {
	id: string;
	label: string;
	template_key: string;
	template_version: string;
	ops: { op: string; path: string; request: string; field_map: { path: string; field: string }[] }[];
	overrides: { op: string; field: string; path: string }[];
}

export interface SpecInput {
	job: string;
	of: 'draft' | 'live';
	ops: SpecOp[];
	/** object → its fields. */
	object_fields: Record<string, SpecField[]>;
	/** `object.verb{variant}@env` → the D67 cell (jobOperations `cells`). */
	cells: Record<string, RouteCell>;
	instances: SpecInstance[];
}

export const SPEC_COLUMNS = [
	'operation',
	'side_effect',
	'identity_level',
	'direction',
	'field',
	'type',
	'required',
	'description',
	'env',
	'system',
	'template',
	'mapping_path',
	'mapping_source',
	'status',
] as const;

export type SpecRow = Record<(typeof SPEC_COLUMNS)[number], string>;

/** The envs in export order. */
export const SPEC_ENVS = ['test', 'prod'] as const;

const TARGET_LABEL: Record<string, string> = {
	builtin: 'built-in (agent)',
	agent_cases: 'agent_cases (fallback)',
	deep_link: 'deep link',
	not_offered: 'not offered',
};

/** Every row of the export, in op order, inputs then outputs, field order, `test` then `prod`. */
export function specRows(input: SpecInput): SpecRow[] {
	const rows: SpecRow[] = [];
	for (const op of input.ops) {
		const key = opKey(op);
		const star = opKey({ ...op, variant: '*' });
		const fields = input.object_fields[op.object] ?? [];
		const dirs: ['input' | 'output', string[]][] = [
			['input', op.input_fields],
			['output', op.output_fields],
		];
		for (const [direction, names] of dirs)
			for (const name of names) {
				const def = fields.find((f) => f.field === name);
				for (const env of SPEC_ENVS) {
					const cell = input.cells[`${key}@${env}`];
					const t = cell?.target ?? null;
					let system = '';
					let template = '';
					let mapping_path = '';
					let mapping_source = 'unmapped';
					if (t && t.kind === 'instance') {
						const inst = input.instances.find((i) => i.id === t.instance_id);
						system = inst ? `${inst.id} (${inst.label})` : t.instance_id;
						if (inst) {
							template = `${inst.template_key}@${inst.template_version}`;
							const top = inst.ops.find((o) => o.op === key) ?? inst.ops.find((o) => o.op === star);
							if (top && direction === 'output') {
								const ov = inst.overrides.find((o) => o.op === top.op && o.field === name);
								const fm = top.field_map.find((f) => f.field === name);
								if (ov) [mapping_path, mapping_source] = [ov.path, 'override'];
								else if (fm) [mapping_path, mapping_source] = [fm.path, 'default'];
							} else if (top) {
								const ph = `{{canonical.${name}}}`;
								if (top.path.includes(ph)) [mapping_path, mapping_source] = ['path', 'default'];
								else if (top.request.includes(ph)) [mapping_path, mapping_source] = ['request', 'default'];
							}
						}
					} else if (t) system = TARGET_LABEL[t.kind] ?? t.kind;
					rows.push({
						operation: key,
						side_effect: op.side_effect,
						identity_level: op.identity_level,
						direction,
						field: name,
						type: def?.data_type ?? '',
						required: def ? (def.required ? 'yes' : 'no') : '',
						description: def?.description ?? '',
						env,
						system,
						template,
						mapping_path,
						mapping_source,
						status: cell?.state ?? 'unrouted',
					});
				}
			}
	}
	return rows;
}

/** One CSV cell: formula-injection guard (`= + - @` → leading `'`), then RFC 4180 quoting when needed. */
export function csvCell(v: string): string {
	const s = /^[=+\-@]/.test(v) ? `'${v}` : v;
	return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

/** RFC 4180: header row, CRLF line ends (also after the last row). */
export function specCsv(input: SpecInput): string {
	const lines = [SPEC_COLUMNS.join(',')];
	for (const r of specRows(input)) lines.push(SPEC_COLUMNS.map((c) => csvCell(r[c])).join(','));
	return lines.map((l) => `${l}\r\n`).join('');
}

/** One Markdown table cell: pipes escaped, line breaks flattened. */
export function mdCell(v: string): string {
	return v
		.replace(/\\/g, '\\\\')
		.replace(/\|/g, '\\|')
		.replace(/\r\n|\r|\n/g, ' ');
}

const MD_COLUMNS = SPEC_COLUMNS.filter(
	(c) => c !== 'operation' && c !== 'side_effect' && c !== 'identity_level',
);

/** One section per operation (`## op`, side effect + identity level), then its table; `_No fields._` when empty. */
export function specMd(input: SpecInput): string {
	const rows = specRows(input);
	let out = `# Integration spec: ${mdCell(input.job)} (${input.of})\n`;
	for (const op of input.ops) {
		const key = opKey(op);
		out += `\n## ${mdCell(key)}\n\nSide effect ${mdCell(op.side_effect)} · identity level ${mdCell(op.identity_level)}\n\n`;
		const mine = rows.filter((r) => r.operation === key);
		if (!mine.length) {
			out += '_No fields._\n';
			continue;
		}
		out += `| ${MD_COLUMNS.join(' | ')} |\n`;
		out += `|${MD_COLUMNS.map(() => '---').join('|')}|\n`;
		for (const r of mine) out += `| ${MD_COLUMNS.map((c) => mdCell(r[c])).join(' | ')} |\n`;
	}
	return out;
}

/** `<job_key>-integration-spec-<of>.<csv|md>`. */
export function specFilename(job: string, of: 'draft' | 'live', format: 'csv' | 'md'): string {
	return `${job}-integration-spec-${of}.${format}`;
}
