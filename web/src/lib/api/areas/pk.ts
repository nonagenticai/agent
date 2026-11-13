// The `PkApi` contract.
import type { PlatformExclusion } from '$lib/api/types/tenant';
import type {
	CuratorDocumentInput,
	CuratorDocumentPage,
	CuratorPack,
	CuratorPackInput,
	CuratorPackStores,
	CuratorStore,
	CuratorStoreInput,
	PackReferenceInput,
	PackReferenceResult,
} from '$lib/api/types/ontology';

export type PkApi = {
	// ---- T7 tenant exclusions
	/** `GET /api/knowledge/platform-exclusions` — the tenant's exclusions of platform packs (`[]` when platform
	 *  knowledge is off). Any member. */
	platformExclusions(): Promise<PlatformExclusion[]>;
	/**
	 * `PUT /api/knowledge/platform-exclusions` `{key_prefix, reason}` (knowledge_manager) — exclude every pack whose
	 * key is the prefix or below it from every job of the tenant. Live on save (not a draft change). 422
	 * `invalid_key_prefix`, `invalid_reason`, `secret_rejected`; 503 `platform_knowledge_off`.
	 */
	setPlatformExclusion(key_prefix: string, reason: string): Promise<PlatformExclusion>;
	/** `DELETE /api/knowledge/platform-exclusions?key_prefix=` (knowledge_manager) — restore; 404 when there is none. */
	removePlatformExclusion(key_prefix: string): Promise<void>;

	// ---- T8 platform curation (super_admin; 503 `platform_tenant_unset` until the platform tenant is configured)
	/** `GET /api/platform/knowledge/stores` — the platform plane's store tree (strata `StoreView`s). */
	curatorStores(): Promise<CuratorStore[]>;
	/** `POST /api/platform/knowledge/stores` `{name, parent_id?}` — a store on the platform plane. */
	createCuratorStore(input: CuratorStoreInput): Promise<CuratorStore>;
	/** `GET /api/platform/knowledge/stores/{id}/documents` — a platform store's documents (strata page). */
	curatorDocuments(storeId: string): Promise<CuratorDocumentPage>;
	/**
	 * `POST /api/platform/knowledge/stores/{id}/documents` `{title, text, source_id?, topic_keys, provenance}` — ingest
	 * one text document. 422 `invalid_topic_keys` / `invalid_provenance` (no topics, a non-https source, a bad date); strata's 422 for a licence
	 * it does not allow; 409 `pack_published` inside a published pack.
	 */
	ingestCuratorDocument(storeId: string, input: CuratorDocumentInput): Promise<Record<string, unknown>>;
	/** `DELETE /api/platform/knowledge/documents/{id}` — 409 `pack_published` inside a published pack. */
	deleteCuratorDocument(id: string): Promise<void>;
	/** `GET /api/platform/knowledge/packs` — every pack the platform owns: drafts and retired included. */
	curatorPacks(): Promise<CuratorPack[]>;
	/** `POST /api/platform/knowledge/packs` `{key, version, store_id}` — register a draft pack over a store tree. */
	registerPack(input: CuratorPackInput): Promise<CuratorPack>;
	/** `POST /api/platform/knowledge/packs/{key}/{version}/publish` — immutable from now on. */
	publishPack(key: string, version: number): Promise<CuratorPack>;
	/**
	 * `POST /api/platform/knowledge/packs/{key}/{version}/retire` — no longer listed; pins stay readable. 422
	 * `pack_not_published` for a draft (strata drops a draft with its root store).
	 */
	retirePack(key: string, version: number): Promise<CuratorPack>;
	/** `GET /api/platform/knowledge/packs/{key}/{version}/stores` — the pack root's subtree (drafts and retired too). */
	curatorPackStores(key: string, version: number): Promise<CuratorPackStores>;
	/**
	 * `POST /api/platform/knowledge/packs/{key}/{version}/references` `{layer, stage, kind}` — pin a published pack on
	 * a template layer (a `knowledge_pack` row) and cut the layer family's next tag. 409 `pack_unpublished` /
	 * `pack_retired`.
	 */
	referencePack(key: string, version: number, input: PackReferenceInput): Promise<PackReferenceResult>;
	/** `DELETE /api/platform/knowledge/packs/{key}/references?layer=&stage=&kind=` — tombstone the reference + a tag. */
	unreferencePack(key: string, input: PackReferenceInput): Promise<PackReferenceResult>;
};
