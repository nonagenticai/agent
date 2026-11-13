// The platform operation vocabulary (N1, 11 §4.3.5 + the ITSD/utilities ops) and the seven journey stages, as
// the ontology-area pages and the fixture world both use them. Owned by the ontology area. The backend serves the
// vocabulary from `operation_def`; these constants are what the concept UI shows until then.
import type { CatalogueOperation, Modality, SideEffect } from '$lib/api/types/ontology';

function op(
	o: string,
	modality: Modality,
	side_effect: SideEffect,
	identity_level: string,
	variant: string | null = null,
): CatalogueOperation {
	return { op: o, variant, modality, side_effect, identity_level };
}

/** The platform operation vocabulary this area shows (N1). Keyed `object.verb`; variants listed separately. */
export const OPERATION_CATALOGUE: CatalogueOperation[] = [
	op('knowledge.search', 'observe', 'R', '0'),
	op('knowledge.answer', 'observe', 'R', '0'),
	op('offering.list', 'observe', 'R', '0'),
	op('offering.read', 'observe', 'R', '0'),
	op('party.list', 'observe', 'R', '1'),
	op('party.read', 'observe', 'R', '2'),
	op('contact_point.update', 'do', 'W', '2'),
	op('consent.update', 'do', 'W', '1'),
	op('order.list', 'observe', 'R', '2'),
	op('order.read', 'observe', 'R', '2'),
	op('order.modify', 'do', 'W', '2'),
	op('order.cancel', 'do', 'X', '2'),
	op('delivery.read', 'observe', 'R', '1/2'),
	op('delivery.redirect', 'do', 'W', '2'),
	op('delivery.report_problem', 'request', 'W', '2'),
	op('payment.list', 'observe', 'R', '2'),
	op('payment.refund', 'do', 'X', '2'),
	op('payment.create', 'do', 'P', '2'),
	op('rule.evaluate', 'observe', 'R', '2'),
	op('case.create', 'request', 'W', '1–2'),
	op('case.read', 'observe', 'R', '2'),
	op('case.comment', 'do', 'W', '2'),
	op('document.read', 'observe', 'R', '2'),
	op('contract.read', 'observe', 'R', '2'),
	op('interaction.send', 'do', 'W', '2'),
	op('interaction.create', 'do', 'W', '1'),
	op('access_grant.list', 'observe', 'R', '2'),
	op('access_grant.create', 'request', 'W', '2'),
	op('credential.reset', 'do', 'X', 'S'),
	op('asset.list', 'observe', 'R', '2'),
	op('usage.list', 'observe', 'R', '2'),
	op('arrangement.create', 'request', 'W', '2'),
];

export const SIDE_EFFECT_RANK: Record<SideEffect, number> = { R: 0, W: 1, X: 2, P: 3 };

export function catalogueOp(key: string): CatalogueOperation | undefined {
	return OPERATION_CATALOGUE.find((o) => o.op === key);
}

/** `order.read`, `case.create{return}`. */
export function opLabel(o: string, variant: string | null): string {
	return variant ? `${o}{${variant}}` : o;
}

/** The seven D23 journey stages: key, label, this job's description (GS options). */
export const STAGES: [string, string, string][] = [
	['orientation', 'Orientation', 'Browsing, product info, general policy questions'],
	['investigation', 'Investigation', 'Stock, price, preorder, delivery options'],
	['purchase', 'Purchase', 'Payment failures, order confirmation, terms'],
	['delivery', 'Delivery', 'Tracking, delivery problems, invoices, usage'],
	['change', 'Change', 'Change or cancel before dispatch, contact details'],
	['support', 'Support', 'Returns, refunds, warranty, complaints, a person'],
	['exit', 'Exit', 'Close account, data requests, unsubscribe'],
];
