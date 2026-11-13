// Display helpers for connector instances (T10/T11).
import type { ConnectorInstance } from '$lib/api/types/connectors';

export const STATUS_TEXT: Record<ConnectorInstance['status'], string> = {
	ok: 'ok',
	untested: 'untested',
	auth_failed: 'connector_auth_failed',
	invalid_target: 'invalid_target',
	draft_template: 'draft template · test only',
};

/** "T3 6/7 green": results that ran (green or red) vs green; write-unverified counted apart. */
export function t3Summary(i: Pick<ConnectorInstance, 'contract'>): {
	green: number;
	run: number;
	unverified: number;
} {
	const ran = i.contract.filter((r) => r.outcome === 'green' || r.outcome === 'red');
	return {
		green: ran.filter((r) => r.outcome === 'green').length,
		run: ran.length,
		unverified: i.contract.filter((r) => r.outcome === 'write_unverified').length,
	};
}
