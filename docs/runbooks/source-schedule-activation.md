# Runbook — activating scheduled source resync (D82)

Status: **switched off.** The scheduler (`crates/agent-api/src/knowledge/schedule.rs`, B3 T8b) is built and tested, but the
platform rule "no CronJob or scheduled job may trigger GPU calls, even indirectly" forbids running it: a resync re-embeds.

What is built (T8b):
- `SOURCE_SCHEDULE_ENABLED` (default `false`) in `src/config.rs`; `true` stops the boot with
  `source_schedule_disabled_by_policy`. `src/main.rs` calls `knowledge::schedule::spawn`, which starts nothing while
  the flag is off. `GET /api/jobs/{job}/sources[/{id}]` answers `schedule_active: false` (true only for a source with a
  schedule in a process whose ticker runs).
- The ticker is in-process (no k8s CronJob): every 60 s, a session `pg_try_advisory_lock(hashtext('agent:source-schedule'))`
  on a connection closed at the end of the tick, so one replica runs each tick and the others skip it.
- Due: the cron (five fields, UTC; DOM/DOW "either" when both are restricted, DOW 7 = Sunday) fires in a whole minute
  after the source's latest run start (any trigger), else its creation, at most 24 h back; older fires are dropped.
- Scanned: active tenants, enabled sources with a schedule, never `file` sources. A due source starts through
  `runs::start` (the `resyncSource` path), actor `System` `service:agent-api-source-schedule`, audit `source.resync`
  `{trigger: "schedule"}`; a source whose run is still running is skipped.
- Tests: `crates/agent-api/tests/b3/schedule.rs` (through the test-only `Scheduler::unguarded`).

Prerequisite (only the owner can satisfy it): a written owner ruling, recorded as a D-row, that lifts or narrows the rule for source resync
(e.g. only for tenant-BYO embed providers, or within a GPU budget per tenant).

Steps, in one agent PR + one gitops-production PR:
1. Remove the boot refusal `source_schedule_disabled_by_policy` in `src/config.rs`; replace the test
   `boot_refuses_schedule_enabled_true` with one that pins the new condition from the ruling (e.g. BYO-only).
2. If the ruling narrows the scope, gate it in `schedule.rs` (per-tenant check) with a test per allowed/refused case.
3. Web: remove the "switched off" label from the source form's `schedule` field; `schedule_active` now comes back `true`.
4. gitops-production: set `SOURCE_SCHEDULE_ENABLED=true` in `services/agent/api.yaml`.
5. Live check: a source with a schedule a few minutes ahead gets one run with audit `source.resync {trigger:"schedule"}`; GPU usage
   is metered to the tenant (usage events `funding` as for a manual resync).
6. Rollback: set the env back to `false` (the scheduler task is not spawned; no data change).
