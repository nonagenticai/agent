//! Where a set of design changes stands (spec B2 §2.7 "keys lifecycle"): still in the draft, carried by an approved
//! cut, live, or gone. A port of `web/src/lib/api/areas/journey-J18.ts` `lifecycle` (B8 reads it for a discovery
//! proposal's status), over `draft_ledger`, `release_bundled_draft` and `published_change`.
use sqlx::PgConnection;

use super::JobRef;
use super::ledger;
use crate::error::ApiResult;

/// The state of a set of place keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeysLifecycle {
    /// At least one key has an unpublished ledger row; no approved cut carries any of them yet.
    Draft,
    /// An approved (not yet applied) cut carries one of the keys' ledger rows: it ships with that release.
    InRelease(String),
    /// No key is in the draft; the newest published change at one of the keys went live in this release.
    Live(String),
    /// Neither in the draft nor ever published (the change was discarded, or never made).
    Discarded,
}

/// The lifecycle of `keys` (fixture `lifecycle`): the keys with a ledger row decide first (the first one, in `keys`
/// order, that an approved cut bundles names the release); else the newest `published_change` at any key; else
/// [`KeysLifecycle::Discarded`].
pub async fn lifecycle(
    c: &mut PgConnection,
    job: &JobRef,
    keys: &[String],
) -> ApiResult<KeysLifecycle> {
    let mut drafted = false;
    for k in keys {
        if ledger::find(&mut *c, job, k).await?.is_none() {
            continue;
        }
        drafted = true;
        if let Some(cut) = ledger::approved_cuts(&mut *c, job, k)
            .await?
            .into_iter()
            .next()
        {
            return Ok(KeysLifecycle::InRelease(cut));
        }
    }
    if drafted {
        return Ok(KeysLifecycle::Draft);
    }
    let published: Option<String> = sqlx::query_scalar(
        "SELECT published_in FROM published_change WHERE job_id = $1 AND key = ANY($2) \
          ORDER BY id DESC LIMIT 1",
    )
    .bind(job.id)
    .bind(keys.to_vec())
    .fetch_optional(&mut *c)
    .await?;
    Ok(match published {
        Some(release) => KeysLifecycle::Live(release),
        None => KeysLifecycle::Discarded,
    })
}
