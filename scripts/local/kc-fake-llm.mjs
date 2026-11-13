#!/usr/bin/env node
// B4 known-customer web chat, LOCAL acceptance: a deterministic OpenAI-compatible fake for the platform's own model
// (INTERNAL_LLM_URL, normally the pgx vLLM, unreachable from dev boxes). Follows the B8 precedent
// (scripts/local/b8-fake-llm.mjs, which is the generator/discovery fake and is left untouched).
//
//   GET  /health, /__calls          liveness; call counters + the last qualifier decisions (message -> space, slots)
//   GET  */models                   model list (provider probes)
//   POST */chat/completions         every model call a web chat turn makes, told apart by the system message:
//     - qualifier  `[task qualify]` (crates/agent-api/src/turn/qualify.rs): the user message is JSON
//         {message, draft, context_spaces:[{key,label,slots}]}; the answer is {"context_space": <key>|null, "slots":{}}.
//         The space is picked from the job's OWN context_spaces (key/label match), never a hard-coded key, so the fake
//         follows whatever keys the design really carries (response_format.json_schema.enum is honoured as well):
//           "where is my parcel", "track ..."          -> the space whose key ends in `track_parcel`, NO slots
//           "where is my <x> order"                    -> order_status (item_hint); "cancel ..." -> cancel_order
//           "parcel late/damaged/lost"                 -> delivery_problem (problem); "send it to <address>/neighbour" -> redirect (address)
//           "charged twice / dispute" -> dispute, else payment_errors; "payment pending" -> pending_charge
//           "broken / warranty / guarantee" -> warranty (defect); "complain" -> complaint_intake; "delete my data" -> data_rights
//           "refund it" -> refund_request (falls back to refunds); "where is my refund" -> refunds
//           refund questions                           -> the refunds space
//           "return the rug", "send it back"          -> the returns space; "the <noun>" goes under `item_hint`
//           "speak to a person/human/colleague"       -> the space whose key ends in `handover_to_person` (handover_reason filled)
//           anything else                              -> null
//     - answer     `[task answer]` (knowledge step, crates/agent-api/src/knowledge/citations.rs): the first numbered
//         passage, cited `[1]`; no passage -> NO_ANSWER. (Same contract as tests/kh/t7_answer.rs turn_model.)
//     - assistant  `[task assistant]` (staff assistant loop): a short plain reply, no tool calls.
//     - anything else (interaction playground passthrough, `stream: true` SSE): a short plain reply.
//   POST */v1/systemone             Laya stand-in: neutral answers (the gates are "skipped" without INTERNAL_LAYA_*).
//
// No reply ever contains an order/payment/delivery/customer id or asks for one (owner KYC ruling 2026-10-26).
//
// Env: KC_LLM_PORT (required).
import { createServer } from 'node:http';
import { bearerOf, verifyBundleToken } from './ps-bundle.mjs';
// opt-in (platform-stack): chat completions need a bundle token minted for inference-router, else 401 (agent-api's smoke sends a bogus token first)
const BUNDLE_SECRET = process.env.PS_BUNDLE_SECRET ?? '';

const PORT = Number(process.env.KC_LLM_PORT ?? 0);
if (!PORT) throw new Error('KC_LLM_PORT is required');
const calls = {};
const decisions = [];
const bump = (k) => (calls[k] = (calls[k] ?? 0) + 1);

const SYSTEM_SLOTS = new Set(['order_id', 'payment_id', 'delivery_id', 'tracking_ref']);

/** Slot values the fake gives for slots a returns/refund space asks for (never an id-like value). Slots the message itself states
 *  (problem, address, defect, complaint_summary, desired_outcome, subject) are extracted by slotsFrom(), not invented. */
const SLOT_VALUES = { reason: 'I changed my mind', note: 'No further details', handover_reason: 'The customer asked to speak to a person' };

/** want -> acceptable key tails (first listed wins), then a label/key regex fallback. */
const TAILS = {
	track_parcel: [['track_parcel'], /track|parcel/],
	order_status: [['order_status'], /order.?status/],
	delivery_problem: [['delivery_problem'], /delivery.?problem|problem/],
	redirect_delivery: [['redirect_delivery', 'redirect'], /redirect/],
	cancel_order: [['cancel_order'], /cancel/],
	returns: [['returns'], /return/],
	refunds: [['refunds'], /refund/],
	refund_request: [['refund_request'], /refund.?request/],
	payment_errors: [['payment_errors'], /payment.?error/],
	pending_charge: [['pending_charge'], /pending/],
	dispute: [['dispute'], /dispute/],
	warranty: [['warranty'], /warrant/],
	complaint: [['complaint_intake', 'complaint'], /complain/],
	data_rights: [['data_rights'], /data.?rights|privacy/],
	handover: [['handover_to_person'], /hand.?over|person/],
	shipping_info: [['shipping_info'], /shipping/],
};

/** wants: ordered fallback list; the first want that one of the offered spaces matches wins. */
function pickSpace(spaces, wants) {
	const hay = (s) => `${s.key} ${s.label ?? ''}`.toLowerCase();
	for (const want of [].concat(wants)) {
		const [tails] = TAILS[want];
		const hit = spaces.find((s) => tails.some((t) => s.key.toLowerCase() === t || s.key.toLowerCase().endsWith(`:${t}`)));
		if (hit) return hit;
	}
	for (const want of [].concat(wants)) {
		const hit = spaces.find((s) => TAILS[want][1].test(hay(s)));
		if (hit) return hit;
	}
	return null;
}

/** knowledge question about shipping (platform-stack knowledge proof): selects the space bound to the `shipping` knowledge kind */
const SHIPPING_Q = /how long (does|do|will) (shipping|delivery)|shipping (policy|times?|information)|how (is|are) (it|my (order|parcel)|parcels?) (packed|packaged)/;

/** message -> { name, wants } (ordered: overlapping messages pick the most specific intent first). */
function intentOf(message) {
	const m = message.toLowerCase();
	const is = (name, ...wants) => ({ name, wants });
	if (/\b(speak|talk|chat) to (a|an|some|someone|my)\b.*\b(person|human|agent|colleague|someone)|real person|\bhuman\b/.test(m)) return is('handover', 'handover');
	if (SHIPPING_Q.test(m)) return is('shipping_info', 'shipping_info');
	if (/delete (all )?my (personal )?data|personal data|\bgdpr\b|right to be forgotten|erase my data|export my data/.test(m)) return is('data_rights', 'data_rights');
	if (/complain|unhappy with (the|your) (service|decision)/.test(m)) return is('complaint', 'complaint');
	if (/dispute|charged (me )?(twice|two times|2 times)|double[- ]charge|charged double|unauthori[sz]ed (charge|payment)|chargeback/.test(m))
		return is('dispute', 'dispute', 'payment_errors', 'pending_charge');
	if (/refund|money back|reimburs/.test(m)) {
		const status = /where|status|when|still|missing|late|haven.?t|have not|not (yet )?(received|got|arrived|seen)|how long|any news|waiting|expected|update|coming|wrong amount/.test(m);
		return status ? is('refunds', 'refunds') : is('refund_request', 'refund_request', 'refunds');
	}
	if (/\breturn\b|send (it|this|that) back|exchange/.test(m)) return is('returns', 'returns');
	if (/\bcancel(l?ed|l?ing)?\b|cancellation/.test(m)) return is('cancel_order', 'cancel_order');
	if (/(parcel|package|delivery|shipment).*(late|damaged|lost|missing|delayed|stolen|never arrived)|arrived (damaged|broken)|damaged on arrival|(late|lost|damaged|delayed|missing) (parcel|package|delivery)|says delivered|marked as delivered/.test(m))
		return is('delivery_problem', 'delivery_problem');
	if (/redirect|re-?route|(different|another|other|new) address|change (the |my )?(delivery )?address|neighbou?r|\b(send|deliver|ship|forward)\b[^.?!]*\bto\b/.test(m) && !/\bback\b/.test(m))
		return is('redirect_delivery', 'redirect_delivery');
	if (/warranty|guarantee|faulty|defective|stopped working|not working|\bbroke\b|broken|cracked|fell apart|wobbl/.test(m)) return is('warranty', 'warranty');
	if (/\b(payment|charge|charged|authori[sz]ation)\b.*(pending|not cleared|hasn.?t cleared)|pending (payment|charge|authori[sz]ation)|(don.?t|do not) recogni[sz]e (a |the |this )?charge/.test(m))
		return is('pending_charge', 'pending_charge', 'dispute', 'payment_errors');
	if (/(declined|payment (failed|error|problem))/.test(m)) return is('payment_errors', 'payment_errors', 'dispute');
	if (/parcel|package|tracking|\btrack\b|shipment|has (it|my [a-z ]+) (shipped|arrived)|when.*(arrive|deliver)/.test(m)) return is('track_parcel', 'track_parcel');
	if (/where.*(order|delivery)|order status|status of (my|the) order|is my order/.test(m)) return is('order_status', 'order_status');
	return null;
}

/** "the rug" / "my headphones" -> "rug" / "headphones" (the qualifier's `item_hint`, words only). */
function itemHint(message) {
	for (const m of message.matchAll(/(?=\b(?:the|my|this|that)\s+([a-z][a-z ]{1,40}))/gi)) {
		const w = m[1]
			.replace(/\s+(to|is|are|was|were|arrived|should|still|shows?|for|from|instead|it|has|have|please|and|in|on|at|about|did|does|will|would|never)\b.*$/i, '')
			.replace(/\s+(order|parcel|package|delivery|shipment|payment|charge)$/i, '')
			.trim();
		if (w && !/^(order|parcel|package|refund|return|item|one|payment|charge|label|last|personal data|money|mind|neighbou?r|delivery|shipment|address)$/i.test(w)) return w;
	}
	return null;
}

/** Slots the message itself states (a missing slot stays missing, so the platform asks and the follow-up answers it). */
function slotsFrom(space, message) {
	const m = message.toLowerCase();
	const out = {};
	for (const slot of space.slots ?? []) {
		if (SYSTEM_SLOTS.has(slot)) continue;
		let v = null;
		if (slot === 'problem') v = /\b(late|delayed|damaged|lost|missing|stolen|broken)\b/.exec(m)?.[1] ?? (/says delivered|marked as delivered/.test(m) ? 'not received' : null);
		else if (slot === 'address') v = /\bto\s+(.+?)(?:\s+instead)?\s*[.?!]*$/i.exec(message.trim())?.[1] ?? null;
		else if (slot === 'defect') v = /\b(broken|broke|cracked|faulty|defective|damaged|wobbl\w*|not working|stopped working|fell apart)\b/.exec(m)?.[1] ?? null;
		else if (slot === 'complaint_summary' || slot === 'subject') v = message.trim().slice(0, 200);
		else if (slot === 'desired_outcome') v = /\b(replacement|refund|money back|compensation|apolog\w*)\b/.exec(m)?.[1] ?? null;
		else v = SLOT_VALUES[slot] ?? null;
		if (v) out[slot] = v;
	}
	return out;
}

function qualify(body) {
	let user = {};
	try {
		user = JSON.parse(String(body.messages?.[1]?.content ?? '{}'));
	} catch {
		/* not JSON: no space */
	}
	const spaces = Array.isArray(user.context_spaces) ? user.context_spaces : [];
	const message = String(user.message ?? '');
	const draft = user.draft && typeof user.draft === 'object' ? user.draft : null;
	// the guided-decoding enum, when present, bounds the answer
	const enumKeys = body.response_format?.json_schema?.schema?.properties?.context_space?.enum;
	const allowed = Array.isArray(enumKeys) ? spaces.filter((s) => enumKeys.includes(s.key)) : spaces;

	let space = null;
	const slots = {};
	if (draft?.context_space && draft.awaiting) {
		// a follow-up that answers the slot the last turn asked for
		space = allowed.find((s) => s.key === draft.context_space) ?? null;
		if (space) slots[draft.awaiting] = SLOT_VALUES[draft.awaiting] ?? message.trim().slice(0, 80) ?? '';
	} else {
		const intent = intentOf(message);
		space = intent ? pickSpace(allowed, intent.wants) : null;
		if (space && !['track_parcel', 'complaint', 'data_rights'].includes(intent.name)) {
			const hint = itemHint(message);
			if (hint) slots.item_hint = hint;
		}
		if (space && intent.name !== 'track_parcel') Object.assign(slots, slotsFrom(space, message));
	}
	decisions.push({ message, context_space: space?.key ?? null, slots });
	if (decisions.length > 50) decisions.shift();
	return JSON.stringify({ context_space: space?.key ?? null, slots });
}

function answer(system, question = '') {
	const after = system.includes('[passages]') ? system.slice(system.indexOf('[passages]')) : '';
	// a shipping question is answered from EVERY passage (own document + platform pack), each sentence cited with its own number
	if (SHIPPING_Q.test(question.toLowerCase())) {
		const parts = [...after.matchAll(/^\[(\d+)\]\s*([^\n]+)/gm)].slice(0, 4).map((m) => `${m[2].split(/(?<=[.!?])\s/)[0].trim()} [${m[1]}]`);
		if (parts.length) return parts.join(' ');
	}
	const m = /^\[1\]\s*([^\n]+)/m.exec(after) ?? /^1[.)]\s*([^\n]+)/m.exec(after);
	if (!m) return 'NO_ANSWER';
	const sentence = m[1].split(/(?<=[.!?])\s/)[0].trim();
	return `${sentence} [1]`;
}

function contentFor(body) {
	const system = String(body.messages?.[0]?.content ?? '');
	if (system.includes('[task qualify]') || body.response_format?.json_schema?.name === 'qualify') {
		bump('qualify');
		return qualify(body);
	}
	if (system.includes('[task answer]')) {
		bump('answer');
		return answer(system, String(body.messages?.[1]?.content ?? ''));
	}
	if (system.includes('[task assistant]')) {
		bump('assistant');
		return 'I can help with that. Here is what I found in the case.';
	}
	bump('other');
	return 'Thanks for your message. A colleague will follow up shortly.';
}

function laya(body) {
	const answers = {};
	for (const [key, q] of Object.entries(body.questions ?? {})) {
		const options = q.criteria && !Array.isArray(q.criteria) ? Object.keys(q.criteria) : [];
		if (q.type === 'choice' && options.length) {
			const choice = options.find((o) => o === 'none') ?? options[0];
			answers[key] = { choice, confidence: 0.9, probabilities: { [choice]: 0.9 } };
		} else if (q.type === 'score') answers[key] = { score: 0, confidence: 0.9, probabilities: { 0: 0.9 } };
		else answers[key] = { noul: false, confidence: 0.9 };
	}
	bump('laya');
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

function stream(res, content) {
	res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
	const chunk = (delta, finish = null, extra = {}) =>
		`data: ${JSON.stringify({ id: 'kc-fake', object: 'chat.completion.chunk', model: 'kc-fake-llm', choices: [{ index: 0, delta, finish_reason: finish }], ...extra })}\n\n`;
	for (const w of content.split(/(?<= )/)) res.write(chunk({ content: w }));
	res.write(chunk({}, 'stop'));
	res.write(
		`data: ${JSON.stringify({ id: 'kc-fake', object: 'chat.completion.chunk', model: 'kc-fake-llm', choices: [], usage: { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 } })}\n\n`,
	);
	res.end('data: [DONE]\n\n');
}

createServer(async (req, res) => {
	const path = new URL(req.url, 'http://x').pathname;
	if (req.method === 'GET' && path === '/health') return send(res, 200, { ok: true });
	if (req.method === 'GET' && path === '/__calls') return send(res, 200, { calls, decisions });
	if (req.method === 'GET' && path.endsWith('/models'))
		return send(res, 200, { object: 'list', data: [{ id: 'local-llm', object: 'model' }] });
	if (BUNDLE_SECRET && req.method === 'POST' && path.endsWith('/chat/completions') && !verifyBundleToken(BUNDLE_SECRET, bearerOf(req), 'inference-router')) {
		res.writeHead(401, { 'content-type': 'application/json' });
		return res.end(JSON.stringify({ error: { code: 'unauthorized', message: 'invalid licence token' } }));
	}
	if (req.method === 'POST' && path.endsWith('/chat/completions')) {
		const b = await readJson(req);
		const content = contentFor(b);
		if (b.stream) return stream(res, content);
		return send(res, 200, {
			id: 'kc-fake',
			object: 'chat.completion',
			model: b.model ?? 'local-llm',
			choices: [{ index: 0, message: { role: 'assistant', content }, finish_reason: 'stop' }],
			usage: { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 },
		});
	}
	if (req.method === 'POST' && path.endsWith('/systemone')) return send(res, 200, laya(await readJson(req)));
	send(res, 404, { error: 'not_found', path });
}).listen(PORT, '127.0.0.1', () => console.log(`kc-fake-llm listening on http://127.0.0.1:${PORT}`));
