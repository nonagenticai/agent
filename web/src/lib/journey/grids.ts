// Journey grid content: retail/customer_service (fully populated, 11§4.3), utilities/billing (partly populated),
// it_services/it_service_desk (generator run #7 draft). Fresh objects on every call (tests build a new world).
import type {
	IndustryJourney,
	JourneyActivity,
	StageKey,
	StageKnowledgeKind,
	StageMessage,
	ThemeStagePrior,
} from '$lib/api/types/journey';
import { CHANNELS, THEMES, act, baseStages } from './catalogue';

function msgs(rows: [StageKey, 'chat' | 'email' | 'contact_form', string[]][]): StageMessage[] {
	return rows.map(([stage, channel, utterances]) => ({ stage, channel, utterances }));
}
function kinds(rows: [StageKey, string, boolean, string][]): StageKnowledgeKind[] {
	return rows.map(([stage, kind, required, description]) => ({ stage, kind, required, description }));
}
function priors(rows: [string, string, number][]): ThemeStagePrior[] {
	return rows.map(([theme, stage, weight]) => ({
		theme: theme as ThemeStagePrior['theme'],
		stage: stage as StageKey,
		weight,
	}));
}

/** D61: place each activity in its department area (fixture data: the area lists its activities). Throws when an
 * activity is in no area, in two, or an area names an activity the grid does not have. */
function inAreas(acts: JourneyActivity[], areas: Record<string, string[]>): JourneyActivity[] {
	const areaOf = new Map<string, string>();
	for (const [area, keys] of Object.entries(areas))
		for (const k of keys) {
			if (areaOf.has(k)) throw new Error(`activity ${k} is in two areas`);
			if (!acts.some((a) => a.key === k)) throw new Error(`area ${area} names unknown activity ${k}`);
			areaOf.set(k, area);
		}
	return acts.map((a) => {
		const area = areaOf.get(a.key);
		if (!area) throw new Error(`activity ${a.key} is in no area`);
		return { ...a, area };
	});
}

// ---- retail / customer_service ----------------------------------------------------------------------------------

/** Retail customer service areas (D60: order support, logistics, payments, FAQ) and the activities in each. */
export const RETAIL_ACTIVITY_AREAS: Record<string, string[]> = {
	faq: [
		'orientation:browse_catalogue',
		'orientation:read_product_info',
		'orientation:ask_policy_question',
		'orientation:ask_contact_question',
		'orientation:publish_catalogue_faq',
		'investigation:check_stock',
		'investigation:check_price_and_shipping',
		'investigation:check_preorder_date',
		'investigation:bundle_contents',
		'investigation:back_in_stock_alert',
		'investigation:answer_stock_question',
		'purchase:read_terms',
		'delivery:learn_to_use',
		'delivery:give_feedback',
	],
	'order-support': [
		'purchase:check_order_placed',
		'purchase:get_confirmation',
		'change:change_order',
		'change:cancel_order',
		'change:update_contact_details',
		'change:update_preferences',
		'support:start_return',
		'support:track_refund',
		'support:claim_warranty',
		'support:complain',
		'support:ask_for_person',
		'support:resolve_case',
		'exit:close_account',
		'exit:export_delete_data',
		'exit:unsubscribe',
		'exit:confirm_closure',
	],
	logistics: [
		'delivery:track_parcel',
		'delivery:give_delivery_instructions',
		'delivery:report_delivery_problem',
		'delivery:ship_and_notify',
		'change:notify_delay',
	],
	payments: [
		'purchase:payment_failed',
		'purchase:take_payment_by_link',
		'delivery:get_invoice',
		'support:dispute_charge',
	],
};

export function retailActivities(): JourneyActivity[] {
	return inAreas(retailBareActivities(), RETAIL_ACTIVITY_AREAS);
}

function retailBareActivities(): JourneyActivity[] {
	return [
		act(
			'orientation:browse_catalogue',
			'customer',
			'Browse the catalogue',
			'“do you sell …”',
			[['offering', 'subject', 'sku name description price availability']],
			['offering.list O', 'offering.read O'],
		),
		act(
			'orientation:read_product_info',
			'customer',
			'Read product information',
			'asks about materials, sizes, care',
			[
				['offering', 'subject', 'sku name description'],
				['knowledge', 'context', 'article passages citation'],
			],
			['offering.read O', 'knowledge.search R'],
		),
		act(
			'orientation:ask_policy_question',
			'customer',
			'Ask a general policy question',
			'“do you ship to Belgium?”, “can I pay with iDEAL?”',
			[['knowledge', 'subject', 'article passages citation']],
			['knowledge.search R'],
		),
		act(
			'orientation:ask_contact_question',
			'customer',
			'Ask how to reach us',
			'opening hours, showroom, phone',
			[['knowledge', 'subject', 'article passages citation']],
			['knowledge.search R'],
		),
		act(
			'orientation:publish_catalogue_faq',
			'company',
			'Publish catalogue and FAQ',
			'catalogue sync, help-centre crawl',
			[
				['offering', 'subject', 'sku name description price'],
				['knowledge', 'subject', 'article'],
			],
			[],
		),
		act(
			'investigation:check_stock',
			'customer',
			'Check stock',
			'“is the Oslo throw in stock?”',
			[['offering', 'subject', 'sku availability stock_level', 'sku']],
			['offering.read O', 'knowledge.search R'],
		),
		act(
			'investigation:check_price_and_shipping',
			'customer',
			'Check price and delivery options',
			'“what does shipping to Antwerp cost?”',
			[
				['offering', 'subject', 'sku price availability'],
				['knowledge', 'context', 'passages citation'],
			],
			['offering.read O', 'knowledge.search R'],
		),
		act(
			'investigation:check_preorder_date',
			'customer',
			'Check a pre-order date',
			'“when will the linen duvet be back?”',
			[['offering', 'subject', 'sku availability valid_from valid_to', 'sku']],
			['offering.read O'],
		),
		act(
			'investigation:bundle_contents',
			'customer',
			'Ask what a bundle contains',
			'“what is in the bedroom starter set?”',
			[['offering', 'subject', 'sku combinatory components', 'sku']],
			['offering.read O'],
		),
		act(
			'investigation:back_in_stock_alert',
			'customer',
			'Ask for a back-in-stock alert',
			'“tell me when it is back”',
			[
				['party', 'context', 'contact_point'],
				['consent', 'subject', 'topic channel granted'],
			],
			['consent.update{alerts:back_in_stock} O'],
		),
		act(
			'investigation:answer_stock_question',
			'company',
			'Answer stock and lead-time questions',
			'agent answers from offering + shipping policy',
			[['offering', 'subject', 'sku stock_level valid_from']],
			['offering.read O', 'knowledge.search R'],
		),
		act(
			'purchase:payment_failed',
			'customer',
			'Ask about a failed payment',
			'“payment failed”, “card declined”',
			[
				['order', 'context', 'order_id status total'],
				['payment', 'subject', 'payment_id amount status method_masked at'],
				['rule', 'context', 'rule_id outcome reason'],
			],
			['party.list R', 'order.read R', 'payment.list R', 'rule.evaluate{PAY-04} R', 'payment.create O'],
		),
		act(
			'purchase:check_order_placed',
			'customer',
			'Check that an order was placed',
			'“did my order go through?”',
			[
				['party', 'context', 'name email'],
				['credential', 'context', 'locked'],
				['order', 'subject', 'order_id status placed_at total lines'],
			],
			['party.list R', 'order.list R', 'order.read R'],
		),
		act(
			'purchase:get_confirmation',
			'customer',
			'Get the order confirmation again',
			'“I never got a confirmation email”',
			[
				['order', 'subject', 'order_id status placed_at'],
				['interaction', 'context', 'type'],
			],
			['order.read R', 'interaction.send{order_confirmation} O'],
		),
		act(
			'purchase:read_terms',
			'customer',
			'Read the terms of sale',
			'“what did I agree to?”',
			[
				['contract', 'subject', 'terms_version accepted_at acceptance_channel order_ids'],
				['knowledge', 'context', 'passages citation'],
			],
			['contract.read{tc} O', 'knowledge.search R'],
		),
		act(
			'purchase:take_payment_by_link',
			'company',
			'Take payment by hosted link',
			'agent sends a PSP payment link (D27)',
			[
				['payment', 'subject', 'amount status'],
				['order', 'context', 'order_id total'],
			],
			['payment.create O'],
		),
		act(
			'delivery:track_parcel',
			'customer',
			'Track a parcel',
			'“where is my parcel?”',
			[
				['delivery', 'subject', 'carrier tracking_ref tracking_url status expected_window events'],
				['order', 'context', 'order_id status lines'],
			],
			['delivery.read{by_tracking_ref} R', 'order.read R'],
		),
		act(
			'delivery:give_delivery_instructions',
			'customer',
			'Give delivery instructions',
			'“leave it with the neighbours”',
			[
				['delivery', 'subject', 'delivery_id status'],
				['party', 'context', 'postal_address'],
			],
			['delivery.read R', 'delivery.redirect O'],
		),
		act(
			'delivery:report_delivery_problem',
			'customer',
			'Report a late, missing or damaged delivery',
			'“arrived damaged”, “still not here”',
			[
				['delivery', 'subject', 'delivery_id carrier status events'],
				['order', 'context', 'order_id lines'],
			],
			['delivery.read R', 'delivery.report_problem O'],
		),
		act(
			'delivery:get_invoice',
			'customer',
			'Get an invoice',
			'“can I have the invoice?”',
			[['document{invoice}', 'subject', 'document_id issue_date url']],
			['document.read{invoice} O'],
			['exit'],
		),
		act(
			'delivery:learn_to_use',
			'customer',
			'Learn to use or care for a product',
			'“how do I wash the wool throw?”',
			[['knowledge', 'subject', 'article passages citation']],
			['knowledge.search R'],
		),
		act(
			'delivery:give_feedback',
			'customer',
			'Give feedback',
			'post-delivery survey reply',
			[['interaction{survey}', 'subject', 'score text']],
			['interaction.create{survey} O'],
		),
		act(
			'delivery:ship_and_notify',
			'company',
			'Ship, notify, collect feedback',
			'dispatch event from the shop',
			[
				['delivery', 'subject', 'carrier tracking_ref status'],
				['interaction', 'context', 'type'],
			],
			['interaction.send O'],
		),
		act(
			'change:change_order',
			'customer',
			'Change an order before dispatch',
			'“can I change the colour?”',
			[['order', 'subject', 'order_id status lines delivery_address']],
			['order.read R', 'order.modify O'],
		),
		act(
			'change:cancel_order',
			'customer',
			'Cancel an order',
			'“cancel my order”',
			[['order', 'subject', 'order_id status lines']],
			['order.read R', 'order.cancel O'],
		),
		act(
			'change:update_contact_details',
			'customer',
			'Update contact details',
			'“I moved, new address”',
			[
				['party', 'subject', 'name postal_address billing_address'],
				['contact_point', 'subject', 'type value verified preferred', 'value'],
			],
			['party.read O', 'contact_point.update O'],
		),
		act(
			'change:update_preferences',
			'customer',
			'Update preferences',
			'“only email me about orders”',
			[['consent', 'subject', 'topic channel granted', 'topic']],
			['consent.update O'],
			['investigation'],
		),
		act(
			'change:notify_delay',
			'company',
			'Notify delays and substitutions',
			'shop flags a delayed line',
			[
				['order', 'subject', 'order_id status lines'],
				['case{service_request}', 'context', 'subject reason'],
			],
			['interaction.send{change_notice} O', 'case.create{service_request} O'],
		),
		act(
			'support:start_return',
			'customer',
			'Start a return',
			'“I want to return this”',
			[
				['case{return}', 'subject', 'case_id status return_lines refund_type deadline label', 'return_lines'],
				['order', 'context', 'order_id placed_at delivered_at lines'],
				['rule', 'context', 'rule_id outcome reason'],
			],
			['order.read R', 'rule.evaluate{RET-01} R', 'case.create{return} R'],
		),
		act(
			'support:track_refund',
			'customer',
			'Track a refund',
			'“where is my refund?”',
			[
				['payment', 'subject', 'amount status eta original_payment_id direction'],
				['case', 'context', 'case_id status'],
			],
			['payment.list R', 'case.read R'],
		),
		act(
			'support:claim_warranty',
			'customer',
			'Claim warranty',
			'“broken after 3 months”',
			[
				['case{warranty_claim}', 'subject', 'case_id status reason', 'reason'],
				['asset', 'context', 'serial warranty_until'],
				['rule', 'context', 'rule_id outcome reason'],
			],
			['rule.evaluate{WAR-01} R', 'case.create{warranty_claim} R', 'case.read R'],
		),
		act(
			'support:dispute_charge',
			'customer',
			'Dispute a charge',
			'“I was charged twice”',
			[
				['payment', 'subject', 'payment_id amount status at'],
				['case{dispute}', 'context', 'case_id status reason'],
			],
			['payment.list R', 'case.create{dispute} R'],
		),
		act(
			'support:complain',
			'customer',
			'Complain',
			'GC complaint about an outcome (late, wrong, damaged)',
			[
				['case{complaint}', 'subject', 'case_id status reason', 'reason'],
				['order', 'context', 'order_id status'],
			],
			['case.create{complaint} R', 'case.comment O'],
		),
		act(
			'support:ask_for_person',
			'customer',
			'Ask for a person',
			'“I want to talk to someone”, GF ≥ 4 twice',
			[['case{handover}', 'subject', 'case_id reason priority']],
			['case.create{handover} R'],
		),
		act(
			'support:resolve_case',
			'company',
			'Process return, refund, resolve case',
			'staff resolves in the helpdesk',
			[
				['case', 'subject', 'case_id status'],
				['payment', 'context', 'amount direction'],
			],
			['payment.refund O', 'case.comment O'],
		),
		act(
			'exit:close_account',
			'customer',
			'Close the account',
			'“delete my account”',
			[
				['party', 'subject', 'party_id status'],
				['case{service_request}', 'context', 'case_id status reason'],
			],
			['party.list R', 'case.create{service_request} O'],
		),
		act(
			'exit:export_delete_data',
			'customer',
			'Export or delete my data',
			'GDPR request',
			[
				['party', 'subject', 'party_id'],
				['case{service_request}', 'context', 'case_id status reason'],
			],
			['case.create{service_request} O'],
		),
		act(
			'exit:unsubscribe',
			'customer',
			'Unsubscribe',
			'“stop emails”',
			[['consent', 'subject', 'topic channel granted']],
			['consent.update{marketing:*=off} O'],
		),
		act(
			'exit:confirm_closure',
			'company',
			'Confirm closure and send the last invoice',
			'closure case resolved',
			[
				['case', 'subject', 'case_id status'],
				['document{invoice}', 'context', 'document_id url'],
			],
			['document.read{invoice} O'],
		),
	];
}

export function retailJourney(): IndustryJourney {
	return {
		industry: 'retail',
		label: 'Retail / ecommerce',
		department: 'customer_service',
		status: 'tagged',
		tag: 'industry/retail@v2',
		dept_tag: 'industry_dept/retail/customer_service@v2',
		draft_rev: 0,
		updated_at: '2026-10-02T15:20:00Z',
		updated_by: 'owner',
		themes: THEMES(),
		stages: baseStages(),
		priors: priors([
			['need', 'orientation', 0.8],
			['need', 'investigation', 0.2],
			['interest', 'orientation', 0.45],
			['interest', 'investigation', 0.55],
			['choice', 'investigation', 0.4],
			['choice', 'purchase', 0.6],
			['awaiting', 'purchase', 0.3],
			['awaiting', 'delivery', 0.7],
			['elated', 'delivery', 1],
			['issue', 'change', 0.35],
			['issue', 'support', 0.65],
			['issue_resolved', 'support', 1],
			['need_change', 'exit', 0.85],
			['need_change', 'change', 0.15],
		]),
		channels: CHANNELS(),
		activities: retailActivities(),
		messages: msgs([
			[
				'orientation',
				'chat',
				['do you sell …', 'do you ship to Belgium?', 'do you accept iDEAL?', 'what are your opening hours?'],
			],
			['orientation', 'email', ['Question about your product range']],
			['orientation', 'contact_form', ['General question']],
			[
				'investigation',
				'chat',
				[
					'is the Oslo throw in stock?',
					'when is it back?',
					'what does shipping cost?',
					'what is in the starter set?',
				],
			],
			['investigation', 'email', ['Pre-order question']],
			['purchase', 'chat', ['payment failed', 'no confirmation', 'did my order go through?']],
			['purchase', 'email', ['Order confirmation missing']],
			[
				'delivery',
				'chat',
				['where is my parcel', 'arrived damaged', 'can I get the invoice?', 'how do I wash this?'],
			],
			['delivery', 'email', ['Parcel not received', 'Invoice request']],
			['delivery', 'contact_form', ['Damaged delivery (photo upload)']],
			['change', 'chat', ['change my address', 'cancel', 'can I change the colour?']],
			['change', 'email', ['Change to my order']],
			['support', 'chat', ['return this', 'refund?', 'broken after 3 months', 'I want to talk to someone']],
			['support', 'email', ['Return request', 'Warranty claim']],
			['support', 'contact_form', ['Complaint form']],
			['exit', 'chat', ['delete my account', 'stop emails', 'send me my data']],
			['exit', 'email', ['Account closure request']],
		]),
		knowledge_kinds: kinds([
			['orientation', 'catalog', true, 'Product sheets'],
			['orientation', 'faq.general', true, 'General FAQ'],
			['orientation', 'policy.shipping', true, 'Countries and costs'],
			['orientation', 'policy.payment', true, 'Accepted methods'],
			['orientation', 'contact.hours', false, 'Opening hours, showroom'],
			['investigation', 'catalog', true, 'Product sheets'],
			['investigation', 'policy.shipping', true, 'Lead times'],
			['investigation', 'faq.preorder', false, 'Pre-order FAQ'],
			['purchase', 'policy.terms', true, 'T&C versions'],
			['purchase', 'policy.payment', true, 'Payment policy incl. PAY-04'],
			['purchase', 'faq.checkout', false, 'Checkout FAQ'],
			['delivery', 'policy.shipping', true, 'Carrier, redirect rules'],
			['delivery', 'guide.product_usage', true, 'Care and usage guides'],
			['change', 'policy.order_changes', true, 'Cut-off before dispatch'],
			['change', 'policy.privacy', true, 'Contact data'],
			['support', 'policy.returns', true, 'RET-01 text'],
			['support', 'policy.warranty', true, 'WAR-01 text'],
			['support', 'policy.payment', true, 'PAY-04 text'],
			['support', 'faq.refunds', false, 'Refund FAQ'],
			['exit', 'policy.privacy', true, 'Retention, export, deletion'],
			['exit', 'policy.account', false, 'Account closure'],
		]),
		model_objects: [
			'knowledge',
			'offering',
			'party',
			'credential',
			'contact_point',
			'consent',
			'order',
			'delivery',
			'payment',
			'rule',
			'case',
			'document',
			'contract',
			'interaction',
		],
		findings: [],
		source_run: 6,
	};
}

// ---- utilities / billing (partly populated) ---------------------------------------------------------------------

/** Utilities billing & collections areas (bills, arrears) and the activities in each. */
export const UTILITIES_ACTIVITY_AREAS: Record<string, string[]> = {
	bills: [
		'orientation:compare_tariffs',
		'purchase:switch_supplier',
		'delivery:submit_meter_reading',
		'delivery:understand_bill',
		'support:outage_or_billing',
		'exit:move_out',
	],
	arrears: ['support:payment_arrangement'],
};

function utilitiesBareActivities(): JourneyActivity[] {
	return [
		act(
			'orientation:compare_tariffs',
			'customer',
			'Compare tariffs',
			'“what is your fixed tariff?”',
			[['offering', 'subject', 'name price valid_from']],
			['offering.list O', 'knowledge.search R'],
		),
		act(
			'purchase:switch_supplier',
			'customer',
			'Sign up / switch supplier',
			'“I want to switch to you”',
			[
				['contract', 'subject', 'terms_version accepted_at'],
				['location', 'context', 'address meter_point', 'address'],
			],
			['knowledge.search R', 'case.create{service_request} O'],
		),
		act(
			'delivery:submit_meter_reading',
			'customer',
			'Submit a meter reading',
			'“my reading is 18342”',
			[
				['usage', 'subject', 'period kwh reading_at', 'reading'],
				['meter_point', 'context', 'ean meter_type'],
			],
			['party.list R', 'usage.list R'],
		),
		act(
			'delivery:understand_bill',
			'customer',
			'Understand my bill',
			'“why is my bill so high?”',
			[
				['document{bill}', 'subject', 'document_id issue_date'],
				['usage', 'context', 'period kwh estimated'],
			],
			['usage.list R', 'document.read{bill} O', 'knowledge.search R'],
		),
		act(
			'support:payment_arrangement',
			'customer',
			'Ask for a payment arrangement',
			'“I can’t pay this month”',
			[['arrangement', 'subject', 'instalments amount status', 'amount']],
			['arrangement.create R', 'case.create{handover} R'],
		),
		act(
			'support:outage_or_billing',
			'customer',
			'Report an outage vs a billing problem',
			'gate I3 outage vs billing',
			[['knowledge', 'context', 'passages citation']],
			['knowledge.search R'],
		),
		act(
			'exit:move_out',
			'customer',
			'Move out — final reading and end bill',
			'“I am moving on 1 November”',
			[
				['location', 'subject', 'address occupied_from'],
				['document{bill}', 'context', 'document_id'],
			],
			['case.create{service_request} O'],
		),
	];
}

export function utilitiesJourney(): IndustryJourney {
	return {
		industry: 'utilities',
		label: 'Utilities',
		department: 'billing',
		status: 'partial',
		tag: 'industry/utilities@v1',
		dept_tag: 'industry_dept/utilities/billing@v1',
		draft_rev: 4,
		updated_at: '2026-10-13T11:05:00Z',
		updated_by: 'owner',
		themes: THEMES(),
		stages: baseStages({
			purchase: 'The customer signs up or switches supplier and accepts the contract.',
			delivery: 'Supply runs: meter readings, the first bills, understanding the bill.',
			change: 'Tariff, instalment amount or move-in/move-out changes.',
			support: 'Bill queries, payment problems, arrangements, outages vs billing.',
			exit: 'Moving out or switching away: final reading and end bill.',
		}),
		priors: priors([
			['need', 'orientation', 0.7],
			['interest', 'investigation', 0.6],
			['choice', 'purchase', 0.7],
			['awaiting', 'delivery', 0.8],
			['issue', 'support', 0.8],
			['issue_resolved', 'support', 1],
			['need_change', 'exit', 0.7],
		]),
		channels: CHANNELS(),
		activities: inAreas(utilitiesBareActivities(), UTILITIES_ACTIVITY_AREAS),
		messages: msgs([
			['orientation', 'chat', ['what is your fixed tariff?', 'green energy?']],
			['purchase', 'chat', ['I want to switch to you']],
			['delivery', 'chat', ['my reading is 18342', 'why is my bill so high?']],
			['support', 'chat', ['I can’t pay this month', 'power is out — is that my bill?']],
		]),
		knowledge_kinds: kinds([
			['orientation', 'catalog.tariffs', true, 'Tariff sheets'],
			['delivery', 'guide.bill_explained', true, 'How to read the bill'],
			['support', 'policy.arrangements', true, 'Payment arrangement policy'],
			['support', 'policy.disconnection', true, 'Disconnection notice rules (I1)'],
		]),
		model_objects: [
			'party',
			'offering',
			'contract',
			'usage',
			'meter_point',
			'arrangement',
			'location',
			'service_event',
			'document',
			'case',
			'knowledge',
		],
		findings: [],
		source_run: 5,
	};
}

// ---- it_services / it_service_desk (generator run #7 draft) -----------------------------------------------------

/** IT service desk areas (access, incidents) and the activities in each. */
export const ITSD_ACTIVITY_AREAS: Record<string, string[]> = {
	access: ['delivery:request_software', 'change:reset_password', 'change:request_access'],
	incidents: [
		'orientation:find_kb_article',
		'investigation:check_service_status',
		'support:report_incident',
		'support:check_ticket_status',
	],
};

export function itsdActivities(): JourneyActivity[] {
	return inAreas(itsdBareActivities(), ITSD_ACTIVITY_AREAS);
}

function itsdBareActivities(): JourneyActivity[] {
	return [
		act(
			'orientation:find_kb_article',
			'customer',
			'Find a how-to article',
			'“how do I set up VPN?”',
			[['knowledge', 'subject', 'article passages citation']],
			['knowledge.search R'],
		),
		act(
			'investigation:check_service_status',
			'customer',
			'Check whether a service is down',
			'“is Outlook down?”',
			[['knowledge', 'subject', 'passages citation']],
			['knowledge.search R'],
		),
		act(
			'delivery:request_software',
			'customer',
			'Request software',
			'“I need a Figma licence”',
			[
				['case{service_request}', 'subject', 'case_id status subject', 'subject'],
				['party', 'context', 'employee_id department manager'],
			],
			['party.list R', 'case.create{service_request} R'],
		),
		act(
			'change:reset_password',
			'customer',
			'Reset password / unlock account',
			'J2 locked out',
			[['credential', 'subject', 'username factor locked last_reset_at']],
			['credential.reset O'],
		),
		act(
			'change:request_access',
			'customer',
			'Request access to a system',
			'“I need access to the finance share”',
			[
				['access_grant', 'subject', 'resource role approver expires_at', 'resource role'],
				['party', 'context', 'employee_id manager'],
			],
			['access_grant.create R', 'access_grant.list O'],
		),
		act(
			'support:report_incident',
			'customer',
			'Report an incident',
			'“my laptop won’t boot”',
			[
				['case{incident}', 'subject', 'case_id status reason priority', 'subject'],
				['asset', 'context', 'asset_tag model'],
			],
			['case.create{incident} R', 'asset.list O'],
		),
		act(
			'support:check_ticket_status',
			'customer',
			'Check ticket status',
			'“any news on INC0012345?”',
			[['case', 'subject', 'case_id status assignment_group']],
			['case.read R'],
		),
	];
}

export function itsdJourney(): IndustryJourney {
	return {
		industry: 'it_services',
		label: 'IT services',
		department: 'it_service_desk',
		status: 'draft',
		tag: null,
		dept_tag: null,
		draft_rev: 7,
		updated_at: '2026-10-17T16:40:00Z',
		updated_by: 'owner',
		themes: THEMES(),
		stages: baseStages(
			{
				orientation: 'An employee looks for self-help: how-to articles, the service catalogue.',
				investigation: 'An employee checks whether a service is down before reporting it.',
				delivery: 'Fulfilment of a service request: software, hardware, onboarding.',
				change: 'Access and credential changes: passwords, MFA, permissions.',
				support: 'Incidents: something is broken and the employee needs it fixed.',
				exit: 'Offboarding: return equipment, revoke access.',
			},
			['purchase'],
		),
		priors: priors([
			['need', 'orientation', 0.7],
			['issue', 'support', 0.75],
			['issue', 'change', 0.25],
			['issue_resolved', 'support', 1],
			['need_change', 'exit', 0.6],
		]),
		channels: CHANNELS(),
		activities: itsdActivities(),
		messages: msgs([
			['orientation', 'chat', ['how do I set up VPN?', 'where is the printer guide?']],
			['investigation', 'chat', ['is Outlook down?', 'Teams is slow for everyone?']],
			['delivery', 'chat', ['I need a Figma licence', 'new starter Monday needs a laptop']],
			['change', 'chat', ['I am locked out', 'reset my MFA', 'access to the finance share']],
			['support', 'chat', ['my laptop won’t boot', 'any news on INC0012345?']],
		]),
		knowledge_kinds: kinds([
			['orientation', 'kb.howto', true, 'How-to articles'],
			['investigation', 'status.services', true, 'Service status page'],
			['change', 'policy.access', true, 'Access policy'],
			['support', 'kb.troubleshooting', true, 'Troubleshooting'],
		]),
		model_objects: ['case', 'credential', 'access_grant', 'asset', 'knowledge', 'party'],
		findings: [],
		source_run: 7,
	};
}
