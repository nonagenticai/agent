//! `GET /api/connections/systems/{id}/insight` (B7 T12): how the pinned template translates for this system, how
//! its calls ended and what a re-pin to the newest active version would change. Shape = `SystemInsight` of
//! `web/src/lib/api/types/connectors.ts`; oracle = the fixture's `systemInsight` (`web/src/lib/api/areas/connectors.ts`).
//!
//! - Per op: status map and fault map as pinned, each fault code classified against the canonical taxonomy (B6
//!   `error_state` rows of the head catalogue; TS `classifyCode`: user-facing → `reply`, else `escalate`, not listed
//!   → `unlisted`), idempotency label, verify read, timeout.
//! - `errors_30d` is empty: it counts B4 `operation_call` outcomes, which do not exist yet.
//! - `upgrade` (platform scope, non-platform-connector, a newer active version): `diffTemplates` + `upgradeVerdict`
//!   (crate::connectors::diff) with the routed ops and overrides of the working rows (0094). Tenant-scope pins: the
//!   fixture shape (no fault map, verify read, timeout, rate limit, vendor pin, webhooks or upgrade).
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::coverage::{self, HeadRow};
use super::diff::{self, ErrorState, Handling, InstanceUse, OverrideRef};
use super::instances::{self as db, cmp_version};
use super::model::{ConnectorTemplate, OpRef};
use super::opkey::op_key;
use super::overrides;
use crate::error::ApiResult;

/// The canonical error taxonomy (`error_state` rows of B6's head catalogue, folded over layers).
pub async fn taxonomy(conn: &mut PgConnection) -> ApiResult<Vec<ErrorState>> {
    let rows: Vec<HeadRow> = sqlx::query_as(
        "SELECT layer_key, kind, key, op, fields FROM layer_head WHERE kind = 'error_state' \
          ORDER BY layer_key, kind, key",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(coverage::fold(&rows, "error_state")
        .into_iter()
        .map(|(code, f)| ErrorState {
            code,
            user_facing: f
                .get("user_facing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            description: f
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
        .collect())
}

/// `ErrorHandling` of a code: `unknown` reads `unlisted`.
pub fn handling_of(code: &str, taxonomy: &[ErrorState]) -> &'static str {
    match diff::classify_code(code, taxonomy) {
        Handling::Reply => "reply",
        Handling::Escalate => "escalate",
        Handling::Unknown => "unlisted",
    }
}

/// The ops jobs route to `instance` (working rows of every job and env), as op keys.
async fn routed_ops(conn: &mut PgConnection, instance: &str) -> ApiResult<Vec<String>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT DISTINCT object, verb, variant FROM operation_route \
          WHERE target_kind = 'instance' AND instance_id = $1 ORDER BY object, verb, variant",
    )
    .bind(instance)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(object, verb, variant)| {
            op_key(&OpRef {
                object,
                verb,
                variant,
            })
        })
        .collect())
}

/// The newest active platform version of `key`.
async fn newest_active(conn: &mut PgConnection, key: &str) -> ApiResult<Option<ConnectorTemplate>> {
    let newest = db::versions_of(conn, key)
        .await?
        .into_iter()
        .filter(|(_, status)| status == "active")
        .map(|(v, _)| v)
        .max_by(|a, b| cmp_version(a, b));
    match newest {
        Some(v) => db::platform_template(conn, key, &v).await,
        None => Ok(None),
    }
}

/// `SystemInsight` of instance `id` (404 `instance_not_found`, `template_not_found`), in the caller's tenant
/// transaction.
pub async fn system_insight(conn: &mut PgConnection, id: &str) -> ApiResult<Value> {
    let inst = db::require(conn, id).await?;
    let pinned = db::pinned_template(conn, &inst).await?;
    let tax = taxonomy(conn).await?;
    let codes: Vec<Value> = tax
        .iter()
        .map(|t| {
            let handling = if t.user_facing { "reply" } else { "escalate" };
            json!({"code": t.code, "handling": handling, "description": t.description})
        })
        .collect();
    let tenant_scope = inst.template_scope == "tenant";
    let ops: Vec<Value> = pinned
        .ops
        .iter()
        .map(|o| {
            let fault_map: Vec<Value> = if tenant_scope {
                Vec::new()
            } else {
                o.fault_map
                    .iter()
                    .map(|f| json!({"system": f.system, "code": f.code, "handling": handling_of(&f.code, &tax)}))
                    .collect()
            };
            let (verify_read, timeout_ms) = if tenant_scope {
                (None, None)
            } else {
                (o.verify_read.clone(), Some(o.timeout_ms))
            };
            json!({
                "op": op_key(&o.op_ref()),
                "object": o.object,
                "side_effect": o.side_effect.as_str(),
                "status_map": o.status_map,
                "fault_map": fault_map,
                "idempotency": diff::idempotency_label(&o.idempotency),
                "verify_read": verify_read,
                "timeout_ms": timeout_ms,
                // B4 seam: outcome_code counts of operation_call for this instance, last 30 days
                "errors_30d": [],
            })
        })
        .collect();
    if tenant_scope {
        return Ok(json!({
            "ops": ops,
            "rate_limit": null,
            "vendor_api_pin": null,
            "webhooks": [],
            "upgrade": null,
            "taxonomy": codes,
        }));
    }
    let mut upgrade = Value::Null;
    if !inst.platform {
        let latest = newest_active(conn, &inst.template_key).await?;
        if let Some(latest) =
            latest.filter(|l| cmp_version(&l.version, &inst.template_version).is_gt())
        {
            let routed = routed_ops(conn, &inst.id).await?;
            let ovr = overrides::working(conn, Some(&inst.id)).await?;
            let use_ = InstanceUse {
                pinned_version: inst.template_version.clone(),
                routed_ops: routed,
                overrides: ovr
                    .iter()
                    .map(|o| OverrideRef {
                        op: o.op.clone(),
                        field: o.field.clone(),
                    })
                    .collect(),
            };
            let verdict = diff::upgrade_verdict(&use_, Some(&latest));
            if matches!(
                verdict,
                diff::UpgradeVerdict::CanUpgrade { .. } | diff::UpgradeVerdict::Blocked { .. }
            ) {
                let d = diff::diff_templates(Some(&pinned), &latest);
                upgrade = json!({
                    "from": pinned.version,
                    "to": latest.version,
                    "meta": d.meta,
                    "ops": d.ops,
                    "counts": d.counts,
                    "verdict": verdict,
                });
            }
        }
    }
    let webhooks = match &pinned.webhooks {
        Value::Null => json!([]),
        w => w.clone(),
    };
    Ok(json!({
        "ops": ops,
        "rate_limit": pinned.rate_limit_model,
        "vendor_api_pin": pinned.vendor_api_pin,
        "webhooks": webhooks,
        "upgrade": upgrade,
        "taxonomy": codes,
    }))
}
