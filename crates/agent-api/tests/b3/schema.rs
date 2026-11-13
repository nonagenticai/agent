//! B3 schema (migrations 0050–0057 and 0122): RLS, the `tenant_isolation` policy, the tenant default and the grants of every
//! B3 table (pattern tests/b2_schema.rs), tenant isolation, and the CHECKs. Each B3 task appends its tables to
//! [`B3_TABLES`] and its CHECK cases below. Real Postgres 18 (one container for this binary, a fresh database per test).
use crate::common;

use crate::common::classes::{Class, class_of};
use crate::common::{insert_tenant, sqlstate};
use agent_api::tenancy::begin_scoped;
use uuid::Uuid;

const ALL: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE"];

/// Every B3 table and the table privileges `agent_app` holds on it (plan B3 §4).
const B3_TABLES: &[(&str, &[&str])] = &[
    // 0050 (T3)
    ("llm_provider", ALL),
    ("llm_use_binding", ALL),
    // 0051 (T4)
    ("provider_slot", &["SELECT", "INSERT", "UPDATE"]),
    ("provider_slot_secret", &["SELECT", "INSERT"]),
    ("provider_slot_remote", &["SELECT", "INSERT", "UPDATE"]),
    // 0052 (T5)
    ("knowledge_store", ALL),
    ("knowledge_upload", ALL),
    // 0053 (T7)
    ("source", ALL),
    ("source_run", &["SELECT", "INSERT", "UPDATE"]),
    // 0054 (T10)
    ("job_knowledge_kind", ALL),
    ("platform_pack", ALL),
    // 0122 (T11): a reply's grounding is written once with it; ratings are upserted, never deleted
    ("answer_grounding", &["SELECT", "INSERT"]),
    ("answer_feedback", &["SELECT", "INSERT", "UPDATE"]),
    // 0056 (T4a)
    ("store_generation", ALL),
    ("job_embedding_pin", ALL),
    ("model_change", ALL),
    ("model_change_job", ALL),
];

/// One row of `pg_policies` for a table.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Policy {
    name: String,
    permissive: String,
    cmd: String,
    qual: Option<String>,
    with_check: Option<String>,
}

#[tokio::test]
async fn every_b3_table_has_rls_policy_default_and_grants() {
    let e = common::env().await;
    // B1's `jobs` (0033) is the reference: every B3 table copies its policy and tenant default exactly.
    let (ref_qual, ref_check): (String, String) = sqlx::query_as(
        "SELECT qual, with_check FROM pg_policies \
          WHERE schemaname = 'public' AND tablename::text = 'jobs' AND policyname::text = 'tenant_isolation'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let ref_default: String = sqlx::query_scalar(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
           JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
          WHERE d.adrelid = 'public.jobs'::regclass AND a.attname::text = 'tenant_id'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let want_policy = [Policy {
        name: "tenant_isolation".to_string(),
        permissive: "PERMISSIVE".to_string(),
        cmd: "ALL".to_string(),
        qual: Some(ref_qual),
        with_check: Some(ref_check),
    }];
    let mut problems = Vec::new();
    for &(t, privs) in B3_TABLES {
        if class_of(t) != Some(Class::Tenant) {
            problems.push(format!("{t}: not Class::Tenant in tests/common/classes.rs"));
        }
        let (rls, force): (bool, bool) = sqlx::query_as(
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = ('public.' || $1)::regclass",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        if !rls || force {
            problems.push(format!(
                "{t}: rls={rls} force={force}, want RLS on, not forced"
            ));
        }
        let policies: Vec<Policy> = sqlx::query_as(
            "SELECT policyname::text AS name, permissive, cmd, qual, with_check FROM pg_policies \
                  WHERE schemaname = 'public' AND tablename::text = $1 ORDER BY 1",
        )
        .bind(t)
        .fetch_all(&e.owner)
        .await
        .unwrap();
        if policies != want_policy {
            problems.push(format!(
                "{t}: policies {policies:?}, want exactly {want_policy:?}"
            ));
        }
        let tenant_col: Option<(bool, Option<String>)> = sqlx::query_as(
            "SELECT a.attnotnull, pg_get_expr(d.adbin, d.adrelid) \
               FROM pg_attribute a \
               LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
              WHERE a.attrelid = ('public.' || $1)::regclass AND a.attname::text = 'tenant_id' \
                AND NOT a.attisdropped",
        )
        .bind(t)
        .fetch_optional(&e.owner)
        .await
        .unwrap();
        if tenant_col != Some((true, Some(ref_default.clone()))) {
            problems.push(format!(
                "{t}: tenant_id {tenant_col:?}, want NOT NULL DEFAULT {ref_default}"
            ));
        }
        for p in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
            for role in ["agent_app", "agent_platform"] {
                let has: bool = sqlx::query_scalar(
                    "SELECT has_table_privilege($1::text::name, 'public.' || $2, $3)",
                )
                .bind(role)
                .bind(t)
                .bind(p)
                .fetch_one(&e.owner)
                .await
                .unwrap();
                let want = role == "agent_app" && privs.contains(&p);
                if has != want {
                    problems.push(format!("{t}: {role} {p} is {has}, want {want}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[tokio::test]
async fn tenants_platform_owned_defaults_false() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("B3 schema {t}")).await;
    let owned: bool = sqlx::query_scalar("SELECT platform_owned FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert!(!owned);
}

/// A provider row of `tenant` as the owner, with `kind` and `sealed`; the insert's result.
async fn insert_provider(
    e: &common::TestEnv,
    tenant: Uuid,
    name: &str,
    kind: &str,
    sealed: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO llm_provider (tenant_id, id, name, kind, base_url, models, secret_sealed, secret_fpr, \
                                   secret_updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 'https://llm.example.com/v1', ARRAY['m1'], $5, \
                 CASE WHEN $5::text IS NULL THEN NULL ELSE 'abcd1234' END, \
                 CASE WHEN $5::text IS NULL THEN NULL ELSE now() END, 'test')",
    )
    .bind(tenant)
    .bind(id)
    .bind(name)
    .bind(kind)
    .bind(sealed)
    .execute(&e.owner)
    .await
    .map(|_| id)
}

#[tokio::test]
async fn llm_checks_reject_bad_rows() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("B3 checks {t}")).await;
    // provider kind, sealed shape
    insert_provider(&e, t, "ok", "openai", Some("v2:k1:abc"))
        .await
        .unwrap();
    let bad_kind = insert_provider(&e, t, "bad-kind", "anthropic", None)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&bad_kind).as_deref(), Some("23514"), "{bad_kind}");
    let not_sealed = insert_provider(&e, t, "plain", "openai", Some("sk-plaintext"))
        .await
        .unwrap_err();
    assert_eq!(
        sqlstate(&not_sealed).as_deref(),
        Some("23514"),
        "{not_sealed}"
    );
    let dup = insert_provider(&e, t, "ok", "laya", None)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&dup).as_deref(), Some("23505"), "{dup}");
    // binding use, voice only on tts, a model needs a provider
    let pid = Uuid::new_v4();
    for (use_name, provider, model, voice, ok) in [
        ("qualify", Some(pid), Some("m1"), None, true),
        ("tts", Some(pid), Some("m1"), Some("alloy"), true),
        ("summarise", Some(pid), Some("m1"), None, false),
        ("gates", Some(pid), Some("m1"), Some("alloy"), false),
        ("generator", None, Some("m1"), None, false),
    ] {
        let r = sqlx::query(
            "INSERT INTO llm_use_binding (tenant_id, \"use\", provider_id, model, voice, updated_by) \
             VALUES ($1, $2, $3, $4, $5, 'test')",
        )
        .bind(t)
        .bind(use_name)
        .bind(provider)
        .bind(model)
        .bind(voice)
        .execute(&e.owner)
        .await;
        match (ok, r) {
            (true, r) => {
                r.unwrap_or_else(|err| panic!("{use_name}: {err}"));
            }
            (false, Ok(_)) => panic!("{use_name}: the CHECK admitted a bad binding"),
            (false, Err(err)) => {
                assert_eq!(
                    sqlstate(&err).as_deref(),
                    Some("23514"),
                    "{use_name}: {err}"
                )
            }
        }
    }
}

#[tokio::test]
async fn llm_tables_are_tenant_isolated() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, &format!("B3 iso A {a}")).await;
    insert_tenant(&e.owner, b, &format!("B3 iso B {b}")).await;
    insert_provider(&e, a, "only-a", "openai", None)
        .await
        .unwrap();
    let mut tx = begin_scoped(&e.state.db, b).await.unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_provider")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, 0, "agent_app in tenant B sees none of A's providers");
    let err = sqlx::query(
        "INSERT INTO llm_provider (tenant_id, id, name, kind, base_url, models, updated_by) \
         VALUES ($1, $2, 'sneak', 'openai', 'https://x.example.com', ARRAY['m'], 'test')",
    )
    .bind(a)
    .bind(Uuid::new_v4())
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

/// One `knowledge_store` row as the owner; the insert's result.
async fn insert_store(
    e: &common::TestEnv,
    tenant: Uuid,
    key: &str,
    kind: &str,
    region: &str,
    parent: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, region, updated_by) \
         VALUES ($1, $2, $3, 'Label', $4, $5, $6, 'test')",
    )
    .bind(tenant)
    .bind(id)
    .bind(key)
    .bind(parent)
    .bind(kind)
    .bind(region)
    .execute(&e.owner)
    .await
    .map(|_| id)
}

#[tokio::test]
async fn knowledge_store_checks_reject_bad_rows() {
    let e = common::env().await;
    let (t, other) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, t, &format!("B3 ks {t}")).await;
    insert_tenant(&e.owner, other, &format!("B3 ks other {other}")).await;
    let root = insert_store(&e, t, "faq", "group", "eu", None)
        .await
        .unwrap();
    insert_store(&e, t, "faq.policy", "leaf", "us", Some(root))
        .await
        .unwrap();
    for (key, kind, region, parent, want) in [
        ("Bad Key", "leaf", "eu", None, "23514"),
        ("x", "leaf", "eu", None, "23514"),
        ("ok.key", "folder", "eu", None, "23514"),
        ("ok.key", "leaf", "asia", None, "23514"),
        ("faq", "leaf", "eu", None, "23505"),
        // a parent of another tenant is no parent
        ("ok.key2", "leaf", "eu", Some(Uuid::new_v4()), "23503"),
    ] {
        let err = insert_store(&e, t, key, kind, region, parent)
            .await
            .unwrap_err();
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some(want),
            "{key} {kind} {region}: {err}"
        );
    }
    let cross = insert_store(&e, other, "child", "leaf", "eu", Some(root))
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&cross).as_deref(), Some("23503"), "{cross}");
    // tenant isolation
    let mut tx = begin_scoped(&e.state.db, other).await.unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_store")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        seen, 0,
        "agent_app in another tenant sees none of the stores"
    );
}

#[tokio::test]
async fn slot_checks_reject_bad_rows() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("B3 slot checks {t}")).await;
    // a sealed key row; a plaintext one is refused
    let key = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO provider_slot_secret (tenant_id, id, slot, key_sealed, fingerprint, created_by) \
         VALUES ($1, $2, 'embed', 'v2:k1:abc', '0a1b2c3d', 'test')",
    )
    .bind(t)
    .bind(key)
    .execute(&e.owner)
    .await
    .unwrap();
    let plain = sqlx::query(
        "INSERT INTO provider_slot_secret (tenant_id, id, slot, key_sealed, fingerprint, created_by) \
         VALUES ($1, $2, 'embed', 'sk-plaintext', '0a1b2c3d', 'test')",
    )
    .bind(t)
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&plain).as_deref(), Some("23514"), "{plain}");
    struct Case {
        slot: &'static str,
        service: &'static str,
        configured: bool,
        kind: Option<&'static str>,
        model: Option<&'static str>,
        key: bool,
        kept: bool,
        ok: bool,
    }
    let unset = |slot, service| Case {
        slot,
        service,
        configured: false,
        kind: None,
        model: None,
        key: false,
        kept: false,
        ok: true,
    };
    let set = |slot, service, kind| Case {
        configured: true,
        kind: Some(kind),
        model: Some("m"),
        ..unset(slot, service)
    };
    let cases = [
        Case {
            key: true,
            ..set("embed", "strata_rag", "voyage")
        },
        unset("ocr", "strata_ingest"),
        // the service is the slot's
        Case {
            ok: false,
            ..unset("vision", "strata_rag")
        },
        Case {
            ok: false,
            ..unset("audio", "strata_ingest")
        },
        // configured needs kind and model; unconfigured holds nothing
        Case {
            kind: None,
            ok: false,
            ..set("chat", "strata_rag", "x")
        },
        Case {
            kind: Some("cohere"),
            ok: false,
            ..unset("rerank", "strata_rag")
        },
        // a key agent holds and a key strata keeps are exclusive
        Case {
            key: true,
            kept: true,
            ok: false,
            ..set("transcribe", "strata_ingest", "openai_compatible")
        },
    ];
    for c in cases {
        let r = sqlx::query(
            "INSERT INTO provider_slot (tenant_id, slot, service, configured, kind, model, secret_id, key_kept, \
                                        updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'test')",
        )
        .bind(t)
        .bind(c.slot)
        .bind(c.service)
        .bind(c.configured)
        .bind(c.kind)
        .bind(c.model)
        .bind(c.key.then_some(key))
        .bind(c.kept)
        .execute(&e.owner)
        .await;
        let slot = c.slot;
        match (c.ok, r) {
            (true, r) => {
                r.unwrap_or_else(|err| panic!("{slot}: {err}"));
            }
            (false, Ok(_)) => panic!("{slot}: the CHECK admitted a bad slot row"),
            (false, Err(err)) => {
                assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{slot}: {err}")
            }
        }
    }
    // a key of another tenant cannot be referenced
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, &format!("B3 slot other {other}")).await;
    let cross = sqlx::query(
        "INSERT INTO provider_slot (tenant_id, slot, service, configured, kind, model, secret_id, updated_by) \
         VALUES ($1, 'embed', 'strata_rag', true, 'voyage', 'm', $2, 'test')",
    )
    .bind(other)
    .bind(key)
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&cross).as_deref(), Some("23503"), "{cross}");
}

/// One `source` row of `tenant` as the owner; the insert's result.
#[allow(clippy::too_many_arguments)]
async fn insert_source(
    e: &common::TestEnv,
    tenant: Uuid,
    job: Uuid,
    store: Uuid,
    name: &str,
    kind: &str,
    language: &str,
    sealed: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO source (tenant_id, job_id, id, name, type, target_store_id, language, credentials_sealed, \
                             credentials_fpr, credentials_updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 CASE WHEN $8::text IS NULL THEN NULL ELSE 'abcd1234' END, \
                 CASE WHEN $8::text IS NULL THEN NULL ELSE now() END, 'test')",
    )
    .bind(tenant)
    .bind(job)
    .bind(id)
    .bind(name)
    .bind(kind)
    .bind(store)
    .bind(language)
    .bind(sealed)
    .execute(&e.owner)
    .await
    .map(|_| id)
}

/// One `source_run` row as the owner; the insert's result.
async fn insert_run(
    e: &common::TestEnv,
    tenant: Uuid,
    job: Uuid,
    source: Uuid,
    status: &str,
    finished: bool,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO source_run (tenant_id, job_id, id, source_id, status, finished_at, started_by) \
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN now() END, 'test')",
    )
    .bind(tenant)
    .bind(job)
    .bind(Uuid::new_v4())
    .bind(source)
    .bind(status)
    .bind(finished)
    .execute(&e.owner)
    .await
}

#[tokio::test]
async fn source_checks_reject_bad_rows() {
    let e = common::env().await;
    let (t, other) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, t, &format!("B3 src {t}")).await;
    insert_tenant(&e.owner, other, &format!("B3 src other {other}")).await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'support', 'Support', '[]', 'test') \
         RETURNING id",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let store = insert_store(&e, t, "faq.sources", "leaf", "eu", None)
        .await
        .unwrap();
    let src = insert_source(&e, t, job, store, "Help centre", "website", "nl", None)
        .await
        .unwrap();
    insert_source(
        &e,
        t,
        job,
        store,
        "Promos",
        "api",
        "en-GB",
        Some("v2:k1:abc"),
    )
    .await
    .unwrap();
    for (name, kind, language, sealed, want) in [
        ("Feed", "rss", "nl", None, "23514"),
        ("Feed", "api", "Dutch", None, "23514"),
        ("Feed", "api", "nl", Some("plain-key"), "23514"),
        ("", "api", "nl", None, "23514"),
        // one name per job
        ("Help centre", "file", "nl", None, "23505"),
    ] {
        let err = insert_source(&e, t, job, store, name, kind, language, sealed)
            .await
            .unwrap_err();
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some(want),
            "{name} {kind} {language}: {err}"
        );
    }
    // a store or job of another tenant is no target
    let cross = insert_source(&e, other, job, store, "Sneak", "file", "nl", None)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&cross).as_deref(), Some("23503"), "{cross}");
    // at most one running run per source; a finished run needs finished_at
    insert_run(&e, t, job, src, "running", false).await.unwrap();
    let second = insert_run(&e, t, job, src, "running", false)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&second).as_deref(), Some("23505"), "{second}");
    insert_run(&e, t, job, src, "completed", true)
        .await
        .unwrap();
    let open = insert_run(&e, t, job, src, "failed", false)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&open).as_deref(), Some("23514"), "{open}");
    // deleting the source deletes its runs
    sqlx::query("DELETE FROM source WHERE id = $1")
        .bind(src)
        .execute(&e.owner)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM source_run WHERE source_id = $1")
        .bind(src)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(left, 0);
    // tenant isolation
    let mut tx = begin_scoped(&e.state.db, other).await.unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM source")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        seen, 0,
        "agent_app in another tenant sees none of the sources"
    );
}

/// One `store_generation` row as the owner; the insert's result.
async fn insert_generation(
    e: &common::TestEnv,
    tenant: Uuid,
    store: Uuid,
    build: &str,
    state: &str,
    strata: Option<Uuid>,
    source: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_generation (tenant_id, id, store_id, strata_store_id, embed, build, state, source_id, \
                                       created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'test')",
    )
    .bind(tenant)
    .bind(id)
    .bind(store)
    .bind(strata)
    .bind(format!("e-{id}"))
    .bind(build)
    .bind(state)
    .bind(source)
    .execute(&e.owner)
    .await
    .map(|_| id)
}

#[tokio::test]
async fn generation_and_model_change_checks_reject_bad_rows() {
    let e = common::env().await;
    let (t, other) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, t, &format!("B3 gen {t}")).await;
    insert_tenant(&e.owner, other, &format!("B3 gen other {other}")).await;
    let kb = insert_store(&e, t, "kb", "leaf", "eu", None).await.unwrap();
    let first = insert_generation(&e, t, kb, "initial", "ready", None, None)
        .await
        .unwrap();
    for (build, state, strata, source, want) in [
        // one initial generation per store
        ("initial", "ready", None, None, "23505"),
        // an initial generation is the store's own binding
        ("initial", "ready", Some(Uuid::new_v4()), None, "23514"),
        // a rebuild names its source, and a ready one its strata root
        ("reembed", "building", None, None, "23514"),
        ("reembed", "ready", None, Some(first), "23514"),
        (
            "rebuilt",
            "ready",
            Some(Uuid::new_v4()),
            Some(first),
            "23514",
        ),
        ("copy", "done", Some(Uuid::new_v4()), Some(first), "23514"),
    ] {
        let err = insert_generation(&e, t, kb, build, state, strata, source)
            .await
            .unwrap_err();
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some(want),
            "{build} {state}: {err}"
        );
    }
    insert_generation(&e, t, kb, "copy", "building", None, Some(first))
        .await
        .unwrap();
    // a model change's rebuild follows its slot, and the model moves
    for (slot, from, to, rebuild) in [
        ("embed", "a", "b", "copy"),
        ("ocr", "a", "b", "reembed"),
        ("chat", "a", "a", "copy"),
        ("search", "a", "b", "copy"),
    ] {
        let err = sqlx::query(
            "INSERT INTO model_change (tenant_id, id, scope, slot, from_model, to_model, rebuild, created_by) \
             VALUES ($1, $2, 'tenant', $3, $4, $5, $6, 'test')",
        )
        .bind(t)
        .bind(Uuid::new_v4())
        .bind(slot)
        .bind(from)
        .bind(to)
        .bind(rebuild)
        .execute(&e.owner)
        .await
        .unwrap_err();
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("23514"),
            "{slot} {rebuild}: {err}"
        );
    }
    // tenant isolation
    let mut tx = begin_scoped(&e.state.db, other).await.unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM store_generation")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        seen, 0,
        "agent_app in another tenant sees none of the generations"
    );
}
