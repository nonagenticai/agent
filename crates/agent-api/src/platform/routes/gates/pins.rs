//! Which tenant jobs pin a library row (plan B6 Revision 2026-10-22 R3): a job pins it when one of its `jobs.pins`
//! (`[{layer: family, version}]`, B1 0033) is a tag of the row's family that holds a live revision of that
//! (layer, kind, key). Read the B7 way, without a definer migration: the tenant list under `agent_platform`
//! (`agent_list_tenants()`, 0031), then each tenant's jobs back as `agent_app` with `app.current_tenant` set (RLS).
//! `retireGateQuestion` answers it as a warning (`jobs[]`, T12d); `gateLibraryUsage` (B5 bucket) will reuse it.
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::set_tenant;

/// One tenant job that pins the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinnedJob {
    pub tenant_id: String,
    pub tenant_name: String,
    pub job: String,
    pub job_label: String,
}

/// The jobs of the tenant set on `conn` that pin (`layer_id`, `kind`, `key`).
async fn tenant_jobs(
    conn: &mut PgConnection,
    layer_id: Uuid,
    kind: &str,
    key: &str,
) -> ApiResult<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT j.key, j.label FROM jobs j \
          WHERE EXISTS ( \
                SELECT 1 FROM jsonb_array_elements(j.pins) p \
                  JOIN layer_tag_row m ON m.family = p->>'layer' AND m.version = (p->>'version')::int \
                  JOIN layer_row t ON t.id = m.row_id \
                 WHERE t.layer_id = $1 AND t.kind = $2 AND t.key = $3 AND t.op <> 'tombstone') \
          ORDER BY j.key",
    )
    .bind(layer_id)
    .bind(kind)
    .bind(key)
    .fetch_all(conn)
    .await?)
}

/// Every tenant job that pins (`layer_id`, `kind`, `key`), by tenant name then job key. Runs in its own read-only
/// transaction (rolled back), so the caller's platform write transaction keeps its role and GUCs.
pub async fn pinned_jobs(
    st: &AppState,
    layer_id: Uuid,
    kind: &str,
    key: &str,
) -> ApiResult<Vec<PinnedJob>> {
    let mut tx = st.db.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    platform_db::enter(&mut tx).await?;
    let mut tenants: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM agent_list_tenants()")
            .fetch_all(&mut *tx)
            .await?;
    platform_db::leave(&mut tx).await?;
    tenants.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let mut out = Vec::new();
    for (id, name) in tenants {
        set_tenant(&mut tx, id).await?;
        for (job, job_label) in tenant_jobs(&mut tx, layer_id, kind, key).await? {
            out.push(PinnedJob {
                tenant_id: id.to_string(),
                tenant_name: name.clone(),
                job,
                job_label,
            });
        }
    }
    tx.rollback().await?;
    Ok(out)
}
