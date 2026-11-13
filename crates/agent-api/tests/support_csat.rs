//! SW-6: the CSAT report (`GET /api/support/csat`, crate::support::csat_report). The window is on `asked_at`, the
//! cohort is prod only, and only `support_manager` / `tenant_admin` read it (comments are customer text). Real
//! Postgres 18 (one container for this binary).
use crate::common;

use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};

/// One CSAT row (and its case, FK) through the owner pool. `age_h` = hours since it was asked (None: never asked),
/// `answer` = (rating, comment, hours since answered).
struct Row<'a> {
    case: &'a str,
    channel: &'a str,
    env: &'a str,
    state: &'a str,
    by: (&'a str, Option<&'a str>),
    age_h: Option<i32>,
    answer: Option<(i16, Option<&'a str>, i32)>,
}

async fn seed(c: &Ctx, r: Row<'_>) {
    let conv = format!("conv-{}", r.case);
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev) \
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $6::text = 'test' THEN 1::bigint END)",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(r.case)
    .bind(&conv)
    .bind(r.channel)
    .bind(r.env)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (rating, comment, answered_h) = match r.answer {
        Some((n, cm, h)) => (Some(n), cm, Some(h)),
        None => (None, None, None),
    };
    sqlx::query(
        "INSERT INTO case_csat (tenant_id, job_id, case_id, conversation_id, channel, env, state, asked_by, \
                                asked_by_sub, asked_at, rating, answered_at, comment, comment_asked_at, comment_at, \
                                dismissed_at, draft_rev) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                 now() - make_interval(hours => $10), $11, now() - make_interval(hours => $12), $13, \
                 CASE WHEN $13::text IS NOT NULL THEN now() END, CASE WHEN $13::text IS NOT NULL THEN now() END, \
                 CASE WHEN $7 = 'dismissed' THEN now() END, \
                 CASE WHEN $6::text = 'test' THEN 1::bigint END)",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(r.case)
    .bind(&conv)
    .bind(r.channel)
    .bind(r.env)
    .bind(r.state)
    .bind(r.by.0)
    .bind(r.by.1)
    .bind(r.age_h)
    .bind(rating)
    .bind(answered_h)
    .bind(comment)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

fn row<'a>(case: &'a str, channel: &'a str, state: &'a str) -> Row<'a> {
    Row {
        case,
        channel,
        env: "prod",
        state,
        by: ("agent", None),
        age_h: Some(5),
        answer: None,
    }
}

async fn report(c: &Ctx, sub: &str, query: &str) -> (StatusCode, Value) {
    c.get(&format!("/api/support/csat{query}"), sub).await
}

#[tokio::test]
async fn report_counts_rates_and_splits() {
    let c = conv::setup().await;
    sqlx::query("UPDATE users SET display_name = 'Pat Agent' WHERE sub = 'u-agent'")
        .execute(&c.e.owner)
        .await
        .unwrap();
    let staff = |sub| ("staff", Some(sub));
    for r in [
        Row {
            answer: Some((5, None, 4)),
            ..row("CS-1", "web_chat", "answered")
        },
        Row {
            by: staff("u-agent"),
            answer: Some((4, None, 4)),
            ..row("CS-2", "web_chat", "answered")
        },
        Row {
            by: staff("u-agent"),
            answer: Some((1, Some("too slow"), 1)),
            ..row("CS-3", "telegram", "answered")
        },
        row("CS-4", "telegram", "asked"),
        row("CS-5", "telegram", "dismissed"),
        // armed, never asked: not in the cohort
        Row {
            age_h: None,
            ..row("CS-6", "telegram", "awaiting_confirmation")
        },
        Row {
            by: staff("u-mgr"),
            answer: Some((2, Some("still broken"), 2)),
            ..row("CS-7", "api", "answered")
        },
        // asked 60 days ago: outside the default window
        Row {
            age_h: Some(60 * 24),
            answer: Some((5, None, 60 * 24 - 1)),
            ..row("CS-9", "web_chat", "answered")
        },
    ] {
        seed(&c, r).await;
    }
    for sub in ["u-mgr", "u-admin"] {
        let (s, b) = report(&c, sub, "").await;
        assert_eq!(s, StatusCode::OK, "{sub}: {b}");
        assert_eq!(
            (b["asked"].as_i64(), b["answered"].as_i64()),
            (Some(6), Some(4)),
            "{b}"
        );
        assert!(
            (b["response_rate"].as_f64().unwrap() - 4.0 / 6.0).abs() < 1e-9,
            "{b}"
        );
        assert_eq!(b["mean"], 3.0, "{b}");
        assert_eq!(
            b["distribution"],
            json!({"1": 1, "2": 1, "3": 0, "4": 1, "5": 1}),
            "{b}"
        );
        assert_eq!(
            b["by_channel"],
            json!([
                {"channel": "telegram", "asked": 3, "answered": 1, "mean": 1.0},
                {"channel": "web_chat", "asked": 2, "answered": 2, "mean": 4.5},
                {"channel": "api", "asked": 1, "answered": 1, "mean": 2.0},
            ]),
            "{b}"
        );
        assert_eq!(
            b["by_resolver"],
            json!([
                {"asked_by": "agent", "sub": null, "name": null, "asked": 3, "answered": 1, "mean": 5.0},
                {"asked_by": "staff", "sub": "u-agent", "name": "Pat Agent", "asked": 2, "answered": 2, "mean": 2.5},
                {"asked_by": "staff", "sub": "u-mgr", "name": "u-mgr", "asked": 1, "answered": 1, "mean": 2.0},
            ]),
            "{b}"
        );
        assert!(b["from"].is_string() && b["to"].is_string(), "{b}");
    }
    // a window that holds the old row only
    let (s, b) = report(
        &c,
        "u-mgr",
        &format!("?from={d}&to={d}", d = day_60_ago(&c).await),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            b["asked"].as_i64(),
            b["answered"].as_i64(),
            b["mean"].as_f64()
        ),
        (Some(1), Some(1), Some(5.0)),
        "{b}"
    );
    // an empty window: rate and mean are null
    let (s, b) = report(&c, "u-mgr", "?from=2020-01-01&to=2020-01-02").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["asked"], 0, "{b}");
    assert!(b["response_rate"].is_null() && b["mean"].is_null(), "{b}");
    assert_eq!(b["by_channel"], json!([]), "{b}");
    // a bad query is a 400 for a manager
    let (s, b) = report(&c, "u-mgr", "?from=yesterday").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, "invalid_query"),
        "{b}"
    );
}

/// The UTC date 60 days ago (the day CS-9 was asked, 1 h of slack either way is inside the day window it names).
async fn day_60_ago(c: &Ctx) -> String {
    c.texts("SELECT to_char((now() - interval '60 days') AT TIME ZONE 'UTC', 'YYYY-MM-DD')")
        .await
        .remove(0)
}

#[tokio::test]
async fn observer_and_support_agent_are_403_even_with_a_bad_query() {
    let c = conv::setup().await;
    for sub in ["u-obs", "u-agent", "u-int"] {
        for q in ["", "?from=garbage", "?to=&from=2026-99-99", "?from[]=x"] {
            let (s, b) = report(&c, sub, q).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{sub} {q}: {b}");
            assert_eq!(
                b["error"]["missing_role"], "support_manager",
                "{sub} {q}: {b}"
            );
        }
    }
    let (s, _) = report(&c, "u-mgr", "").await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn test_env_rows_are_excluded() {
    let c = conv::setup().await;
    seed(
        &c,
        Row {
            answer: Some((4, None, 1)),
            ..row("CS-1", "web_chat", "answered")
        },
    )
    .await;
    seed(
        &c,
        Row {
            env: "test",
            answer: Some((1, Some("test row"), 1)),
            ..row("CS-2", "web_chat", "answered")
        },
    )
    .await;
    let (s, b) = report(&c, "u-mgr", "").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            b["asked"].as_i64(),
            b["answered"].as_i64(),
            b["mean"].as_f64()
        ),
        (Some(1), Some(1), Some(4.0)),
        "{b}"
    );
    assert_eq!(b["distribution"]["1"], 0, "{b}");
    assert_eq!(b["low_scores"], json!([]), "{b}");
}

#[tokio::test]
async fn low_scores_list_comments_newest_first() {
    let c = conv::setup().await;
    // answered 1 h, 3 h, 2 h ago: newest first is CS-1, CS-3, CS-2; a 3 and a 5 are not low
    for (case, rating, comment, h) in [
        ("CS-1", 1, Some("newest"), 1),
        ("CS-2", 2, None, 3),
        ("CS-3", 2, Some("middle"), 2),
        ("CS-4", 3, Some("fine"), 1),
        ("CS-5", 5, None, 1),
    ] {
        seed(
            &c,
            Row {
                answer: Some((rating, comment, h)),
                ..row(case, "web_chat", "answered")
            },
        )
        .await;
    }
    let (s, b) = report(&c, "u-mgr", "").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let low = b["low_scores"].as_array().unwrap();
    let got: Vec<(&str, i64, Option<&str>)> = low
        .iter()
        .map(|r| {
            (
                r["case_id"].as_str().unwrap(),
                r["rating"].as_i64().unwrap(),
                r["comment"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("CS-1", 1, Some("newest")),
            ("CS-3", 2, Some("middle")),
            ("CS-2", 2, None)
        ],
        "{b}"
    );
    assert_eq!(low[0]["conversation_id"], "conv-CS-1", "{b}");
    assert_eq!(
        (low[0]["job_key"].as_str(), low[0]["channel"].as_str()),
        (Some("j1"), Some("web_chat")),
        "{b}"
    );
    assert!(low[0]["answered_at"].is_string(), "{b}");
}
