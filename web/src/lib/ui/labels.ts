// The UI glossary (design artifact §05). Backend names stay in code and APIs; every user-facing word for them comes
// from here, so a rename is one edit.
import type { AgentState } from '$lib/api/types/status';
import { prefs } from './prefs.svelte';

const LABELS = {
	tenant: 'Workspace',
	job: 'Agent',
	jobs: 'Agents',
	bus_agent: 'Specialist',
	readiness: 'Go-live checklist',
	gap: 'Item to fix',
	cut: 'Prepare',
	approve: 'Approve',
	apply: 'Go live',
	refinements: 'Your changes',
	template_pin: 'Template version',
	validation_grid: 'Test suite',
	contained: 'Resolved by the agent',
	operation_calls: 'Actions on your systems',
	dead_letters: 'Failed messages',
	drift: 'Changed elsewhere',
	release_not_ready: "This release can't go live yet",
	release_model_not_built: 'Release not prepared yet',
	live_for_customers: 'Answering customers',
	not_live_for_customers: 'Not answering customers yet',
	required_settings: 'Required settings',
	checkpoints: 'Checkpoints',
} as const satisfies Record<string, string>;

export type LabelKey = keyof typeof LABELS;

export const L: Readonly<Record<LabelKey, string>> = LABELS;

/** The one status an agent shows everywhere. */
export type { AgentState };

export const STATUS_LABEL: Record<AgentState, string> = {
	draft: 'Draft',
	ready: 'Ready',
	live: 'Live',
	live_with_issues: 'Live with issues',
	paused: 'Paused',
};

const RELEASE_ID = /cfg-(\d{4})-(\d{2})-(\d{2})\.(\d+)/g;
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** A release id (`cfg-2026-10-18.1`) the way people say it: "18 Oct release", "18 Oct release (2)" for a second one that day. */
export function releaseName(id: string): string {
	return id.replace(RELEASE_ID, (_, _y: string, m: string, d: string, n: string) => {
		const name = `${Number(d)} ${MONTHS[Number(m) - 1] ?? m} release`;
		return n === '1' ? name : `${name} (${n})`;
	});
}

/** Release id in the UI: the plain name, or the raw id when "Show technical details" is on. */
export function releaseLabel(id: string): string {
	return prefs.showTech ? id : releaseName(id);
}

const KNOWN_KEYS: Record<string, string> = {
	'strata_ingest.vision': 'Image reading',
	'strata_ingest.ocr': 'Document reading',
	strata_ingest: 'Document ingest',
	strata_rag: 'Knowledge search',
	licence_ensure: 'Licence setup',
	licence_revoke: 'Licence removal',
	connections_put: 'Connection setup',
	store_ensure: 'Knowledge store setup',
	slot_not_configured: 'Setting not configured',
	job_setting: 'Agent setting',
	window_days: 'Return window (days)',
	excluded_categories: 'Excluded categories',
	not_offered: 'Not offered',
	'policy.order_changes': 'Order-change policy',
};

const capitalise = (s: string) => (s ? s[0]!.toUpperCase() + s.slice(1) : s);

/** A snake_case or dotted key as plain words: `excluded_categories` -> "Excluded categories". Known names get their own label. */
export function humanKey(key: string): string {
	const known = KNOWN_KEYS[key];
	if (known) return known;
	const words = key
		.replace(/[._-]+/g, ' ')
		.trim()
		.toLowerCase();
	return capitalise(words);
}

/** The key as plain words unless technical details are on (then the raw key). For inline text; use <TechDetail> for the raw form beside it. */
export function keyLabel(key: string): string {
	return prefs.showTech ? key : humanKey(key);
}

/** A template layer ref in plain words: `job/customer_care_agent@v3` -> "Customer care agent template v3",
 * `retail/customer_service@v2` -> "Retail customer service v2", `retail@v2` -> "Retail v2", `base@v3` -> "Base template v3". */
export function layerLabel(ref: string): string {
	const m = /^(?:(job|industry|industry_dept|department)\/)?([a-z0-9_]+(?:\/[a-z0-9_]+)*)(?:@v(\d+))?$/.exec(
		ref,
	);
	if (!m) return humanKey(ref);
	const [, kind, path, v] = m;
	const name = capitalise(path!.replace(/[/_]+/g, ' '));
	const ver = v ? ` v${v}` : '';
	if (kind === 'job') return `${name} template${ver}`;
	if (!kind && path === 'base') return `Base template${ver}`;
	return `${name}${ver}`;
}

/** Layer ref in the UI: plain words, or the raw ref when technical details are on. */
export function layerRef(ref: string): string {
	return prefs.showTech ? ref : layerLabel(ref);
}

/** Text from the backend or a logic module, with raw release ids, layer refs and identifiers replaced unless technical details are on. */
export function plain(text: string): string {
	if (prefs.showTech) return text;
	return releaseName(text)
		.replace(/\b(?:409 )?release_not_ready\b/g, L.release_not_ready)
		.replace(/\b(?:409 )?release_model_not_built\b/g, L.release_model_not_built)
		.replace(/\bunknown_outcome\b/g, 'unknown outcome')
		.replace(/\bW5 slots\b/g, 'required settings')
		.replace(/\bW5 slot\b/g, 'required setting')
		.replace(/\bGT\b/g, 'themes')
		.replace(/\bGS\b/g, 'stages')
		.replace(/\bGA\b/g, 'activities')
		.replace(/\bdead letters\b/gi, (m) => (m[0] === 'D' ? 'Failed messages' : 'failed messages'))
		.replace(/\b[a-z][a-z0-9_-]*(?::[a-z][a-z0-9_-]*)+\b/g, (k) =>
			k
				.split(':')
				.map((x) => humanKey(x).toLowerCase())
				.join(' \u203a '),
		)
		.replace(
			/\b([A-Z][A-Za-z]+)\.([a-z][a-z0-9_]*)\b/g,
			(_, o: string, k: string) => `${o} \u203a ${humanKey(k).toLowerCase()}`,
		)
		.replace(/\b([a-z][a-z0-9]*(?:_[a-z0-9]+)+) = /g, (_, k: string) => `${humanKey(k)}: `)
		.replace(/\b(?:job|industry|industry_dept|department)\/[a-z0-9_]+(?:\/[a-z0-9_]+)*(?:@v\d+)?/g, (r) =>
			layerLabel(r),
		)
		.replace(/\b[a-z0-9_]+\/[a-z0-9_]+@v\d+/g, (r) => layerLabel(r))
		.replace(/\b[a-z0-9_]+@v\d+/g, (r) => layerLabel(r))
		.replace(/@v(\d+)\b/g, 'v$1')
		.replace(/\b(?:strata_[a-z]+\.[a-z_]+|policy\.[a-z_]+)\b/g, (k) => humanKey(k).toLowerCase())
		.replace(/\b[a-z]+(?:_[a-z0-9]+)+\b/g, (k) => humanKey(k).toLowerCase());
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** A user subject for people to read: a readable slug (`mara-visser`) becomes "Mara Visser", an opaque UUID becomes a
 * generic label; the raw subject is shown only with technical details on. */
export function subjectLabel(sub: string): string {
	if (prefs.showTech) return sub;
	if (UUID.test(sub)) return 'Account holder';
	return sub
		.split(/[-_.]+/)
		.filter(Boolean)
		.map(capitalise)
		.join(' ');
}
