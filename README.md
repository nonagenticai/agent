# agent

Multi-tenant AI agent platform: chat channels, connectors, knowledge providers and
licensing integration. A Rust API with a SvelteKit web console.

| Path | What |
|---|---|
| `crates/agent-api` | the API, the public channel edge and the runtime binaries — one image, five entrypoints |
| `crates/agent-model` | the pure platform model: layer keys, row kinds, canonical JSON |
| `web/` | the SvelteKit console |

## Build and test

The workspace pins its toolchain in `rust-toolchain.toml`.

```bash
cargo build --locked
cargo test --locked
cargo fmt --all -- --check && cargo clippy --all-targets --locked -- -D warnings
```

The test suite reads `TEST_DATABASE_URL` and `REDIS_URL` when they are set, and otherwise
starts throwaway containers of its own. `compose.local.yml` starts the same dependencies
locally (PostgreSQL 18 and Redis).

Web:

```bash
cd web
npm ci
npm run dev        # dev server
npm test           # vitest
npm run ci:verify  # svelte-check
```

CI runs the same commands on GitHub-hosted runners.

## Images

The API image is `Dockerfile.rust` (build with `--build-arg PACKAGE=agent-api`); it carries
`agent-api`, `agent-edge`, `orchestrator`, `runner` and `interaction`. The web image is
`web/Dockerfile`. Both default to Docker Hub base images.

## Licensing

The code in this repository is Apache-2.0 and needs no licence key to build, test or run.
The agent platform integrates with an **optional** licence gate: `LICENSE_GATE_2_ENABLED`
(default `false`) checks the stored tenant verdict at tenant-context resolution, and
enabling it also requires `LICENSE_INTROSPECT_URL`. With the flag off, no licence request is
ever made. Configured, enforcement is fail-closed: an unreachable licences service denies the
request rather than allowing it.

The licensing stack is public too — [`licenses`](https://github.com/nonagenticai/licenses) (entitlements, plans and tokens) and [`license-authz`](https://github.com/nonagenticai/license-authz) (the Envoy `ext_authz` enforcement point). Neither is required to run this code.

## License

Apache 2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
