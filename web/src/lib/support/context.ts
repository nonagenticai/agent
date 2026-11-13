// Plain wording for the customer-context block of the staff context pane: product + date first, ids secondary.
import type { ContextFocus, ContextOrder, CustomerContext, IdentitySource } from '$lib/api/types/support';

/** The lists collapse after this many rows. */
export const COLLAPSE_AT = 3;

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
/** "12 Oct" (UTC date; the year only when it is not this year). */
export function shortDate(iso: string | null | undefined, now = Date.now()): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '—';
	const y = d.getUTCFullYear() === new Date(now).getUTCFullYear() ? '' : ` ${d.getUTCFullYear()}`;
	return `${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]}${y}`;
}

export const IDENTITY_CHIP: Record<IdentitySource, string> = {
	assertion: 'Verified · signed in',
	staff_pick: 'Picked by staff',
	staff_test: 'Test customer',
};

/** What the admin is told when a staff test chat's typed email did not resolve to one customer (never an id). */
export interface LookupNote {
	text: string;
	/** The unrouted lookup is fixed on the job's operations page. */
	href?: string;
	linkLabel?: string;
}

export function lookupNote(degraded: string[] | undefined, job: string): LookupNote | null {
	const d = degraded ?? [];
	if (d.includes('party_not_found')) return { text: 'No customer with that email in your connected system.' };
	if (d.includes('party_ambiguous'))
		return { text: 'More than one customer has that email in your connected system.' };
	if (d.includes('party_lookup_unavailable'))
		return {
			text: "Your connected system can't look customers up by email yet — route Find a customer (party.list).",
			href: `/admin/jobs/${encodeURIComponent(job)}/operations`,
			linkLabel: 'Open operations',
		};
	return null;
}

/** "Wool rug 160x230" or "Linen cushion cover ×2, Table lamp" or "Linen cushion cover +2 more". */
export function itemsSummary(o: ContextOrder, max = 2): string {
	const parts = o.items.map((i) => (i.qty > 1 ? `${i.name} ×${i.qty}` : i.name));
	if (parts.length <= max) return parts.join(', ') || 'Order';
	return `${parts.slice(0, max).join(', ')} +${parts.length - max} more`;
}

export const money = (amount: number, currency: string): string => {
	try {
		return new Intl.NumberFormat('en-NL', { style: 'currency', currency }).format(amount);
	} catch {
		return `${amount.toFixed(2)} ${currency}`;
	}
};

/** The focused record as the human reads it: "Order 12 Oct · Wool rug", with the record's id as secondary text. */
export function focusLabel(
	f: ContextFocus,
	c: CustomerContext,
	now = Date.now(),
): { label: string; id: string } {
	const order = (id: string) => c.orders.find((o) => o.order_id === id);
	const of = (id: string, what: string) => {
		const o = order(id);
		return o ? `${what} ${shortDate(o.placed_at, now)} · ${itemsSummary(o, 1)}` : what;
	};
	switch (f.kind) {
		case 'order': {
			const o = order(f.id);
			return { label: o ? of(f.id, 'Order') : 'Order', id: f.id };
		}
		case 'payment': {
			const p = c.payments.find((x) => x.payment_id === f.id);
			return { label: p ? of(p.order_id, 'Payment for the order of') : 'Payment', id: f.id };
		}
		case 'delivery': {
			const d = c.deliveries.find((x) => x.delivery_id === f.id);
			return { label: d ? of(d.order_id, 'Delivery of the order of') : 'Delivery', id: f.id };
		}
		case 'case': {
			const k = c.cases.find((x) => x.case_id === f.id);
			return { label: k ? `Case · ${k.title}` : 'Case', id: f.id };
		}
	}
}
