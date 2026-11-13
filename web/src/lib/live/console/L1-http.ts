// L1 operator console over HTTP: B12's console rows (`GET /api/jobs/{job}/console/conversations`) and the open
// conversation's transcript (`GET /api/jobs/{job}/conversations/{id}`) read as the console's ConsoleConversation.
// B12 carries no turn trace (gates, qualification, operations are the runtime's, B4): a transcript line becomes a
// message (customer / staff), a reply (agent) or a state note (system). The row carries the owner record
// (`owner: OwnerView`), shown as it is; ownership actions live on /support. Pure, unit-tested in
// tests/unit/console-L1-http.test.ts.
import type { ConsoleConversationRow } from '$lib/api/types/conversations';
import type { MessageView } from '$lib/api/types/support';
import type { ConsoleConversation, ConsoleSnapshot } from '$lib/api/types/live';
import type { CaseRow, CaseStatus, Channel, TimelineEvent } from '$lib/api/types/runtime';
import { CHANNEL_LABEL } from '$lib/conversations/labels';

const STATUSES: readonly CaseStatus[] = [
	'open',
	'identity_required',
	'confirmation_required',
	'handover',
	'resolved',
	'not_eligible',
	'failed',
	'cancelled_offboard',
];

const isoOf = (ms: number) => new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z');

/** The row's last line, as the first event of a conversation whose transcript was not read. */
const lastLine = (row: ConsoleConversationRow): MessageView | null =>
	row.last_message
		? {
				message_id: 'last',
				kind: row.last_message.role === 'system' ? 'system' : row.last_message.role,
				event: null,
				text: row.last_message.text,
				author_name: null,
				at: isoOf(row.last_message.at_ms),
				case_id: null,
				client_msg_id: null,
				delivery: null,
				mirror: null,
			}
		: null;

/** One transcript line (the typed `MessageView`) as a console timeline event. */
export function toTimelineEvent(m: MessageView, customer: string, status: CaseStatus): TimelineEvent {
	const at = m.at;
	switch (m.kind) {
		case 'customer':
			return { kind: 'message', at, from: 'customer', author: customer, text: m.text };
		case 'staff':
			return { kind: 'message', at, from: 'staff', author: m.author_name || 'Staff', text: m.text };
		case 'agent':
			return { kind: 'reply', at, text: m.text };
		default:
			// system lines and internal notes are state notes here
			return { kind: 'state', at, status, note: m.text };
	}
}

/**
 * A console row (and, for the open conversation, its transcript) as the operator console reads it. Without a
 * transcript the row's last line is the only event. `staff` (the staff test chat) runs the draft, every other
 * channel production on the live release (D54).
 */
export function toConsoleConversation(
	row: ConsoleConversationRow,
	messages: MessageView[] | null = null,
): ConsoleConversation {
	const status: CaseStatus = STATUSES.includes(row.status as CaseStatus)
		? (row.status as CaseStatus)
		: 'open';
	const customer = row.customer_name || 'Customer';
	const last = lastLine(row);
	const lines = messages ?? (last ? [last] : []);
	const events = lines.map((m) => toTimelineEvent(m, customer, status));
	const env = row.channel === 'staff' ? 'test' : 'prod';
	const channelName = CHANNEL_LABEL[row.channel] ?? row.channel;
	const caseRow: CaseRow = {
		case_id: row.case_id ?? row.conversation_id,
		tenant_id: '' as CaseRow['tenant_id'],
		job: '' as CaseRow['job'],
		conversation_id: row.conversation_id,
		channel: row.channel as Channel,
		env,
		activity: '',
		request_type: '',
		title: row.open_item_id ? `Handover ${row.open_item_id} · ${channelName}` : `${channelName} conversation`,
		status,
		outcome: null,
		escalation_reason: null,
		// a known customer (B13 party) is the KYC'd one: level 2, as the runtime opens its cases
		identity_level: row.party?.known ? 2 : 0,
		customer_id: row.party?.id ?? '',
		customer_name: customer,
		initiator_sub: null,
		release_id: null,
		draft_rev: null,
		started_at: row.updated_at,
		updated_at: row.updated_at,
		sla: null,
		assignee: null,
		alert: null,
		op_calls: 0,
	};
	return {
		case: caseRow,
		events,
		active: false,
		release: env === 'test' ? { id: 'draft', source: 'draft' } : { id: 'live release', source: 'live' },
		owner: row.owner,
	};
}

/** The console's snapshot: every row, the open conversation with its whole transcript. */
export function toConsoleSnapshot(
	rows: ConsoleConversationRow[],
	open: { id: string; messages: MessageView[] } | null,
	now: number,
): ConsoleSnapshot {
	return {
		at_ms: now,
		conversations: rows.map((r) =>
			toConsoleConversation(r, open && open.id === r.conversation_id ? open.messages : null),
		),
	};
}

/** Frames of the job / conversation feed that change what the console shows (B12 hub + runtime `case`). */
export const CONSOLE_FRAMES: ReadonlySet<string> = new Set(['message', 'case', 'handover', 'owner', 'chat']);

/**
 * Latest wins: each run is numbered when it starts; an answer older than one already handed out resolves `null`,
 * so a slow earlier read never overwrites a newer snapshot.
 */
export function latestWins<T>(): (run: () => Promise<T>) => Promise<T | null> {
	let issued = 0;
	let applied = 0;
	return async (run) => {
		const n = ++issued;
		const v = await run();
		if (n < applied) return null;
		applied = n;
		return v;
	};
}
