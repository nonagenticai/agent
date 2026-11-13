// J11 Ontology — pure helpers for "what is this entity for" and "what did this tenant add": the layer stack summary
// on top of the page and the "Used by" section of the entity drawer. Owned by J11.
import type { ContextSpace, OntologyEntity, OntologyRelation } from '$lib/api/types/ontology';
import { OPERATION_CATALOGUE } from './catalogue';
import type { CatalogueOperation } from '$lib/api/types/ontology';
import { laneOf, title, type Lane } from './layout';

export interface LayerBand {
	lane: Lane;
	title: string;
	/** Industry / department this band specialises for, e.g. `Retail × Customer Service`; null for base/tenant. */
	scope: string | null;
	/** The template version the rows come from (`base@v3`), or the newest refinement for the tenant band. */
	layer: string | null;
	entities: number;
	/** Inherited bands: read-only for the tenant. */
	locked: boolean;
}

export interface TenantChanges {
	entities: number;
	/** Attributes the tenant added on an inherited entity. */
	attributes: number;
	relations: number;
	hidden: number;
	/** Tenant attributes whose base parent changed (Release → Conflicts). */
	conflicts: number;
}

const one = (vals: (string | null)[]): string | null => {
	const set = [...new Set(vals.filter((v): v is string => !!v))];
	return set.length === 1 ? title(set[0]!) : null;
};

/** The three inherited bands then the tenant band, counting visible (not hidden) entities. */
export function layerBands(entities: OntologyEntity[]): LayerBand[] {
	const lanes: [Lane, string][] = [
		['generic', 'Base'],
		['industry', 'Industry'],
		['industry_dept', 'Specialized'],
		['tenant', 'Added by you'],
	];
	return lanes.map(([lane, label]) => {
		const rows = entities.filter((e) => laneOf(e) === lane);
		const visible = rows.filter((e) => !e.hidden);
		const layers = [...new Set(rows.map((e) => e.layer))];
		const ind = one(rows.map((e) => e.industry));
		const dep = one(rows.map((e) => e.department));
		return {
			lane,
			title: label,
			scope: lane === 'industry' ? ind : lane === 'industry_dept' && ind && dep ? `${ind} × ${dep}` : null,
			layer: layers.length === 1 ? layers[0]! : lane === 'tenant' ? (layers.sort().at(-1) ?? null) : null,
			entities: visible.length,
			locked: lane !== 'tenant',
		};
	});
}

export function tenantChanges(entities: OntologyEntity[], relations: OntologyRelation[]): TenantChanges {
	const inherited = entities.filter((e) => e.origin !== 'tenant');
	return {
		entities: entities.filter((e) => e.origin === 'tenant').length,
		attributes: inherited.flatMap((e) => e.attributes).filter((a) => a.origin === 'tenant').length,
		relations: relations.filter((r) => r.origin === 'tenant').length,
		hidden: entities.filter((e) => e.hidden).length,
		conflicts: entities.flatMap((e) => e.attributes).filter((a) => a.conflict).length,
	};
}

export const changeCount = (c: TenantChanges) => c.entities + c.attributes + c.relations + c.hidden;

export interface EntityUsage {
	/** Context spaces (activities) that put this entity in front of the agent. */
	spaces: ContextSpace[];
	/** Slots the agent must collect from this entity's attributes before it acts (union over the spaces). */
	required: string[];
	/** Distinct procedures those spaces run. */
	procedures: string[];
	/** Platform operations on the object this entity is a view of (what the agent can do with it). */
	operations: CatalogueOperation[];
}

export function usageOf(entity: OntologyEntity, spaces: ContextSpace[]): EntityUsage {
	const using = spaces.filter((s) => s.entities.includes(entity.name));
	const attrs = new Set(entity.attributes.map((a) => a.name));
	const required = [...new Set(using.flatMap((s) => s.required).filter((r) => attrs.has(r)))];
	const procedures = [...new Set(using.map((s) => s.procedure).filter(Boolean))];
	const operations = entity.object_key
		? OPERATION_CATALOGUE.filter((o) => o.op.split('.')[0] === entity.object_key)
		: [];
	return { spaces: using, required, procedures, operations };
}

/** Entity names no context space lists: the agent never brings them into a conversation. */
export function unusedEntities(entities: OntologyEntity[], spaces: ContextSpace[]): string[] {
	const used = new Set(spaces.flatMap((s) => s.entities));
	return entities.filter((e) => !e.hidden && !used.has(e.name)).map((e) => e.name);
}

/** Distinct platform objects the visible entities are views of. */
export function platformObjects(entities: OntologyEntity[]): string[] {
	return [...new Set(entities.filter((e) => !e.hidden && e.object_key).map((e) => e.object_key!))].sort();
}
