//! Step 7b, the per-case cap (B4 T6a, F-15): an X or P operation runs at most `max_per_case` times in one case
//! (default 1, at most 20). The count is this case's `operation_call` rows of the same operation in state `ok`,
//! `pending` or `unknown_outcome`, not counting the call's own idempotency key (a redelivery or a retry of the same
//! step answers its stored outcome, step 8b). R and W calls have no cap. At the cap the call is not sent: `denied` /
//! `case_cap_reached` (D57), and the case hands over with reason `tool_failed`.
use sqlx::PgConnection;

use crate::error::ApiResult;

/// The cap of a step that names none.
pub const DEFAULT_CAP: u8 = 1;
/// The highest cap a step may name.
pub const MAX_CAP: u8 = 20;
/// The `denied` outcome code at the cap.
pub const CODE: &str = "case_cap_reached";

/// The cap of a step: `max_per_case` within 1..=20, else [`DEFAULT_CAP`].
pub fn limit(max_per_case: Option<u8>) -> i64 {
    i64::from(max_per_case.unwrap_or(DEFAULT_CAP).clamp(1, MAX_CAP))
}

/// The handover trigger of a capped call.
pub fn trigger(op_key: &str) -> String {
    format!("per-case limit reached for {op_key}")
}

/// Has case `case_id` reached the cap of `op_key`? Runs in the transaction that inserts the call's `pending` row,
/// after locking the case row, so two calls of one case are counted one after the other. No `tenants` row lock:
/// `agent_runtime` holds only column SELECT on `tenants` (0065), and `FOR KEY SHARE` needs UPDATE.
pub async fn reached(
    c: &mut PgConnection,
    case_id: &str,
    op_key: &str,
    own: &CallKey<'_>,
    limit: i64,
) -> ApiResult<bool> {
    sqlx::query("SELECT 1 FROM agent_case WHERE case_id = $1 FOR NO KEY UPDATE")
        .bind(case_id.to_owned())
        .execute(&mut *c)
        .await?;
    let n: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM operation_call \
          WHERE case_id = $1 AND operation = $2 AND state IN ('ok', 'pending', 'unknown_outcome') \
            AND call_id <> $3 AND idempotency_key IS DISTINCT FROM $4",
    )
    .bind(case_id.to_owned())
    .bind(op_key.to_owned())
    .bind(own.call_id.to_owned())
    .bind(own.idempotency_key.map(str::to_owned))
    .fetch_one(&mut *c)
    .await?;
    Ok(n >= limit)
}

/// The call being counted (its own rows never count against it).
pub struct CallKey<'a> {
    pub call_id: &'a str,
    pub idempotency_key: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_defaults_to_one_and_stays_within_twenty() {
        assert_eq!(limit(None), 1);
        assert_eq!(limit(Some(0)), 1);
        assert_eq!(limit(Some(3)), 3);
        assert_eq!(limit(Some(200)), 20);
        assert_eq!(
            trigger("payment.refund"),
            "per-case limit reached for payment.refund"
        );
    }
}
