# Acceptance: tenant admin configures, tests and publishes a web chat channel

Real local stack, real browser, no mocks. Spec: `web/tests/acceptance/tenant-admin-channels.spec.ts`, config
`web/playwright.acceptance.config.ts`. Runs on the platform-stack e2e harness.

The journey (ten serial tests): admin signs in and creates a job from a template, configures a web chat channel (allowed
origin, known-customer identity, photos and PDFs for customers and staff), channel is in TEST, the admin tries it as a
known customer from the console (types the customer reference of the tenant's system, "Try as this customer"; core signs
the proof, the hosted test page opens as that customer), the customer picks a photo before the first message and writes one
sentence (the photo follows by itself), staff see the photo and reply with a PDF, the customer sees the PDF, publish is
refused without a release, the admin approves and goes live and publishes, the customer tab reloads and the next message
runs in Production, the admin switches customer attachments off (the open tab is refused in plain words and loses its
paperclip at once, a new session has none), and an EICAR PDF is refused as malware. KYC: the customer types only natural
sentences; the customer reference is typed by the admin (staff), never asked of the customer.

## Run

The harness (`scripts/local/web-e2e.sh`) boots the platform-stack instance (shifted ports from `PORT_BASE`, never 8000),
runs the platform-stack smoke (tenant, offering, tenant admin, support agent, customers: `journey.json`), and runs
Playwright under the browser lease with `PS_E2E_*` set. The spec builds no tenant, offering or users of its own.

```sh
cd /Users/georgedekker/Movies/nonagenticai/agent/.worktrees/seed-removal
PORT_BASE=700 PS_DB_SUFFIX=_creation PS_LOCAL_DIR=<state dir> \
  scripts/local/web-e2e.sh --reuse -c playwright.acceptance.config.ts      # add --bin <dir> when the instance is down
```

Preconditions: managed dependency stack up (`$MAIN/target/local/dependencies.env`, Postgres 5433, Redis 6380), `cd web && npm ci`,
Chromium installed, frozen binaries from the build queue (this runbook never runs Cargo). `--reuse` keeps the last journey;
without it a fresh one is built.

The spec's own data: step 1 always creates a NEW job (the smoke's job is already released; step 7 needs one with no release)
and staffs it with the journey's support agent. Identities are the journey's `admin` and `agent`. Overrides: `TAC_TENANT`,
`TAC_SITE_ORIGIN`, `TAC_EDGE_URL`, `TAC_ARTIFACTS_URL`, `TAC_OWNER_URL`. Steps depend on the earlier ones; rerun from step 1
(`-g "^1 "` etc. only with the earlier state alive). The owner DB URL is exported as `PS_E2E_OWNER_URL` and is never printed.

## 5. What the product cannot do yet, and what the spec works around

- **Known-customer test is TEST-only by design.** `POST /api/jobs/{job}/channel-bindings/{id}/test-session` (console
  "Try as this customer", `webchat-customer-identity.md`) signs the proof for a TEST binding in HS256 mode; a published
  channel is refused (409 `binding_live`) and the console hides the field. Publishing also ends the open test session
  (D130), so after step 7 the same tab's next message opens a Production session as an anonymous visitor (step 8 checks
  `prod/` and that it is not `assertion`). A known customer in Production needs the tenant's own site signing the proof;
  that is covered by `crates/agent-api/tests/webchat_sessions.rs`, not by this walk. Steps 9 and 10 unpublish first so the
  admin can open fresh known-customer sessions.
- **The photo goes in before the first message.** The paperclip works as soon as the chat is open; a file picked before the
  conversation exists waits as a chip, the typed first message goes out, then the file uploads and follows as a message of
  its own (`(photo)`), so there is no second-message workaround. A photo alone cannot start a chat.
- **Customer-attachments OFF reaches an open tab at its first refusal.** An upload answered 422 `attachments_off` hides the
  paperclip at once and stores the flag in the tab's session; a reload reads the current switch from the history read
  (`attachments` in `GET /api/webchat/conversations/{id}/messages`); a new session reads it at the start. A tab that has no
  conversation yet learns it on its first upload.
- **EICAR cannot be a PNG.** Images are decoded and re-encoded (`attachments/validate.rs:180`), which drops it, and a text
  file fails the magic-byte check with `unsupported_type`. A PDF is stored byte-identical (`validate.rs:170-178`), so the
  spec sends a valid PDF whose one content stream is the EICAR string, built at run time. **Unverified:** that ClamAV flags
  EICAR inside a PDF stream (it should when it unpacks the stream). If it does not, test 10 fails with the widget showing a
  success, and a raw EICAR upload direct to artifact_service (422 `malware`) remains the proof of the scanner.
- **A tenant admin can reach an applied release in the console** (release page: Edit selection, Approve, Go live;
  `web/src/routes/admin/jobs/[job]/release/+page.svelte:213-267`, API `releases.rs:63`, `apply.rs:37`, both `release_manager`,
  which a tenant admin holds). Approve stays disabled with "Nothing to release" until the draft has a change
  (`web/src/lib/release/flow.ts:42-45`) and both buttons stay disabled while the go-live checklist has blockers
  (`+page.svelte:339-345`). Whether a freshly created minimal job has none was not evaluated (nothing was run); the spec puts the
  page's blocker list into the failure message of the Approve/Go live assertions.
