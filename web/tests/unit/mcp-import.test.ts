import { describe, expect, it } from 'vitest';
import {
	canImportMcp,
	importedLine,
	importSummary,
	issueLine,
	pinnedSystems,
	transportLabel,
} from '../../src/lib/connectors/mcp-import';
import type { McpImportResult } from '../../src/lib/api/types/connectors';

const op = (action: 'created' | 'updated', own = false) => ({
	op: 'card.move',
	action,
	tool: 'card',
	side_effect: 'Write',
	own,
	inputs: [],
	outputs: [],
});

describe('mcp import helpers', () => {
	it('pins only this template version', () => {
		const pins = [
			{ key: 'a', version: '1', instance_id: 'x' },
			{ key: 'a', version: '2', instance_id: 'y' },
			{ key: 'b', version: '1', instance_id: 'z' },
		];
		expect(pinnedSystems(pins, 'a', '1').map((p) => p.instance_id)).toEqual(['x']);
	});
	it('offers import for draft mcp templates only', () => {
		expect(canImportMcp({ transport: 'mcp', status: 'draft' })).toBe(true);
		expect(canImportMcp({ transport: 'mcp', status: 'active' })).toBe(false);
		expect(canImportMcp({ transport: 'rest', status: 'draft' })).toBe(false);
	});
	it('summarises counts and omits zero extras', () => {
		const r: McpImportResult = {
			ops: [op('created', true), op('created'), op('updated')],
			invalid_operations: [],
			skipped: [],
		};
		expect(importSummary(r)).toBe('MCP import: 3 imported (1 your own) — 2 created, 1 updated.');
		expect(importedLine({ ops: [] })).toBe('0 imported (0 your own)');
	});
	it('formats issues and transports', () => {
		expect(issueLine({ path: 'ops[0].path', message: 'bad' })).toBe('ops[0].path — bad');
		expect(issueLine({ code: 'x' })).toBe('x');
		expect(transportLabel('mcp')).toBe('MCP');
		expect(transportLabel('soap')).toBe('soap');
	});
});
