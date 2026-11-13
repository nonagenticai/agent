// Platform knowledge over HTTP (PK T9): the tenant's platform exclusions (T7, `routes/platform_exclusions.rs`) and the
// super_admin curator routes on the platform plane (T8, `routes/platform_knowledge.rs`). Each route is listed in
// `ROUTES` and in the method's doc comment in `areas/pk.ts`. A pack key is ONE path segment, percent-encoded
// (`industry%2Fretail`), as the Rust routes take it.
import type { Api } from '../client';
import { seg, type Req } from '../http-client';
import type { LiveMethod } from './routes';
import type { CuratorPack, CuratorStore } from '../types/ontology';

/** The answer type of `m`. */
type R<M extends keyof Api> = Awaited<ReturnType<Api[M]>>;

/** The 15 PK methods, in route-table order. */
export const PK_METHODS = [
	// T7 tenant exclusions
	'platformExclusions',
	'setPlatformExclusion',
	'removePlatformExclusion',
	// T8 platform curation
	'curatorStores',
	'createCuratorStore',
	'curatorDocuments',
	'ingestCuratorDocument',
	'deleteCuratorDocument',
	'curatorPacks',
	'registerPack',
	'publishPack',
	'retirePack',
	'curatorPackStores',
	'referencePack',
	'unreferencePack',
] as const satisfies readonly LiveMethod[];

export type PkMethod = (typeof PK_METHODS)[number];

/**
 * A strata listing as T8 passes it on: a bare array, or strata's own envelope (`{stores: [...]}`, `{packs: [...]}`).
 * T8 (`routes/platform_knowledge.rs` WIP) answers a bare array; the envelope branch is tolerance only.
 */
function listOf<T>(body: unknown, envelope: string): T[] {
	if (Array.isArray(body)) return body as T[];
	const inner = (body as Record<string, unknown> | null)?.[envelope];
	return Array.isArray(inner) ? (inner as T[]) : [];
}

export function httpPk(req: Req): Pick<Api, PkMethod> {
	return {
		// ---- T7 tenant exclusions
		platformExclusions: () => req<R<'platformExclusions'>>('GET', '/api/knowledge/platform-exclusions'),
		setPlatformExclusion: (key_prefix, reason) =>
			req<R<'setPlatformExclusion'>>('PUT', '/api/knowledge/platform-exclusions', {
				body: { key_prefix, reason },
			}),
		removePlatformExclusion: async (key_prefix) => {
			await req<unknown>('DELETE', '/api/knowledge/platform-exclusions', { query: { key_prefix } });
		},

		// ---- T8 platform curation
		curatorStores: async () =>
			listOf<CuratorStore>(await req<unknown>('GET', '/api/platform/knowledge/stores'), 'stores'),
		createCuratorStore: (input) => {
			const body: Record<string, unknown> = { name: input.name };
			if (input.parent_id) body.parent_id = input.parent_id;
			return req<R<'createCuratorStore'>>('POST', '/api/platform/knowledge/stores', { body });
		},
		curatorDocuments: (storeId) =>
			req<R<'curatorDocuments'>>('GET', `/api/platform/knowledge/stores/${seg(storeId)}/documents`),
		ingestCuratorDocument: (storeId, input) => {
			const body: Record<string, unknown> = {
				title: input.title,
				text: input.text,
				topic_keys: input.topic_keys,
				provenance: input.provenance,
			};
			if (input.source_id) body.source_id = input.source_id;
			return req<R<'ingestCuratorDocument'>>(
				'POST',
				`/api/platform/knowledge/stores/${seg(storeId)}/documents`,
				{ body },
			);
		},
		deleteCuratorDocument: async (id) => {
			await req<unknown>('DELETE', `/api/platform/knowledge/documents/${seg(id)}`);
		},
		curatorPacks: async () =>
			listOf<CuratorPack>(await req<unknown>('GET', '/api/platform/knowledge/packs'), 'packs'),
		registerPack: (input) =>
			req<R<'registerPack'>>('POST', '/api/platform/knowledge/packs', {
				body: { key: input.key, version: input.version, store_id: input.store_id },
			}),
		publishPack: (key, version) =>
			req<R<'publishPack'>>(
				'POST',
				`/api/platform/knowledge/packs/${seg(key)}/${seg(String(version))}/publish`,
			),
		retirePack: (key, version) =>
			req<R<'retirePack'>>(
				'POST',
				`/api/platform/knowledge/packs/${seg(key)}/${seg(String(version))}/retire`,
			),
		curatorPackStores: (key, version) =>
			req<R<'curatorPackStores'>>(
				'GET',
				`/api/platform/knowledge/packs/${seg(key)}/${seg(String(version))}/stores`,
			),
		referencePack: (key, version, input) =>
			req<R<'referencePack'>>(
				'POST',
				`/api/platform/knowledge/packs/${seg(key)}/${seg(String(version))}/references`,
				{
					body: { layer: input.layer, stage: input.stage, kind: input.kind },
				},
			),
		unreferencePack: (key, input) =>
			req<R<'unreferencePack'>>('DELETE', `/api/platform/knowledge/packs/${seg(key)}/references`, {
				query: { layer: input.layer, stage: input.stage, kind: input.kind },
			}),
	};
}
