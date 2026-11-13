# Agent web — area ownership (U1)

Read this first. Every page in spec 14 §2 is in `src/lib/pages.ts` and has a placeholder route file. You own one
area: replace your placeholders, add your types/methods/fixtures. **Edit only the files listed for your area.**
Foundation files (`pages.ts`, `world.ts`, `client.ts`, `fixture-client.ts`, `fixture-helpers.ts`, `areas/core.ts`,
`types/core.ts`, `fixtures/core.ts`, `app.svelte.ts`, `auth/*`, `+layout.svelte`, `src/lib/ui/*`) are read-only; ask
the orchestrator for a change there. Copy cb files one at a time (`git -C ../../../care-bus-agents show 45ee128:web/…`).

| Area       | Your files (`src/lib/…`)                                              | Route files you own (`src/routes/…/+page.svelte`, plus new files under them)                                                                                                                                               |
| ---------- | --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| runtime    | `api/types/runtime.ts`, `api/areas/runtime.ts`, `fixtures/runtime.ts` | `admin/jobs/[job]/` (J1 root `+page.svelte` only), `…/analytics`, `…/cases`, `…/cases/[id]`, `…/operation-calls`, `…/agents`, `…/buses`, `…/tools`, `…/backends`                                                           |
| tenant     | `…/tenant.ts` ×3                                                      | `admin/` (T2 root page only), `admin/jobs/` (T1 page only), `admin/users`, `admin/usage`, `admin/audit`, `admin/knowledge/stores`, `admin/knowledge/jobs`, `admin/providers`, `admin/channels`, `admin/jobs/[job]/sources` |
| platform   | `…/platform.ts` ×3                                                    | `platform/tenants`, `platform/tenants/[id]`, `platform/signups`, `platform/layers`, `platform/compile`, `platform/business-models`, `platform/dimensions`, `platform/audit`, `platform/licensing`                          |
| model      | `…/model.ts` ×3                                                       | `platform/templates`, `platform/templates/[layer]/[v]`, `platform/objects`, `platform/objects/[object]`, `platform/operations`                                                                                             |
| connectors | `…/connectors.ts` ×3                                                  | `platform/connector-templates`, `…/[key]/[v]`, `admin/connections/systems`, `…/systems/[id]`, `admin/connections/templates`, `admin/jobs/[job]/operations`                                                                 |
| release    | `…/release.ts` ×3                                                     | `admin/jobs/[job]/refinements`, `…/release` (J14 + J16 `#conflicts` tab), `…/release/[id]`, `…/readiness`, `…/rules`                                                                                                       |
| journey    | `…/journey.ts` ×3                                                     | `platform/journeys/[industry]`, `platform/generator`, `platform/generator/[run]`, `admin/jobs/[job]/journey`, `…/journey/[activity]`, `…/validation`, `…/discovery`, `…/handover`                                          |
| ontology   | `…/ontology.ts` ×3                                                    | `admin/jobs/[job]/ontology`, `…/procedures`, `…/procedures/[name]`, `…/knowledge-kinds`, `…/gates`, `…/toolsets`, `platform/gates`, `platform/agent-roles`                                                                 |
| live       | `…/live.ts` ×3                                                        | `console`, `chat`, `demo` (and `demo/[table]`, `demo/[table]/[id]` if you add them)                                                                                                                                        |

New shared-looking code you need goes in your own route dir (e.g. `routes/platform/tenants/TenantRow.svelte`) or a
new `src/lib/<area>/` folder that only you edit. Unit tests: `tests/unit/<area>-*.test.ts`.

## Adding a client method + fixture rows

1. Shapes in `api/types/<area>.ts` (spec 14 §3 field names, snake_case).
2. Rows in `fixtures/<area>.ts`: add fields to `<Area>Seed` and return fresh objects from `<area>Seed()`. Reference
   core rows only via `TENANTS`, `JOBS`, `USERS`, `RELEASES` from `fixtures/core.ts`. Key rows by `tenant_id` (+ `job`).
3. Method in `api/areas/<area>.ts`: declare it on `<Area>Api` with a doc comment naming the future route
   (`/** \`GET /api/jobs/{job}/cases\` — … \*/`, unversioned, never `/api/v1`), implement it in `create<Area>Fixture`:
`cases: () => respond('cases', () => { const job = currentJob(world, ctx); return paged(world.runtime.cases.filter(…)); })`.
Writes: `requireTenantAdmin`/`requireSuperAdmin`first,`clone()`the input before storing, mutate`world`,
refuse with `refuse(409, 'release_not_ready', '…')`. Method names must be unique across areas (tested).
4. Pages call `api().<method>()` (`import { api } from '$lib/app.svelte'`). `?fail=<method>` → 503, `?deny=<method>`
   → 403 in the browser; tests use `fixtureConfig.fail/deny`. Latency 120–400 ms in the app, 0 under vitest.
   Never `fetch(`, `EventSource(` or Keycloak (guard test) — only `auth/session.ts` (keycloak-js) and
   `api/http-client.ts` (`/api/me`, `/api/signup-approvals*`) may. Live feeds = fake timers replaying fixture rows.

## Page skeleton and shared components (`src/lib/ui/`)

`<PageHead id="J19" sub="…">{#snippet actions()}…{/snippet}</PageHead>` — title + milestone chip from the registry.
`const res = new Resource(() => api().cases()); onMount(() => res.refresh());` then
`<LoadState resource={res} empty="No cases yet.">{#snippet children(page)}…{/snippet}</LoadState>` (loading / unavailable
with retry / empty / data). Paged lists: `<Pagination next={page.next} count={page.items.length} />` (`?before=`).
Others: `Card, Chip, Status, Kpi, Callout, Unavailable, Freshness, KeyValues, JsonView, Tabs, Seg, Meter, BarsH,
LineChart, Drawer, Toast` (`toast(msg, 'ok'|'err')`), `ReconcileStatus`, `MilestoneChip` (per-control milestone),
`StateDiagram {states, transitions, current}`, `MatrixTable {rows, columns, rowKey, colKey, rowHeader, cell}`,
`StepFlow {steps:[{key,label,status,detail,href}]}`, `TenantPicker`, `JobList`. Tables: `.tbl` markup inline (no table
component); forms: `.field/.input/.form-grid`. Writes go through `guard(() => api().x(), 'Saved.')` from `app.svelte`.

## Roles (`$lib/auth/roles`, `session` from `$lib/auth/session.svelte`)

`canWrite(session)` tenant_admin or SA-in-tenant · `canRead(session)` · `isSuperAdmin(session)` ·
`canSuperAdminInTenant(session)` (SA-only controls on tenant pages) · `writeAttrs(session)` → spread on write buttons:
`<button class="btn" {...writeAttrs(session)}>Save</button>` (disabled + title `WRITE_HINT` = "Needs the tenant_admin
role" for tenant_user). Never hide a write from a tenant_user — disable it. Platform pages are already guarded by the
layout (non-SA sees "Super-admin only", no call). Everyone signs in through Keycloak; identity and roles come from `GET /api/me`.
The in-memory fixture world and API are test support (`tests/support`, alias `$support`); `src/` never imports them.
