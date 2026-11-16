# Agent development instructions

## ⛔ I KNOW MY CUSTOMER THAT I SERVE — owner ruling 2026-10-26

Customers are KYC'd: we know who we are talking to and hold all their order, payment, delivery and case data
when the conversation starts. The agent **never** asks a customer for an order number, order id, customer id,
postcode, email or any other lookup key. It resolves "my order / my parcel / my refund" from the customer's own
data, and when genuinely ambiguous names the candidates in plain words (product, date). Demos and acceptance
scripts use a logged-in customer and natural messages only — never typed ids. Never build a demo that asks.

## Owner direction — 2026-10-26

Develop `nonagenticai/agent` locally until the **whole agreed application spec** is
implemented, integrated and verified. Create one new PR only after that completion
gate. A task, feature, lane or individual plan finishing is not a PR boundary.

Read [the local Rust workflow](docs/runbooks/local-rust-development.md) before
building, testing, pushing or opening a PR. This direction supersedes older
instructions in plans, handoffs and memories that forbid local Cargo, require
push-to-CI verification, or create/merge a PR per plan or checkpoint. Product
requirements, explicit deferrals and production acceptance requirements still apply.

This policy applies first to this Rust application, including its web UI as part
of application acceptance. Other Rust applications may adopt the same approach;
do not change unrelated applications or shared infrastructure as part of this task.

## Working together

- Keep concurrent feature sessions in separate worktrees. One integration session
  owns the assembled application, shared local runtime and Rust build queue.
- Use native Rust/Node processes and Docker for Postgres 18, Redis and other
  dependencies when needed. Reuse one managed local dependency stack; keep test
  databases separate from development data.
- Use one persistent Rust target directory and one compiling command at a time.
  Never create independent build caches for every session or scratch worktree.
- Commit progress locally. Do not create draft PRs, dispatch CI or push to an
  existing PR merely to get compiler feedback. Existing PRs are not authority to
  continue incremental merge cycles. Preserve their work for integration.
- Track the existing spec's remaining requirements, tests and acceptance evidence
  in its existing plans/handoffs. Do not silently narrow the completion scope or
  reintroduce explicitly deferred features.

The repository's CI workflows remain the final Linux/amd64 validation and normal
deployment path after local completion. Local success does not prove deployment.
