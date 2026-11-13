// Pure helpers for the "Import tools from MCP" panel of a tenant template (T12).
import type { McpImportResult } from '$lib/api/types/connectors';

/** Systems that may be read: the ones pinned to exactly this template version. */
export function pinnedSystems<P extends { key: string; version: string }>(
	pins: P[],
	key: string,
	version: string,
): P[] {
	return pins.filter((p) => p.key === key && p.version === version);
}

/** The import panel is offered for draft MCP templates only. */
export function canImportMcp(t: { transport: string; status: string }): boolean {
	return t.transport === 'mcp' && t.status === 'draft';
}

/** One plain-words line for the toast after an import. */
export function importSummary(r: McpImportResult): string {
	const created = r.ops.filter((o) => o.action === 'created').length;
	const updated = r.ops.filter((o) => o.action === 'updated').length;
	const parts = [`${created} created`, `${updated} updated`];
	if (r.invalid_operations.length) parts.push(`${r.invalid_operations.length} invalid`);
	if (r.skipped.length) parts.push(`${r.skipped.length} skipped`);
	return `MCP import: ${importedLine(r)} — ${parts.join(', ')}.`;
}

/** "N imported (M your own)" — the ops written to the draft, and how many of them are the tenant's own. */
export function importedLine(r: Pick<McpImportResult, 'ops'>): string {
	const own = r.ops.filter((o) => o.own).length;
	return `${r.ops.length} imported (${own} your own)`;
}

/** An issue as one line: `path — message`. */
export function issueLine(i: { path?: string; code?: string; message?: string }): string {
	const text = i.message ?? i.code ?? 'invalid';
	return i.path ? `${i.path} — ${text}` : text;
}

export const TRANSPORT_LABEL: Record<string, string> = {
	rest: 'REST',
	mcp: 'MCP',
};
export const transportLabel = (t: string): string => TRANSPORT_LABEL[t] ?? t;
