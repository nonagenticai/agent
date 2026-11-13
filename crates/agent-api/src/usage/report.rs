//! `tenantUsage` (`UsageReport`, web `types/tenant.ts`): the tenant's metered usage over the last 7, 30 or 90 UTC
//! days (today included), from `usage_event` (0064) under the tenant's RLS, plus knowledge work from
//! `operation_call`, `source_run` and `knowledge_upload`. No currency (billing is K52's).
//!
//! - A **turn** is one qualifier call (`use = 'qualify'`): the runtime qualifies every drained customer or staff turn
//!   once. **Tokens** are `tokens_in + tokens_out` of every call.
//! - `totals`, `funding`, `rows`, `daily` and `staff_daily` count conversation usage: every source but the
//!   generator's (`generator`, `discovery`), which is `sources[generator]` (D56). Customers vs staff is the actor:
//!   `callers:<channel>` or a staff sub.
//! - `previous` is the window before this one, `null` when it would reach past 90 days.
//! - `drivers`: turns and tokens per (job, qualified activity) — the row's own activity, else the case the call
//!   qualified (the conversation's first case started at or after the call), else `unqualified` — and `searches`,
//!   the `knowledge.*` operation calls of those cases.
//! - `generator` is B8's runs: `[]` until B8 records them.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::error::{ApiError, ApiResult};
use crate::readmodel::tenant::{TenantRow, licence_status};

/// The funding lanes (0064 `usage_event_funding_check`), in report order.
pub const FUNDING: [&str; 3] = ["local_gpu", "platform_external", "tenant_byo"];
/// The sources that are generator usage, not conversation usage (D56).
const GENERATOR_SOURCES: [&str; 2] = ["generator", "discovery"];
/// The furthest back the report reaches (`previous` is null beyond it).
pub const LEDGER_DAYS: i64 = 90;

/// `UsageGroup`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Job,
    User,
    Project,
    Provider,
}

impl Group {
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "job" => Some(Self::Job),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "provider" => Some(Self::Provider),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Job => "job",
            Self::User => "user",
            Self::Project => "project",
            Self::Provider => "provider",
        }
    }
}

/// `UsageWindow` (7, 30 or 90 days).
pub fn parse_window(v: &str) -> Option<i64> {
    match v {
        "7" => Some(7),
        "30" => Some(30),
        "90" => Some(90),
        _ => None,
    }
}

/// The days of a `window` ending `today`, oldest first, and the start of the window before it when the ledger
/// reaches that far back.
pub fn days(today: NaiveDate, window: i64) -> (Vec<NaiveDate>, Option<NaiveDate>) {
    let from = today - Duration::days(window - 1);
    let days = (0..window).map(|i| from + Duration::days(i)).collect();
    let previous = (window * 2 <= LEDGER_DAYS).then(|| from - Duration::days(window));
    (days, previous)
}

fn midnight(d: NaiveDate) -> DateTime<Utc> {
    d.and_time(chrono::NaiveTime::MIN).and_utc()
}

/// One grouped ledger slice.
#[derive(sqlx::FromRow)]
struct Slice {
    day: NaiveDate,
    job_key: Option<String>,
    job_label: Option<String>,
    actor: String,
    project: String,
    provider_id: String,
    model: String,
    funding: String,
    source: String,
    turns: i64,
    tokens: i64,
}

impl Slice {
    fn generator(&self) -> bool {
        GENERATOR_SOURCES.contains(&self.source.as_str())
    }

    fn customer(&self) -> bool {
        self.actor.starts_with("callers:")
    }
}

#[derive(sqlx::FromRow)]
struct DriverRow {
    job: String,
    activity: String,
    turns: i64,
    tokens: i64,
}

#[derive(sqlx::FromRow)]
struct SearchRow {
    job: String,
    activity: String,
    searches: i64,
}

#[derive(sqlx::FromRow)]
struct KnowledgeRow {
    ingest_jobs: i64,
    ingest_failed: i64,
    documents: i64,
    pages: i64,
}

/// Tokens per lane; every lane present.
#[derive(Clone, Default)]
struct Lanes(BTreeMap<&'static str, i64>);

impl Lanes {
    fn add(&mut self, funding: &str, tokens: i64) {
        if let Some(lane) = FUNDING.iter().find(|f| **f == funding) {
            *self.0.entry(*lane).or_default() += tokens;
        }
    }

    fn json(&self) -> Value {
        let mut m = serde_json::Map::new();
        for f in FUNDING {
            m.insert(f.into(), json!(self.0.get(f).copied().unwrap_or(0)));
        }
        Value::Object(m)
    }
}

#[derive(Default)]
struct Bucket {
    label: String,
    turns: i64,
    tokens: i64,
    lanes: Lanes,
}

const SLICES: &str = "SELECT (u.occurred_at AT TIME ZONE 'UTC')::date AS day, j.key AS job_key, j.label AS job_label, \
            u.actor, u.project, u.provider_id, u.model, u.funding, u.source, \
            count(*) FILTER (WHERE u.use = 'qualify')::bigint AS turns, \
            COALESCE(sum(u.tokens_in + u.tokens_out), 0)::bigint AS tokens \
       FROM usage_event u LEFT JOIN jobs j ON j.tenant_id = u.tenant_id AND j.id = u.job_id \
      WHERE u.occurred_at >= $1 AND u.occurred_at < $2 \
      GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9";

const DRIVERS: &str = "SELECT j.key AS job, COALESCE(u.activity, c.activity, c.request_type, 'unqualified') AS activity, \
            count(*) FILTER (WHERE u.use = 'qualify')::bigint AS turns, \
            COALESCE(sum(u.tokens_in + u.tokens_out), 0)::bigint AS tokens \
       FROM usage_event u JOIN jobs j ON j.tenant_id = u.tenant_id AND j.id = u.job_id \
       LEFT JOIN LATERAL ( \
            SELECT NULLIF(ac.activity, '') AS activity, NULLIF(ac.request_type, '') AS request_type FROM agent_case ac \
             WHERE ac.tenant_id = u.tenant_id AND ac.job_id = u.job_id \
               AND (ac.case_id = u.case_id \
                    OR (u.case_id IS NULL AND ac.conversation_id = u.conversation_id \
                        AND ac.started_at >= u.occurred_at)) \
             ORDER BY ac.started_at LIMIT 1) c ON true \
      WHERE u.occurred_at >= $1 AND u.occurred_at < $2 AND NOT u.source = ANY ($3) \
      GROUP BY 1, 2";

const SEARCHES: &str = "SELECT j.key AS job, COALESCE(NULLIF(ac.activity, ''), NULLIF(ac.request_type, ''), 'unqualified') AS activity, \
            count(*)::bigint AS searches \
       FROM operation_call o JOIN jobs j ON j.tenant_id = o.tenant_id AND j.id = o.job_id \
       LEFT JOIN agent_case ac ON ac.tenant_id = o.tenant_id AND ac.job_id = o.job_id AND ac.case_id = o.case_id \
      WHERE o.operation LIKE 'knowledge.%' AND o.started_at >= $1 AND o.started_at < $2 \
      GROUP BY 1, 2";

const KNOWLEDGE: &str = "SELECT \
         ((SELECT count(*) FROM source_run WHERE started_at >= $1 AND started_at < $2) \
        + (SELECT count(*) FROM knowledge_upload WHERE created_at >= $1 AND created_at < $2))::bigint AS ingest_jobs, \
         ((SELECT count(*) FROM source_run WHERE started_at >= $1 AND started_at < $2 AND status = 'failed') \
        + (SELECT count(*) FROM knowledge_upload WHERE created_at >= $1 AND created_at < $2 \
              AND jsonb_path_exists(files, '$[*] ? (@.error != null)')))::bigint AS ingest_failed, \
         ((SELECT COALESCE(sum(documents), 0) FROM source_run WHERE started_at >= $1 AND started_at < $2) \
        + (SELECT COALESCE(sum(jsonb_array_length(files)), 0) FROM knowledge_upload \
            WHERE created_at >= $1 AND created_at < $2))::bigint AS documents, \
         (SELECT COALESCE(sum(CASE WHEN jsonb_typeof(pages -> 'total') = 'number' \
                                   THEN (pages ->> 'total')::numeric END), 0) \
            FROM source_run WHERE started_at >= $1 AND started_at < $2)::bigint AS pages";

/// The label of a driver key: the case's activity or request type (`order_status` → `Order status`).
fn driver_label(key: &str) -> String {
    if key == "unqualified" {
        return "Not qualified".to_string();
    }
    let words = key.replace(['_', '-'], " ");
    let mut c = words.trim().chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => key.to_string(),
    }
}

/// The report for the tenant `t` (the connection must be scoped to it, with no job GUC), `today` being the last day.
pub async fn tenant_usage(
    conn: &mut PgConnection,
    t: &TenantRow,
    plan: &str,
    window: i64,
    group: Group,
    today: NaiveDate,
) -> ApiResult<Value> {
    if !matches!(window, 7 | 30 | 90) {
        return Err(ApiError::Unprocessable(
            "invalid_query",
            "window must be 7, 30 or 90.".into(),
        ));
    }
    let (days, previous_from) = days(today, window);
    let from = days[0];
    let lo = midnight(from);
    let hi = midnight(today + Duration::days(1));
    let slices: Vec<Slice> = sqlx::query_as(SLICES)
        .bind(lo)
        .bind(hi)
        .fetch_all(&mut *conn)
        .await?;
    let generator_sources: Vec<String> = GENERATOR_SOURCES.iter().map(|s| s.to_string()).collect();
    let driver_rows: Vec<DriverRow> = sqlx::query_as(DRIVERS)
        .bind(lo)
        .bind(hi)
        .bind(&generator_sources)
        .fetch_all(&mut *conn)
        .await?;
    let search_rows: Vec<SearchRow> = sqlx::query_as(SEARCHES)
        .bind(lo)
        .bind(hi)
        .fetch_all(&mut *conn)
        .await?;
    let k: KnowledgeRow = sqlx::query_as(KNOWLEDGE)
        .bind(lo)
        .bind(hi)
        .fetch_one(&mut *conn)
        .await?;
    let previous = match previous_from {
        None => Value::Null,
        Some(p) => {
            let prev: Vec<Slice> = sqlx::query_as(SLICES)
                .bind(midnight(p))
                .bind(lo)
                .fetch_all(&mut *conn)
                .await?;
            let conv = prev.iter().filter(|s| !s.generator());
            let (turns, tokens) = conv.fold((0, 0), |(a, b), s| (a + s.turns, b + s.tokens));
            json!({"turns": turns, "tokens": tokens})
        }
    };
    let staff: BTreeSet<String> = slices
        .iter()
        .filter(|s| !s.customer())
        .map(|s| s.actor.clone())
        .collect();
    let staff: Vec<String> = staff.into_iter().collect();
    let names = crate::members::names::lookup(conn, &staff).await?;

    let label = |s: &Slice| -> (String, String) {
        match group {
            Group::Job => {
                let key = s.job_key.clone().unwrap_or_default();
                let l = s.job_label.clone().unwrap_or_else(|| key.clone());
                (key, l)
            }
            Group::User => {
                let l = match s.actor.strip_prefix("callers:") {
                    Some(ch) => format!("Callers ({ch})"),
                    None => names
                        .get(&s.actor)
                        .cloned()
                        .unwrap_or_else(|| s.actor.clone()),
                };
                (s.actor.clone(), l)
            }
            Group::Project => {
                let l = if s.project == t.id.to_string() {
                    format!("{} (tenant project)", t.name)
                } else {
                    s.project.clone()
                };
                (s.project.clone(), l)
            }
            Group::Provider => {
                let key = format!("{} · {}", s.provider_id, s.model);
                (key.clone(), key)
            }
        }
    };

    let mut funding = Lanes::default();
    let mut groups: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut daily: BTreeMap<NaiveDate, (i64, i64, i64)> = BTreeMap::new();
    let (mut customers, mut staff_src, mut gen_src) =
        (Bucket::default(), Bucket::default(), Bucket::default());
    let (mut turns, mut tokens) = (0i64, 0i64);
    for s in &slices {
        if s.generator() {
            gen_src.tokens += s.tokens;
            gen_src.lanes.add(&s.funding, s.tokens);
            continue;
        }
        turns += s.turns;
        tokens += s.tokens;
        funding.add(&s.funding, s.tokens);
        let (key, l) = label(s);
        let g = groups.entry(key).or_insert_with(|| Bucket {
            label: l,
            ..Bucket::default()
        });
        g.turns += s.turns;
        g.tokens += s.tokens;
        g.lanes.add(&s.funding, s.tokens);
        let d = daily.entry(s.day).or_default();
        d.0 += s.turns;
        d.1 += s.tokens;
        let src = if s.customer() {
            &mut customers
        } else {
            d.2 += s.turns;
            &mut staff_src
        };
        src.turns += s.turns;
        src.tokens += s.tokens;
        src.lanes.add(&s.funding, s.tokens);
    }
    let mut rows: Vec<(String, Bucket)> = groups.into_iter().collect();
    rows.sort_by_key(|(key, b)| {
        (
            std::cmp::Reverse(b.turns),
            std::cmp::Reverse(b.tokens),
            key.clone(),
        )
    });
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|(key, b)| {
            json!({"key": key, "label": b.label, "turns": b.turns, "tokens": b.tokens, "funding": b.lanes.json()})
        })
        .collect();
    let day_str = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    let daily_json: Vec<Value> = days
        .iter()
        .map(|d| {
            let (t, k, _) = daily.get(d).copied().unwrap_or_default();
            json!({"day": day_str(*d), "turns": t, "tokens": k})
        })
        .collect();
    let staff_daily: Vec<i64> = days
        .iter()
        .map(|d| daily.get(d).map_or(0, |v| v.2))
        .collect();

    let mut searches: BTreeMap<(String, String), i64> = BTreeMap::new();
    for r in search_rows {
        *searches.entry((r.job, r.activity)).or_default() += r.searches;
    }
    let total_searches: i64 = searches.values().sum();
    let mut drivers: Vec<Value> = driver_rows
        .into_iter()
        .map(|r| {
            let s = searches.get(&(r.job.clone(), r.activity.clone())).copied().unwrap_or(0);
            let label = driver_label(&r.activity);
            json!({"job": r.job, "activity": r.activity, "label": label, "stage": "", "turns": r.turns,
                   "tokens": r.tokens, "searches": s})
        })
        .collect();
    drivers.sort_by_key(|d| {
        (
            d["job"].as_str().unwrap_or_default().to_string(),
            std::cmp::Reverse(d["turns"].as_i64().unwrap_or(0)),
            d["activity"].as_str().unwrap_or_default().to_string(),
        )
    });
    let source = |key: &str, label: &str, b: &Bucket, turns: Option<i64>| {
        json!({"key": key, "label": label, "turns": turns, "tokens": b.tokens,
               "funding": b.lanes.json()})
    };
    Ok(json!({
        "window": window,
        "group": group.as_str(),
        "from": day_str(from),
        "to": day_str(today),
        "totals": {"turns": turns, "tokens": tokens},
        "funding": funding.json(),
        "rows": rows,
        "daily": daily_json,
        "attributed": t.licence_id.is_some(),
        "previous": previous,
        "staff_daily": staff_daily,
        "drivers": drivers,
        "knowledge": {
            "searches": total_searches,
            "ingest_jobs": k.ingest_jobs,
            "ingest_failed": k.ingest_failed,
            "documents": k.documents,
            "pages": k.pages,
        },
        "generator": [],
        "sources": [
            source("customers", "Customer conversations", &customers, Some(customers.turns)),
            source("staff", "Staff test chats", &staff_src, Some(staff_src.turns)),
            source("generator", "Generator runs", &gen_src, None),
        ],
        "plan": {"name": plan, "licence": licence_status(t), "allowance": null},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_end_today_and_previous_stops_at_90_days() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
        let (d7, p7) = days(today, 7);
        assert_eq!(d7.len(), 7);
        assert_eq!(d7[0], NaiveDate::from_ymd_opt(2026, 10, 15).unwrap());
        assert_eq!(*d7.last().unwrap(), today);
        assert_eq!(p7, NaiveDate::from_ymd_opt(2026, 10, 8));
        assert!(
            days(today, 30).1.is_some(),
            "60 days back is inside the ledger"
        );
        assert_eq!(days(today, 90).1, None, "180 days back is beyond it");
        assert_eq!(parse_window("14"), None);
        assert_eq!(Group::parse("team"), None);
    }
}
