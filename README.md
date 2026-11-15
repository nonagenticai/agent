# agent

[![CI](https://github.com/nonagenticai/agent/actions/workflows/ci-public.yml/badge.svg)](https://github.com/nonagenticai/agent/actions/workflows/ci-public.yml)
[![Release](https://img.shields.io/github/v/release/nonagenticai/agent)](https://github.com/nonagenticai/agent/releases)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A multi-tenant AI agent platform: chat channels, connectors, knowledge providers and
licensing integration. A Rust API with a SvelteKit web console.

Tenants define jobs, knowledge, connectors and releases through the API; customers reach
those jobs over web chat, Telegram and the staff inbox. The whole platform builds from one
Rust workspace into one image with five entrypoints, so a deployment builds a single
container and scales its processes independently.

| | |
|---|---|
| **Runtime** | Rust 1.99.1 (pinned in [`rust-toolchain.toml`](rust-toolchain.toml)); Node.js >= 26 for `web/` |
| **Depends on** | PostgreSQL 18 and a Keycloak OIDC realm; Redis 8.10.1 for the Redis-backed runtime |
| **Runs on** | A container ([`Dockerfile.rust`](Dockerfile.rust), [`web/Dockerfile`](web/Dockerfile)) or a host process |

| Path | What |
|---|---|
| `crates/agent-api` | the API, the public channel edge and the runtime binaries — one image, five entrypoints |
| `crates/agent-model` | the pure platform model: layer keys, row kinds, canonical JSON |
| `web/` | the SvelteKit console |

## Quick start

```bash
git clone https://github.com/nonagenticai/agent.git
cd agent
cargo build --locked
cargo test --locked
```

The test suite reads `TEST_DATABASE_URL` and `REDIS_URL` when they are set, and otherwise
starts throwaway containers of its own, so `cargo test` needs no prepared database.

The web console:

```bash
cd web
npm ci
npm run dev        # http://127.0.0.1:3000
```

Running `agent-api` itself needs PostgreSQL 18 and a Keycloak realm; Redis is optional, and
several runtime features (the scope registry, the inbox sweeper) need it.
[`compose.local.yml`](compose.local.yml) starts dev and test instances of the first two;
it reads its passwords from the environment and refuses to start without them, so export
`AGENT_TEST_POSTGRES_PASSWORD`, `AGENT_DEV_POSTGRES_PASSWORD`, `AGENT_TEST_REDIS_PASSWORD`
and `AGENT_DEV_REDIS_PASSWORD` first. The API's own required variables are in
[Configuration](#configuration).

The image carries `agent-api`, `agent-edge`, `orchestrator`, `runner` and `interaction`,
built with `--build-arg PACKAGE=agent-api`. Both image builds default to Docker Hub base
images; `BASE_REGISTRY` points them at a mirror.

## Usage

`agent-api` listens on `0.0.0.0:8098` by default. Liveness and readiness are
unauthenticated:

```console
$ curl -s http://127.0.0.1:8098/health
{"status":"ok"}

$ curl -s http://127.0.0.1:8098/ready
{"status":"ready","db":"ok","signup":"dark"}
```

`/ready` answers `503` with `{"status":"not_ready",...}` when the database does not
answer, and reports `signup` as `on` or `dark` depending on whether early-access signup is
enabled.

Every other route is authenticated. With a token issued by your Keycloak realm:

```console
$ curl -s -H 'Authorization: Bearer <token>' http://127.0.0.1:8098/api/me
```

answers who the verified caller is and which tenant they act in. A `super_admin` may switch
with `X-Active-Tenant-Id`.

## Configuration

`agent-api` fails at startup, naming the variable, when a required one is missing or
malformed.

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | required | the application database |
| `MIGRATE_DATABASE_URL` | required | the owner role; used only to run migrations at startup, then closed |
| `REDIS_URL` | unset (Redis-backed runtime off) | `redis://host:port`, no path; setting it enables the scope registry, the inbox sweeper and design nudges |
| `AGENT_PREFIX` | empty | stream-key prefix; may not contain whitespace |
| `KEYCLOAK_ISSUER` | required | OIDC issuer |
| `KEYCLOAK_AUDIENCE` | required | expected token audience |
| `KEYCLOAK_CLIENT_ID` | equal to `KEYCLOAK_AUDIENCE` | client id used for admin calls |
| `KEYCLOAK_JWKS_URL` | `<KEYCLOAK_ISSUER>/protocol/openid-connect/certs` | key set for token verification |
| `PORT` | `8098` | listen port, when `AGENT_API_BIND` is unset |
| `AGENT_API_BIND` | `0.0.0.0:<PORT>` | full listen address; overrides `PORT` |
| `METRICS_BIND` | `0.0.0.0:9098` | Prometheus metrics listener |
| `LOG_FORMAT` | `json` | `json` or `pretty` |
| `SUPER_ADMIN_ROLE` | `super_admin` | the realm role that grants the cross-tenant switch |
| `ATTACHMENT_QUOTA_BYTES` | `1073741824` (1 GiB) | per-tenant attachment quota |
| `TURNSTILE_TIMEOUT_MS` | `5000` | web-chat Turnstile verification timeout |
| `SOURCE_SCHEDULE_ENABLED` | `false` | scheduled source re-sync; `true` is refused at startup by policy |
| `LICENSE_GATE_2_ENABLED` | `false` | the optional tenant licence gate (see [Licensing](#licensing)) |
| `LICENSE_INTROSPECT_URL` | unset | required when `LICENSE_GATE_2_ENABLED=true` |

Booleans accept `true`/`false`, `1`/`0`, `yes`/`no` and `on`/`off`.

The SvelteKit console is configured separately, at request time:

| Variable | Default | Meaning |
|---|---|---|
| `PUBLIC_KEYCLOAK_URL` | `https://auth.nonagentic.ai` | the realm's base URL |
| `PUBLIC_KEYCLOAK_REALM` | `portal` | realm name |
| `PUBLIC_KEYCLOAK_CLIENT_ID` | `agent` | browser client id |

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

All three must pass before a pull request is ready. CI runs the same commands, and runs the
test suite through [`scripts/test-lean.sh`](scripts/test-lean.sh), which builds and runs one
test target at a time so the disk holds one test binary rather than all of them.

Web:

```bash
cd web
npm run lint        # prettier --check
npm run ci:verify   # svelte-check --fail-on-warnings
npm test            # vitest
npm run build
```

CI is [`.github/workflows/ci-public.yml`](.github/workflows/ci-public.yml): secret scan, a
private-reference scan, the Rust suite against a live PostgreSQL 18 and Redis, the web
checks, and `cargo audit`. Image build, smoke test and GHCR publish run only when the
repository variable `PUBLISH_IMAGE` is `true`.

## Security

Report vulnerabilities privately through the repository's Security tab — see
[`SECURITY.md`](SECURITY.md). Do not open a public issue for a security problem.

## Contributing

Issues and pull requests are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the
ground rules, branch and commit conventions, and the local gate; this repository's own
commands are in **Development** above. Commits carry DCO sign-off (`git commit -s`).

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
