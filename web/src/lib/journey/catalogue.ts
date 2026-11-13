// Static catalogue the journey area reads: the 7 stages, 8 themes, slide channels, the operation attributes
// (11§4.3.5) and the canonical objects with their fields (11§1, §4.3.2). Owned by the journey area agent.
// Names line up with spec 14 §4 so they match the model and connectors areas after merge.
import type {
	ActivityObject,
	Actor,
	JourneyActivity,
	JourneyChannel,
	JourneyStage,
	JourneyTheme,
	OperationExpectation,
	StageKey,
	ThemeKey,
} from '$lib/api/types/journey';

export const STAGE_KEYS: StageKey[] = [
	'orientation',
	'investigation',
	'purchase',
	'delivery',
	'change',
	'support',
	'exit',
];

export const STAGE_LABEL: Record<StageKey, string> = {
	orientation: 'Orientation',
	investigation: 'Investigation',
	purchase: 'Purchase',
	delivery: 'Delivery',
	change: 'Change',
	support: 'Support',
	exit: 'Exit',
};

export const THEMES = (): JourneyTheme[] => [
	{ key: 'need', label: 'Need', order: 1, escalation_weight: 0.1 },
	{ key: 'interest', label: 'Interest', order: 2, escalation_weight: 0.1 },
	{ key: 'choice', label: 'Choice', order: 3, escalation_weight: 0.15 },
	{ key: 'awaiting', label: 'Awaiting', order: 4, escalation_weight: 0.3 },
	{ key: 'elated', label: 'Elated', order: 5, escalation_weight: 0.05 },
	{ key: 'issue', label: 'Issue', order: 6, escalation_weight: 0.7 },
	{ key: 'issue_resolved', label: 'Issue Resolved', order: 7, escalation_weight: 0.2 },
	{ key: 'need_change', label: 'Need Change', order: 8, escalation_weight: 0.6 },
];

export const CHANNELS = (): JourneyChannel[] => [
	{ key: 'chat', label: 'Chat', status: 'v1', note: 'Telegram + staff /chat (D11)' },
	{ key: 'email', label: 'Emails', status: 'later', note: 'Later channel on the same grid' },
	{ key: 'contact_form', label: 'Contact forms', status: 'later', note: 'Later channel on the same grid' },
	{ key: 'phone', label: 'Phone call', status: 'out_of_scope', note: 'Out of product scope' },
	{ key: 'visit', label: 'Visit', status: 'out_of_scope', note: 'Out of product scope' },
	{
		key: 'signing',
		label: 'Signing platforms',
		status: 'out_of_scope',
		note: 'Does not apply: retail T&C is click-accept (D25)',
	},
];

/** The slide's theme band above each column (11§4.3.1). */
export const STAGE_THEMES: Record<StageKey, ThemeKey[]> = {
	orientation: ['need', 'interest'],
	investigation: ['interest', 'choice'],
	purchase: ['choice', 'awaiting'],
	delivery: ['awaiting', 'elated'],
	change: ['issue'],
	support: ['issue', 'issue_resolved'],
	exit: ['need_change'],
};

/** Base stage descriptions + synonyms (D23; Change/Exit synonyms from the Addendum, K48). */
export function baseStages(
	desc: Partial<Record<StageKey, string>> = {},
	outOfScope: StageKey[] = [],
): JourneyStage[] {
	const base: Record<StageKey, [string, string[]]> = {
		orientation: [
			'The customer explores what we sell and how we work: catalogue, policies, contact options.',
			['explore', 'browse', 'awareness'],
		],
		investigation: [
			'The customer compares a specific product: stock, price, delivery options, pre-order dates.',
			['consider', 'compare', 'evaluate'],
		],
		purchase: [
			'The customer places and pays for an order and accepts the terms.',
			['buy', 'checkout', 'order'],
		],
		delivery: [
			'The order travels to the customer and is put to use: tracking, invoices, first use.',
			['fulfilment', 'shipping', 'onboarding'],
		],
		change: [
			'The customer or we change something about an order or the account before it is final.',
			['modify', 'change request', 'GB955 Manage / Realize change'],
		],
		support: [
			'Something went wrong after delivery: returns, refunds, warranty, complaints.',
			['aftercare', 'service', 'issue resolution'],
		],
		exit: [
			'The customer leaves: closes the account, withdraws consent, asks for their data.',
			['leave', 'offboarding', 'cancellation', 'GB955 Leave'],
		],
	};
	return STAGE_KEYS.map((key, i) => ({
		key,
		label: STAGE_LABEL[key],
		order: i + 1,
		description: desc[key] ?? base[key][0],
		synonyms: [...base[key][1]],
		out_of_scope: outOfScope.includes(key),
		themes: [...STAGE_THEMES[key]],
	}));
}

/** Operation attributes (11§4.3.5 + the ITSD wave): modality, side effect, identity level. */
export const OPS: Record<
	string,
	{ modality: 'do' | 'request' | 'observe'; effect: 'R' | 'W' | 'X' | 'P'; id: string }
> = {
	'knowledge.search': { modality: 'observe', effect: 'R', id: '0' },
	'offering.list': { modality: 'observe', effect: 'R', id: '0' },
	'offering.read': { modality: 'observe', effect: 'R', id: '0' },
	'party.list': { modality: 'observe', effect: 'R', id: '1' },
	'party.read': { modality: 'observe', effect: 'R', id: '2' },
	'contact_point.update': { modality: 'do', effect: 'W', id: '2' },
	'consent.update': { modality: 'do', effect: 'W', id: '1' },
	'order.list': { modality: 'observe', effect: 'R', id: '2' },
	'order.read': { modality: 'observe', effect: 'R', id: '2' },
	'order.modify': { modality: 'do', effect: 'W', id: '2' },
	'order.cancel': { modality: 'do', effect: 'X', id: '2' },
	'delivery.read': { modality: 'observe', effect: 'R', id: '1 / 2' },
	'delivery.redirect': { modality: 'do', effect: 'W', id: '2' },
	'delivery.report_problem': { modality: 'request', effect: 'W', id: '2' },
	'payment.list': { modality: 'observe', effect: 'R', id: '2' },
	'payment.refund': { modality: 'do', effect: 'X', id: '2' },
	'payment.create': { modality: 'do', effect: 'P', id: '2' },
	'rule.evaluate': { modality: 'observe', effect: 'R', id: '2' },
	'case.create': { modality: 'request', effect: 'W', id: '1–2' },
	'case.read': { modality: 'observe', effect: 'R', id: '2' },
	'case.comment': { modality: 'do', effect: 'W', id: '2' },
	'document.read': { modality: 'observe', effect: 'R', id: '2' },
	'contract.read': { modality: 'observe', effect: 'R', id: '2' },
	'interaction.send': { modality: 'do', effect: 'W', id: '2' },
	'interaction.create': { modality: 'do', effect: 'W', id: '1' },
	'access_grant.create': { modality: 'request', effect: 'W', id: '2' },
	'access_grant.list': { modality: 'observe', effect: 'R', id: '2' },
	'credential.reset': { modality: 'do', effect: 'X', id: '3' },
	'asset.list': { modality: 'observe', effect: 'R', id: '1' },
	'usage.list': { modality: 'observe', effect: 'R', id: '2' },
	'arrangement.create': { modality: 'request', effect: 'W', id: '2' },
};

/** Canonical objects (base, industry, optional) and the fields an activity may select (11§4.3.2). */
export const OBJECT_FIELDS: Record<string, string[]> = {
	knowledge: ['article', 'passages', 'citation'],
	offering: [
		'sku',
		'name',
		'description',
		'price',
		'availability',
		'stock_level',
		'combinatory',
		'components',
		'valid_from',
		'valid_to',
	],
	party: [
		'party_id',
		'name',
		'email',
		'postal_address',
		'billing_address',
		'status',
		'contact_point',
		'employee_id',
		'department',
		'manager',
	],
	credential: ['verified_identity', 'verified_at', 'username', 'factor', 'locked', 'last_reset_at'],
	contact_point: ['type', 'value', 'verified', 'preferred'],
	consent: ['topic', 'channel', 'granted'],
	order: ['order_id', 'status', 'placed_at', 'delivered_at', 'total', 'lines', 'delivery_address'],
	delivery: [
		'delivery_id',
		'items',
		'carrier',
		'tracking_ref',
		'tracking_url',
		'status',
		'expected_window',
		'events',
		'direction',
	],
	payment: [
		'payment_id',
		'amount',
		'status',
		'method_masked',
		'at',
		'direction',
		'eta',
		'original_payment_id',
	],
	rule: ['rule_id', 'outcome', 'reason', 'threshold'],
	case: [
		'case_id',
		'case_type',
		'status',
		'subject',
		'reason',
		'return_lines',
		'refund_type',
		'deadline',
		'label',
		'priority',
		'assignment_group',
	],
	document: ['document_id', 'kind', 'issue_date', 'url'],
	contract: ['terms_version', 'accepted_at', 'acceptance_channel', 'order_ids', 'end_date'],
	interaction: ['interaction_id', 'type', 'score', 'text'],
	appointment: ['slot', 'location', 'status'],
	access_grant: ['resource', 'role', 'approver', 'expires_at', 'status'],
	location: ['address', 'meter_point', 'occupied_from'],
	service_event: ['event_id', 'kind', 'started_at', 'resolved_at', 'area'],
	asset: ['asset_tag', 'serial', 'model', 'assigned_to', 'warranty_until'],
	usage: ['period', 'kwh', 'estimated', 'reading_at'],
	arrangement: ['arrangement_id', 'instalments', 'amount', 'status'],
	meter_point: ['ean', 'meter_type', 'last_reading'],
	promotion: ['code', 'valid_to', 'discount'],
};

export const CANONICAL_OBJECTS = Object.keys(OBJECT_FIELDS);

/** Object key without a variant: `case{return}` → `case`. */
export const objectKey = (o: string) => o.replace(/\{.*\}$/, '');

/** `'order.read R'`, `'case.create{return} R'`, `'payment.create O'` → an expectation. */
export function op(spec: string): OperationExpectation {
	const [name, ro] = spec.trim().split(/\s+/);
	const m = /^([a-z_]+\.[a-z_]+)(?:\{(.+)\})?$/.exec(name!);
	if (!m) throw new Error(`bad op spec ${spec}`);
	return { op: m[1]!, variant: m[2] ?? null, required: ro === 'R' };
}

/** `['order', 'subject', 'order_id status', 'order_number']` → an ActivityObject. */
export type ObjSpec = [string, 'subject' | 'context', string, string?];
export function obj([object, role, fields, inputs]: ObjSpec): ActivityObject {
	return {
		object,
		role,
		fields: fields.split(/\s+/).filter(Boolean),
		required_inputs: (inputs ?? '').split(/\s+/).filter(Boolean),
	};
}

export function act(
	key: string,
	actor: Actor,
	label: string,
	trigger: string,
	objects: ObjSpec[],
	ops: string[],
	also: StageKey[] = [],
): JourneyActivity {
	return {
		key,
		stage: key.split(':')[0] as StageKey,
		actor,
		label,
		trigger,
		also_in_stages: also,
		objects: objects.map(obj),
		operations: ops.map(op),
	};
}

/** `op{variant}` display form. */
export const opLabel = (o: { op: string; variant: string | null }) =>
	o.variant ? `${o.op}{${o.variant}}` : o.op;
