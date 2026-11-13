// Go-live checklist (J3): the readiness board regrouped for people. "Must fix" stops a release going live, "customers
// would notice" does not, and areas with nothing open collapse into "passing". Display only: the backend owns the
// verdict (D67); this never re-derives what blocks apply, it reads `gap.blocks`.
import { ROLE_LABEL, type Role } from '$lib/api/types/core';
import type { ReadinessArea, ReadinessAreaKey, ReadinessBoard, ReadinessGap } from '$lib/api/types/release';

export type Owner = Role | 'platform';

export interface Item {
	key: string;
	title: string;
	why: string;
	cause: string | null;
	ownerRole: Owner;
	/** Whether the tenant can fix it at all (false: the platform team does). */
	fixer: ReadinessGap['fixer'];
	/** True for a Test-env route gap: limits testing the draft, never publishing. */
	testOnly: boolean;
	fixLabel: string;
	href: string;
	/** Backend subject; shown only with "Show technical details". */
	tech: string;
}

export interface Checklist {
	mustFix: Item[];
	noticeable: Item[];
	passing: ReadinessArea[];
	pass: number;
	total: number;
	firstFix: Item | null;
}

const OWNER_BY_AREA: Partial<Record<ReadinessAreaKey, Role>> = {
	providers: 'integrations_manager',
	routes: 'integrations_manager',
	tests: 'job_designer',
	knowledge: 'knowledge_manager',
};

export function ownerOf(g: Pick<ReadinessGap, 'area' | 'fixer'>): Owner {
	if (g.fixer === 'super_admin') return 'platform';
	return OWNER_BY_AREA[g.area] ?? 'tenant_admin';
}

export function ownerLabel(o: Owner): string {
	return o === 'platform' ? 'Platform team' : ROLE_LABEL[o];
}

function itemOf(g: ReadinessGap): Item {
	return {
		key: g.key,
		title: g.title,
		why: g.why,
		cause: g.cause,
		ownerRole: ownerOf(g),
		fixer: g.fixer,
		testOnly: g.blocks === 'testing',
		fixLabel: g.fix_label,
		href: g.href,
		tech: g.subject,
	};
}

/** The other gaps (any group) that the same link fixes. */
export function alsoFixes(b: ReadinessBoard, item: Item): number {
	return b.gaps.filter((g) => g.key !== item.key && g.href === item.href).length;
}

export function checklistOf(b: ReadinessBoard): Checklist {
	const mustFix = b.gaps.filter((g) => g.blocks === 'apply').map(itemOf);
	const noticeable = b.gaps.filter((g) => g.blocks !== 'apply').map(itemOf);
	const open = new Set(b.gaps.map((g) => g.area));
	const passing = b.areas.filter((a) => a.status === 'ok' && !open.has(a.key));
	let firstFix: Item | null = null;
	let best = -1;
	for (const m of mustFix) {
		const n = alsoFixes(b, m);
		if (n > best) {
			best = n;
			firstFix = m;
		}
	}
	return {
		mustFix,
		noticeable,
		passing,
		pass: passing.length,
		total: passing.length + mustFix.length + noticeable.length,
		firstFix,
	};
}

const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? '' : 's'}`;

/** The one sentence the page leads with. */
export function statusSentence(c: Checklist): string {
	if (!c.total) return 'Nothing to check yet.';
	if (c.mustFix.length)
		return `${c.pass} of ${c.total} checks pass. ${c.mustFix.length} must be fixed before going live.`;
	if (c.noticeable.length)
		return `${c.pass} of ${c.total} checks pass. ${plural(c.noticeable.length, 'item')} customers would notice.`;
	return `All ${c.total} checks pass.`;
}

/** The collapsed "passing" line: how many, and which areas. */
export function passingSummary(c: Checklist): string {
	if (!c.passing.length) return 'No checks pass yet';
	const names = c.passing.slice(0, 3).map((a) => a.label.toLowerCase());
	const more = c.passing.length - names.length;
	return `${plural(c.pass, 'check')} pass: ${names.join(', ')}${more > 0 ? ` and ${more} more` : ''}`;
}

/** Items grouped by who can fix them, in first-seen order. */
export function byOwner(items: Item[]): { owner: Owner; items: Item[] }[] {
	const out: { owner: Owner; items: Item[] }[] = [];
	for (const it of items) {
		const g = out.find((x) => x.owner === it.ownerRole);
		if (g) g.items.push(it);
		else out.push({ owner: it.ownerRole, items: [it] });
	}
	return out;
}
