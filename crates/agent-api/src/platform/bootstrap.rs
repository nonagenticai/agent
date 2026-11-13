//! Identity bootstrap of the platform tenant (E3). The platform's own knowledge routes
//! (`/api/platform/knowledge/*`, curation) run as the tenant `AGENT_PLATFORM_TENANT_ID` names, and `POST /api/tenants`
//! takes no id, so nothing else can create that row. This inserts the tenants row and nothing else: no layers, no
//! objects, no content (the platform offering is authored by super_admin through the platform routes).
use sqlx::PgConnection;
use uuid::Uuid;

/// The display name of the platform tenant.
pub const PLATFORM_TENANT_NAME: &str = "Platform";

/// Idempotent: `INSERT ... ON CONFLICT DO NOTHING` (an existing row by id, or a tenant already holding the name
/// “Platform”, is left as it is). Runs on the migration-owner connection at boot (the same one that reconciles
/// `platform_owned`; RLS does not apply to it). No-op when `AGENT_PLATFORM_TENANT_ID` is unset.
pub async fn ensure_platform_tenant(
    owner: &mut PgConnection,
    platform_tenant_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let Some(id) = platform_tenant_id else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by) VALUES ($1, $2, 'eu', 'system:platform-bootstrap') \
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(PLATFORM_TENANT_NAME)
    .execute(owner)
    .await?;
    Ok(())
}
