# Agent development instructions

## ⛔ I KNOW MY CUSTOMER THAT I SERVE — owner ruling 2026-10-26

Customers are KYC'd: we know who we are talking to and hold all their order, payment, delivery and case data
when the conversation starts. The agent **never** asks a customer for an order number, order id, customer id,
postcode, email or any other lookup key. It resolves "my order / my parcel / my refund" from the customer's own
data, and when genuinely ambiguous names the candidates in plain words (product, date). Demos and acceptance
scripts use a logged-in customer and natural messages only — never typed ids. Never build a demo that asks.

## Owner direction — whole spec, one PR (2026-10-26; amended 2026-11-20)

Develop `nonagenticai/agent` until the **whole agreed application spec** is
implemented, integrated and verified. Create one new PR only after that completion
gate. A task, feature, lane or individual plan finishing is not a PR boundary.

Read [the local Rust workflow](docs/runbooks/local-rust-development.md) before
running, pushing or opening a PR.

**Amendment 2026-11-20 — local Rust compilation is retired.** `rbuild`, the m1
build server and the mini1 overflow host are gone. The pre-merge gates you run
locally are sub-second and are run by the git hooks: `cargo fmt --all -- --check`,
`cargo metadata --locked --format-version 1` and actionlint. A bare compiling
`cargo` command (`build`, `check`, `clippy`, `test`, `run`) is denied. The compile
and lint verdict is the repository's GitHub-hosted `preflight.yml`: dispatch it on
your branch with `gh workflow run preflight.yml --ref <branch>` and follow it with
`gh run watch`. On m4 only, `~/.claude/scripts/cargo-guard.py` is the route for
interactive local work; it owns the shared target directory and build slots.

Older plans, handoffs and memories that assume local Cargo builds, the local build
queue or a local compile verdict are stale on that point only; their technical
requirements and acceptance assertions still apply.

This policy applies first to this Rust application, including its web UI as part
of application acceptance. Other Rust applications may adopt the same approach;
do not change unrelated applications or shared infrastructure as part of this task.

## Working together

- Keep concurrent feature sessions in separate worktrees. One integration session
  owns the assembled application and the shared local runtime.
- Run the application and its Node tooling natively against Dockerized Postgres 18,
  Redis and other dependencies when needed. Reuse one managed local dependency
  stack; keep test databases separate from development data.
- On m4, interactive Cargo work goes through `~/.claude/scripts/cargo-guard.py`;
  never create independent build caches for every session or scratch worktree.
- Commit progress locally. The PR discipline is unchanged: implement and integrate
  the whole agreed spec before opening one PR — no per-feature or per-plan draft
  PRs, and never push or dispatch CI after every edit merely as a compiler. Use
  preflight at meaningful checkpoints, not as the next diagnostic iteration.
- Track the existing spec's remaining requirements, tests and acceptance evidence
  in its existing plans/handoffs. Do not silently narrow the completion scope or
  reintroduce explicitly deferred features.

The repository's CI workflows remain the final Linux/amd64 validation and normal
deployment path. A local pass does not prove deployment.
