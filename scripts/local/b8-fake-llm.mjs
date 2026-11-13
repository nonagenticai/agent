#!/usr/bin/env node
// B8 local acceptance: one fake for every model call B8 makes.
//
//   POST */chat/completions  OpenAI-compatible: the generator drafter (steps business_model .. use_cases,
//                            journey_merge) and discovery's cluster drafter. The step is read from the system prompt
//                            (crates/agent-api/src/generator/prompts/*.md first sentence).
//   POST */v1/systemone      Laya: discovery's per-row classifier (question `activity`) and the validation judge
//                            (question `same_answer`). Any other question gets a neutral answer.
//   GET  */models            model lists for provider probes
//   GET  /__calls            call counters by kind (acceptance specs read them to prove a real model call happened)
//
// Reference for the shapes: crates/agent-api/tests/b8/generator_llm.rs, discovery_classify.rs.
// No answer ever asks a customer for an order number, id, postcode, email or reference (owner KYC ruling 2026-10-26).
//
// Env: B8_LLM_PORT (required).
import { createServer } from 'node:http';

const PORT = Number(process.env.B8_LLM_PORT ?? 0);
if (!PORT) throw new Error('B8_LLM_PORT is required');
const calls = {};
const bump = (k) => (calls[k] = (calls[k] ?? 0) + 1);

const DEFAULT_ACTIVITIES = [
	['orientation:browse_products', 'Browse products', 'customer', 'Customer asks what is available'],
	['investigation:compare_products', 'Compare products', 'customer', 'Customer wants to compare two products'],
	['purchase:place_order', 'Place an order', 'customer', 'Customer wants to buy what is in the basket'],
	['change:change_order', 'Change an order', 'customer', 'Customer wants to change what they ordered'],
	['delivery:track_order', 'Track a delivery', 'customer', 'Where is my parcel?'],
	['support:return_item', 'Return an item', 'customer', 'I want to send this back'],
	['exit:close_account', 'Close an account', 'customer', 'Customer wants to stop using the shop'],
];

/** `stage:activity` keys the context mentions (accepted items of earlier steps), else the defaults. */
function activityKeys(text) {
	const found = [...new Set([...text.matchAll(/\b([a-z][a-z0-9_]{1,40}):([a-z][a-z0-9_]{1,40})\b/g)].map((m) => m[0]))];
	const keys = found.slice(0, 6);
	return keys.length ? keys : DEFAULT_ACTIVITIES.map((a) => a[0]);
}

function item(kind, title, detail, payload, extra = {}) {
	return { kind, title, detail, payload, example: null, new_object: null, confidence: 0.9, ...extra };
}

function journeyItems() {
	const themes = [
		['need', 'Need', 1, 0.1],
		['issue', 'Issue', 2, 0.6],
	].map(([key, label, order, w]) =>
		item('journey_theme', label, `Customers in the ${label.toLowerCase()} state`, {
			key,
			label,
			order,
			escalation_weight: w,
		}),
	);
	const stages = ['orientation', 'investigation', 'purchase', 'change', 'delivery', 'support', 'exit'].map((key, i) =>
		item('journey_stage', key[0].toUpperCase() + key.slice(1), `The ${key} stage`, {
			key,
			label: key[0].toUpperCase() + key.slice(1),
			order: i + 1,
			description: `The ${key} stage of the journey`,
			synonyms: [],
			themes: ['need', 'issue'],
		}),
	);
	const activities = DEFAULT_ACTIVITIES.map(([key, label, actor, trigger]) =>
		item('activity', label, trigger, { key, stage: key.split(':')[0], actor, label, trigger, also_in_stages: [] }),
	);
	return [...themes, ...stages, ...activities];
}

function stepItems(system, userText) {
	const first = system.split('\n', 1)[0];
	const keys = activityKeys(userText);
	if (first.startsWith('You draft the business model')) {
		return [
			['channel_mix', 'chat_and_email'],
			['delivery_model', 'home_delivery'],
			['payment_model', 'online_payment'],
		].map(([axis, value]) =>
			item('profile', `${axis}: ${value}`, `Fits a web shop`, { axis, value, why: 'Typical for the description' }),
		);
	}
	if (first.startsWith('You draft the customer journey') || first.startsWith('You receive several independent drafts'))
		return journeyItems();
	if (first.startsWith('You map each accepted activity')) {
		return keys.map((a) =>
			item('activity_object', `${a} uses order`, 'The known customer’s order', {
				activity: a,
				object: 'order',
				role: 'subject',
				fields: [],
				required_inputs: [],
			}),
		);
	}
	if (first.startsWith('You draft the operation expectations')) {
		return keys.map((a) =>
			item('operation_expectation', `${a} reads order`, 'Reads the most recent order', {
				activity: a,
				op: 'order.read',
				variant: null,
				required: true,
				modality: 'do',
				errors: [],
			}),
		);
	}
	if (first.startsWith('You harmonise')) return [];
	if (first.startsWith('You draft one use-case card')) {
		return keys.map((a) =>
			item(
				'use_case_card',
				`${a} card`,
				'A use case',
				{
					activity: a,
					summary: `The customer asks for help with ${a.split(':')[1].replaceAll('_', ' ')}.`,
					examples: ['where is my parcel?', 'can I send this back?'],
					metric: 'Resolved without staff',
					sample_dialogue: [
						{ role: 'customer', text: 'Where is my parcel?' },
						{ role: 'agent', text: 'I see your order from 12 October; it is out for delivery today.' },
					],
					scenarios: [
						{ id: 'plain', style: 'plain', phrasing: 'where is my parcel?', expected: 'Out for delivery today.', cites: [] },
					],
				},
				{ example: null },
			),
		);
	}
	if (first.startsWith('You receive anonymised customer messages')) {
		let input = {};
		try {
			input = JSON.parse(userText);
		} catch {
			/* not JSON: no cluster */
		}
		const stages = input.stages ?? [];
		const members = (input.messages ?? [])
			.filter((m) => /lawn|garden|sprinkler/i.test(m.text ?? ''))
			.map((m) => m.id);
		if (!members.length) return [];
		return [
			item(
				'discovery_cluster',
				'Garden and lawn',
				'Customers ask about lawn care',
				{
					label: 'Lawn care',
					members,
					suggestion: {
						kind: 'new_activity',
						stage: stages.includes('support') ? 'support' : (stages.at(-1) ?? null),
						slug: 'lawn_care',
						label: 'Lawn care help',
					},
				},
				{ confidence: 0.8 },
			),
		];
	}
	return [];
}

function laya(body) {
	const state = body.state ?? {};
	const msg = String(state.message ?? state.response ?? '').toLowerCase();
	const answers = {};
	for (const [key, q] of Object.entries(body.questions ?? {})) {
		const options = q.criteria && !Array.isArray(q.criteria) ? Object.keys(q.criteria) : [];
		if (key === 'same_answer') {
			bump('laya_judge');
			answers[key] = { choice: 'yes', confidence: 0.95, probabilities: { yes: 0.95 } };
		} else if (q.type === 'choice' && options.length) {
			bump('laya_classify');
			let choice;
			if (/lawn|garden|sprinkler/.test(msg)) choice = 'none';
			else {
				const word = /refund|return|send (it|this) back/.test(msg)
					? 'return'
					: /cancel/.test(msg)
						? 'cancel'
						: 'track';
				choice = options.find((o) => o !== 'none' && o.includes(word)) ?? options.find((o) => o !== 'none') ?? 'none';
			}
			answers[key] = { choice, confidence: 0.9, probabilities: { [choice]: 0.9 } };
		} else if (q.type === 'score') {
			answers[key] = { score: 0, confidence: 0.9, probabilities: { 0: 0.9 } };
		} else {
			answers[key] = { noul: false, confidence: 0.9 };
		}
	}
	return { model: 'laya-fake-1', answers, usage: { input_tokens: 40, output_tokens: 4 } };
}

async function readJson(req) {
	const chunks = [];
	for await (const c of req) chunks.push(c);
	try {
		return JSON.parse(Buffer.concat(chunks).toString() || '{}');
	} catch {
		return {};
	}
}
const send = (res, status, body) => {
	res.writeHead(status, { 'content-type': 'application/json' });
	res.end(JSON.stringify(body));
};

createServer(async (req, res) => {
	const path = new URL(req.url, 'http://x').pathname;
	if (req.method === 'GET' && path === '/health') return send(res, 200, { ok: true });
	if (req.method === 'GET' && path === '/__calls') return send(res, 200, calls);
	if (req.method === 'GET' && path.endsWith('/models')) return send(res, 200, { data: [{ id: 'fake-gen' }], models: ['fake-gen'] });
	if (req.method === 'POST' && path.endsWith('/chat/completions')) {
		const b = await readJson(req);
		const system = String(b.messages?.[0]?.content ?? '');
		const userText = String(b.messages?.[1]?.content ?? '');
		bump('chat');
		const items = stepItems(system, userText);
		bump(`chat:${system.split('\n', 1)[0].slice(0, 40)}`);
		return send(res, 200, {
			choices: [{ message: { role: 'assistant', content: JSON.stringify({ items }) } }],
			usage: { prompt_tokens: 100, completion_tokens: 20 },
		});
	}
	if (req.method === 'POST' && path.endsWith('/systemone')) return send(res, 200, laya(await readJson(req)));
	send(res, 404, { error: 'not_found', path });
}).listen(PORT, '127.0.0.1', () => console.log(`b8-fake-llm listening on http://127.0.0.1:${PORT}`));
