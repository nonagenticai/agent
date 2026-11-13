//! B8 leased claims (migration 0190): a grid run / discovery import is run by one lease holder, a live lease keeps
//! the boot / sweep readers away, an expired one is claimable again. Real Postgres 18; the fixtures are
//! `discovery_import`'s. Rows are inserted straight through the owner pool so no upload worker races the assertions.
use std::sync::Arc;

use agent_api::discovery::worker::{self, Outcome};
use agent_api::validation::grid::{self, GridOutcome};
use uuid::Uuid;

use crate::b8::discovery_import::{Fake, Fx, import_ref, setup};
use crate::common::design_contract;

const A: &str = "replica-a:1";
const B: &str = "replica-b:2";
const TTL: i32 = 600;

/// A `queued` grid run of a published release of the fixture job.
async fn grid_row(x: &Fx) -> Uuid {
    let rel = design_contract::publish(&x.e, &x.f).await;
    sqlx::query_scalar(
        "INSERT INTO validation_grid_run (tenant_id, job_id, release_id, trigger, status, total, by) \
         VALUES ($1, $2, $3, 'manual', 'queued', 0, 'u-lease') RETURNING id",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(&rel)
    .fetch_one(&x.e.owner)
    .await
    .unwrap()
}

/// A `classifying` import with no rows (a worker over it classifies nothing and finishes it).
async fn import_row(x: &Fx) -> String {
    sqlx::query_scalar(
        "INSERT INTO discovery_import (tenant_id, job_id, file, rows, distinct_rows, uploaded_by) \
         VALUES ($1, $2, 't.csv', 0, 0, 'u-lease') RETURNING id",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap()
}

async fn claim_grid(x: &Fx, id: Uuid, owner: &str) -> bool {
    sqlx::query_scalar("SELECT validation_grid_claim($1, $2, $3)")
        .bind(id)
        .bind(owner)
        .bind(TTL)
        .fetch_one(&x.e.state.db)
        .await
        .unwrap()
}

async fn claim_import(x: &Fx, id: &str, owner: &str) -> bool {
    sqlx::query_scalar("SELECT discovery_import_claim($1, $2, $3)")
        .bind(id)
        .bind(owner)
        .bind(TTL)
        .fetch_one(&x.e.state.db)
        .await
        .unwrap()
}

async fn expire(x: &Fx, table: &str, id: &str) {
    let sql =
        format!("UPDATE {table} SET lease_until = now() - interval '1 minute' WHERE id::text = $1");
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .execute(&x.e.owner)
        .await
        .unwrap();
}

async fn active_grids(x: &Fx) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT id FROM validation_grid_active()")
        .fetch_all(&x.e.state.db)
        .await
        .unwrap()
}

async fn classifying(x: &Fx) -> Vec<String> {
    sqlx::query_scalar("SELECT id FROM discovery_classifying_imports()")
        .fetch_all(&x.e.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn grid_claim_is_exclusive_until_the_lease_lapses_or_is_released() {
    let x = setup().await;
    let id = grid_row(&x).await;
    assert!(claim_grid(&x, id, A).await);
    assert!(!claim_grid(&x, id, B).await, "a live lease is refused");
    assert!(!claim_grid(&x, id, A).await, "not even to its own holder");
    let renew = |owner: &'static str| {
        let db = x.e.state.db.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT validation_grid_renew($1, $2, $3)")
                .bind(id)
                .bind(owner)
                .bind(TTL)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert!(renew(A).await);
    assert!(!renew(B).await, "only the holder renews");
    expire(&x, "validation_grid_run", &id.to_string()).await;
    assert!(claim_grid(&x, id, B).await, "an expired lease is claimable");
    assert!(
        !renew(A).await,
        "the former holder lost it: renewal says stop"
    );
    let released: bool = sqlx::query_scalar("SELECT validation_grid_release($1, $2)")
        .bind(id)
        .bind(A)
        .fetch_one(&x.e.state.db)
        .await
        .unwrap();
    assert!(!released, "a non-holder releases nothing");
    let released: bool = sqlx::query_scalar("SELECT validation_grid_release($1, $2)")
        .bind(id)
        .bind(B)
        .fetch_one(&x.e.state.db)
        .await
        .unwrap();
    assert!(released);
    assert!(claim_grid(&x, id, A).await, "a released row is claimable");
}

#[tokio::test]
async fn import_claim_is_exclusive_until_the_lease_lapses() {
    let x = setup().await;
    let id = import_row(&x).await;
    assert!(claim_import(&x, &id, A).await);
    assert!(!claim_import(&x, &id, B).await, "a live lease is refused");
    expire(&x, "discovery_import", &id).await;
    assert!(
        claim_import(&x, &id, B).await,
        "an expired lease is claimable"
    );
    let renewed: bool = sqlx::query_scalar("SELECT discovery_import_renew($1, $2, $3)")
        .bind(&id)
        .bind(A)
        .bind(TTL)
        .fetch_one(&x.e.state.db)
        .await
        .unwrap();
    assert!(!renewed, "the former holder lost it");
}

#[tokio::test]
async fn readers_skip_a_live_lease_and_list_an_expired_one() {
    let x = setup().await;
    let g = grid_row(&x).await;
    let i = import_row(&x).await;
    assert_eq!(active_grids(&x).await, [g]);
    assert_eq!(classifying(&x).await, std::slice::from_ref(&i));
    assert!(claim_grid(&x, g, A).await);
    assert!(claim_import(&x, &i, A).await);
    assert!(
        active_grids(&x).await.is_empty(),
        "a held grid run is skipped"
    );
    assert!(classifying(&x).await.is_empty(), "a held import is skipped");
    // and so does a resume: it starts nothing
    assert_eq!(grid::resume_as(&x.e.state, B).await.unwrap(), 0);
    assert!(
        worker::resume_as(&x.e.state.db, Arc::new(Fake::default()), B)
            .await
            .unwrap()
            .is_empty()
    );
    expire(&x, "validation_grid_run", &g.to_string()).await;
    expire(&x, "discovery_import", &i).await;
    assert_eq!(active_grids(&x).await, [g]);
    assert_eq!(classifying(&x).await, [i]);
}

#[tokio::test]
async fn two_replicas_run_a_grid_run_and_an_import_once() {
    let x = setup().await;
    let g = grid_row(&x).await;
    let (a, b) = tokio::join!(
        grid::run_grid_as(&x.e.state, x.f.tenant, x.f.job.job_id, g, A),
        grid::run_grid_as(&x.e.state, x.f.tenant, x.f.job.job_id, g, B),
    );
    // a run that fails (no upstreams in this fixture) is still the one that ran: only the claim is under test
    let outs = [a.unwrap_or(GridOutcome::Ran), b.unwrap_or(GridOutcome::Ran)];
    assert_eq!(
        outs.iter().filter(|o| **o == GridOutcome::Ran).count(),
        1,
        "{outs:?}"
    );
    assert_eq!(
        outs.iter()
            .filter(|o| **o == GridOutcome::HeldElsewhere)
            .count(),
        1,
        "{outs:?}"
    );
    let (status, owner): (String, Option<String>) =
        sqlx::query_as("SELECT status, lease_owner FROM validation_grid_run WHERE id = $1")
            .bind(g)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert!(status == "done" || status == "failed", "{status}");
    assert_eq!(owner, None, "the lease is cleared when the run ends");

    let id = import_row(&x).await;
    let at = import_ref(&x.f, &id);
    let fake = Fake::default();
    let (a, b) = tokio::join!(
        worker::run_leased(&x.e.state.db, &fake, &at, A),
        worker::run_leased(&x.e.state.db, &fake, &at, B),
    );
    let mut outs = [a, b];
    outs.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(outs, [Outcome::Classified, Outcome::HeldElsewhere]);
    let owner: Option<String> =
        sqlx::query_scalar("SELECT lease_owner FROM discovery_import WHERE id = $1")
            .bind(&id)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(owner, None, "the lease is cleared when the import ends");
}
