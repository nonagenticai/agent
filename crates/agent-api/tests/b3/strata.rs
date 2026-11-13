//! B3 T2: the strata data-plane gate (G1: no token pair → 409 `provision_pending`, failed outbox → 503
//! `provision_failed`), the clients' headers (rag: pinned token + `X-Tenant-Id` + uuid actor; ingest: token only), the
//! fake's tenant pinning, D91/G7 (every query and eval names its root; `ambiguous_root` passed through and counted),
//! rag #23/#24 response shapes and the unreadable-store 404 (R10/R11). Real Postgres 18 (one container per binary) and
//! the in-process strata fakes (`common::strata_fake`).
use crate::common;

use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{TestEnv, insert_tenant};
use agent_api::observability;
use agent_api::strata::rag::{self, NewStore, QueryReq, QueryTarget, StoreKind};
use agent_api::strata::{self, Service, StrataStoreId, ingest};
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

/// A fresh env whose strata config points at a new pair of fakes, with token pairs for `pinned` only. Every tenant
/// in `pinned` and `unpinned` exists.
async fn setup(pinned: &[Uuid], unpinned: &[Uuid]) -> (TestEnv, StrataFake) {
    let fake = strata_fake::start().await;
    let e = common::env_with(|v| fake.env(pinned, v)).await;
    for t in pinned.iter().chain(unpinned) {
        insert_tenant(&e.owner, *t, &format!("T2 {t}")).await;
    }
    (e, fake)
}

/// `strata::gate` for `tenant` in its own scoped transaction (committed state only; nothing is held after).
async fn gate(e: &TestEnv, tenant: Uuid, svc: Service) -> agent_api::error::ApiResult<()> {
    let mut tx = begin_scoped(&e.state.db, tenant).await.unwrap();
    strata::gate(&e.state, &mut tx, tenant, svc)
        .await
        .map(|_| ())
}

/// A plane that passed the gate.
async fn plane(e: &TestEnv, tenant: Uuid, svc: Service) -> strata::Plane<'_> {
    let mut tx = begin_scoped(&e.state.db, tenant).await.unwrap();
    strata::gate(&e.state, &mut tx, tenant, svc)
        .await
        .expect("gate")
}

fn ambiguous_count(op: &str) -> f64 {
    let text = observability::metrics_handle().render();
    text.lines()
        .find(|l| {
            l.starts_with(&format!("{}{{", rag::AMBIGUOUS_ROOT_TOTAL))
                && l.contains(&format!("op=\"{op}\""))
        })
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

fn q(text: &str) -> QueryReq {
    QueryReq {
        query: text.to_string(),
        ..QueryReq::default()
    }
}

#[tokio::test]
async fn gate_without_token_pair_is_409_provision_pending() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let (e, fake) = setup(&[a], &[b]).await;
    for svc in [Service::Rag, Service::Ingest] {
        let err = gate(&e, b, svc).await.unwrap_err();
        assert_eq!(
            err.status_and_code(),
            (StatusCode::CONFLICT, "provision_pending"),
            "{svc:?}"
        );
        gate(&e, a, svc).await.expect("a has its pair");
    }
    assert!(fake.rag_requests().is_empty() && fake.ingest_requests().is_empty());
    // no URL at all (strata not wired in this deployment): 503, not a call to nowhere
    let bare = common::env().await;
    insert_tenant(&bare.owner, a, "bare").await;
    let err = gate(&bare, a, Service::Rag).await.unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    );
}

#[tokio::test]
async fn gate_with_failed_outbox_row_is_503_provision_failed() {
    let a = Uuid::new_v4();
    let (e, _fake) = setup(&[a], &[]).await;
    gate(&e, a, Service::Rag).await.expect("nothing queued");
    for (target, status, svc) in [
        ("strata_rag", "failed", Service::Rag),
        ("strata_ingest", "dead", Service::Ingest),
    ] {
        sqlx::query(
            "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, status, created_by) \
             VALUES ($1, $2, 'connections_put', $2 || ':connections_put', $3::provision_status, 't')",
        )
        .bind(a)
        .bind(target)
        .bind(status)
        .execute(&e.owner)
        .await
        .unwrap();
        let err = gate(&e, a, svc).await.unwrap_err();
        assert_eq!(
            err.status_and_code(),
            (StatusCode::SERVICE_UNAVAILABLE, "provision_failed"),
            "{target} {status}"
        );
    }
    // a pending row is still 409 provision_pending (B1 chip), through the same gate
    let p = Uuid::new_v4();
    let (e2, _f2) = setup(&[p], &[]).await;
    sqlx::query(
        "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, status, created_by) \
         VALUES ($1, 'strata_rag', 'store_ensure', 'strata_rag:store_ensure:faq', 'pending', 't')",
    )
    .bind(p)
    .execute(&e2.owner)
    .await
    .unwrap();
    let err = gate(&e2, p, Service::Rag).await.unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::CONFLICT, "provision_pending")
    );
    gate(&e2, p, Service::Ingest)
        .await
        .expect("ingest has nothing queued");
}

#[tokio::test]
async fn rag_calls_carry_tenant_header_and_pinned_token() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let (e, fake) = setup(&[a, b], &[]).await;
    let p = plane(&e, a, Service::Rag).await;
    let root = rag::create_store(
        &e.state.http,
        &p,
        &NewStore {
            parent_id: None,
            kind: StoreKind::Router,
            name: "faq".into(),
            description: "FAQ".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(root.kind, StoreKind::Router);
    let listed = rag::stores(&e.state.http, &p).await.unwrap();
    assert_eq!(listed, vec![root.clone()]);
    rag::query(
        &e.state.http,
        &p,
        &QueryTarget::Root(root.id),
        &q("opening hours"),
    )
    .await
    .unwrap();
    rag::providers_get(&e.state.http, &p).await.unwrap();
    let reqs = fake.rag_requests();
    assert_eq!(reqs.len(), 4, "{reqs:?}");
    for r in &reqs {
        assert_eq!(
            r.token.as_deref(),
            Some(strata_fake::rag_token(a).as_str()),
            "{r:?}"
        );
        assert_eq!(r.tenant_header.as_deref(), Some(a.to_string().as_str()));
        assert_eq!(r.tenant, Some(a));
    }
    // b's tree is b's: a's store is not listed for b
    let pb = plane(&e, b, Service::Rag).await;
    assert!(rag::stores(&e.state.http, &pb).await.unwrap().is_empty());
}

#[tokio::test]
async fn ingest_calls_carry_pinned_token_only() {
    let a = Uuid::new_v4();
    let (e, fake) = setup(&[a], &[]).await;
    let leaf = StrataStoreId(Uuid::new_v4());
    let p = plane(&e, a, Service::Ingest)
        .await
        .with_actor(&Uuid::new_v4().to_string());
    let created = ingest::create_json(
        &e.state.http,
        &p,
        json!({"store_id": leaf, "url": "https://example.com/a", "source_id": "src-1"}),
    )
    .await
    .unwrap();
    assert_eq!(created.jobs.len(), 1);
    let id = created.jobs[0].job_id;
    let form = reqwest::multipart::Form::new()
        .text("store_id", leaf.to_string())
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"hello".to_vec()).file_name("a.txt"),
        )
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"world".to_vec()).file_name("b.txt"),
        );
    let up = ingest::create_multipart(&e.state.http, &p, form)
        .await
        .unwrap();
    assert_eq!(up.jobs.len(), 2, "one job per file");
    let view = ingest::get(&e.state.http, &p, id).await.unwrap();
    assert_eq!(view["meta"]["source_id"], "src-1");
    let list = ingest::list(
        &e.state.http,
        &p,
        &ingest::ListQuery {
            store_id: Some(leaf),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(list["jobs"].as_array().unwrap().len(), 3);
    ingest::providers_get(&e.state.http, &p).await.unwrap();
    let reqs = fake.ingest_requests();
    assert_eq!(reqs.len(), 5, "{reqs:?}");
    for r in &reqs {
        assert_eq!(
            r.token.as_deref(),
            Some(strata_fake::ingest_token(a).as_str()),
            "{r:?}"
        );
        assert_eq!(
            r.tenant_header, None,
            "ingest: the token decides the tenant"
        );
        assert_eq!(r.actor, None, "ingest takes no actor header");
    }
    assert!(fake.rag_requests().is_empty());
}

#[tokio::test]
async fn actor_header_only_for_uuid_subs() {
    let a = Uuid::new_v4();
    let (e, fake) = setup(&[a], &[]).await;
    let person = Uuid::new_v4().to_string();
    let cases: [(&str, Option<String>); 4] = [
        ("service-account-agent", None),
        ("", None),
        ("00000000-0000-0000-0000-000000000000", None),
        (person.as_str(), Some(person.clone())),
    ];
    for (sub, want) in &cases {
        let p = plane(&e, a, Service::Rag).await.with_actor(sub);
        rag::stores(&e.state.http, &p).await.unwrap();
        let last = fake.rag_requests().pop().unwrap();
        assert_eq!(&last.actor, want, "sub {sub:?}");
    }
    // no actor unless one is given
    let p = plane(&e, a, Service::Rag).await;
    rag::stores(&e.state.http, &p).await.unwrap();
    assert_eq!(fake.rag_requests().pop().unwrap().actor, None);
}

#[tokio::test]
async fn fake_refuses_token_of_other_tenant() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let (e, fake) = setup(&[a, b], &[]).await;
    let http = reqwest::Client::new();
    let url = format!("{}/api/stores", fake.rag_url);
    // a's token naming b
    let r = http
        .get(&url)
        .bearer_auth(strata_fake::rag_token(a))
        .header("x-tenant-id", b.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "tenant_mismatch");
    // no token, an unknown token
    for token in [None, Some("rag-not-a-pair")] {
        let mut req = http.get(&url).header("x-tenant-id", a.to_string());
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 401, "{token:?}");
    }
    // ingest: b's job is invisible to a's token (404), however the request names the tenant
    let pb = plane(&e, b, Service::Ingest).await;
    let job = ingest::create_json(
        &e.state.http,
        &pb,
        json!({"store_id": Uuid::new_v4(), "url": "https://example.com/b"}),
    )
    .await
    .unwrap()
    .jobs[0]
        .job_id;
    let r = http
        .get(format!("{}/api/ingest/jobs/{job}", fake.ingest_url))
        .bearer_auth(strata_fake::ingest_token(a))
        .header("x-tenant-id", b.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let pa = plane(&e, a, Service::Ingest).await;
    let err = ingest::get(&e.state.http, &pa, job).await.unwrap_err();
    assert_eq!(err.status_and_code(), (StatusCode::NOT_FOUND, "not_found"));
    ingest::get(&e.state.http, &pb, job).await.expect("b's own");
}

#[tokio::test]
async fn every_query_and_eval_request_carries_root_id() {
    let a = Uuid::new_v4();
    let (e, fake) = setup(&[a], &[]).await;
    let (r1, r2, leaf) = {
        let mut g = lock(&fake.rag);
        let r1 = g.add_store(a, None, "router", "faq");
        let r2 = g.add_store(a, None, "router", "faq-gen2");
        let leaf = g.add_store(a, Some(r1), "leaf", "policy");
        (r1, r2, leaf)
    };
    // the fake enforces D91 like strata: no root with two roots is 409 ambiguous_root
    let raw = reqwest::Client::new()
        .post(format!("{}/api/query", fake.rag_url))
        .bearer_auth(strata_fake::rag_token(a))
        .json(&json!({"query": "hours"}))
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), 409);
    let body: Value = raw.json().await.unwrap();
    assert_eq!(body["error"]["code"], "ambiguous_root");
    let before = fake.rag_requests().len();
    let p = plane(&e, a, Service::Rag).await;
    let http = &e.state.http;
    for target in [
        QueryTarget::Root(StrataStoreId(r1)),
        QueryTarget::Root(StrataStoreId(r2)),
        QueryTarget::Roots(vec![StrataStoreId(leaf), StrataStoreId(r2)]),
        QueryTarget::Leaves(vec![StrataStoreId(leaf)]),
    ] {
        rag::query(http, &p, &target, &q("opening hours"))
            .await
            .unwrap_or_else(|err| panic!("{target:?}: {err:?}"));
    }
    lock(&fake.rag)
        .questions
        .push((a, json!({"id": Uuid::new_v4(), "query": "q1"})));
    rag::eval_run(http, &p, StrataStoreId(r2), &rag::EvalReq::default())
        .await
        .unwrap();
    // an empty leaf set makes no call
    rag::query(http, &p, &QueryTarget::Roots(Vec::new()), &q("x"))
        .await
        .unwrap();
    let sent: Vec<_> = fake.rag_requests().into_iter().skip(before).collect();
    assert_eq!(sent.len(), 5, "{sent:?}");
    for r in &sent {
        assert!(r.path == "/api/query" || r.path == "/api/eval", "{r:?}");
        let named: Vec<&str> = ["root_id", "root_ids", "leaf_ids"]
            .into_iter()
            .filter(|k| r.body.get(*k).is_some_and(|v| !v.is_null()))
            .collect();
        assert_eq!(named.len(), 1, "exactly one target field: {}", r.body);
        for k in ["supersedes", "platform_penalty", "platform_min_score"] {
            assert!(r.body.get(k).is_none(), "{k} is never sent: {}", r.body);
        }
    }
    let eval = sent.iter().find(|r| r.path == "/api/eval").unwrap();
    assert_eq!(eval.body["root_id"], json!(r2));
    assert_eq!(eval.body["source"], "tenant");
    assert_eq!(sent[0].body["root_id"], json!(r1));
    assert_eq!(sent[2].body["root_ids"], json!([leaf, r2]));
    assert_eq!(sent[3].body["leaf_ids"], json!([leaf]));
}

#[tokio::test]
async fn ambiguous_root_passthrough_409_and_counted() {
    let a = Uuid::new_v4();
    let (e, fake) = setup(&[a], &[]).await;
    let root = lock(&fake.rag).add_store(a, None, "router", "faq");
    let p = plane(&e, a, Service::Rag).await;
    let ambiguous = json!({"error": {"code": "ambiguous_root", "message": "ambiguous root: this tenant has 2 root stores"}});
    for (op, path) in [("query", "/api/query"), ("eval_run", "/api/eval")] {
        let before = ambiguous_count(op);
        lock(&fake.rag).script("POST", path, 409, ambiguous.clone());
        let err = match op {
            "query" => rag::query(
                &e.state.http,
                &p,
                &QueryTarget::Root(StrataStoreId(root)),
                &q("hours"),
            )
            .await
            .map(|_| ()),
            _ => rag::eval_run(
                &e.state.http,
                &p,
                StrataStoreId(root),
                &rag::EvalReq::default(),
            )
            .await
            .map(|_| ()),
        }
        .unwrap_err();
        assert_eq!(
            err.status_and_code(),
            (StatusCode::CONFLICT, "ambiguous_root"),
            "{op}"
        );
        assert!(
            ambiguous_count(op) >= before + 1.0,
            "{op}: counted ({before} → {})",
            ambiguous_count(op)
        );
    }
    // any other refusal is not counted as ambiguous
    let before = ambiguous_count("query");
    lock(&fake.rag).script(
        "POST",
        "/api/query",
        409,
        json!({"error": {"code": "conflict", "message": "conflict: retry"}}),
    );
    let err = rag::query(
        &e.state.http,
        &p,
        &QueryTarget::Root(StrataStoreId(root)),
        &q("hours"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert_eq!(ambiguous_count("query"), before);
}

#[tokio::test]
async fn query_response_accepts_mode_used_null_and_layer() {
    let a = Uuid::new_v4();
    let (e, fake) = setup(&[a], &[]).await;
    let (leaf, pack_root) = {
        let mut g = lock(&fake.rag);
        let root = g.add_store(a, None, "router", "faq");
        let leaf = g.add_store(a, Some(root), "leaf", "policy");
        let pack_root = g.add_pack("base", 1);
        let (c1, c2, d) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        g.hits = vec![
            // a tenant hit as older strata sent it: no `layer`
            json!({
                "chunk_id": c1, "document_id": d, "store_path": ["faq", "policy"], "ord": 0,
                "text": "We open at 9.", "page_start": null, "page_end": null, "source": "upload",
                "title": "Hours", "scores": {"dense": 0.8, "lexical": null, "rerank": 0.9},
                "source_id": "upload:1", "source_run": null, "metadata": {}
            }),
            json!({
                "chunk_id": c2, "document_id": Uuid::new_v4(), "store_path": ["base", "hours"],
                "layer": "platform", "pack": "base@1", "ord": 3, "text": "Typical hours are 9-5.",
                "page_start": 1, "page_end": 2, "source": "platform", "title": null,
                "scores": {"dense": 0.7, "lexical": 0.2, "rerank": null}, "source_id": null,
                "source_run": null, "metadata": {"provenance": {"license": "cc-by-4.0"}}
            }),
        ];
        (leaf, pack_root)
    };
    let p = plane(&e, a, Service::Rag).await;
    let r = rag::query(
        &e.state.http,
        &p,
        &QueryTarget::Leaves(vec![StrataStoreId(leaf), StrataStoreId(pack_root)]),
        &q("hours"),
    )
    .await
    .unwrap();
    assert_eq!(r.routing.mode_used, None, "leaf_ids routes nothing");
    assert_eq!(r.timings_ms.total, 11);
    assert_eq!(r.timings_by_model.len(), 1, "extra timing fields tolerated");
    assert_eq!(r.results.len(), 2);
    assert_eq!(r.results[0].layer, rag::Layer::Tenant);
    assert_eq!(r.results[0].pack, None);
    assert_eq!(r.results[1].layer, rag::Layer::Platform);
    assert_eq!(r.results[1].pack.as_deref(), Some("base@1"));
    assert_eq!(r.results[1].metadata["provenance"]["license"], "cc-by-4.0");
    let routed = rag::query(
        &e.state.http,
        &p,
        &QueryTarget::Root(StrataStoreId(leaf)),
        &q("hours"),
    )
    .await
    .unwrap();
    assert_eq!(routed.routing.mode_used, Some(rag::RouteMode::Hybrid));
}

/// R10/R11 client half: a pack root the tenant can no longer read answers 404 naming the store, told apart from
/// other errors; the pack listing reads 404 (platform knowledge off) as no packs.
#[tokio::test]
async fn unreadable_pack_root_is_a_distinct_404_and_packs_off_is_empty() {
    let a = Uuid::new_v4();
    let (e, fake) = setup(&[a], &[]).await;
    let (leaf, pack_root) = {
        let mut g = lock(&fake.rag);
        let root = g.add_store(a, None, "router", "faq");
        let leaf = g.add_store(a, Some(root), "leaf", "policy");
        let pack_root = g.add_pack("industry/retail", 2);
        (leaf, pack_root)
    };
    let p = plane(&e, a, Service::Rag).await;
    let packs = rag::platform_packs(&e.state.http, &p).await.unwrap();
    assert_eq!(packs.len(), 1);
    assert_eq!(packs[0].root_id, StrataStoreId(pack_root));
    assert_eq!(
        (packs[0].key.as_str(), packs[0].version),
        ("industry/retail", 2)
    );
    lock(&fake.rag).unreadable.insert(pack_root);
    let target = QueryTarget::Roots(vec![StrataStoreId(leaf), StrataStoreId(pack_root)]);
    let err = rag::query(&e.state.http, &p, &target, &q("returns"))
        .await
        .unwrap_err();
    assert_eq!(err.status_and_code(), (StatusCode::NOT_FOUND, "not_found"));
    assert_eq!(rag::unreadable_store(&err), Some(StrataStoreId(pack_root)));
    // another error names no store
    let other = rag::query(&e.state.http, &p, &target, &q("   "))
        .await
        .unwrap_err();
    assert_eq!(rag::unreadable_store(&other), None);
    lock(&fake.rag).platform_off = true;
    assert!(
        rag::platform_packs(&e.state.http, &p)
            .await
            .unwrap()
            .is_empty()
    );
}

// ---- per-tenant strata licences (tenant_app_licence): static pair, then the tenant's own token, then pending ------

/// Store `token` as `tenant`'s licence for `app_key` as the fake bundle minter (common::minter, shared by every test env) hands it out on demand.
async fn seed_licence(_e: &TestEnv, tenant: Uuid, app_key: &str, token: &str) {
    common::minter::grant_shared(tenant, app_key, token);
}

#[tokio::test]
async fn a_tenant_without_a_static_pair_uses_its_own_strata_licences() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    // `a` has a static pair; `b` has none, but the fakes know the tokens of its licences
    let (e, fake) = setup(&[a], &[b]).await;
    fake.pin(&[b]);
    assert_eq!(
        gate(&e, b, Service::Rag)
            .await
            .unwrap_err()
            .status_and_code(),
        (StatusCode::CONFLICT, "provision_pending"),
        "no pair and no licence"
    );
    seed_licence(&e, b, "strata-rag", &strata_fake::rag_token(b)).await;
    seed_licence(&e, b, "strata-ingest", &strata_fake::ingest_token(b)).await;
    for svc in [Service::Rag, Service::Ingest] {
        gate(&e, b, svc).await.expect("b's own licence");
    }
    let p = plane(&e, b, Service::Rag).await;
    rag::stores(&e.state.http, &p).await.expect("stores");
    let seen = fake.rag_requests();
    let last = seen.last().expect("a request");
    assert_eq!(
        last.token.as_deref(),
        Some(strata_fake::rag_token(b).as_str())
    );
    let shown = format!("{p:?}");
    assert!(!shown.contains(&strata_fake::rag_token(b)), "{shown}");
    // the sibling without a licence is still pending: b's rows are b's alone
    let c = Uuid::new_v4();
    insert_tenant(&e.owner, c, "c").await;
    assert_eq!(
        gate(&e, c, Service::Rag).await.unwrap_err().code(),
        "provision_pending"
    );
}

#[tokio::test]
async fn the_static_strata_pair_wins_over_the_tenants_licence() {
    let a = Uuid::new_v4();
    let (e, fake) = setup(&[a], &[]).await;
    // a licence the fakes do not know: were it sent, strata would answer 401
    seed_licence(&e, a, "strata-rag", "licence-token-not-pinned").await;
    let p = plane(&e, a, Service::Rag).await;
    rag::stores(&e.state.http, &p)
        .await
        .expect("static pair used");
    let seen = fake.rag_requests();
    let last = seen.last().expect("a request");
    assert_eq!(
        last.token.as_deref(),
        Some(strata_fake::rag_token(a).as_str())
    );
    assert!(!format!("{p:?}").contains("licence-token-not-pinned"));
}
