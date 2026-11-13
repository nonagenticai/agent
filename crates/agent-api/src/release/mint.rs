//! Release ids (spec B2 §5 approve step 9): `cfg-<UTC date>.<N>`, N = 1 + the highest N the job already used that
//! day (a port of `api/fixture-helpers.ts` `nextReleaseId`). Approve mints with the job row locked, so two cuts of
//! one job never race for an id.
use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::design::JobRef;
use crate::error::ApiResult;

/// The id prefix of the releases cut on `day` (`cfg-2026-10-20.`).
pub fn prefix(day: &str) -> String {
    format!("cfg-{day}.")
}

/// The next release id of `day` (`YYYY-MM-DD`) given the ids that already exist: ids of other days and ids whose
/// suffix is not a number are ignored.
pub fn next_release_id(existing: &[String], day: &str) -> String {
    let p = prefix(day);
    let n = existing
        .iter()
        .filter_map(|id| id.strip_prefix(p.as_str()))
        .filter_map(|n| n.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{p}{}", n + 1)
}

/// The UTC day of `at` as `YYYY-MM-DD`.
pub fn day_of(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

/// The next release id of `job` for `day`, read from its `job_release` rows (call it with the job row locked).
pub async fn mint(c: &mut PgConnection, job: &JobRef, day: &str) -> ApiResult<String> {
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT release_id FROM job_release WHERE job_id = $1 AND starts_with(release_id, $2)",
    )
    .bind(job.id)
    .bind(prefix(day))
    .fetch_all(&mut *c)
    .await?;
    Ok(next_release_id(&existing, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn next_id_is_one_past_the_days_highest() {
        let day = "2026-10-20";
        assert_eq!(next_release_id(&[], day), "cfg-2026-10-20.1");
        assert_eq!(
            next_release_id(&ids(&["cfg-2026-10-20.1", "cfg-2026-10-20.2"]), day),
            "cfg-2026-10-20.3"
        );
        // numeric, not text order; other days and junk ignored
        assert_eq!(
            next_release_id(
                &ids(&[
                    "cfg-2026-10-20.9",
                    "cfg-2026-10-20.10",
                    "cfg-2026-10-21.40",
                    "cfg-2026-10-20.x"
                ]),
                day
            ),
            "cfg-2026-10-20.11"
        );
    }

    #[test]
    fn day_is_utc_iso_date() {
        let t = DateTime::parse_from_rfc3339("2026-10-20T23:30:00-02:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(day_of(t), "2026-10-21");
    }
}
