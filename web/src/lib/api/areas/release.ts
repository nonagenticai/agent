// The `ReleaseApi` contract.
import { ApiError } from '$lib/api/errors';
import { blockingItem } from './readiness';
import type { Paged, Selection } from '$lib/api/types/core';
import type {
	ApplyResult,
	ApproveInput,
	ApproveResult,
	BundleViolation,
	BusinessRule,
	RuleDefinition,
	ContextSpacePatch,
	InstructionPatch,
	JobLanguagesView,
	JobSettingsView,
	JobTarget,
	MissingItem,
	OverlayLayers,
	PolicyPassage,
	RefinableObject,
	RefineResult,
	RefinementOp,
	ReleaseCompare,
	ReleaseConflict,
	ReleaseDetail,
	ReleaseRow,
	ReleaseStatus,
	RuleEvaluation,
	DesignDiff,
	TenantDesignDiff,
	DesignState,
	DraftChange,
	DraftSummary,
	PublishedChange,
	ConflictBoard,
	RollbackPreview,
} from '$lib/api/types/release';
import type { ReadinessBoard, ReadinessItem, ReleaseReadiness } from '$lib/api/types/release';

export type ReleaseApi = {
	/** `GET /api/jobs/{job}/target` — the job's selection, the options it may name, and its audit trail. */
	releaseTarget(): Promise<JobTarget>;
	/** `PUT /api/jobs/{job}/target` — edit the selection (release_manager, audited `job.selection_update`). */
	saveReleaseTarget(selection: Selection, expected_updated_at: string | null): Promise<JobTarget>;
	/** `GET /api/jobs/{job}/releases` — every release of the job, newest first, with its derived state. */
	releaseList(): Promise<Paged<ReleaseRow>>;
	/** `GET /api/jobs/{job}/releases/{id}` — immutable content, selection and provenance. */
	releaseDetail(id: string): Promise<ReleaseDetail>;
	/** `GET /api/jobs/{job}/releases/{id}/compare?with=current` — what differs from the current release. */
	releaseCompare(id: string): Promise<ReleaseCompare>;
	/** `GET /api/jobs/{job}/releases/status` — `{base_changed, approved_waiting, selection_error, missing_slots}`. */
	releaseStatus(): Promise<ReleaseStatus>;
	/** `POST /api/jobs/{job}/releases` `{"apply": bool}` — approve (cut); with apply, release in the same txn. */
	approveRelease(input: ApproveInput): Promise<ApproveResult>;
	/** `POST /api/jobs/{job}/releases/{id}/apply` — make a release current (apply, or roll back to it). */
	applyRelease(id: string): Promise<ApplyResult>;
	/** `GET /api/jobs/{job}/release-conflicts?status=open|all`. */
	releaseConflicts(status: 'open' | 'all'): Promise<Paged<ReleaseConflict>>;
	/** `POST /api/jobs/{job}/refinements/{revert|rebase}` for the conflict's row (Accept base / Keep mine). */
	resolveConflict(id: number, op: 'revert' | 'rebase'): Promise<RefineResult>;
	/** `GET /api/jobs/{job}/release-conflicts/board?status=open|all` — J16: each conflict with base before → after,
	 * the tenant's row, cause, customer impact and where its resolution is (draft / published). Any member. */
	conflictBoard(status: 'open' | 'all'): Promise<ConflictBoard>;
	/** `GET /api/jobs/{job}/{object}?layer=base` + `?layer=tenant` — both sides of one refinable object. */
	refinementLayers(object: Exclude<RefinableObject, 'job_setting'>): Promise<OverlayLayers>;
	/** `POST /api/jobs/{job}/refinements/{op}` `{object, key}` — hide / unhide / revert / rebase. Saved to the draft
	 * (D54, overrides K7's "live at once"); published by the next release. */
	refine(op: RefinementOp, object: RefinableObject, key: string): Promise<RefineResult>;
	/** `GET /api/jobs/{job}/settings` — base, override and effective preamble / actor / stance. */
	jobSettingsView(): Promise<JobSettingsView>;
	/** `PUT /api/jobs/{job}/settings` — override a setting; `null` hides the base value. */
	saveJobSettingOverride(patch: Record<string, string | null>): Promise<JobSettingsView>;
	/** `DELETE /api/jobs/{job}/settings/{key}` — back to the release's value. */
	revertJobSettingOverride(key: string): Promise<JobSettingsView>;
	/** D59 `GET /api/jobs/{job}/languages` — the languages the job serves (live vs draft). Any member. */
	jobLanguages(): Promise<JobLanguagesView>;
	/** D59 `PUT /api/jobs/{job}/languages` — set the languages (first = default). Saved to the draft (D54). */
	saveJobLanguages(languages: string[]): Promise<JobLanguagesView>;
	/** `PUT /api/jobs/{job}/context-spaces/{key}` — the tenant's version of a context space. */
	saveContextSpaceOverride(key: string, patch: ContextSpacePatch): Promise<RefineResult>;
	/** `PUT /api/jobs/{job}/instructions/{slot}` `{text, language?}` (job_designer) — the tenant's own text for the
	 * `tone`, `escalation` or `format` slot, saved to the draft (D54); any other slot is 422 `instruction_not_refinable`.
	 * Live HTTP only: the fixture world has no instruction overlay. Back to the inherited text: `refine('revert',
	 * 'instruction', slot)`. */
	saveInstruction(slot: string, patch: InstructionPatch): Promise<RefineResult>;
	/** `GET /api/jobs/{job}/release-readiness?of=release|draft` — D67: the backend's verdict on whether a release can
	 * go out: `routing_ready`, the `blocking` items a publish refuses on (409 `release_not_ready` carries the same
	 * list) and non-blocking `warnings` (Test-env routes, go-live gaps). `job` defaults to the active job; D54:
	 * `release` (default) checks the release in question (an approved cut, else the current one) against its design,
	 * `draft` the unpublished draft — "would the next cut be ready?". Any member. */
	releaseReadiness(job?: string, opts?: { of?: 'release' | 'draft' }): Promise<ReleaseReadiness>;
	/** `GET /api/jobs/{job}/readiness/board` — J3: the release checked, its apply vs go-live gaps (why, who, where),
	 * and every kind of thing the job needs (slots, routes, tests, knowledge, rules, gates, procedures, toolsets). */
	readinessBoard(opts?: { of?: 'release' | 'draft' }): Promise<ReadinessBoard>;
	/** `GET /api/jobs/{job}/rules` — business-rule slots with the tenant's values. */
	businessRules(): Promise<Paged<BusinessRule>>;
	/** `POST /api/jobs/{job}/rules` — author a validated rule in the job draft. */
	createBusinessRule(input: RuleDefinition & { id: string }): Promise<BusinessRule>;
	/** `PUT /api/jobs/{job}/rules/{id}/definition` — edit a tenant-authored rule. Template definitions are read-only. */
	saveRuleDefinition(id: string, input: RuleDefinition): Promise<BusinessRule>;
	/** `DELETE /api/jobs/{job}/rules/{id}` — remove a tenant-authored definition from the draft. */
	deleteBusinessRule(id: string): Promise<void>;
	/** `GET /api/jobs/{job}/rules/passages` — policy passages a rule may cite. */
	rulePassages(): Promise<PolicyPassage[]>;
	/** `PUT /api/jobs/{job}/rules/{id}` — set the rule's values. */
	saveBusinessRule(id: string, values: Record<string, string | null>): Promise<BusinessRule>;
	/** `PUT /api/jobs/{job}/rules/{id}/passage` — link (or unlink with null) the policy passage it cites. */
	linkRulePassage(id: string, passage_id: string | null): Promise<BusinessRule>;
	/** `POST /api/jobs/{job}/rules/{id}/evaluate` — dry run of the built-in `rule.evaluate` with these facts; `values`
	 * previews unsaved values, `as_of` evaluates on that day (default today). Stores nothing; any member may call it. */
	evaluateRule(
		id: string,
		facts: Record<string, string>,
		values?: Record<string, string | null>,
		as_of?: string,
	): Promise<RuleEvaluation>;
	/** `GET /api/jobs/{job}/draft` — D54: the job's unpublished tenant changes (saved to draft, testable in the test
	 * chat, published by the next release cut → approve → apply), newest first. Any member may read it. */
	draftSummary(): Promise<DraftSummary>;
	/** `GET /api/jobs/{job}/draft/diff` — D54: every unpublished change with its live value (what Production runs)
	 * next to its draft value (what the test chat runs), newest first. Any member may read it. */
	draftDiff(): Promise<DraftChange[]>;
	/** `GET /api/jobs/{job}/design/live` — D62: the live design = the active release's stored snapshot (a job never
	 * released: its baseline as provisioned), one line per design place. Any member may read it. */
	liveDesign(): Promise<DesignState>;
	/** `GET /api/jobs/{job}/design/draft` — D62: the draft = the working design rows (what the test chat runs). */
	draftDesign(): Promise<DesignState>;
	/** `GET /api/jobs/{job}/releases/{id}/design` — D62: the immutable snapshot one release stores (what it put or puts
	 * live). 404 `release_not_found`; 422 `bundle_invalid` when the release stores none. */
	releaseDesign(id: string): Promise<DesignState>;
	/** `GET /api/jobs/{job}/design/diff` — D62: draft vs live, from the active release's stored snapshot (never from
	 * the change history), with the unpublished changes saved at each place. Any member may read it. */
	designDiff(): Promise<DesignDiff>;
	/** `GET /api/design/tenant-diff?kinds=` — D69: tenant-wide design places (provider bindings + slots, connector
	 * mapping overrides, tenant connector templates) whose draft is not yet live in every job, with each job's live
	 * value. Any member may read it. */
	tenantDesignDiff(kinds?: string[]): Promise<TenantDesignDiff>;
	/** `DELETE /api/jobs/{job}/draft/{id}` (or `/draft` for all) — D54: throw unpublished changes away; the live value
	 * comes back at each location. A change an approved cut already carries still ships with that cut. */
	discardDraft(id: string | 'all'): Promise<DraftSummary>;
	/** `GET /api/jobs/{job}/draft/published` — D54: changes releases made live, newest first, with `published_in`
	 * (and `rolled_back` / `restored_to_draft` once a rollback took them out of Production). */
	publishedChanges(): Promise<PublishedChange[]>;
	/** `GET /api/jobs/{job}/releases/{id}/rollback-preview` — 05 §4.4: what making an earlier release current again
	 * would do, before it is done: the published design changes customers lose (or get back), selection + template
	 * changes, unpublished changes whose live value moves, and the readiness gaps apply would refuse on. Any member. */
	rollbackPreview(id: string): Promise<RollbackPreview>;
	/** `POST /api/jobs/{job}/draft/restore` `{ids | "all"}` — put published changes a rollback took out of Production
	 * back in the draft (one ledger row per place), to re-publish with the next release. job_designer. */
	restoreRolledBack(ids: string[] | 'all'): Promise<DraftSummary>;
};

/**
 * A refusal that carries the list the page shows: 409 `release_not_ready` with the `blocking` readiness items (D67:
 * the same items `releaseReadiness()` lists — body `{"error":{"code","message","blocking":[…]}}`), or 422 bundle
 * violations. The fixture builds it from `MissingItem[]`; the live client (`http/b2.ts`) from the error body
 * (`fromApiError`: `blocking` / `violations` / `errors`, the latter `compile_failed`'s per-path compile errors).
 */
export class ReleaseError extends ApiError {
	readonly blocking: ReadinessItem[];
	/** 422 `compile_failed`: the template compiler's errors (same `{path, message}` shape as violations). */
	readonly errors: BundleViolation[];
	constructor(
		status: number,
		code: string,
		message: string,
		missing: MissingItem[] = [],
		readonly violations: BundleViolation[] = [],
		extra: { blocking?: ReadinessItem[]; errors?: BundleViolation[]; details?: Record<string, unknown> } = {},
	) {
		super(status, code, message, extra.details ?? {});
		this.name = 'ReleaseError';
		this.blocking = extra.blocking ?? missing.map(blockingItem);
		this.errors = extra.errors ?? [];
	}

	/** A refusal that already carries readiness items (the API's `blocking` list, D67). */
	static fromItems(
		status: number,
		code: string,
		message: string,
		blocking: ReadinessItem[],
		violations: BundleViolation[] = [],
		errors: BundleViolation[] = [],
	): ReleaseError {
		return new ReleaseError(status, code, message, [], violations, { blocking, errors });
	}

	/** `e` as a `ReleaseError` when its body carries `blocking`, `violations` or `errors`; otherwise `e` itself. */
	static fromApiError(e: ApiError): ApiError {
		if (e instanceof ReleaseError) return e;
		const d = e.details;
		const list = <T>(k: string): T[] | null => (Array.isArray(d[k]) ? (d[k] as T[]) : null);
		const blocking = list<ReadinessItem>('blocking');
		const violations = list<BundleViolation>('violations');
		const errors = list<BundleViolation>('errors');
		if (!blocking && !violations && !errors) return e;
		return new ReleaseError(e.status, e.code, e.message, [], violations ?? [], {
			blocking: blocking ?? [],
			errors: errors ?? [],
			details: d,
		});
	}
}
