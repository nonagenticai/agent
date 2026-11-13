//! The audit action vocabulary (B1 X3). Every `Event::new` / `PlatformEvent::new` literal in `src/` must be listed
//! here (tests/audit.rs `vocab_is_sorted_unique_and_covers_every_literal`), so an unlisted action fails CI, not
//! production. Sorted, unique, one line per action with the plan that owns it. Later plans append their own.

/// Actions of the tenant log (`audit_event`).
pub const TENANT: &[&str] = &[
    "answer_feedback.submit",                  // B3 T11
    "app_licence.provisioned",                 // app licences (license_id, app)
    "canned_reply.create",                     // SW T7 (title length only; never the text)
    "canned_reply.delete",                     // SW T7
    "case.csat",                               // B12
    "case.csat_comment",                       // B12 (the follow-up comment recorded: channel)
    "case.handover",                           // B12
    "case.party_link",                         // B13 (linkCaseParty: party_id, from_party, merged)
    "channel_binding.create",                  // B12
    "channel_binding.delete",                  // B12
    "channel_binding.identity_secret_rotate",  // B7 known-customer web chat (kid, revoke_sessions)
    "channel_binding.publish",                 // D130 (publishChannel: customer binding live)
    "channel_binding.rotate_key",              // B13 (rotateWebChatKey: sessions_blocked)
    "channel_binding.test_message",            // B7 T15 (sendChannelTestMessage: kind)
    "channel_binding.test_session", // admin test-as-customer on a TEST web chat (ids only)
    "channel_binding.unpublish",    // D130 (back to the test env)
    "channel_binding.update",       // B12
    "channel_binding.verify",       // B12 (review #8)
    "channel_turn_refused",         // B4 T9 (gate 2 at the drain; once per tenant per hour)
    "chat.card_redacted",           // B4 T9 (N-03; once per conversation per hour)
    "connector.contract_test.run",  // B7 T10
    "connector.credential.set", // B7 T9 (details: instance, credential_version, fingerprint only)
    "connector.instance.create", // B7 T9
    "connector.instance.repin", // B7 T9
    "connector.instance.update", // B7 T9
    "connector.mapping_override.revert", // B7 T12 (design: draft seam)
    "connector.mapping_override.save", // B7 T12 (design: draft seam)
    "connector.route.clear",    // B7 T13 (design: job draft seam, kind route)
    "connector.route.set",      // B7 T13 (design: job draft seam, kind route)
    "connector.sample.fetch",   // B7 T10
    "connector.tenant_template.contract_test", // B7 T11 (tenant T1 run; result, not design)
    "connector.tenant_template.create", // B7 T11 (design: draft seam)
    "connector.tenant_template.draft", // B7 T11 (design: draft seam)
    "connector.tenant_template.mcp_import", // caller identity B1: MCP tools imported into a draft
    "connector.tenant_template.save", // B7 T11 (design: draft seam)
    "connector.tenant_template.status", // B7 T11 (design: draft seam)
    "connector.tenant_template.validate", // B7 T11 (result, not design)
    "conversation.assign",      // SW T2 (Assign…: item_id, assignee)
    "conversation.auto_assign", // SW routing (item_id, assignee; the system assigned a queued or stale item)
    "conversation.hand_back",   // SW T2 (items handed back; the note stays out)
    "conversation.identity_upgraded", // A3 owner 2026-11-01 (procedure, strength, step, previous_source; never the person)
    "conversation.note",              // SW T3 (a staff note: channel, message_id; never the text)
    "conversation.resolve", // SW T4 (Resolve: item_id, outcome; never the closing message)
    "conversation.staff_reply", // B12
    "conversation.take",    // SW T2 (item_id, assignee)
    "conversation_attachment.download_staff", // SW-9 (attachment id, media_type, byte_size; never the name)
    "conversation_attachment.upload", // SW-9 (attachment id or direction, media_type, byte_size; never content or names)
    "dead_letter.discard",            // B4 T14 (discardDeadLetter: env)
    "dead_letter.replay",             // B4 T14 (replayDeadLetter: env, stream, new_entry_id)
    "discovery.import_delete",        // B8
    "discovery.import_upload",        // B8
    "discovery.proposal_accept",      // B8
    "discovery.proposal_reject",      // B8
    "discovery.shadow_start",         // B8
    "discovery.shadow_stop",          // B8
    "discovery.weight_set",           // B8
    "eval_question.create",           // B3 T9 (D72)
    "eval_question.delete",           // B3 T9
    "eval_question.import",           // B3 T9
    "eval_question.update",           // B3 T9
    "eval_run.start",                 // B3 T9
    "execute", // B4 operation calls (one row per request; listed by B1 for the dedupe index)
    "generator.advance",
    "generator.evidence_upload",
    "generator.item_decide",
    "generator.object_propose",
    "generator.operation_propose",
    "generator.proposal_resolve",
    "generator.stage_scope",
    "generator.start",
    "generator.step_draft",
    "generator.tag",
    "handover.assign",                     // B12
    "handover.feedback",                   // B4 T16
    "handover.reply",                      // B12
    "handover.state",                      // B12
    "iam_attachment.update", // A3 (job iam_attachment: procedure, attached; design: draft seam)
    "iam_procedure.create",  // A3 (tenant IAM procedure: name, steps)
    "iam_procedure.delete",  // A3
    "iam_procedure.update",  // A3 (name, steps)
    "ingest.cancelled",      // B3 T6
    "ingest.deleted",        // B3 T6
    "ingest.retried",        // B3 T6
    "ingest.upload",         // B3 T6 (knowledge_upload)
    "job.change_reapplied",  // B2 T12 (apply: re-apply)
    "job.change_restored_to_draft", // B2 T14 (restoreRolledBack)
    "job.change_rolled_back", // B2 T12 (apply: rollback)
    "job.create",            // B1
    "job.draft_change",      // B2 T4 (D54: design::track)
    "job.draft_discarded",   // B2 T14 (discardDraft)
    "job.draft_published",   // B2 T12 (apply publishes a cut's drafts)
    "job.pins_update",       // owner 2026-11-01 (a hierarchy move reconciles jobs.pins)
    "job.selection_update",  // B2 T7
    "job.staff_update",      // TJ D125 (members, routing)
    "job.update",            // TJ D123 (label)
    "job.upgrade_request",   // B1
    "job_api_key.create",    // B13 (prefix, expires_at; never the secret)
    "job_api_key.revoke",    // B13
    "job_operation.update", // 0246 (a job's own op from a tenant template: op, set|cleared, template)
    "knowledge.platform_exclusion.remove", // PK T7 (details: key_prefix only)
    "knowledge.platform_exclusion.set", // PK T7 (details: key_prefix only; the reason stays out)
    "knowledge_document.delete", // B3 T5
    "knowledge_store.create", // B3 T5
    "knowledge_store.delete", // B3 T5
    "license.issue",        // B1 (D77)
    "license.revoke",       // B1 (D77)
    "license.revoke_failed", // B1 (D77 L6)
    "model_change.accept",  // B3 T4a (D83: embedding_pin drafts)
    "model_change.reembed", // B3 T4a (D83: a person starts a rebuild)
    "operation.denied",     // B4 T6a (per-case cap: operation, code, case_id)
    "operation_call.check", // B4 T13 (checkUnknownOutcome: verdict, operation, case_id)
    "provider.bind",        // B3 T3 (D69 draft)
    "provider.create",      // B3 T3 (key: set|none)
    "provider.delete",      // B3 T3
    "provider.test",        // B3 T3 (stored provider only)
    "provider.update",      // B3 T3 (key: kept|cleared|replaced)
    "provider_slot.adopt_remote", // B3 T4 (D69 draft)
    "provider_slot.push",   // B3 review I4 (retry an unconfirmed push: slot, outbox_id)
    "provider_slot.reset",  // B3 T4b (F-08: slot, from_source)
    "provider_slot.save",   // B3 T4 (D69 draft; key: set|kept|none)
    "provision.cancelled",  // B1
    "provision.dead",       // B1
    "provision.enqueued",   // B1
    "provision.failed",     // B1
    "provision.retried",    // B1
    "provision.succeeded",  // B1
    "release.apply",        // B2 T12
    "release.approve",      // B2 T11
    "release.integrity_error", // B2 T12 (stored bundle no longer hashes; own transaction)
    "release.reapply",      // B2 T12
    "release.rollback",     // B2 T12
    "source.create",        // B3 T7 (credentials: set|none)
    "source.delete",        // B3 T7 (purge, documents_deleted, documents_kept)
    "source.replace",       // B3 T8 (a completed run replaced earlier documents; system actor)
    "source.resync", // B3 T8 (run_id, status, trigger, jobs, mapping, files; never credentials)
    "source.update", // B3 T7 (credentials: kept|cleared|replaced)
    "stream.claim_stuck", // B4 T14 (claimStuck: env, claimed)
    "support_routing.update", // SW routing (mode, reassign_after_secs)
    "tenant.attachment_quota", // SW-9 (owner 2026-10-28: quota_bytes, null = default; super_admin only)
    "tenant.create",           // B1
    "tenant.draft_change",     // B2 T4 (D69: design::track_tenant)
    "tenant.import",           // B1
    "tenant.offboard",         // B1
    "tenant.profile_update",   // TJ D124 (old, new)
    "tenant.rename",           // DR (owner 2026-10-23: old_name, new_name)
    "tenant.resume",           // B1
    "tenant.support_auto_assign", // SW-7 (owner 2026-10-27: enabled; super_admin only)
    "tenant.suspend",          // B1
    "tenant.switch",           // B1
    "test_case.save",          // B4 T18 (saveTestCase: conversation_id, turns, draft_rev)
    "tool_policy.update",      // A2 (job tool_policy: op, set|cleared; design: draft seam)
    "user.add",                // B1
    "user.disable",            // B1
    "user.enable",             // B1
    "user.invite",             // B1
    "user.invite_accept",      // B1 (CP acceptInvite, verified email)
    "user.invite_email",       // B1
    "user.invite_expire",      // B1b T2 (F-01: the hourly DB-only expiry flip)
    "user.invite_resend",      // B1b T1 (F-01)
    "user.invite_revoke",      // B1b T2 (F-01)
    "user.rehome",             // B1
    "user.role",               // B1
    "user.view_as.end",        // B1 T21 (D79)
    "user.view_as.start",      // B1 T21 (D79)
    "validation.cut_run",      // B8
    "validation.draft_run",    // B8 T13
    "validation.grid_finished", // B8
    "validation.grid_queued",  // B8
    "validation.question_add", // B8
    "validation.question_delete", // B8
    "validation.question_import", // B8
    "validation.question_update", // B8
    "validation.verdict_set",  // B8
    "web_chat_session.block",  // B13 (abuse: reason rate_limited|repeat_text, 1 h)
    "web_chat_session.identified", // B7 known-customer web chat (sha256(sub) prefix only)
];

/// Actions of the platform log (`platform_audit`).
pub const PLATFORM: &[&str] = &[
    "business_model.axis_cardinality",  // B6b T12e
    "business_model.axis_create",       // B6b T12e
    "business_model.axis_value_add",    // B6b T10
    "business_model.axis_value_retire", // B6b T12e
    "business_model.rule_create",       // B6b T10/T12g
    "business_model.save",              // B6b T10
    "connector_template.create", // D130 (super_admin creates a new platform template as a draft)
    "connector_template.draft",  // B7 T7 (new draft version of a platform template)
    "connector_template.op_save", // B7 T7 (draft op saved; t1 reset)
    "connector_template.publish", // B7 T7 (validator + T1 replay; failed rows carry t1_failed)
    "connector_template.restore", // retire/restore (retired → active; the catalogue offers it again)
    "connector_template.retire", // retire/restore (active → retired; pinned instances keep working)
    "connector_template.t2_run", // B7 T7 (sandbox run; result only)
    "department.create",         // plan 2026-10-30 T3 (industries + departments)
    "department.rename",         // plan 2026-10-30 T3
    "department.restore",        // plan 2026-10-30 T3
    "department.retire",         // plan 2026-10-30 T3 (refused while a tenant job selects it)
    "department.update",         // label and/or areas (the release target's area choices)
    "dimension.create",          // B6b T12e
    "dimension.value_add",       // B6b T10
    "generator.advance",
    "generator.evidence_upload",
    "generator.item_decide",
    "generator.object_propose",
    "generator.operation_propose",
    "generator.proposal_resolve",
    "generator.stage_scope",
    "generator.start",
    "generator.step_draft",
    "generator.tag",
    "industry.create",            // plan 2026-10-30 T3 (industries + departments)
    "industry.rename",            // plan 2026-10-30 T3
    "industry.restore",           // plan 2026-10-30 T3
    "industry.retire",            // plan 2026-10-30 T3 (refused while a tenant job selects it)
    "invite.unresolved",          // B1
    "job_template.restore", // retire/restore (retired_at cleared; the tenant catalogue offers it again)
    "job_template.retire",  // retire/restore (hidden from new jobs; pinned jobs keep working)
    "journey.activity_add", // B8 T4/T4a/T4b (journey designer)
    "journey.activity_move", // B8 T4/T4a/T4b (journey designer)
    "journey.activity_retire", // B8 T4/T4a/T4b (journey designer)
    "journey.activity_update", // B8 T4/T4a/T4b (journey designer)
    "journey.card_update",  // B8 T4 (platform card writer)
    "journey.channel_status_set", // B8 T4/T4a/T4b (journey designer)
    "journey.object_bind",  // B8 T4/T4a/T4b (journey designer)
    "journey.object_unbind", // B8 T4/T4a/T4b (journey designer)
    "journey.operation_remove", // B8 T4/T4a/T4b (journey designer)
    "journey.operation_set", // B8 T4/T4a/T4b (journey designer)
    "journey.prior_set",    // B8 T4/T4a/T4b (journey designer)
    "journey.stage_add",    // B8 T4/T4a/T4b (journey designer)
    "journey.stage_kind_remove", // B8 T4/T4a/T4b (journey designer)
    "journey.stage_kind_set", // B8 T4/T4a/T4b (journey designer)
    "journey.stage_messages_set", // B8 T4/T4a/T4b (journey designer)
    "journey.stage_reorder", // B8 T4/T4a/T4b (journey designer)
    "journey.stage_update", // B8 T4/T4a/T4b (journey designer)
    "journey.theme_add",    // B8 T4/T4a/T4b (journey designer)
    "journey.theme_update", // B8 T4/T4a/T4b (journey designer)
    "layer.create",         // B6b T12b (createLayer)
    "layer.revision",       // B6a
    "license.issue_failed", // B1 (D77)
    "license.reissue",      // B7 T3a (F-31)
    "license.reissue_request", // B7 T3a (F-31)
    "license.reseal",       // B7 T3 (counts only; resealLicences / resealTenantLicence)
    "platform.export",      // B6b T12f (exportPlatformSeed; details: family, tag, counts)
    "platform.knowledge_document.delete", // PK T8 (details: document_id)
    "platform.knowledge_document.ingest", // PK T8 (details: store_id, document_id, topic_keys, license; never text)
    "platform.knowledge_document.ingest_urls", // PK T8 (strata_ingest URL jobs; details: store_id, jobs, topic_keys, license)
    "platform.knowledge_document.upload", // PK T6b (strata_ingest file jobs; details: store_id, jobs, files, topic_keys, license)
    "platform.knowledge_pack.publish",    // PK T8
    "platform.knowledge_pack.reference",  // PK T8 (template layer knowledge_pack row + tag)
    "platform.knowledge_pack.register",   // PK T8
    "platform.knowledge_pack.retire",     // PK T8
    "platform.knowledge_pack.unreference", // PK T8 (tombstone + tag)
    "platform.knowledge_store.create",    // PK T8
    "platform.knowledge_store.delete",    // PK T8
    "signup.approve",                     // B1
    "signup.intake",                      // B1
    "signup.invite",                      // B1 (CP pre-approved invite)
    "signup.reject",                      // B1
    "signup.reject_repair", // B1 (CP reject repair: aborted workspace, minted identity)
    "signup.retry",         // B1
    "template.tag",         // B6a
    "tenant.attachment_quota", // SW-9 (owner 2026-10-28: quota_bytes, null = default; super_admin only)
    "tenant.create",           // B1
    "tenant.import",           // B1
    "tenant.offboard",         // B1
    "tenant.rename",           // DR (owner 2026-10-23: old_name, new_name)
    "tenant.resume",           // B1
    "tenant.support_auto_assign", // SW-7 (owner 2026-10-27: enabled; super_admin only)
    "tenant.suspend",          // B1
    "user.invite_resend",      // B1b T1 (F-01, super_admin path)
    "user.invite_revoke",      // B1b T2 (F-01, super_admin path)
    "user.rehome",             // B1
];

/// Is `list` sorted ascending without duplicates?
pub fn sorted_unique(list: &[&str]) -> bool {
    list.windows(2).all(|w| w[0] < w[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_vocabularies_are_sorted_and_unique() {
        assert!(sorted_unique(TENANT));
        assert!(sorted_unique(PLATFORM));
    }
}
