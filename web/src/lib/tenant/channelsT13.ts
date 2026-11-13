// T13 Channels — pure helpers: the customer → channel → edge → gate 2 → job@release path, the identity-level
// coverage of a channel (10 §3.1, 12 §3 N5) and the refusal causes the preview cycles through (03 §2.9).
import type { Channel, ChannelIdentityLevel, ChannelJobOperation, ChannelsView } from '$lib/api/types/tenant';
import { WEBCHAT_PUBLIC_BASE, type ChannelBinding } from '$lib/api/types/conversations';
import { jobPath } from '$lib/scope';

export const IDENTITY_LEVELS: { level: ChannelIdentityLevel; label: string; meaning: string }[] = [
	{ level: '0', label: 'Public', meaning: 'nobody known' },
	{ level: '1', label: 'Identified', meaning: 'claimed, not proven' },
	{ level: '2', label: 'Verified', meaning: 'ID&V passed' },
	{ level: '3', label: 'Step-up', meaning: 'OTP / MFA' },
	{ level: 'S', label: 'Staff', meaning: 'staff-only' },
];

const RANK: Record<ChannelIdentityLevel, number> = { '0': 0, '1': 1, '2': 2, '3': 3, S: 9 };
export const levelRank = (l: ChannelIdentityLevel): number => RANK[l];
export const levelLabel = (l: ChannelIdentityLevel | undefined): string =>
	l ? `${l} · ${IDENTITY_LEVELS.find((x) => x.level === l)!.label}` : '—';

export type Coverage = 'at_start' | 'after_verify' | 'refused';

/** Where an operation lands on a channel: allowed from the first message, after a step-up, or refused. */
export function coverage(
	op: ChannelJobOperation,
	ch: Pick<Channel, 'identity_start' | 'identity_ceiling'>,
): Coverage {
	const need = levelRank(op.identity_level);
	if (op.identity_level === 'S' || !ch.identity_ceiling) return 'refused';
	if (need <= levelRank(ch.identity_start ?? '0')) return 'at_start';
	if (need <= levelRank(ch.identity_ceiling)) return 'after_verify';
	return 'refused';
}

export function coverageGroups(
	ops: ChannelJobOperation[],
	ch: Pick<Channel, 'identity_start' | 'identity_ceiling'>,
): Record<Coverage, ChannelJobOperation[]> {
	const out: Record<Coverage, ChannelJobOperation[]> = { at_start: [], after_verify: [], refused: [] };
	for (const op of ops) out[coverage(op, ch)].push(op);
	return out;
}

/** The edge's drain check (03 §2.9): verdict active AND tenant active. Freshness (< 5 min) is the edge's job. */
export function gateOpen(v: Pick<ChannelsView, 'gate' | 'tenant_status'>): boolean {
	return v.gate.status === 'active' && (v.tenant_status ?? 'active') === 'active';
}

/** Why the gate is closed, as the code staff would see (customers never do). */
export function gateCause(v: Pick<ChannelsView, 'gate' | 'tenant_status'>): string | null {
	if (v.tenant_status && v.tenant_status !== 'active') return `tenant_${v.tenant_status}`;
	if (v.gate.status !== 'active') return v.gate.reason ?? `licence_${v.gate.status}`;
	return null;
}

export type HopState = 'ok' | 'warn' | 'err' | 'off';
export interface Hop {
	key: string;
	label: string;
	value: string;
	detail: string;
	state: HopState;
	stateText: string;
	href?: string;
}

/** The first channel that delivers real customers to this tenant (today: the shared Telegram bot when pinned here). */
export const primaryCustomerChannel = (v: ChannelsView): Channel | undefined =>
	v.channels.find((c) => c.side !== 'worker' && c.kind !== 'staff_chat' && c.state === 'live');

/** The channel the "Send test message" button sends through: the first live Telegram binding (only Telegram has a
 *  test send), whatever kind the primary customer channel is. */
export const testMessageChannel = (v: ChannelsView): Channel | undefined =>
	v.channels.find((c) => c.side !== 'worker' && c.kind === 'telegram' && c.state === 'live');

/** The public base a web chat snippet loads its widget from (`<base>/webchat/widget.js`, as the API builds it);
 *  `WEBCHAT_PUBLIC_BASE` when there is no snippet, it names no widget script, or its base is not an http(s) URL
 *  (only the base's origin is kept: no `javascript:` / `data:` link, no path smuggled into the test page link). */
export function webChatBase(snippet: string | null | undefined): string {
	const m = snippet?.match(/src="([^"]+)\/webchat\/widget\.js"/);
	if (!m?.[1]) return WEBCHAT_PUBLIC_BASE;
	try {
		const u = new URL(m[1]);
		return u.protocol === 'https:' || u.protocol === 'http:' ? u.origin : WEBCHAT_PUBLIC_BASE;
	} catch {
		return WEBCHAT_PUBLIC_BASE;
	}
}

/** The edge's hosted test page of a web chat binding (`<base>/webchat/demo?key=<widget key>`): it opens the real
 *  widget on the edge's own origin, which the edge always allows — so it works before any site origin is added.
 *  Null unless the binding is an enabled, verified web chat with a key (an unverified door carries no traffic). */
export function webChatDemoUrl(
	b: Pick<ChannelBinding, 'kind' | 'enabled' | 'verified' | 'external_id' | 'snippet'>,
): string | null {
	if (b.kind !== 'web_chat' || !b.enabled || !b.verified || !b.external_id) return null;
	return `${webChatBase(b.snippet)}/webchat/demo?key=${encodeURIComponent(b.external_id)}`;
}

/** `https://<host>` of a bare edge host (as `/api/tenant/channels` sends it, scheme dropped); null unless it is a plain
 *  host[:port] (no path, query, credentials or scheme smuggled into the test page link). */
export function edgeOrigin(host: string | null | undefined): string | null {
	if (!host) return null;
	try {
		const u = new URL(`https://${host}`);
		return u.host === host.toLowerCase() ? u.origin : null;
	} catch {
		return null;
	}
}

/** The web chat door the "Try it" action opens: the first live web chat binding with a job. With its widget key and
 *  edge host (`/api/tenant/channels` sends both for a live web chat) it opens the hosted test page
 *  `https://<edge_host>/webchat/demo?key=<widget_key>` directly (`direct`); otherwise that job's Channels page, whose
 *  web chat panel has the test page. */
export function webChatTry(v: ChannelsView): { channel: Channel; href: string; direct: boolean } | undefined {
	const channel = v.channels.find((c) => c.kind === 'web_chat' && c.state === 'live' && c.bound_job);
	if (!channel) return undefined;
	const base = edgeOrigin(channel.edge_host);
	if (channel.widget_key && base)
		return {
			channel,
			href: `${base}/webchat/demo?key=${encodeURIComponent(channel.widget_key)}`,
			direct: true,
		};
	return { channel, href: jobPath(channel.bound_job!, '/channels'), direct: false };
}

/** Customer → channel → edge → gate 2 → job@release, each hop with a health state. */
export function reachPath(v: ChannelsView, ch: Channel): Hop[] {
	const wh = ch.webhook;
	// Only Telegram reaches the edge through a webhook; any other kind has no webhook to report on.
	const noWebhook = !wh && ch.kind !== 'telegram';
	const whState: HopState = noWebhook
		? 'ok'
		: !wh
			? 'off'
			: wh.status === 'ok'
				? 'ok'
				: wh.status === 'degraded'
					? 'warn'
					: wh.status === 'quiet'
						? 'off'
						: 'err';
	const open = gateOpen(v);
	const a = ch.answered_by;
	return [
		{
			key: 'customer',
			label: 'Customer',
			value: ch.kind === 'telegram' ? 'Allow-listed Telegram user' : 'Customer',
			detail: `Starts at identity ${levelLabel(ch.identity_start)}`,
			state: 'ok',
			stateText: 'writes',
		},
		{
			key: 'channel',
			label: 'Channel',
			value: ch.bot_username ?? ch.label,
			detail: ch.binding === 'shared_env_bot' ? 'One bot per deployment, pinned by env (K40)' : ch.label,
			state: ch.state === 'live' ? 'ok' : 'off',
			stateText: ch.state === 'live' ? 'pinned here' : ch.state.replace('_', ' '),
		},
		{
			key: 'edge',
			label: noWebhook ? 'Edge' : 'Edge webhook',
			value: ch.edge_host ?? '—',
			detail: noWebhook
				? ch.kind === 'web_chat'
					? 'No webhook: the edge serves the widget directly'
					: 'No webhook for this channel'
				: wh
					? wh.last_error
						? `Last error: ${wh.last_error}`
						: wh.pending_update_count === null
							? 'No errors'
							: `${wh.pending_update_count} pending · no errors`
					: 'The edge has reported nothing yet',
			state: whState,
			stateText: noWebhook ? 'direct' : wh ? wh.status : 'none',
		},
		{
			key: 'gate',
			label: 'Gate 2',
			value: `Licence ${v.gate.status} · tenant ${v.tenant_status ?? 'active'}`,
			detail: open ? 'Checked at every inbox drain' : 'Every turn gets the neutral refusal text',
			state: open ? 'ok' : 'err',
			stateText: open ? 'lets through' : 'refuses',
		},
		{
			key: 'job',
			label: 'Answers',
			value: a ? a.job_label : '—',
			detail: a
				? a.release_id
					? `Release ${a.release_id} (${a.release_hash}) · ${a.env}`
					: 'No release cut yet — nothing to answer with'
				: 'No job bound',
			state: !a ? 'off' : a.release_id ? (open ? 'ok' : 'off') : 'err',
			stateText: !a ? 'none' : a.release_id ? (open ? 'answering' : 'idle') : 'no release',
			href: a ? `/admin/jobs/${a.job}/release` : undefined,
		},
	];
}

/** Causes the refusal preview can simulate. The customer text is the same for all of them — that is the point. */
export const REFUSAL_CAUSES: { id: string; label: string; staff: string }[] = [
	{ id: 'tenant_suspended', label: 'Tenant suspended', staff: '403 tenant_suspended' },
	{ id: 'licence_expired', label: 'Licence expired', staff: '403 licence expired' },
	{ id: 'licence_inactive', label: 'Licence inactive', staff: '403 licence inactive' },
	{ id: 'verdict_stale', label: 'Verdict older than 5 min', staff: '403 unavailable (stale verdict)' },
	{ id: 'licences_down', label: 'Licence service down', staff: '403 unavailable' },
];

/** Groups channels the way a tenant admin thinks about them. */
export function channelGroups(
	chs: Channel[],
): { key: string; title: string; sub: string; items: Channel[] }[] {
	// D126.4: worker-side rows are the job's staff handover targets, never a customer door
	const staff = chs.filter((c) => c.side === 'worker');
	const doors = chs.filter((c) => c.side !== 'worker');
	const live = doors.filter((c) => c.kind !== 'staff_chat' && c.state === 'live');
	const test = doors.filter((c) => c.kind === 'staff_chat');
	const notHere = doors.filter((c) => c.kind !== 'staff_chat' && c.state !== 'live');
	return [
		{
			key: 'live',
			title: 'Reaching your customers',
			sub: 'Real customer turns, gated by licence',
			items: live,
		},
		{
			key: 'test',
			title: 'For your staff to test',
			sub: 'Staff play a customer; defaults to the test env',
			items: test,
		},
		{
			key: 'later',
			title: 'Not reaching your customers yet',
			sub: 'Not set up, not live yet, pinned to another tenant, or not built yet',
			items: notHere,
		},
		{
			key: 'staff',
			title: 'Staff handover channels',
			sub: 'Where each job tells your staff about handovers (worker side); open a row to edit it in its job',
			items: staff,
		},
	].filter((g) => g.items.length > 0);
}
