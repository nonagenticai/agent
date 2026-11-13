//! B8 T15a — discovery hygiene (F-51 scorecard weight, F-52 preview / chosen columns / XLSX / delete). Real
//! Postgres 18 through the HTTP router as `agent_app`; the fixtures are `discovery_import`'s.
use std::io::{Cursor, Write};
use std::sync::Arc;
use std::time::Duration;

use agent_api::discovery::worker::{self, Classified, Classifier, ImportRef, Outcome, Pending};
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::b8::discovery_import::{
    Fx, classified, form, import_ref, latest, ok, other, post_form, refused, setup, stored_rows,
    upload, uri, wait_unleased,
};
use crate::common::{self, get, send};

const CSV: &str = "text\nwhere is my order\nWhere is my order\nwhere is my order\nrefund please\n";

async fn put_weight(x: &Fx, activity: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(
            "PUT",
            &uri(&x.f, &format!("/scorecard/{activity}")),
            Some(&x.designer),
            body,
        ),
    )
    .await
}

fn volume<'a>(state: &'a Value, activity: &str) -> &'a Value {
    state["imports"][0]["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["activity"] == activity)
        .unwrap_or_else(|| panic!("no volume for {activity}: {state}"))
}

#[tokio::test]
async fn weight_scales_valuable() {
    let x = setup().await;
    classified(&x, CSV).await;
    let before = ok(send(&x.e.app, get(&uri(&x.f, ""), Some(&x.designer))).await);
    let v = volume(&before, "delivery:where_is_my_order");
    assert_eq!(v["weight"], 100);
    assert!((v["valuable"].as_f64().unwrap() - 0.75).abs() < 1e-6, "{v}");
    let after = ok(put_weight(&x, "delivery:where_is_my_order", json!({"weight": 200})).await);
    let v = volume(&after, "delivery:where_is_my_order");
    assert_eq!(v["weight"], 200);
    assert!((v["valuable"].as_f64().unwrap() - 1.5).abs() < 1e-6, "{v}");
    // the other activity keeps the default
    assert_eq!(volume(&after, "billing:refund")["weight"], 100);
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'discovery.weight_set'",
    )
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[tokio::test]
async fn weight_bounds_422() {
    let x = setup().await;
    classified(&x, CSV).await;
    for bad in [
        json!({"weight": 1001}),
        json!({"weight": -1}),
        json!({"weight": 2.5}),
        json!({"weight": "100"}),
    ] {
        refused(
            put_weight(&x, "delivery:where_is_my_order", bad).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_weight",
        );
    }
    ok(put_weight(&x, "delivery:where_is_my_order", json!({"weight": 0})).await);
    ok(put_weight(&x, "delivery:where_is_my_order", json!({"weight": 1000})).await);
    refused(
        put_weight(&x, "nowhere:nothing", json!({"weight": 50})).await,
        StatusCode::NOT_FOUND,
        "activity_not_found",
    );
}

#[tokio::test]
async fn default_weight_deletes_row() {
    let x = setup().await;
    classified(&x, CSV).await;
    ok(put_weight(&x, "delivery:where_is_my_order", json!({"weight": 300})).await);
    let n = || {
        let owner = x.e.owner.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_weight")
                .fetch_one(&owner)
                .await
                .unwrap()
        }
    };
    assert_eq!(n().await, 1);
    ok(put_weight(&x, "delivery:where_is_my_order", json!({"weight": 100})).await);
    assert_eq!(n().await, 0);
}

#[tokio::test]
async fn preview_stores_nothing_and_calls_no_model() {
    let x = setup().await;
    let body = form(
        (
            "t.csv",
            b"id,Subject,count\n1,mail anna@example.com,3\n2,refund please,1\n",
        ),
        &[],
    );
    let v = ok(post_form(&x, &x.f, &x.designer, "/imports/preview", body).await);
    assert_eq!(v["format"], "csv");
    assert_eq!(v["columns"], json!(["id", "Subject", "count"]));
    assert_eq!(v["rows"], 2);
    assert_eq!(v["suggested_text_column"], "Subject");
    assert_eq!(v["suggested_count_column"], "count");
    assert_eq!(v["sample_rows"][0][1], "mail [email]", "{v}");
    let imports: i64 = sqlx::query_scalar("SELECT count(*) FROM discovery_import")
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(imports, 0);
    let usage: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event")
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(usage, 0);
    refused(
        post_form(
            &x,
            &x.f,
            &x.designer,
            "/imports/preview",
            form(("t.docx", b"x"), &[]),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported_file",
    );
}

#[tokio::test]
async fn chosen_text_column_is_used() {
    let x = setup().await;
    let v = ok(upload(
        &x,
        "t.csv",
        b"subject,notes\nwhere is my order,customer called twice\n",
        &[("text_column", "notes")],
    )
    .await);
    assert_eq!(v["imports"][0]["text_column"], "notes");
    let rows = stored_rows(&x, &latest(&v)).await;
    assert_eq!(rows[0].1.as_deref(), Some("customer called twice"));
}

#[tokio::test]
async fn count_column_weights_rows() {
    let x = setup().await;
    let v = ok(upload(
        &x,
        "t.csv",
        b"text,n\nwhere is my order,40\nWHERE is my order,2\nrefund,\nignored,0\n",
        &[("count_column", "n")],
    )
    .await);
    assert_eq!(v["imports"][0]["count_column"], "n");
    let rows = stored_rows(&x, &latest(&v)).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].2, 42);
    assert_eq!(rows[1].2, 1, "a blank count counts once");
    refused(
        upload(
            &x,
            "t.csv",
            b"text,n\nwhere is my order,lots\n",
            &[("count_column", "n")],
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "count_not_numeric",
    );
}

#[tokio::test]
async fn unknown_column_422() {
    let x = setup().await;
    for field in ["text_column", "count_column"] {
        refused(
            upload(&x, "t.csv", b"text\nhello\n", &[(field, "nope")]).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_column",
        );
    }
}

/// A zip of `(part name, contents)`, deflated, built in memory.
fn zip_of(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in parts {
        w.start_file(name.as_str(), opts).unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// A workbook of `(sheet name, rows)`, every cell an inline string.
fn workbook(sheets: &[(&str, Vec<Vec<&str>>)]) -> Vec<u8> {
    const NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let mut wb = format!("<?xml version=\"1.0\"?><workbook {NS}><sheets>");
    let mut rels = String::from(
        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    for (i, (name, rows)) in sheets.iter().enumerate() {
        let n = i + 1;
        wb.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
        ));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
        ));
        let data: String = rows
            .iter()
            .map(|r| {
                let cells: String = r
                    .iter()
                    .map(|c| format!(r#"<c t="inlineStr"><is><t>{c}</t></is></c>"#))
                    .collect();
                format!("<row>{cells}</row>")
            })
            .collect();
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                "<?xml version=\"1.0\"?><worksheet {NS}><sheetData>{data}</sheetData></worksheet>"
            )
            .into_bytes(),
        ));
    }
    wb.push_str("</sheets></workbook>");
    rels.push_str("</Relationships>");
    parts.push(("xl/workbook.xml".into(), wb.into_bytes()));
    parts.push(("xl/_rels/workbook.xml.rels".into(), rels.into_bytes()));
    zip_of(&parts)
}

#[tokio::test]
async fn xlsx_first_sheet_parses() {
    let x = setup().await;
    let book = workbook(&[
        (
            "Tickets",
            vec![
                vec!["Ref", "Summary", "Volume"],
                vec!["ZD-1", "where is my order", "5"],
                vec!["ZD-2", "refund please", "2"],
            ],
        ),
        (
            "Other",
            vec![vec!["Summary"], vec!["this sheet is never read"]],
        ),
    ]);
    let preview = ok(post_form(
        &x,
        &x.f,
        &x.designer,
        "/imports/preview",
        form(("export.xlsx", &book), &[]),
    )
    .await);
    assert_eq!(preview["format"], "xlsx");
    assert_eq!(preview["columns"], json!(["Ref", "Summary", "Volume"]));
    assert_eq!(preview["suggested_count_column"], "Volume");
    let v = ok(upload(
        &x,
        "export.xlsx",
        &book,
        &[("text_column", "Summary"), ("count_column", "Volume")],
    )
    .await);
    assert_eq!(v["imports"][0]["format"], "xlsx");
    let rows = stored_rows(&x, &latest(&v)).await;
    let texts: Vec<_> = rows.iter().map(|r| r.1.clone().unwrap()).collect();
    assert_eq!(texts, ["where is my order", "refund please"]);
    assert_eq!(rows[0].2, 5);
}

#[tokio::test]
async fn delete_cascades_rows_and_clusters_keeps_decisions() {
    let x = setup().await;
    let id = classified(&x, CSV).await;
    sqlx::query(
        "INSERT INTO discovery_cluster (tenant_id, job_id, import_id, id, label, tickets, suggestion) \
         VALUES ($1, $2, $3, 'cl-1', 'Refunds', 1, '{\"kind\": \"enable_activity\"}')",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(&id)
    .execute(&x.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO discovery_decision (tenant_id, job_id, proposal_id, state, by, keys) \
         VALUES ($1, $2, 'cl-1', 'accepted', 'u', '{journey:billing:refund}')",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let (s, v) = send(
        &x.e.app,
        common::json(
            "DELETE",
            &uri(&x.f, &format!("/imports/{id}")),
            Some(&x.designer),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["imports"], json!([]));
    let count = |t: &'static str| {
        let owner = x.e.owner.clone();
        async move {
            sqlx::query_scalar::<_, i64>(match t {
                "row" => "SELECT count(*) FROM discovery_row",
                "cluster" => "SELECT count(*) FROM discovery_cluster",
                _ => "SELECT count(*) FROM discovery_decision",
            })
            .fetch_one(&owner)
            .await
            .unwrap()
        }
    };
    assert_eq!(count("row").await, 0);
    assert_eq!(count("cluster").await, 0);
    assert_eq!(count("decision").await, 1);
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'discovery.import_delete' AND resource_id = $1",
    )
    .bind(&id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, 1);
    // a second delete is 404
    let got = send(
        &x.e.app,
        common::json(
            "DELETE",
            &uri(&x.f, &format!("/imports/{id}")),
            Some(&x.designer),
            Value::Null,
        ),
    )
    .await;
    refused(got, StatusCode::NOT_FOUND, "import_not_found");
}

/// Blocks inside its first batch until released.
struct Gate {
    entered: Notify,
    release: Notify,
}

impl Classifier for Gate {
    fn classify<'a>(
        &'a self,
        _at: &'a ImportRef,
        rows: &'a [Pending],
    ) -> BoxFuture<'a, Option<Vec<Classified>>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Some(
                rows.iter()
                    .map(|r| Classified {
                        text_hash: r.text_hash.clone(),
                        bucket: worker::Bucket::Covered,
                        stage: None,
                        activity: Some("delivery:where_is_my_order".into()),
                        confidence: None,
                        keep_excerpt: true,
                    })
                    .collect(),
            )
        })
    }
}

#[tokio::test]
async fn delete_classifying_import_stops_worker() {
    let x = setup().await;
    let mut csv = String::from("text\n");
    for i in 0..200 {
        csv.push_str(&format!("question number {i} about my parcel\n"));
    }
    let id = latest(&ok(upload(&x, "t.csv", csv.as_bytes(), &[]).await));
    let gate = Arc::new(Gate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    // The upload's own unwired worker may still hold the lease; spawned over it, ours is refused
    // (HeldElsewhere) and never reaches the gate.
    wait_unleased(&x, &id).await;
    let mut run = worker::spawn(x.e.state.db.clone(), gate.clone(), import_ref(&x.f, &id));
    tokio::select! {
        got = tokio::time::timeout(Duration::from_secs(30), gate.entered.notified()) => {
            got.expect("classifier not entered within 30s of spawning the worker");
        }
        out = &mut run => panic!("worker ended before reaching the classifier: {out:?}"),
    }
    let (s, v) = send(
        &x.e.app,
        common::json(
            "DELETE",
            &uri(&x.f, &format!("/imports/{id}")),
            Some(&x.designer),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    gate.release.notify_one();
    let out = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("worker did not stop within 30s after the import was deleted");
    assert_eq!(out.unwrap(), Outcome::Cancelled);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM discovery_row WHERE import_id = $1")
        .bind(&id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn other_tenant_cannot_delete_import() {
    let x = setup().await;
    let id = latest(&ok(
        upload(&x, "t.csv", b"text\nwhere is my order\n", &[]).await
    ));
    let (g, token) = other(&x).await;
    let got = send(
        &x.e.app,
        common::json(
            "DELETE",
            &uri(&g, &format!("/imports/{id}")),
            Some(&token),
            Value::Null,
        ),
    )
    .await;
    refused(got, StatusCode::NOT_FOUND, "import_not_found");
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM discovery_import WHERE id = $1")
        .bind(&id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(still, 1);
}
