// Shapes for B12 (conversations, handover and two-sided channels): the job's console conversation list, a
// conversation's transcript, its live feed frames, and the job's channel bindings (customer- and worker-side
// connectors). Wire shapes of the B12 plan §4 (snake_case, as agent-api sends them).
import type { Iso, Paged } from './core';
import type { OwnerView } from './support';

/** Every channel a conversation can run on (B12 §1: the widened channel CHECK). */
export type ConversationChannel = 'staff' | 'telegram' | 'web_chat' | 'voice' | 'api';

/** Who said a line of a conversation. */
export type TranscriptRole = 'customer' | 'caller' | 'agent' | 'staff' | 'system';

export interface ConversationLastMessage {
	role: TranscriptRole;
	text: string;
	at_ms: number;
}

/** One row of `GET /api/jobs/{job}/console/conversations`. */
export interface ConsoleConversationRow {
	conversation_id: string;
	channel: ConversationChannel;
	customer_name: string | null;
	case_id: string | null;
	/** The case status (`open`, `handover`, `resolved`, …). */
	status: string;
	/** Who answers: the agent, nobody yet (the queue) or a named staff member (SW-1; the `owner` frame carries the same). */
	owner: OwnerView;
	/** The open handover item of this conversation (`HO-n`), if any. */
	open_item_id: string | null;
	last_message: ConversationLastMessage | null;
	updated_at: Iso;
	/** B13: the customer behind the conversation (null before identity resolution ran). */
	party?: PartyRef | null;
}

export type ConsoleConversationPage = Paged<ConsoleConversationRow>;

/** One line of a transcript. */
export interface TranscriptMessage {
	message_id?: string;
	role: TranscriptRole;
	text: string;
	at_ms: number;
	/** The staff member's name on a staff line. */
	author?: string | null;
}

/** One frame of the conversation feed (`GET /api/jobs/{job}/conversations/{id}/events`): its event name + JSON. */
export interface ConversationFrame {
	event: string;
	data: unknown;
	/** The frame's SSE `id`, when the server gave one (a reconnect sends the last as `Last-Event-ID`). */
	id?: string;
}

export interface ConversationEventsOptions {
	onFrame: (f: ConversationFrame) => void;
	signal?: AbortSignal;
	/** Sent as `Last-Event-ID` (a reconnect): the server replays what it still holds after it. */
	lastEventId?: string | null;
	/** The stream is open (the server answered); frames follow. */
	onOpen?: () => void;
}

// ---- channel bindings (B12 §2 `connector_bindings`, §4) -----------------------------------------------------------

/** `telegram` (B7 T15b): customer side only, the tenant's own bot; verified at creation (the token proves it).
 * `helpdesk` (B7 R9): worker side only, Frappe Helpdesk; verified at creation (the site's API key). */
export type BindingKind = 'web_chat' | 'telegram' | 'helpdesk';
export const BINDING_KINDS: readonly BindingKind[] = ['web_chat', 'telegram', 'helpdesk'];

/** Kinds with one side only: web chat and Telegram face customers, Frappe Helpdesk (B7 R9) is a worker target. */
export function sidesOf(kind: BindingKind): BindingSide[] {
	if (kind === 'web_chat' || kind === 'telegram') return ['customer'];
	if (kind === 'helpdesk') return ['worker'];
	return ['customer', 'worker'];
}

/** Kinds whose credential is required, not an optional BYO override (Frappe Helpdesk: the site's API key). */
export function credentialRequired(kind: BindingKind): boolean {
	return kind === 'helpdesk' || kind === 'telegram';
}

/** `customer`: customers reach the job here. `worker`: staff are notified of handovers here and reply in thread. */
export type BindingSide = 'customer' | 'worker';
export type BindingEnv = 'test' | 'prod';
export const BINDING_SIDES: readonly BindingSide[] = ['customer', 'worker'];

/** `settings.attachments` (SW-9, owner 2026-10-28): the admin decides who may send photos and PDFs on a channel. */
export interface AttachmentSwitches {
	/** Customers attach a photo or PDF to a message. */
	customer?: boolean;
	/** Staff attach a photo or PDF to a reply. */
	staff?: boolean;
}

/** Kinds whose connector can carry a file; any other kind refuses `true` (422 `invalid_settings`). */
export const ATTACHMENT_KINDS: readonly BindingKind[] = ['web_chat', 'telegram'];

/** What a binding allows now: the stored switches, or both on for web chat and Telegram, both off for the rest. */
export function attachmentSwitches(
	kind: BindingKind,
	settings: BindingSettings | undefined,
): Required<AttachmentSwitches> {
	const can = ATTACHMENT_KINDS.includes(kind);
	return {
		customer: can && settings?.attachments?.customer !== false,
		staff: can && settings?.attachments?.staff !== false,
	};
}

/** `GET /api/jobs/{job}/attachment-quota`: the tenant's attachment storage, read-only here (a platform super_admin sets it). */
export interface AttachmentQuota {
	quota_bytes: number;
	default_quota_bytes: number;
	/** The platform set a quota of its own for this tenant. */
	custom: boolean;
	used_bytes: number;
}

/** Non-secret settings: web chat `allowed_origins`, `display` and `limits` (B13), `turnstile` (B14); worker targets
 *  Frappe Helpdesk `{base_url, service_user}`. */
export interface BindingSettings {
	allowed_origins?: string[];
	display?: WebChatDisplay;
	limits?: WebChatLimits;
	/** Web chat: the widget asks for a rating when a case closes (default true). */
	csat_enabled?: boolean;
	/** Web chat and Telegram: who may send photos and PDFs on this channel (absent = both on; see [[attachmentSwitches]]). */
	attachments?: AttachmentSwitches;
	/** Web chat (B14): a Cloudflare Turnstile challenge before a chat session starts (absent = off). */
	turnstile?: WebChatTurnstile;
	/** Web chat (known-customer chat): how the customer's own site tells us who is chatting (absent = off). */
	identity?: WebChatIdentity;
	/** Telegram: the only users the bot answers (D75; empty = nobody). */
	allowed_user_ids?: number[];
	/** Telegram: where `sendChannelTestMessage` sends (default: the first allowed user). */
	test_chat_id?: number;
	/** Telegram: the forum topic (`message_thread_id`) of `test_chat_id` the test message goes to. */
	test_thread_id?: number;
	/** Frappe Helpdesk: the site, `http(s)://host[:port]` (an in-cluster site must be admitted by the operator). */
	base_url?: string;
	/** Frappe Helpdesk: the ticket requester (an email) when the customer has none. */
	service_user?: string;
}

// ---- B14: Turnstile on web chat -------------------------------------------------------

/** `off`: no challenge. `platform`: the platform's widget (nonagentic.ai pages only). `own`: the tenant's widget —
 *  `site_key` in settings, its secret in the binding credential as `turnstile_secret`. */
export type TurnstileMode = 'off' | 'platform' | 'own';
export const TURNSTILE_MODES: { mode: TurnstileMode; label: string }[] = [
	{ mode: 'off', label: 'Off' },
	{ mode: 'platform', label: 'Platform widget (nonagentic.ai pages only)' },
	{ mode: 'own', label: 'Own Cloudflare widget' },
];
export interface WebChatTurnstile {
	mode: TurnstileMode;
	/** Mode `own` only. */
	site_key?: string;
}
/** A Turnstile site key as agent-api accepts it. */
export const TURNSTILE_SITE_KEY_RE = /^0x[A-Za-z0-9_-]{10,100}$/;
/** The credential key of an own Turnstile widget's secret. */
export const TURNSTILE_SECRET_KEY = 'turnstile_secret';

/**
 * Why a Turnstile setting is not saveable, or null. `secret`: typed now; `hasCredential`: the binding has one sealed
 * already (an own widget needs its secret once, after that an empty field keeps it).
 */
export function turnstileError(t: WebChatTurnstile, secret: string, hasCredential: boolean): string | null {
	if (t.mode !== 'own') return null;
	if (!TURNSTILE_SITE_KEY_RE.test(t.site_key ?? ''))
		return 'The Turnstile site key looks like 0x4AAAAAAA… (from Cloudflare).';
	if (!secret.trim() && !hasCredential) return 'Paste the Turnstile secret key of your widget.';
	return null;
}

// ---- known-customer web chat: the binding's identity settings --------------------------------------------------

/** `off`: every chat is anonymous. `hs256`: the site signs with a secret we made for it. `es256`: the site signs
 *  with its own private key; we hold only the public keys (JWKs). */
export type WebChatIdentityMode = 'off' | 'hs256' | 'es256';
export const IDENTITY_MODES: { mode: WebChatIdentityMode; label: string }[] = [
	{ mode: 'off', label: 'Off — every chat is anonymous' },
	{ mode: 'hs256', label: 'HS256 — your server signs with a secret we make' },
	{ mode: 'es256', label: 'ES256 — your server signs, we keep only the public keys' },
];
export const IDENTITY_DEFAULT_HOURS = 8;
/** `settings.identity`. The secret of `hs256` is never here: it is shown once when made. `prev_valid_until` is the
 *  server's (the old secret still works until then). */
export interface WebChatIdentity {
	mode: WebChatIdentityMode;
	issuer?: string;
	sign_in_url?: string;
	session_hours?: number;
	jwks?: Record<string, unknown>[];
	prev_valid_until?: string;
}
/** `POST …/test-session` answer: the hosted test page, signed in as the named customer (the proof rides in the URL fragment). */
export interface ChannelTestSession {
	url: string;
}
/** `POST …/identity-secret` answer: the new key id and its secret, once. */
export interface IdentitySecret {
	kid: string;
	secret: string;
}

/** The ES256 public keys as typed (a JSON list of one or two JWKs, or one JWK): the list, or why not. */
export function parseJwks(text: string): { jwks: Record<string, unknown>[]; error: string | null } {
	const t = text.trim();
	if (!t) return { jwks: [], error: null };
	let v: unknown;
	try {
		v = JSON.parse(t);
	} catch {
		return { jwks: [], error: 'The public keys are not valid JSON.' };
	}
	const list = Array.isArray(v) ? v : [v];
	const ok =
		list.length >= 1 &&
		list.length <= 2 &&
		list.every(
			(k) =>
				k && typeof k === 'object' && !Array.isArray(k) && typeof (k as { kid?: unknown }).kid === 'string',
		);
	return ok
		? { jwks: list as Record<string, unknown>[], error: null }
		: { jwks: [], error: 'Give one or two public keys (JWK), each with a "kid".' };
}

/** What the Identity section sends as `settings.identity`. `off` sends only the mode. */
export function identityOf(f: {
	mode: WebChatIdentityMode;
	issuer: string;
	signInUrl: string;
	hours: string;
	jwks: string;
}): WebChatIdentity {
	if (f.mode === 'off') return { mode: 'off' };
	const out: WebChatIdentity = {
		mode: f.mode,
		issuer: f.issuer.trim(),
		session_hours: f.hours.trim() === '' ? IDENTITY_DEFAULT_HOURS : Number(f.hours),
	};
	if (f.signInUrl.trim()) out.sign_in_url = f.signInUrl.trim();
	if (f.mode === 'es256') out.jwks = parseJwks(f.jwks).jwks;
	return out;
}

/** Why the Identity section is not saveable, or null. */
export function identityError(f: Parameters<typeof identityOf>[0]): string | null {
	if (f.mode === 'off') return null;
	if (!f.issuer.trim()) return 'Say who signs the sign-in proof (the issuer your server puts in it).';
	if (f.issuer.trim().length > 200) return 'The issuer is too long (200 characters at most).';
	if (f.signInUrl.trim() && !/^https:\/\/[^\s]+$/.test(f.signInUrl.trim()))
		return 'The sign-in URL starts with https://.';
	const h = f.hours.trim() === '' ? IDENTITY_DEFAULT_HOURS : Number(f.hours);
	if (!Number.isInteger(h) || h < 1 || h > 24) return 'A signed-in chat lasts 1 to 24 hours.';
	if (f.mode === 'es256') {
		const { jwks, error } = parseJwks(f.jwks);
		if (error) return error;
		if (!jwks.length) return 'Paste the public key (JWK) your server signs with.';
	}
	return null;
}

/** A BYO credential (sealed by the API, never returned). Field names per kind: `CREDENTIAL_FIELDS`. */
export type BindingCredential = Record<string, string>;

export interface ChannelBinding {
	id: string;
	kind: BindingKind;
	side: BindingSide;
	/**
	 * D130: `test` = the channel answers with the job's draft design (a new customer binding starts here); `prod` =
	 * live (published; worker-side bindings are always `prod`).
	 */
	env: BindingEnv;
	/** Telegram bot id / Helpdesk site id / web chat key `wk_…`. */
	external_id: string;
	label: string;
	settings: BindingSettings;
	enabled: boolean;
	has_credential: boolean;
	credential_fpr: string | null;
	/** Ownership (B12): every binding the API creates is verified at creation; an unverified one carries no traffic. */
	verified: boolean;
	verified_at: Iso | null;
	created_at: Iso;
	/** Web chat (B13 §4): the embed snippet as the API builds it (its public base). Absent on other kinds. */
	snippet?: string | null;
	/**
	 * B7 T15b: the webhook health agent-edge observed and reported (`channel_webhook_status`); `null` (or absent) when
	 * the edge has reported nothing yet — shown as "unknown", never guessed.
	 */
	webhook?: BindingWebhook | null;
}

/**
 * From what agent-edge observes plus what Telegram reports about the webhook (read-only), precedence
 * down > degraded > quiet > ok: `down` when no webhook URL is registered, the URL is not this binding's, or Telegram
 * reports a delivery error in the last 15 min newer than the last update (see `down_reason`); `degraded` after 3 failed
 * sends in a row until a send succeeds again (across edge replicas), or with more than 100 updates pending; `quiet`
 * when no update arrived for 24 h — informational, nobody wrote to the bot, not an error; else `ok`.
 */
export type WebhookHealthStatus = 'ok' | 'degraded' | 'quiet' | 'down';

/** Why a binding's webhook is `down`. */
export type WebhookDownReason = 'url_unset' | 'url_mismatch' | 'webhook_error';

/** A binding's webhook health (B7 T15b). `last_error` is a short code, never message text. */
export interface BindingWebhook {
	status: WebhookHealthStatus;
	last_update_at: Iso | null;
	/** Updates Telegram holds undelivered; `null` until an edge has read the webhook info. */
	pending_update_count: number | null;
	/** Set exactly when `status` is `down`. */
	down_reason: WebhookDownReason | null;
	last_error: string | null;
	last_error_at: Iso | null;
	observed_at: Iso;
}

export interface CreateChannelBinding {
	kind: BindingKind;
	side: BindingSide;
	/** Omitted for web chat: the API generates the `wk_` key. */
	external_id?: string;
	label: string;
	settings: BindingSettings;
	credential?: BindingCredential;
}

export interface UpdateChannelBinding {
	label?: string;
	settings?: BindingSettings;
	enabled?: boolean;
	/** An object replaces the credential whole; `null` removes it; absent keeps it. */
	credential?: BindingCredential | null;
}

/** The optional BYO credential fields of each kind (password inputs, never pre-filled). */
export const CREDENTIAL_FIELDS: Record<BindingKind, { key: string; label: string }[]> = {
	web_chat: [],
	telegram: [
		{ key: 'bot_token', label: 'Bot token' },
		{ key: 'webhook_secret', label: 'Webhook secret' },
	],
	helpdesk: [
		{ key: 'api_key', label: 'API key' },
		{ key: 'api_secret', label: 'API secret' },
		{ key: 'webhook_secret', label: 'Webhook secret' },
	],
};

export const BINDING_KIND_LABEL: Record<BindingKind, string> = {
	web_chat: 'Web chat',
	telegram: 'Telegram',
	helpdesk: 'Frappe Helpdesk',
};

/** What the external id means for a kind + side (the form's label). Empty: the API generates it (web chat). */
export function externalIdLabel(kind: BindingKind, side: BindingSide): string {
	if (kind === 'web_chat') return '';
	if (kind === 'telegram') return 'Telegram bot id';
	if (kind === 'helpdesk') return 'Helpdesk site id (a unique name, e.g. helpdesk.example.com#support)';
	return 'External id';
}

/** The text for 409 `binding_unavailable` (the API never says whether another tenant holds the account). */
export const BINDING_UNAVAILABLE =
	'That account or target cannot be bound: it is already bound, here or elsewhere. Remove the other binding first, or bind a different account.';

/** Where the widget is served when the API does not send the binding's `snippet` (B13 §4 `WEBCHAT_PUBLIC_BASE`). */
export const WEBCHAT_PUBLIC_BASE = 'https://chat.nonagentic.ai';

/**
 * The web chat embed snippet of a binding: the API's `snippet` when it sends one, else built from the `wk_` key
 * the way agent-api builds it (B13 §4): `data-widget="{key}"`, loaded `async`.
 */
export function webChatSnippet(b: string | Pick<ChannelBinding, 'external_id' | 'snippet'>): string {
	if (typeof b !== 'string' && b.snippet) return b.snippet;
	const key = typeof b === 'string' ? b : b.external_id;
	return `<script src="${WEBCHAT_PUBLIC_BASE}/webchat/widget.js" data-widget="${key}" async></script>`;
}

// ---- B13: web chat widget display + limits (on the binding's settings) --------------------------------------------

/** What the widget shows (B13 §4 `settings.display`): title ≤60, greeting ≤280, colour `#rrggbb`. */
export interface WebChatDisplay {
	title?: string;
	greeting?: string;
	colour?: string;
}
export const WEBCHAT_TITLE_MAX = 60;
export const WEBCHAT_GREETING_MAX = 280;
export const WEBCHAT_COLOUR_RE = /^#[0-9a-fA-F]{6}$/;

/** Fixed-window rate limits of one web chat binding (B13 §3, `settings.limits`); each 1..1000. */
export interface WebChatLimits {
	sessions_per_ip_hour?: number;
	msgs_per_ip_10m?: number;
	msgs_per_session_hour?: number;
	msgs_per_minute?: number;
}
export type WebChatLimitKey = keyof WebChatLimits;
export const WEBCHAT_LIMIT_MIN = 1;
export const WEBCHAT_LIMIT_MAX = 1000;
/** The limits a binding runs on without its own (B13 §3). */
export const WEBCHAT_LIMIT_DEFAULTS: Required<WebChatLimits> = {
	sessions_per_ip_hour: 10,
	msgs_per_ip_10m: 30,
	msgs_per_session_hour: 60,
	msgs_per_minute: 300,
};
export const WEBCHAT_LIMITS: { key: WebChatLimitKey; label: string }[] = [
	{ key: 'sessions_per_ip_hour', label: 'New chats per visitor IP per hour' },
	{ key: 'msgs_per_ip_10m', label: 'Messages per visitor IP per 10 minutes' },
	{ key: 'msgs_per_session_hour', label: 'Messages per chat per hour' },
	{ key: 'msgs_per_minute', label: 'Messages per minute, whole widget' },
];

/** Why a widget display is not saveable, or null. */
export function displayError(d: WebChatDisplay): string | null {
	if ((d.title ?? '').length > WEBCHAT_TITLE_MAX)
		return `The title is ${WEBCHAT_TITLE_MAX} characters at most.`;
	if ((d.greeting ?? '').length > WEBCHAT_GREETING_MAX)
		return `The greeting is ${WEBCHAT_GREETING_MAX} characters at most.`;
	if (d.colour && !WEBCHAT_COLOUR_RE.test(d.colour)) return 'The colour is #rrggbb (for example #1f6feb).';
	return null;
}

/** Why a set of limits is not saveable, or null. */
export function limitsError(l: WebChatLimits): string | null {
	for (const { key, label } of WEBCHAT_LIMITS) {
		const v = l[key];
		if (v === undefined) continue;
		if (!Number.isInteger(v) || v < WEBCHAT_LIMIT_MIN || v > WEBCHAT_LIMIT_MAX)
			return `${label}: a whole number from ${WEBCHAT_LIMIT_MIN} to ${WEBCHAT_LIMIT_MAX}.`;
	}
	return null;
}

// ---- B13: per-job API keys (machine callers of the interaction routes) ---------------------------------------------

export type JobApiKeyState = 'active' | 'expired' | 'revoked';

/** One row of `GET /api/jobs/{job}/api-keys` (the secret is never returned after create). */
export interface JobApiKey {
	id: string;
	label: string;
	/** The first characters of the secret (`ak_…`), shown so a key can be recognised. */
	prefix: string;
	state: JobApiKeyState;
	created_by: string;
	created_at: Iso;
	last_used_at: Iso | null;
	expires_at: Iso | null;
	revoked_at: Iso | null;
}

export interface CreateJobApiKey {
	label: string;
	/** RFC 3339, in the future; omitted = never expires. */
	expires_at?: string;
}

/** `POST /api/jobs/{job}/api-keys` → the key and its secret, shown this once. */
export interface CreatedJobApiKey {
	key: JobApiKey;
	secret: string;
}

// ---- B13: party identity -------------------------------------------------------------------------------------------

/**
 * A customer as other rows carry it (handover item, console conversation, case). `known` = one of its conversations has
 * an identified customer (state `subject` with a sourcing identity source); `subject` = that customer's id.
 */
export interface PartyRef {
	id: string;
	known: boolean;
	subject: string | null;
	display_name: string | null;
}

/** A channel subject (a Telegram user, web chat session, …) that belongs to a party. */
export interface PartyIdentity {
	channel: ConversationChannel | string;
	subject: string;
	verified: boolean;
	first_seen_at: Iso;
	last_seen_at: Iso;
}

/** One row of `GET /api/parties` (a party without its cases). */
export interface PartySummary extends PartyRef {
	/** This party was merged into another and answers as it. */
	merged_into: string | null;
	created_at: Iso;
	updated_at: Iso;
	identities: PartyIdentity[];
}

/** A case of the party, in any job of the tenant. */
export interface PartyCase {
	job: string;
	case_id: string;
	conversation_id: string | null;
	channel: ConversationChannel | string;
	status: string;
	title: string | null;
	started_at: Iso;
}

/** `GET /api/parties/{id}`. */
export interface PartyDetail extends PartySummary {
	cases: PartyCase[];
}

/** `GET|POST /api/jobs/{job}/cases/{id}/party` — the case and the party behind it. */
export interface CasePartyAnswer {
	case_id: string;
	party: PartyDetail | null;
}

/** The customer's rating of a closed case (1–5) and when it was given. */
export interface CaseCsat {
	rating: number;
	recorded_at: Iso;
}

/** The author agent-api gives the handover notice line of a transcript (role `agent`). */
export const HANDOVER_NOTICE_AUTHOR = 'handover_notice';

/**
 * Who a transcript line is from, for display: the handover notice reads as such, not by its marker; a customer line
 * reads as the customer (`customerName`), never by its raw channel subject (`wc:<session>`, a phone number, …).
 */
export function authorLabel(
	author: string | null | undefined,
	fallback: string,
	line?: { role: string; party?: Pick<PartyRef, 'known' | 'display_name'> | null },
): string {
	if (author === HANDOVER_NOTICE_AUTHOR) return 'Handover notice';
	if (line?.role === 'customer') return customerName(line.party);
	return author || fallback;
}

/** "Noor Bakker", "Anonymous visitor", … */
export function partyName(p: Pick<PartyRef, 'known' | 'display_name'> | null | undefined): string {
	if (!p) return '—';
	return p.display_name?.trim() || (p.known ? 'Known customer' : 'Anonymous visitor');
}

/** The customer on a transcript line: the party's name; without a party (not resolved yet) an anonymous visitor. */
export function customerName(p: Pick<PartyRef, 'known' | 'display_name'> | null | undefined): string {
	return partyName(p ?? { known: false, display_name: null });
}

/**
 * The customer of a handover item or conversation in headings and labels: the party's name, else the channel's
 * customer id — except a web chat session id (`wc:…`), which means nothing to staff.
 */
export function customerLabel(c: {
	customer: string;
	party?: Pick<PartyRef, 'known' | 'display_name'> | null;
}): string {
	const name = c.party?.display_name?.trim();
	if (name) return name;
	return c.customer.startsWith('wc:') ? customerName(c.party) : c.customer;
}

/** The kind badge says something the name does not only when the party has a name of its own. */
export function partyKindShown(p: Pick<PartyRef, 'display_name'> | null | undefined): boolean {
	return !!p?.display_name?.trim();
}
