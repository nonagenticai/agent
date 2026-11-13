// The `JourneyApi` contract.
import type { Paged, Sub } from '$lib/api/types/core';
import type {
	ActivityDetail,
	ActivityObject,
	Actor,
	DiscoveryState,
	GeneratorRun,
	GeneratorRunSummary,
	HandoverItem,
	HandoverHours,
	HandoverPolicyView,
	HandoverQueue,
	HandoverSlaTargets,
	HandoverState,
	IndustryJourney,
	IndustryJourneySummary,
	JobJourney,
	StageKey,
	StartGeneratorRunInput,
	GeneratorEstimate,
	GeneratorEstimateInput,
	ThemeKey,
	DraftValidation,
	ValidationCut,
	ValidationVerdict,
	RejectReason,
	ReleaseValidation,
	ValidationQuestionPatch,
	ValidationImportResult,
	DiscoveryPreview,
	DiscoveryUploadInput,
	ChannelKey,
	ChannelStatus,
	NewJourneyStage,
	NewJourneyTheme,
	OperationDraft,
	OperationExpectationKind,
	StageKnowledgeKindInput,
	UseCaseCardPatch,
} from '$lib/api/types/journey';

export type JourneyApi = {
	// ---- P8 journey designer (super_admin) ----
	/** `GET /api/platform/journeys` — industries with a journey grid, plus those not started. */
	journeyIndustries(): Promise<Paged<IndustryJourneySummary>>;
	/** `GET /api/platform/journeys/{industry}` — the grid with validation findings (11§7 rules). */
	industryJourney(industry: string, department?: string): Promise<IndustryJourney>;
	/** `PATCH /api/platform/journeys/{industry}/stages/{stage}` — label (F-45), description, synonyms, out of scope. */
	updateJourneyStage(
		industry: string,
		stage: StageKey,
		patch: { label?: string; description?: string; synonyms?: string[]; out_of_scope?: boolean },
		department?: string,
	): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/priors/{theme}/{stage}` — weight 0–1 (0 removes the row). */
	setThemeStagePrior(
		industry: string,
		theme: ThemeKey,
		stage: StageKey,
		weight: number,
		department?: string,
	): Promise<IndustryJourney>;
	/** `POST /api/platform/journeys/{industry}/activities` */
	addJourneyActivity(
		industry: string,
		input: { stage: StageKey; slug: string; label: string; actor: Actor; trigger: string },
		department?: string,
	): Promise<IndustryJourney>;
	/** `POST /api/platform/journeys/{industry}/activities/{key}/move` — re-keys `stage:activity`. */
	moveJourneyActivity(
		industry: string,
		key: string,
		stage: StageKey,
		department?: string,
	): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/activities/{key}/objects/{object}` — bind object + field subset. */
	bindActivityObject(
		industry: string,
		key: string,
		binding: ActivityObject,
		department?: string,
	): Promise<IndustryJourney>;
	/** `DELETE /api/platform/journeys/{industry}/activities/{key}/objects/{object}` */
	unbindActivityObject(
		industry: string,
		key: string,
		object: string,
		department?: string,
	): Promise<IndustryJourney>;
	/** `PATCH /api/platform/journeys/{industry}/activities/{key}` — label, trigger, actor, `also_in_stages`, `summary`,
	 * `description` (F-43; 422 `unknown_stage`). A field left out keeps what the row has; an empty summary or
	 * description clears it. */
	updateJourneyActivity(
		industry: string,
		key: string,
		patch: {
			label?: string;
			trigger?: string;
			actor?: Actor;
			also_in_stages?: StageKey[];
			summary?: string;
			description?: string;
		},
		department?: string,
	): Promise<IndustryJourney>;
	/** `DELETE /api/platform/journeys/{industry}/activities/{key}` — tombstones the activity, its card and its references (F-43); pinned jobs keep it. */
	retireJourneyActivity(industry: string, key: string, department?: string): Promise<IndustryJourney>;
	/** `POST /api/platform/journeys/{industry}/stages` — a stage at the industry, or at `base` for every industry (F-45; 409 `stage_exists`). */
	addJourneyStage(industry: string, input: NewJourneyStage, department?: string): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/stages/order` — the whole stage order (F-45; 422 `order_mismatch`). */
	reorderJourneyStages(industry: string, stages: StageKey[], department?: string): Promise<IndustryJourney>;
	/** `POST /api/platform/journeys/{industry}/themes` — F-46 (422 `invalid_weight`, 409 `theme_exists`). */
	addJourneyTheme(industry: string, input: NewJourneyTheme, department?: string): Promise<IndustryJourney>;
	/** `PATCH /api/platform/journeys/{industry}/themes/{theme}` — label, order, escalation weight 0–100 (F-46). */
	updateJourneyTheme(
		industry: string,
		theme: ThemeKey,
		patch: { label?: string; order?: number; escalation_weight?: number },
		department?: string,
	): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/stages/{stage}/knowledge-kinds/{kind}` — F-47 (D107: `stage` may be `*`; `required` only false→true lower down). */
	setStageKnowledgeKind(
		industry: string,
		stage: string,
		kind: string,
		input: StageKnowledgeKindInput,
		department?: string,
	): Promise<IndustryJourney>;
	/** `DELETE /api/platform/journeys/{industry}/stages/{stage}/knowledge-kinds/{kind}[?layer=]` — a tombstone at `layer` (F-47). */
	removeStageKnowledgeKind(
		industry: string,
		stage: string,
		kind: string,
		layer?: string,
		department?: string,
	): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/stages/{stage}/messages/{channel}` — example messages of one channel (F-48; ≤ 50, ≤ 500 characters). */
	setStageMessages(
		industry: string,
		stage: StageKey,
		channel: ChannelKey,
		utterances: string[],
		department?: string,
	): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/channels/{channel}` — this industry's channel status (F-49). */
	setJourneyChannelStatus(
		industry: string,
		channel: ChannelKey,
		status: ChannelStatus,
		department?: string,
	): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/activities/{key}/operations/{op}` — expect an operation, R or O (F-42; `{op}` is URL-encoded). */
	setActivityOperation(
		industry: string,
		key: string,
		op: string,
		expectation: OperationExpectationKind,
		department?: string,
	): Promise<IndustryJourney>;
	/** `DELETE /api/platform/journeys/{industry}/activities/{key}/operations/{op}` — 404 `operation_not_expected` (F-42; `{op}` is URL-encoded). */
	removeActivityOperation(
		industry: string,
		key: string,
		op: string,
		department?: string,
	): Promise<IndustryJourney>;
	/** `PUT /api/platform/journeys/{industry}/activities/{key}/card` — the template's use case card (F-44; untagged revision). */
	updateUseCaseCard(
		industry: string,
		key: string,
		patch: UseCaseCardPatch,
		department?: string,
	): Promise<IndustryJourney>;

	// ---- P18 generator ----
	/** `GET /api/platform/generator/runs` (super_admin) */
	generatorRuns(): Promise<Paged<GeneratorRunSummary>>;
	/** `GET /api/platform/generator/runs/{id}` (super_admin) */
	generatorRun(id: number): Promise<GeneratorRun>;
	/**
	 * `POST /api/generator/runs` — super_admin: platform or tenant scope; job_designer: a tenant-scoped run for their
	 * own tenant only (metered as that tenant's usage, D56; outputs refinements only, D8).
	 */
	startGeneratorRun(input: StartGeneratorRunInput): Promise<GeneratorRun>;
	/**
	 * `POST /api/generator/runs/estimate` — token range, funding lane and whose usage it is before a run starts
	 * (D56: usage, never a separate bill). Same access as `startGeneratorRun`.
	 */
	estimateGeneratorRun(input: GeneratorEstimateInput): Promise<GeneratorEstimate>;
	/**
	 * `POST /api/platform/generator/runs/{id}/draft` — the LLM drafts items for the current step. `hint` (F-54, at
	 * most 2000 characters, else 422 `hint_too_long`) steers the draft; it is never kept in the audit trail. A tenant
	 * run answers 409 `not_a_platform_run`.
	 */
	draftGeneratorStep(id: number, hint?: string): Promise<GeneratorRun>;
	/** `POST /api/platform/generator/runs/{id}/items/{item}/decision` — accept / merge / rename / drop. */
	decideGeneratorItem(
		id: number,
		itemId: string,
		decision: { decision: 'accepted' | 'merged' | 'renamed' | 'dropped' | 'pending'; note?: string },
	): Promise<GeneratorRun>;
	/** `POST /api/platform/generator/runs/{id}/items/{item}/proposal` — "new object" → proposal to super_admin. */
	proposeGeneratorObject(id: number, itemId: string): Promise<GeneratorRun>;
	/** `POST /api/platform/object-proposals/{run}:{item}` — super_admin approves or rejects the new object. */
	resolveGeneratorProposal(id: number, itemId: string, approve: boolean): Promise<GeneratorRun>;
	/** `PUT /api/platform/generator/runs/{id}/stages/{stage}` — mark a drafted stage out of scope (rule 4). */
	setGeneratorStageScope(id: number, stage: StageKey, outOfScope: boolean): Promise<GeneratorRun>;
	/** `POST /api/platform/generator/runs/{id}/advance` — next step; refuses with undecided drafts or red items. */
	advanceGeneratorRun(id: number): Promise<GeneratorRun>;
	/**
	 * `POST /api/platform/generator/runs/{id}/items/{item}/operation-proposal` — super_admin (F-23): propose a new
	 * operation from an Operations-step item; checked by the operation validator (422 `operation_invalid`, 409
	 * `operation_exists`). Approve it with `resolveGeneratorProposal`.
	 */
	proposeGeneratorOperation(id: number, itemId: string, operation: OperationDraft): Promise<GeneratorRun>;
	/**
	 * `POST /api/platform/generator/runs/{id}/evidence` — super_admin (F-53, platform runs only): a ticket export
	 * checked against the run's journey; uncovered clusters become rule 7 information on Validate. 409
	 * `gates_provider_missing` without the platform tenant's gates binding.
	 */
	uploadGeneratorEvidence(id: number, input: DiscoveryUploadInput): Promise<GeneratorRun>;
	/** `GET /api/jobs/{job}/generator/runs/{id}` — any member: this job's tenant-driven run (D97); 404 `run_not_found` otherwise. */
	jobGeneratorRun(id: number): Promise<GeneratorRun>;
	/** `POST /api/jobs/{job}/generator/runs/{id}/draft` — job_designer (D97): drafts with the tenant's own `generator` provider. */
	draftJobGeneratorStep(id: number, hint?: string): Promise<GeneratorRun>;
	/** `POST /api/jobs/{job}/generator/runs/{id}/items/{item}/decision` — job_designer (D97). */
	decideJobGeneratorItem(
		id: number,
		itemId: string,
		decision: { decision: 'accepted' | 'merged' | 'renamed' | 'dropped' | 'pending'; note?: string },
	): Promise<GeneratorRun>;
	/** `PUT /api/jobs/{job}/generator/runs/{id}/stages/{stage}` — job_designer (D97). */
	setJobGeneratorStageScope(id: number, stage: StageKey, outOfScope: boolean): Promise<GeneratorRun>;
	/** `POST /api/jobs/{job}/generator/runs/{id}/advance` — job_designer (D97); writes nothing. */
	advanceJobGeneratorRun(id: number): Promise<GeneratorRun>;
	/** `POST /api/jobs/{job}/generator/runs/{id}/package` — job_designer (D97): saves refinements to THIS job's draft, never live. */
	packageJobGeneratorRun(id: number): Promise<GeneratorRun>;
	/** `POST /api/platform/generator/runs/{id}/tag` — Package: tag `@vN` (platform) or save refinements (tenant). */
	tagGeneratorRun(id: number): Promise<GeneratorRun>;

	// ---- J4 / J5 job journey ----
	/** `GET /api/jobs/{job}/journey` — effective grid with tenant refinements. */
	jobJourney(): Promise<JobJourney>;
	/** `GET /api/jobs/{job}/journey/activities/{key}` — the use case behind one activity. */
	jobActivity(key: string): Promise<ActivityDetail>;
	/** `POST /api/jobs/{job}/refinements/journey/hide` — tombstone a template activity. */
	hideJobActivity(key: string): Promise<JobJourney>;
	/** `POST /api/jobs/{job}/refinements/journey/restore` — undo a hide or a relabel. */
	restoreJobActivity(key: string): Promise<JobJourney>;
	/** `POST /api/jobs/{job}/refinements/journey/extensions` — add a tenant extension activity. */
	addJobExtensionActivity(input: {
		stage: StageKey;
		slug: string;
		label: string;
		actor: Actor;
		trigger: string;
	}): Promise<JobJourney>;
	/** `DELETE /api/jobs/{job}/refinements/journey/extensions/{key}` */
	removeJobExtensionActivity(key: string): Promise<JobJourney>;
	/** `PUT /api/jobs/{job}/refinements/journey/labels/{key}` */
	relabelJobActivity(key: string, label: string): Promise<JobJourney>;
	/** `PUT /api/jobs/{job}/refinements/journey/cards/{key}` — job_designer: refine the activity's use case card, a draft (F-44; 422 `scenarios_not_refinable`, `null` drops a field). */
	refineJobUseCaseCard(key: string, patch: UseCaseCardPatch): Promise<JobJourney>;
	/** `DELETE /api/jobs/{job}/refinements/journey/cards/{key}` — job_designer: back to the template's card (409 `not_refined`). */
	resetJobUseCaseCard(key: string): Promise<JobJourney>;

	// ---- J17 validation grid ----
	/** `GET /api/jobs/{job}/validation` — knowledge Q/A grid per release cut, newest first. */
	validationCuts(): Promise<Paged<ValidationCut>>;
	/** `POST /api/jobs/{job}/validation/{release}/run` — answer every question on that cut, LLM-as-judge verdicts. */
	runValidationCut(releaseId: string): Promise<ValidationCut>;
	/** `PUT /api/jobs/{job}/validation/{release}/rows/{row}` — a person marks the verdict. */
	setValidationVerdict(
		releaseId: string,
		rowId: string,
		verdict: ValidationVerdict,
		reason: RejectReason,
	): Promise<ValidationCut>;
	/** `POST /api/jobs/{job}/validation/questions` — a question every later cut is graded on. */
	addValidationQuestion(input: {
		question: string;
		expected: string;
		reference: string;
		activity: string;
	}): Promise<Paged<ValidationCut>>;
	/** `GET /api/jobs/{job}/releases/{id}/validation` — one cut and its latest grid run (`grid`, null until one ran). */
	releaseValidation(releaseId: string): Promise<ReleaseValidation>;
	/** `PATCH /api/jobs/{job}/validation/questions/{id}` — job_designer: edit a question (F-50); earlier cuts keep the old text. */
	updateValidationQuestion(id: string, patch: ValidationQuestionPatch): Promise<Paged<ValidationCut>>;
	/** `DELETE /api/jobs/{job}/validation/questions/{id}` — job_designer: soft delete; earlier cuts keep it. */
	deleteValidationQuestion(id: string): Promise<void>;
	/** `POST /api/jobs/{job}/validation/questions/import` — job_designer: a CSV (`question,expected,reference[,activity]`, 4 MiB), all or nothing. */
	importValidationQuestions(file: File | string): Promise<ValidationImportResult>;
	/** `GET /api/jobs/{job}/validation/draft` — D54 TEST step: every test scenario graded on live (Production) and on
	 *  the draft (Test environment), from the latest draft test run; regressions name the draft change behind them. */
	draftValidation(): Promise<DraftValidation>;
	/** `POST /api/jobs/{job}/validation/draft/run` — run every scenario against the current draft on the Test
	 *  environment; records a test run (env=test, draft rev + fingerprint). */
	runDraftValidation(): Promise<DraftValidation>;

	// ---- J18 discovery ----
	/** `GET /api/jobs/{job}/discovery` — imports, coverage, clusters, shadow mode, tenant generator runs. */
	discoveryState(): Promise<DiscoveryState>;
	/** `POST /api/jobs/{job}/discovery/imports` — upload a ticket/chat export; GA classifies each row. */
	uploadDiscoveryImport(input: DiscoveryUploadInput): Promise<DiscoveryState>;
	/** `POST /api/jobs/{job}/discovery/imports/preview` — job_designer: the export's columns and 5 sample rows (F-52); stores nothing. */
	previewDiscoveryImport(file: File | string): Promise<DiscoveryPreview>;
	/** `DELETE /api/jobs/{job}/discovery/imports/{id}` — job_designer: drop an import (stops its classifying). */
	deleteDiscoveryImport(id: string): Promise<DiscoveryState>;
	/** `PUT /api/jobs/{job}/discovery/scorecard/{activity}` `{weight}` — job_designer: an integer 0..1000 (F-51; 100 = default). */
	setScorecardWeight(activity: string, weight: number): Promise<DiscoveryState>;
	/** `POST /api/jobs/{job}/discovery/shadow` — handover-only routes, live traffic labelled. */
	startShadowMode(env: 'test' | 'prod'): Promise<DiscoveryState>;
	/** `DELETE /api/jobs/{job}/discovery/shadow` */
	stopShadowMode(): Promise<DiscoveryState>;
	/**
	 * `POST /api/jobs/{job}/discovery/proposals/{id}/accept` — D54: writes the proposal's change to the job's DRAFT
	 * (never live); it reaches Production with the next published release. job_designer.
	 */
	acceptDiscoveryProposal(id: string): Promise<DiscoveryState>;
	/** `POST /api/jobs/{job}/discovery/proposals/{id}/reject` `{reason}` — dismissed, nothing changes. job_designer. */
	rejectDiscoveryProposal(id: string, reason: string): Promise<DiscoveryState>;

	// ---- J21 handover queue (D64) ----
	// Operational (live data, never draft). D88 §R.1: Support agent gives feedback (the owner actions live on /support); Support manager owns the SLA policy (saved to draft, D54); the proposal draft is Job
	// designer. Reads: any tenant member (observers included).
	/**
	 * `GET /api/jobs/{job}/handover?state=&assignee=` — the job's queue: open items first, soonest due first, then
	 * closed ones newest first; plus who items can be assigned to and the clock SLA state is read against.
	 */
	handoverQueue(opts?: { state?: HandoverState | 'open'; assignee?: Sub | 'none' }): Promise<HandoverQueue>;
	/** `GET /api/jobs/{job}/handover/policy` — the SLA policy Production runs, next to the draft. */
	handoverPolicy(): Promise<HandoverPolicyView>;
	/**
	 * `PUT /api/jobs/{job}/handover/policy` `{targets, hours?}` — D54: saved to the job's DRAFT (ledger
	 * `handover_policy`), tried in the test chat, live with the next release. Live items keep the due times they were
	 * stamped with. support_manager. F-14 (T16a): `hours` absent keeps the stored business hours, `null` = always
	 * staffed; a bad `hours` is 422 `invalid_policy` with `details.field = "hours"`.
	 */
	saveHandoverPolicy(targets: HandoverSlaTargets, hours?: HandoverHours | null): Promise<HandoverPolicyView>;
	/**
	 * `POST /api/jobs/{job}/handover/{id}/feedback` (concept) — the person's verdict: "the agent should have handled this" attaches a
	 * design proposal; "right to hand over" records that the handover was correct. Not a design change by itself.
	 */
	handoverFeedback(
		id: string,
		verdict: 'agent_should_handle' | 'right_to_hand_over',
		note: string,
	): Promise<HandoverItem>;
	/**
	 * `POST /api/jobs/{job}/handover/{id}/feedback/draft` (concept) — save a journey proposal (extension / enable activity) to the
	 * job's draft (D54: saved to draft → test → publish). Knowledge and gate proposals are made on their own pages.
	 */
	draftHandoverProposal(id: string): Promise<HandoverItem>;
};
