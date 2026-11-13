//! PK T2: the strata_rag platform clients (`strata::rag` packs, pack stores, exclusions, owner-only pack writes, JSON
//! ingest) against the in-process fake (`common::strata_fake`, which mirrors strata_rag `src/api/platform.rs`): the
//! path and its encoding (`industry%2Fretail`), the body, 404 → `None` / empty, platform-off → 503
//! `platform_knowledge_off`, 403 → `platform_owner_required`, and 409 / 422 passthrough. No database: the planes come
//! from a `StrataConfig` built here.
use crate::common::strata_fake::{self, StrataFake, lock};
use agent_api::config::{StrataConfig, TenantTokens};
use agent_api::error::ApiError;
use agent_api::strata::rag::{self, DocumentIn, Provenance, QueryReq, QueryTarget, StoreKind};
use agent_api::strata::{self, Plane, Service, StrataStoreId};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

/// A fake (owner `owner`) and a config pinning a rag token for each of `tenants`.
async fn setup(owner: Uuid, tenants: &[Uuid]) -> (StrataFake, StrataConfig) {
    let fake = strata_fake::start().await;
    lock(&fake.rag).platform_owner = Some(owner);
    let mut all = vec![owner];
    all.extend_from_slice(tenants);
    fake.pin(&all);
    let pairs = all
        .iter()
        .map(|t| format!("{t}:{}", strata_fake::rag_token(*t)))
        .collect::<Vec<_>>()
        .join(",");
    let cfg = StrataConfig {
        rag_url: Some(fake.rag_url.clone()),
        rag_tokens: TenantTokens::parse("STRATA_RAG_TOKENS", &pairs).unwrap(),
        ..StrataConfig::default()
    };
    (fake, cfg)
}

fn plane(cfg: &StrataConfig, t: Uuid) -> Plane<'_> {
    strata::static_plane(cfg, t, Service::Rag).expect("plane")
}

fn last(fake: &StrataFake) -> strata_fake::Recorded {
    fake.rag_requests().pop().expect("a request")
}

fn prov(license: &str) -> Provenance {
    Provenance {
        source_url: "https://example.org/retail-handbook".into(),
        license: license.into(),
        retrieved_at: "2026-10-21T08:00:00Z".into(),
    }
}

/// Owner-only register / publish / retire: paths, encoding, bodies; the owner listing holds drafts and retired
/// packs, a tenant listing neither; a non-owner is 403 `platform_owner_required`; strata's 409 / 422 pass through.
#[tokio::test]
async fn pack_writes_are_owner_only_and_listings_differ() {
    let (owner, a) = (Uuid::new_v4(), Uuid::new_v4());
    let (fake, cfg) = setup(owner, &[a]).await;
    let http = reqwest::Client::new();
    let (po, pa) = (plane(&cfg, owner), plane(&cfg, a));
    let (r1, r2, r3, hr) = {
        let mut g = lock(&fake.rag);
        let r1 = g.add_store(owner, None, "router", "retail-v1");
        let hr = g.add_store(owner, Some(r1), "leaf", "hr");
        (
            r1,
            g.add_store(owner, None, "router", "retail-v2"),
            g.add_store(owner, None, "router", "retail-v3"),
            hr,
        )
    };

    // a store with a parent cannot root a pack: strata's 422 invalid_target is the pack's own code and message,
    // never the connector-URL one (T10)
    let non_root = rag::pack_register(&http, &po, "industry/retail", 1, StrataStoreId(hr))
        .await
        .unwrap_err();
    assert_eq!(
        non_root.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_pack_root")
    );
    let ApiError::Unprocessable(_, m) = &non_root else {
        panic!("{non_root:?}")
    };
    assert!(m.contains("top-level store") && !m.contains("https"), "{m}");

    // a tenant may not register
    let err = rag::pack_register(&http, &pa, "industry/retail", 1, StrataStoreId(r1))
        .await
        .unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::FORBIDDEN, "platform_owner_required")
    );

    let draft = rag::pack_register(&http, &po, "industry/retail", 1, StrataStoreId(r1))
        .await
        .unwrap();
    let req = last(&fake);
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", "/api/platform/packs")
    );
    assert_eq!(
        req.body,
        json!({"key": "industry/retail", "version": 1, "root_id": r1})
    );
    assert_eq!(draft.published_at, None, "registered as a draft");
    assert_eq!(draft.root_id, StrataStoreId(r1));

    // duplicate key@version: strata's 409 duplicate_name passes through
    let dup = rag::pack_register(&http, &po, "industry/retail", 1, StrataStoreId(r2))
        .await
        .unwrap_err();
    assert_eq!(
        dup.status_and_code(),
        (StatusCode::CONFLICT, "duplicate_name")
    );
    // retiring a draft: strata's 422 invalid_target is 422 pack_not_published with the pack's own message, not the
    // connector-URL "public https address" one (T10)
    let retire_draft = rag::pack_retire(&http, &po, "industry/retail", 1)
        .await
        .unwrap_err();
    assert_eq!(
        retire_draft.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "pack_not_published")
    );
    let ApiError::Unprocessable(_, m) = &retire_draft else {
        panic!("{retire_draft:?}")
    };
    assert!(
        m.contains("industry/retail@1 is a draft")
            && m.contains("only a published pack can be retired")
            && !m.contains("https"),
        "{m}"
    );
    assert_eq!(
        last(&fake).path,
        "/api/platform/packs/industry%2Fretail/1/retire"
    );

    // a tenant may not publish either; the pack stays a draft
    let err = rag::pack_publish(&http, &pa, "industry/retail", 1)
        .await
        .unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::FORBIDDEN, "platform_owner_required")
    );
    assert!(
        rag::platform_packs(&http, &pa).await.unwrap().is_empty(),
        "a draft is not in a tenant listing"
    );

    let published = rag::pack_publish(&http, &po, "industry/retail", 1)
        .await
        .unwrap();
    let req = last(&fake);
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", "/api/platform/packs/industry%2Fretail/1/publish")
    );
    assert!(published.published_at.is_some());
    let again = rag::pack_publish(&http, &po, "industry/retail", 1)
        .await
        .unwrap();
    assert_eq!(again.published_at, published.published_at, "idempotent");

    // v2 stays a draft; v3 is published, then retired
    rag::pack_register(&http, &po, "industry/retail", 2, StrataStoreId(r2))
        .await
        .unwrap();
    rag::pack_register(&http, &po, "industry/retail", 3, StrataStoreId(r3))
        .await
        .unwrap();
    rag::pack_publish(&http, &po, "industry/retail", 3)
        .await
        .unwrap();
    let retired = rag::pack_retire(&http, &po, "industry/retail", 3)
        .await
        .unwrap();
    assert!(retired.retired_at.is_some());

    let mut owner_view: Vec<i32> = rag::platform_packs(&http, &po)
        .await
        .unwrap()
        .iter()
        .map(|p| p.version)
        .collect();
    owner_view.sort_unstable();
    assert_eq!(
        owner_view,
        [1, 2, 3],
        "the owner listing holds drafts and retired packs"
    );
    let tenant_view = rag::platform_packs(&http, &pa).await.unwrap();
    assert_eq!(
        tenant_view.iter().map(|p| p.version).collect::<Vec<_>>(),
        [1],
        "a tenant listing: published and not retired only"
    );
}

/// `…/{key}/{version}/stores`: the encoded path, the subtree (root first, paths, leaves, embed model), and every
/// unreadable answer as `None` (draft for a tenant, missing, platform off); a retired pack stays readable; a draft is
/// readable by the owner.
#[tokio::test]
async fn pack_stores_lists_the_subtree_or_none() {
    let (owner, a) = (Uuid::new_v4(), Uuid::new_v4());
    let (fake, cfg) = setup(owner, &[a]).await;
    let http = reqwest::Client::new();
    let (po, pa) = (plane(&cfg, owner), plane(&cfg, a));
    let (root, leaves, draft_root) = {
        let mut g = lock(&fake.rag);
        let (root, leaves) = g.add_owned_pack("industry/retail", 1, &["returns", "hr"], true);
        let (draft_root, _) = g.add_owned_pack("industry/retail", 2, &["hr"], false);
        (root, leaves, draft_root)
    };

    let ps = rag::pack_stores(&http, &pa, "industry/retail", 1)
        .await
        .unwrap()
        .expect("published pack is readable");
    let req = last(&fake);
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("GET", "/api/platform/packs/industry%2Fretail/1/stores")
    );
    assert_eq!(req.tenant_header.as_deref(), Some(a.to_string().as_str()));
    assert_eq!(ps.pack, "industry/retail@1");
    assert_eq!(ps.root_id, StrataStoreId(root));
    assert_eq!(ps.stores.len(), 3);
    assert_eq!(ps.stores[0].id, StrataStoreId(root), "root first");
    assert_eq!(ps.stores[0].kind, StoreKind::Router);
    assert_eq!(ps.stores[0].parent_id, None);
    assert_eq!(ps.stores[0].path, ["industry/retail@1"]);
    // ordered by name path: hr before returns
    assert_eq!(ps.stores[1].path, ["industry/retail@1", "hr"]);
    assert_eq!(ps.stores[1].id, StrataStoreId(leaves[1]));
    assert_eq!(ps.stores[1].parent_id, Some(StrataStoreId(root)));
    assert_eq!(ps.stores[1].embed_model, strata_fake::DEFAULT_EMBED);
    assert_eq!(
        ps.leaf_ids(),
        [StrataStoreId(leaves[1]), StrataStoreId(leaves[0])]
    );

    // a draft: None for a tenant, Some for the owner
    assert_eq!(
        rag::pack_stores(&http, &pa, "industry/retail", 2)
            .await
            .unwrap(),
        None
    );
    let own = rag::pack_stores(&http, &po, "industry/retail", 2)
        .await
        .unwrap()
        .expect("the owner reads drafts");
    assert_eq!(own.root_id, StrataStoreId(draft_root));
    // missing
    assert_eq!(
        rag::pack_stores(&http, &pa, "industry/wholesale", 1)
            .await
            .unwrap(),
        None
    );
    // retired: gone from the listing, still readable (pins keep working)
    rag::pack_retire(&http, &po, "industry/retail", 1)
        .await
        .unwrap();
    assert!(rag::platform_packs(&http, &pa).await.unwrap().is_empty());
    assert!(
        rag::pack_stores(&http, &pa, "industry/retail", 1)
            .await
            .unwrap()
            .is_some()
    );
    // platform knowledge off
    lock(&fake.rag).platform_off = true;
    assert_eq!(
        rag::pack_stores(&http, &pa, "industry/retail", 1)
            .await
            .unwrap(),
        None
    );
}

/// Exclusions: PUT body, GET list, DELETE `?key_prefix=` encoded, 404 for an unknown prefix; segment-wise prefix
/// semantics (`industry/retail` hides `industry/retail/hr`, not `industry/retailx`) across listing, `…/stores` and a
/// `leaf_ids` query (404 naming the store); another tenant is unaffected.
#[tokio::test]
async fn exclusions_hide_packs_by_segment_prefix() {
    let (owner, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (fake, cfg) = setup(owner, &[a, b]).await;
    let http = reqwest::Client::new();
    let (pa, pb) = (plane(&cfg, a), plane(&cfg, b));
    let (hr_leaf, x_leaf, own_leaf) = {
        let mut g = lock(&fake.rag);
        let (_, hr) = g.add_owned_pack("industry/retail/hr", 1, &["policies"], true);
        let (_, x) = g.add_owned_pack("industry/retailx", 1, &["guides"], true);
        g.add_owned_pack("industry/retail", 4, &["basics"], true);
        let root = g.add_store(a, None, "router", "faq");
        let own = g.add_store(a, Some(root), "leaf", "policy");
        (hr[0], x[0], own)
    };
    assert!(rag::exclusions(&http, &pa).await.unwrap().is_empty());

    let x = rag::exclusion_put(&http, &pa, "industry/retail", "regulated in our market")
        .await
        .unwrap();
    let req = last(&fake);
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("PUT", "/api/platform/exclusions")
    );
    assert_eq!(
        req.body,
        json!({"key_prefix": "industry/retail", "reason": "regulated in our market"})
    );
    assert_eq!(x.key_prefix, "industry/retail");
    assert_eq!(x.reason, "regulated in our market");
    assert!(!x.created_at.is_empty());
    // a prefix that is not a pack key: strata's 422 invalid_input passes through
    let bad = rag::exclusion_put(&http, &pa, "Industry", "r")
        .await
        .unwrap_err();
    assert_eq!(
        bad.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
    );

    let listed = rag::exclusions(&http, &pa).await.unwrap();
    assert_eq!(listed, [x]);
    let keys: Vec<String> = rag::platform_packs(&http, &pa)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.key)
        .collect();
    assert_eq!(
        keys,
        ["industry/retailx"],
        "the prefix hides itself and below, not retailx"
    );
    assert_eq!(
        rag::pack_stores(&http, &pa, "industry/retail/hr", 1)
            .await
            .unwrap(),
        None
    );
    assert!(
        rag::pack_stores(&http, &pa, "industry/retailx", 1)
            .await
            .unwrap()
            .is_some()
    );
    // leaf_ids naming an excluded pack's leaf: 404 naming that store
    let req = QueryReq {
        query: "leave policy".into(),
        ..QueryReq::default()
    };
    let target = QueryTarget::Leaves(vec![StrataStoreId(own_leaf), StrataStoreId(hr_leaf)]);
    let err = rag::query(&http, &pa, &target, &req).await.unwrap_err();
    assert_eq!(err.status_and_code(), (StatusCode::NOT_FOUND, "not_found"));
    assert_eq!(rag::unreadable_store(&err), Some(StrataStoreId(hr_leaf)));
    // tenant leaf + a readable pack leaf: searched as exactly those leaves
    let ok = rag::query(
        &http,
        &pa,
        &QueryTarget::Leaves(vec![StrataStoreId(own_leaf), StrataStoreId(x_leaf)]),
        &req,
    )
    .await
    .unwrap();
    assert_eq!(
        ok.routing.leaves,
        [StrataStoreId(own_leaf), StrataStoreId(x_leaf)]
    );
    let sent = last(&fake).body;
    assert_eq!(sent["leaf_ids"], json!([own_leaf, x_leaf]));
    assert!(sent.get("root_id").is_none() && sent.get("root_ids").is_none());
    // tenant b is not excluded
    assert_eq!(rag::platform_packs(&http, &pb).await.unwrap().len(), 3);
    rag::query(
        &http,
        &pb,
        &QueryTarget::Leaves(vec![StrataStoreId(hr_leaf)]),
        &req,
    )
    .await
    .expect("b reads industry/retail/hr");

    rag::exclusion_delete(&http, &pa, "industry/retail")
        .await
        .unwrap();
    let req = last(&fake);
    assert_eq!(
        (req.method.as_str(), req.path.as_str(), req.query.as_str()),
        (
            "DELETE",
            "/api/platform/exclusions",
            "key_prefix=industry%2Fretail"
        )
    );
    assert!(rag::exclusions(&http, &pa).await.unwrap().is_empty());
    assert_eq!(rag::platform_packs(&http, &pa).await.unwrap().len(), 3);
    // deleting it again: strata's 404 not_found passes through (platform knowledge is on)
    let gone = rag::exclusion_delete(&http, &pa, "industry/retail")
        .await
        .unwrap_err();
    assert_eq!(gone.status_and_code(), (StatusCode::NOT_FOUND, "not_found"));
}

/// Platform knowledge off (strata's `STRATA_PLATFORM_OWNER` unset): reads are empty / `None`, writes are 503
/// `platform_knowledge_off`.
#[tokio::test]
async fn platform_off_reads_empty_and_writes_503() {
    let (owner, a) = (Uuid::new_v4(), Uuid::new_v4());
    let (fake, cfg) = setup(owner, &[a]).await;
    let http = reqwest::Client::new();
    let (po, pa) = (plane(&cfg, owner), plane(&cfg, a));
    let root = {
        let mut g = lock(&fake.rag);
        g.add_owned_pack("base", 1, &["general"], true);
        g.platform_off = true;
        g.add_store(owner, None, "router", "base-v2")
    };
    assert!(rag::platform_packs(&http, &pa).await.unwrap().is_empty());
    assert!(rag::exclusions(&http, &pa).await.unwrap().is_empty());
    assert_eq!(rag::pack_stores(&http, &pa, "base", 1).await.unwrap(), None);
    let off = (StatusCode::SERVICE_UNAVAILABLE, "platform_knowledge_off");
    assert_eq!(
        rag::exclusion_put(&http, &pa, "base", "r")
            .await
            .unwrap_err()
            .status_and_code(),
        off
    );
    assert_eq!(
        rag::exclusion_delete(&http, &pa, "base")
            .await
            .unwrap_err()
            .status_and_code(),
        off
    );
    assert_eq!(
        rag::pack_register(&http, &po, "base", 2, StrataStoreId(root))
            .await
            .unwrap_err()
            .status_and_code(),
        off
    );
    assert_eq!(
        rag::pack_publish(&http, &po, "base", 1)
            .await
            .unwrap_err()
            .status_and_code(),
        off
    );
    assert_eq!(
        rag::pack_retire(&http, &po, "base", 1)
            .await
            .unwrap_err()
            .status_and_code(),
        off
    );
}

/// JSON ingest: the body (topic_keys, supersedes, provenance; empty lists left out), 422 with strata's own message
/// for missing provenance or a licence off the allow-list, 201 `created` with an allowed licence, 409
/// `pack_published` once the pack is published (also for store writes in its subtree); a tenant store takes
/// `supersedes` without provenance.
#[tokio::test]
async fn json_ingest_enforces_provenance_and_the_freeze() {
    let (owner, a) = (Uuid::new_v4(), Uuid::new_v4());
    let (fake, cfg) = setup(owner, &[a]).await;
    let http = reqwest::Client::new();
    let (po, pa) = (plane(&cfg, owner), plane(&cfg, a));
    let (root, leaf) = {
        let mut g = lock(&fake.rag);
        let (root, leaves) = g.add_owned_pack("industry/retail", 1, &["returns"], false);
        (root, leaves[0])
    };
    let doc = |provenance: Option<Provenance>| DocumentIn {
        text: "Customers may return goods within 30 days.".into(),
        title: Some("Returns".into()),
        source_id: Some("handbook:returns".into()),
        topic_keys: vec!["retail.returns.window".into()],
        provenance,
        ..DocumentIn::default()
    };

    let missing = rag::ingest_document_json(&http, &po, StrataStoreId(leaf), &doc(None))
        .await
        .unwrap_err();
    assert_eq!(
        missing.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
    );
    assert!(
        missing.to_string().contains("provenance"),
        "strata's message is kept: {missing}"
    );
    let req = last(&fake);
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", format!("/api/stores/{leaf}/documents").as_str())
    );
    assert_eq!(req.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        req.body,
        json!({
            "text": "Customers may return goods within 30 days.", "title": "Returns",
            "source_id": "handbook:returns", "topic_keys": ["retail.returns.window"]
        }),
        "unset fields and empty lists are left out"
    );

    let nc = rag::ingest_document_json(
        &http,
        &po,
        StrataStoreId(leaf),
        &doc(Some(prov("CC-BY-NC-4.0"))),
    )
    .await
    .unwrap_err();
    assert_eq!(
        nc.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
    );
    assert!(nc.to_string().contains("not allowed"), "{nc}");

    let created = rag::ingest_document_json(
        &http,
        &po,
        StrataStoreId(leaf),
        &doc(Some(prov("CC-BY-4.0"))),
    )
    .await
    .unwrap();
    assert_eq!(created["status"], "created");
    assert_eq!(created["topic_keys"], json!(["retail.returns.window"]));
    assert_eq!(
        last(&fake).body["provenance"],
        json!({
            "source_url": "https://example.org/retail-handbook", "license": "CC-BY-4.0",
            "retrieved_at": "2026-10-21T08:00:00Z"
        })
    );

    // published: the subtree is frozen
    rag::pack_publish(&http, &po, "industry/retail", 1)
        .await
        .unwrap();
    let frozen = rag::ingest_document_json(
        &http,
        &po,
        StrataStoreId(leaf),
        &doc(Some(prov("CC-BY-4.0"))),
    )
    .await
    .unwrap_err();
    assert_eq!(
        frozen.status_and_code(),
        (StatusCode::CONFLICT, "pack_published")
    );
    let child = rag::create_store(
        &http,
        &po,
        &rag::NewStore {
            parent_id: Some(StrataStoreId(root)),
            kind: StoreKind::Leaf,
            name: "exchanges".into(),
            description: "exchanges".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        child.status_and_code(),
        (StatusCode::CONFLICT, "pack_published")
    );
    let renamed = rag::update_store(&http, &po, StrataStoreId(leaf), Some("refunds"), None)
        .await
        .unwrap_err();
    assert_eq!(
        renamed.status_and_code(),
        (StatusCode::CONFLICT, "pack_published")
    );

    // a tenant document: supersedes, no provenance needed
    let tleaf = {
        let mut g = lock(&fake.rag);
        let r = g.add_store(a, None, "router", "faq");
        g.add_store(a, Some(r), "leaf", "returns")
    };
    let mine = rag::ingest_document_json(
        &http,
        &pa,
        StrataStoreId(tleaf),
        &DocumentIn {
            text: "Our window is 14 days.".into(),
            supersedes: vec!["retail.returns.window".into()],
            ..DocumentIn::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(mine["status"], "created");
    assert_eq!(
        last(&fake).body,
        json!({"text": "Our window is 14 days.", "supersedes": ["retail.returns.window"]})
    );
}
