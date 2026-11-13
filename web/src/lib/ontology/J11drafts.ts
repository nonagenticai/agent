// J11 per-row draft marks (D54): which entities, relations and context spaces the job's draft ledger touches, read
// from `api().draftDiff()` keys (`entity:Order`, `attributes:Order`, `synonyms:Order`, `hide:Order`,
// `relation:Order:has:Shipment`, `space:returns`). A row with a mark shows a "Draft" chip: saved, not live yet.
import type { DraftChange } from '$lib/api/types/release';

export interface J11DraftMarks {
	entities: Map<string, DraftChange[]>;
	relations: Map<string, DraftChange[]>;
	spaces: Map<string, DraftChange[]>;
}

const ENTITY = /^(entity|attributes|synonyms|hide):(.+)$/;

export const relationMarkKey = (r: { subject: string; predicate: string; object: string }) =>
	`${r.subject}:${r.predicate}:${r.object}`;

function add(m: Map<string, DraftChange[]>, k: string, c: DraftChange) {
	const xs = m.get(k);
	if (xs) xs.push(c);
	else m.set(k, [c]);
}

export function draftMarks(changes: readonly DraftChange[]): J11DraftMarks {
	const out: J11DraftMarks = { entities: new Map(), relations: new Map(), spaces: new Map() };
	for (const c of changes) {
		const e = ENTITY.exec(c.key);
		if (e) add(out.entities, e[2]!, c);
		else if (c.key.startsWith('relation:')) add(out.relations, c.key.slice('relation:'.length), c);
		else if (c.key.startsWith('space:')) add(out.spaces, c.key.slice('space:'.length), c);
	}
	return out;
}

/** Tooltip for a Draft chip: the change(s) in one line each. */
export const draftTitle = (xs: readonly DraftChange[] | undefined) =>
	xs?.length
		? `Saved to draft, not live yet: ${xs.map((x) => x.label).join('; ')}`
		: 'Saved to draft, not live yet';
