# Local Rust development and final PR

Owner direction, 2026-10-26. Applies first to `nonagenticai/agent`. Other Rust
applications may adopt this pattern with their own paths, resources and dependency
configuration; this is not a change to non-Rust applications' development policies.

## Authority and scope

Implement the whole currently agreed application spec locally before creating a
new PR. The existing spec, accepted amendments and explicit owner deferrals define
scope across all concurrent lanes. Finishing one plan or feature does not satisfy
this gate. Record any unresolved requirement or acceptance blocker honestly; do not
rename an incomplete subset as the whole spec.

This instruction supersedes older local-Cargo bans, CI-only Rust verification,
per-plan PRs and push/merge checkpoints in the design set, implementation plans,
handoffs and Claude memories. Those checkpoints now require equivalent local
validation and recorded results. Keep their technical dependencies and acceptance
assertions. It does not authorize production changes or removal of CI gates.

## Local environment

- Compile and run Rust natively on the Mac. Run the agent web development tools
  natively too. Pin Rust to CI's toolchain (currently 1.99.1), use `Cargo.lock` with
  `--locked`, and use the Node version selected by the web workflow. The installed
  Homebrew Rust version is not evidence that the matching toolchain is installed.
- Use one managed Docker dependency stack for Postgres 18, Redis (matching CI's
  configured version) and other services needed by the implemented feature. Keep
  source code and the Rust compiler on the host; rebuilding application images is
  not the edit/test loop. Use native ARM dependency images on Apple Silicon.
- Discover running containers, health and occupied ports before creating anything.
  Bind local dependencies to loopback; keep internal/external port numbers equal
  when configuring them, and never use port 8000. Do not replace another session's
  containers or invent a Compose command before a Compose file exists.
- Keep development data and test data separate. `TEST_DATABASE_URL` is the
  harness's dedicated, disposable test Postgres server URL; the harness creates a
  fresh database per test. Never point it at production or a development database
  containing valuable data. Use `REDIS_URL` for a dedicated test Redis instance,
  isolated from the running application's Redis data.
- Give each service its own database and role. Keep credentials out of tracked
  documentation and use ignored local environment files for local credentials.
- Configure adequate shared memory on the Postgres container (start with 512 MiB
  and measure). Shared-memory exhaustion is distinct from host disk exhaustion.
  Bound database-test concurrency initially to four threads and adjust from
  measurements. Never make missing dependencies turn tests into silent passes.

This is the development policy, not a claim that a new Compose stack, toolchain or
environment file has already been provisioned. Configure and health-check the
local environment before relying on it for acceptance.

## Shared Rust build resources

One integration session owns the build queue. Feature sessions submit committed
SHAs and focused test requests. Do not edit, switch or integrate the source being
validated while a build/test command is running. Record the source SHA with the
result. Keep source worktrees separate even though dependencies and build storage
are shared.

For this machine, run every compiling Cargo command through the build guard:

```sh
~/.claude/scripts/cargo-guard.py test --workspace --locked -- --test-threads=4
```

Run from the agent checkout or one of its verified Git worktrees. The command
assumes the matching toolchain and dedicated test dependency URLs are configured.
Use the same guard for `check`, `clippy`, `build` or `run`; it owns the target
directory (`<repo>/target/claude`), the machine-wide build slots, the free-disk
floor, the job count and the cache settings. Do not pass `--target-dir`, a
different `CARGO_TARGET_DIR`, an alternate config file or a scratch path; the hook
and the guard both refuse them. `cargo fmt --all -- --check` does not compile and
needs no guard.

This is the only local Cargo route, for every Rust repository on this estate
(owner, 2026-11-09). There is no agent-specific prefix: the older explicit-env
form is retired and the PreToolUse hook denies it. Do not route around a denial
with shell wrappers or alternative tools. `target/local/` remains the local
dependency-stack state directory (for example `dependencies.env`); it is not a
build cache.

Before each compiling command, check host free space, shared target size and Docker
disk usage. Start with a 60 GiB budget for `target/claude` and keep at least 100 GiB
of host space free. These are operational stop thresholds, not an automatically
enforced filesystem quota. If either threshold is exceeded, pause new builds and
have the integration session reclaim only its own inactive artifacts. Do not run
global Docker prune or delete another session's targets, containers or volumes.
Do not run `cargo clean` between routine checks; retain the cache for reuse.

## Static checks before the build queue

Run `scripts/static/gate.sh` in your worktree before submitting a SHA. It compiles
nothing and takes under a minute: rustfmt, the ast-grep rules in
`scripts/static/ast-grep/rules/`, and `scripts/static/sqlcheck.py`, which applies the
migrations to a scratch database and `PREPARE`s every literal `sqlx::query*` string
(wrong table, column or syntax fails; parameter-inference type errors are warnings).
Known intentional failures live in `scripts/static/sqlcheck-allow.txt`; regenerate
with `--update-allowlist` only after reading each new entry. Passing the gate is not
compile or test evidence.

- Tools: `brew install ast-grep`, `uv`. The CI toolchain 1.99.1 lives in rustup
  (keg-only `brew install rustup`, no default set), so it never changes what
  `cargo`/`rustc` resolve to; the gate uses its rustfmt when present.
- Database: `agent_sqlcheck` / `agent_sqlcheck_demo`, owned by role `agent_sqlcheck`
  on the dev Postgres (`127.0.0.1:5433`), URL in `~/.config/agent-sqlcheck/env`. Its
  `public` schema is recreated whenever the migration set changes; never point
  `AGENT_SQLCHECK_URL` at a database holding data, nor at the test server.
- Editor diagnostics: the rust-analyzer LSP plugin runs through
  `~/.local/bin/rust-analyzer`, which serves agent worktrees only, uses 1.99.1,
  builds proc-macros into the one shared `target/ra` (never the build target
  `target/claude`), has
  checkOnSave off, and allows one instance machine-wide. A session that finds the
  slot taken gets no LSP; it does not queue.

## Validation during implementation

Use focused tests while editing, then integrate completed commits continuously.
After shared schema, seed, fixture or interface changes, include the affected
downstream suites. Before final PR creation, run the assembled application's full
local checks rather than relying on the individual branches' results:

- Rust formatting, workspace Clippy on all targets with warnings denied, and
  `cargo test --workspace --locked -- --test-threads=4` through the build guard.
- Web checks in `web/`: `npm ci`, `npm run format:check`, `npm run ci:verify`,
  `npm run test` and `npm run build`; execute the applicable Playwright acceptance
  tests with isolated available ports.
- The spec's integration, migration, seed/golden drift and end-to-end acceptance
  cases, including required ignored/manual acceptance tests. Name what was run;
  default Cargo success does not cover ignored tests or external integrations.
- Relevant security and image/entrypoint checks from the CI workflows. Perform
  final image validation as needed; native Mac tests do not prove Linux/amd64
  image compatibility.

Report failures and infrastructure blockers directly. Static compile review is
useful but does not replace compiling and running the tests. Fix failures before
declaring local completion; do not use CI as the next diagnostic iteration.

## Integration, backups and PR gate

1. Keep frequent local commits in feature worktrees. One integration owner combines
   completed commits into an integration branch and coordinates migration numbers,
   generated fixtures and shared interfaces. Never reset or overwrite a peer's work.
2. Branch backups without an associated PR are allowed when their push will not
   trigger full CI. Check workflow triggers first. Pushing an existing PR triggers
   CI; do not use it as a backup destination during this local completion phase.
3. Keep existing PRs and branches as references until their changes are safely
   accounted for. This direction is not an instruction to close, merge or delete
   existing PRs, or to change repository/runner settings.
4. Create one new application PR only when **all in-scope spec requirements** are
   implemented in the assembled tree, cross-lane integration is complete, full local
   validation passes, and required acceptance/demo evidence is recorded. An
   unresolved in-scope acceptance blocker means the PR gate is not met. Preserve
   existing owner approval gates for production-only acceptance; do not perform a
   production deployment just to satisfy local completion.
5. Run the normal CI once the completed PR is created. Fix any platform-specific
   failures on that same PR. All required checks must pass on its latest head
   before merging; retain the normal image/deployment verification after merge.

Do not open a separate PR for this workflow documentation update. Include it in
the final application integration. Do not bypass hooks or disable CI requirements.

## Interactive platform stack

`scripts/local/platform-stack.sh` runs the real platform on this Mac. It never runs Cargo: the integration owner supplies a
frozen `--bin <frozen bin dir>` (agent-api agent-edge orchestrator runner interaction; demo-backends optional, see below). The platform pre-builds no
tenant, job, procedure or demo data; the journey is the tenant admin's.

- `up --bin <frozen bin dir> [--real-llm]` boots only the platform (migrations, no seed) plus the fakes (IdP, LLM, Strata,
  Keycloak/licences admin), demo-backends (an integration target only) and the web dev server. `--real-llm` uses
  `PS_REAL_LLM_URL`/`PS_REAL_LLM_MODEL` and checks it first. There is no pgx tunnel: the host firewall rejects SSH forwards
  to the vLLM, which is reachable only through the licensed inference-router; real-model acceptance runs post-deploy there. `onboard` runs steps 1-5 below over the API and prints the tenant admin's
  and staff agent's login cookies; `smoke [--bin <frozen bin dir>]` runs all six headless (`platform-stack-smoke.mjs`);
  `status`, `logs <svc> [-f]`, `down` (own pids only), `reset` (drops only its two databases).
- Ports (checked free, never 8000): web 3100, api 8198/9198, edge 8199/9199, demo-backends 8197, IdP 9452, LLM 9453,
  Strata 9454/9456, licences/Keycloak admin 9455. Databases `agent_platform_stack`, `agent_platform_demo`.
- Logins: fake IdP cookie `b8_persona` (`sa` platform admin). A tenant's users are real identities the product made; `onboard`
  prints their `b8_persona` + `b8_sub` + `b8_email` cookies. The customer needs no login (known customer via the web chat
  demo picker, never a typed id).
- Journey (tree fd7543d6; database EMPTY after migrations, NO seed): (0) `sa` authors everything through product routes: layers
  (the `industry_dept` layer carries `areas`, which become the release target's options), channel dimension value, axis +
  profile (`PUT /api/profiles`), two context spaces, job-template tags, platform knowledge store + pack, and the connector
  template `demo_retail@1` (`POST /api/platform/connector-templates` with ops, then `.../publish`; T1 replays the fixtures; no web
  page creates one: printed as a product gap); (1) a customer signs up through the intake door and `sa` approves it
  (`POST /api/signup-approvals/{id}/approve`): the tenant, its tenant_admin identity and seat are made, and provisioning MINTS the
  tenant's inference-router / strata-rag / strata-ingest bundle tokens from the fake licences (`BUNDLE_MINTER_*` in api and
  daemon env; kc-fake-idp.mjs serves `POST /internal/tenants/{id}/bundles/{key}/ensure` and `.../bundle-token`; the fake LLM and
  fake strata verify them via `ps-bundle.mjs`, and refuse a bogus token as the product's smoke requires); `ta` sets the company
  profile and creates a job; (2) bind platform knowledge, upload own documents, author a procedure; (3) connect `demo_retail`
  systems pointed at demo-backends (the tenant's EXTERNAL REST system; it seeds its own partition via its `/api/admin/reset`; taken
  from `bin-1650` when `--bin` lacks it), route the operations, contract tests; (4) customer web chat binding (D130: starts in env
  test) + identity secret, invite a staff agent; (5) test chat (staff and a signed-assertion customer on the TEST binding), clear
  readiness, pick area/channel from the target options, cut and apply the release; (6) `POST .../channel-bindings/{id}/publish`
  (409 `no_release` without an applied release), live customer chat, handover, staff `Take` and reply. A blocker the product cannot
  resolve prints `PRODUCT GAP: <code>`. `PS_WEB_DIR` selects the web checkout (needs its own `node_modules`; the script never installs). Example: `PS_WEB_DIR=$PWD/.worktrees/ps-web/web scripts/local/platform-stack.sh up --bin <frozen bin dir>` then `scripts/local/platform-stack.sh smoke`; `ps-web` is a detached worktree at the frozen commit with `node_modules` symlinked.
- No tenant lives in env, and `up` writes none. **LOCAL-HARNESS ONLY (not product behaviour):** right after signup the smoke runs
  `platform-stack.sh harness-tenant <uuid>` ONCE: only `CONNECTOR_PRIVATE_TARGETS` (loopback demo-backends and the fake LLM, tenant
  listed; netguard unchanged) is written, then api, orchestrator, runner and interaction restart. No strata token pair and no
  per-tenant LLM binding exist any more: the static `STRATA_*_TOKENS` pair is the platform tenant's alone. `--real-llm` cannot
  pass provisioning (the real model does not refuse the negative-control token).
- State and logs: `target/local/platform-stack/` (`*.pid`, `*.log`, `journey.json`).
