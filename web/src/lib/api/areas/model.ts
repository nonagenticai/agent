// The `ModelApi` contract.
import type { Paged } from '$lib/api/types/core';
import type {
	ErrorStateDef,
	CreatedObject,
	NewObjectInput,
	NewOperationInput,
	FieldInput,
	ObjectCatalog,
	ObjectContext,
	ObjectDetail,
	ObjectField,
	ObjectRelation,
	ObjectModelMap,
	ObjectState,
	ObjectTransition,
	OperationEdit,
	OperationFilter,
	OperationRow,
	StateInput,
	TemplateCatalog,
	TemplateRetireResult,
	TemplateTag,
	TemplateTagDetail,
	TransitionInput,
} from '$lib/api/types/model';

export type ModelApi = {
	/** `GET /api/platform/templates` — every template layer with its tags, untagged revisions and job pins. */
	templateCatalog(): Promise<TemplateCatalog>;
	/** `GET /api/platform/templates/{layer}/v{n}` — one tag: contents, diff vs the previous tag, pins. */
	templateTagDetail(layer: string, version: number): Promise<TemplateTagDetail>;
	/**
	 * `POST /api/platform/templates/{layer}/tags` — Package step: tag the layer's untagged revisions as `@v{n+1}`.
	 * 409 `nothing_to_tag`; 422 `validation_red` while a generator run has red validation items.
	 */
	tagLayerRevisions(layer: string, note: string): Promise<TemplateTag>;
	/**
	 * `POST /api/platform/templates/{layer}/tags/v{n}/retire` — take a registered job template back: it leaves every
	 * tenant's catalogue and new jobs (422 `template_retired`); jobs that pin it keep working. 404, 409 `no_change`.
	 */
	retireTemplateTag(layer: string, version: number): Promise<TemplateRetireResult>;
	/** `POST /api/platform/templates/{layer}/tags/v{n}/restore` — offer a retired job template again. */
	restoreTemplateTag(layer: string, version: number): Promise<TemplateRetireResult>;
	/** `GET /api/platform/objects` — `object_def` rows with counts, plus the folded (not-an-object) concepts. */
	objectCatalog(): Promise<ObjectCatalog>;
	/** `POST /api/platform/objects` — add an object as an untagged template revision (super_admin). */
	createObject(input: NewObjectInput): Promise<CreatedObject>;
	/** `GET /api/platform/objects/map` — P9 object map: per object its states, operations and carrying layers, plus every relation. */
	objectModelMap(): Promise<ObjectModelMap>;
	/** `GET /api/platform/objects/{object}` — fields, relations, state machine, operations, derived errors. */
	objectDetail(object: string): Promise<ObjectDetail>;
	/**
	 * `GET /api/platform/objects/{object}/context` — where the object lives outside its own rows: template layers that
	 * define or reference it, connector templates that hold it (with their status mapping), journey activities.
	 */
	objectContext(object: string): Promise<ObjectContext>;
	/** `PUT /api/platform/objects/{object}/fields/{field}` — add or change a field (422 `invalid_field`). */
	saveObjectField(object: string, input: FieldInput): Promise<ObjectField>;
	/** `PUT /api/platform/objects/{object}/states/{status}` — add or change a state (422 `invalid_state_name`). */
	saveObjectState(object: string, input: StateInput): Promise<ObjectState>;
	/**
	 * `PUT /api/platform/objects/{object}/transitions/{verb}/{from}` — add or change a transition, keyed by
	 * (verb, from_status). 422 `unknown_state` / `invalid_verb`.
	 */
	saveObjectTransition(object: string, input: TransitionInput): Promise<ObjectTransition>;
	/** `DELETE /api/platform/objects/{object}/transitions/{verb}/{from}` — 404 `transition_not_found`. */
	deleteObjectTransition(object: string, verb: string, from_status: string): Promise<void>;
	/** F-29 `POST /api/platform/objects/{object}/fields/{field}/retire` — 404 `object_not_found`, `field_not_found`;
	 *  409 `field_retired`, `field_in_use` (`operations[]` that name it). */
	retireObjectField(object: string, field: string): Promise<ObjectField>;
	/** F-28 `PUT /api/platform/objects/{object}/relations/{predicate}/{target}` `{cardinality}` — add or change a
	 *  relation. 404 `object_not_found`, `target_not_found`; 422 `invalid_predicate`, `invalid_cardinality`. */
	saveObjectRelation(
		object: string,
		predicate: string,
		target: string,
		cardinality: ObjectRelation['cardinality'],
	): Promise<ObjectRelation>;
	/** F-28 `DELETE /api/platform/objects/{object}/relations/{predicate}/{target}` → 204. 404 `relation_not_found`. */
	deleteObjectRelation(object: string, predicate: string, target: string): Promise<void>;
	/** F-22 `PUT /api/platform/operations/{object}/{verb}?variant=` — edit one operation row (variant null = `*`).
	 *  404 `operation_not_found`; 422 `invalid_operation`, `unknown_field`, `unknown_verify_read`,
	 *  `invalid_identity_level`; 409 `builtin_operation`. */
	saveOperation(
		object: string,
		verb: string,
		variant: string | null,
		edit: OperationEdit,
	): Promise<OperationRow>;
	/** `GET /api/platform/operations` — resolved `operation_def` rows (variants merged with `*`), filtered. */
	catalogOperations(filter?: OperationFilter): Promise<Paged<OperationRow>>;
	/** `POST /api/platform/operations` — add an operation as an untagged template revision (super_admin). */
	createOperation(input: NewOperationInput): Promise<OperationRow>;
	/** `GET /api/platform/error-states` — the `error_state_def` taxonomy. */
	errorStateDefs(): Promise<ErrorStateDef[]>;
};
