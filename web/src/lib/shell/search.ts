// Search for the command palette (⌘K). Pages and settings are found here, client side (settings are pages: Team &
// access, Providers, Channels, Connections, found by their usual names); cases come from `GET /api/search` and are
// merged in by `paletteRows`.
import { navFor, type NavItem, type NavSession } from '$lib/pages';
import type { CaseHit } from '$lib/api/types/runtime';

/** Old (backend / pre-redesign) terms that still find the renamed page (artifact §05), keyed by page id + label. */
const OLD_TERMS: Record<string, string[]> = {
	J3: ['readiness', 'gaps'],
	J14: ['release', 'cut', 'approve', 'apply'],
	J13: ['refinements', 'template pin', 'base'],
	J17: ['validation grid', 'validation'],
	J7: ['gates', 'gt', 'gs', 'ga'],
	J22: ['operation calls', 'opc', 'operations'],
	J23: ['agents', 'bus'],
	J1: ['dashboard', 'overview'],
	T1: ['jobs'],
	T2: ['tenant', 'overview'],
	T3: ['users', 'roles', 'members'],
	T4: ['audit'],
};

/** Words people use for the settings-like pages: who can do what, model providers, channels, connections. */
const SETTINGS_WORDS = ['settings', 'preferences', 'configuration', 'configure', 'setup'];
const SETTINGS_IDS = ['T3', 'T6', 'T10'];

const MAX = 8;

function terms(i: NavItem): string[] {
	const settings = SETTINGS_IDS.includes(i.id) || i.label === 'Channels' ? SETTINGS_WORDS : [];
	return [i.label.toLowerCase(), ...(OLD_TERMS[i.id] ?? []), ...settings];
}

/** Every page `s` may open: workspace, the current agent's (when `job` is set), Advanced/More included, and the studio. */
function candidates(s: NavSession, job: string): NavItem[] {
	const seen = new Set<string>();
	const out: NavItem[] = [];
	for (const scope of ['workspace', 'agent', 'studio'] as const) {
		for (const g of navFor(s, job, scope))
			for (const sec of g.sections)
				for (const item of sec.items) {
					if (seen.has(item.href)) continue;
					seen.add(item.href);
					out.push(item);
				}
	}
	return out;
}

/** Case-insensitive over the page label and its old term; prefix matches first, then substring; at most 8. */
export function searchPages(q: string, s: NavSession, job: string): NavItem[] {
	const needle = q.trim().toLowerCase();
	if (!needle) return [];
	const scored: { item: NavItem; rank: number }[] = [];
	for (const item of candidates(s, job)) {
		const ts = terms(item);
		const rank = ts.some((t) => t.startsWith(needle)) ? 0 : ts.some((t) => t.includes(needle)) ? 1 : -1;
		if (rank >= 0) scored.push({ item, rank });
	}
	// Array.sort is stable, so nav order breaks ties.
	return scored
		.sort((a, b) => a.rank - b.rank)
		.slice(0, MAX)
		.map((x) => x.item);
}

/** One selectable row of the palette: a page or a case. */
export type PaletteRow =
	| { kind: 'page'; href: string; label: string; icon: string }
	| { kind: 'case'; href: string; label: string; detail: string };

/** A case hit in plain words: the id and title, then who, which agent and where it stands. */
export function caseRow(h: CaseHit): PaletteRow {
	const label = h.title ? `${h.case_id} · ${h.title}` : h.case_id;
	const detail = [h.customer_name, h.job_name ?? h.job, h.status.replace(/_/g, ' ')]
		.filter(Boolean)
		.join(' · ');
	return { kind: 'case', href: h.href, label, detail };
}

/** Pages first, then cases; a case whose page is already listed is not listed twice. */
export function paletteRows(pages: NavItem[], cases: CaseHit[]): PaletteRow[] {
	const rows: PaletteRow[] = pages.map((p) => ({ kind: 'page', href: p.href, label: p.label, icon: p.icon }));
	const seen = new Set(rows.map((r) => r.href));
	for (const h of cases) {
		if (seen.has(h.href)) continue;
		seen.add(h.href);
		rows.push(caseRow(h));
	}
	return rows;
}
