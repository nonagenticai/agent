//! SW-9 staff attachments: upload / download / delete (crate::conversations::attachments_routes), the staff reply that
//! carries attachments (crate::conversations::staff), the transcript that shows them, and the lifecycle (retention
//! cascade, the 1 h sweep) and the artifact_service backing (a fake service pinned to the tenant's licence: auth
//! headers, refusals, tombstones). There is no data-subject erasure path in agent-api
//! to test. Bytes live at the fake artifact_service and are only read through the HTTP download. Real Postgres 18.
use crate::common;

use std::io::Cursor;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use common::artifacts_fake::Mode;
use common::conv::{self, Ctx, code};
use common::{insert_tenant, insert_user, person};
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat, RgbImage};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// A web chat conversation whose customer asked for a person, taken by `u-agent`: its id.
async fn taken(c: &Ctx, wc: &str) -> String {
    let (s, b) = c
        .inbound(wc, &format!("{wc}-1"), "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let id = b["conversation_id"].as_str().unwrap().to_string();
    let (s, b) = c.take(&id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    id
}

fn jpeg_with_exif() -> Vec<u8> {
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([(x * 4) as u8, (y * 5) as u8, 128])
    }));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Jpeg).unwrap();
    let plain = out.into_inner();
    let payload = b"Exif\0\0SECRET-GPS-52.3676N-4.9041E";
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1];
    v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(payload);
    v.extend_from_slice(&plain[2..]);
    v
}

/// A one-page PDF with a correct xref table; `root_extra` goes into the catalog, `extra` become objects 4, 5, ...
fn pdf(root_extra: &str, extra: &[&str]) -> Vec<u8> {
    let mut objs = vec![
        format!("<< /Type /Catalog /Pages 2 0 R {root_extra} >>"),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>".to_owned(),
    ];
    objs.extend(extra.iter().map(|s| (*s).to_owned()));
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn multipart(field: &str, file_name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----sw9boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{file_name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn raw(c: &Ctx, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = c.e.app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

fn as_json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

async fn upload(c: &Ctx, conv: &str, sub: &str, name: &str, bytes: &[u8]) -> (StatusCode, Value) {
    let (ct, body) = multipart("file", name, bytes);
    let req = Request::post(format!("/api/jobs/j1/conversations/{conv}/attachments"))
        .header("authorization", format!("Bearer {}", person(sub, &[])))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let (s, _, b) = raw(c, req).await;
    (s, as_json(&b))
}

async fn download(c: &Ctx, conv: &str, att: &str, sub: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let req = Request::get(format!(
        "/api/jobs/j1/conversations/{conv}/attachments/{att}"
    ))
    .header("authorization", format!("Bearer {}", person(sub, &[])))
    .body(Body::empty())
    .unwrap();
    raw(c, req).await
}

async fn delete(c: &Ctx, conv: &str, att: &str, sub: &str) -> (StatusCode, Value) {
    c.call(
        "DELETE",
        &format!("/api/jobs/j1/conversations/{conv}/attachments/{att}"),
        sub,
        Value::Null,
    )
    .await
}

#[tokio::test]
async fn jpeg_is_reencoded_without_exif_and_served_with_hardening_headers() {
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    let src = jpeg_with_exif();
    assert!(contains(&src, b"SECRET-GPS"));
    let (s, b) = upload(&c, &conv_id, "u-agent", "../../IMG 1.JPG", &src).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["media_type"], "image/jpeg");
    assert_eq!(
        (b["width"].as_i64(), b["height"].as_i64()),
        (Some(64), Some(48))
    );
    assert_eq!(b["display_name"], "IMG 1.jpg");
    assert!(b.get("bytes").is_none() && b.get("sha256").is_none(), "{b}");
    let att = b["id"].as_str().unwrap().to_string();

    // any member of the job reads it
    let (s, h, bytes) = download(&c, &conv_id, &att, "u-obs").await;
    assert_eq!(s, StatusCode::OK);
    assert!(!contains(&bytes, b"SECRET-GPS") && !contains(&bytes, b"Exif"));
    assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 64);
    let hv = |n: &str| h.get(n).unwrap().to_str().unwrap().to_string();
    assert_eq!(hv("content-type"), "image/jpeg");
    assert_eq!(hv("x-content-type-options"), "nosniff");
    assert_eq!(hv("cache-control"), "private, no-store");
    assert_eq!(hv("content-security-policy"), "sandbox; default-src 'none'");
    let cd = hv("content-disposition");
    assert!(
        cd.starts_with("inline;") && cd.contains("filename*=UTF-8''IMG%201.jpg"),
        "{cd}"
    );

    // the audit rows carry id, type and size: never the name
    for action in [
        "conversation_attachment.upload",
        "conversation_attachment.download_staff",
    ] {
        let d: Vec<Value> = sqlx::query_scalar("SELECT details FROM audit_event WHERE action = $1")
            .bind(action)
            .fetch_all(&c.e.owner)
            .await
            .unwrap();
        assert_eq!(d.len(), 1, "{action}");
        assert_eq!(d[0]["media_type"], "image/jpeg");
        assert!(d[0]["byte_size"].as_i64().unwrap() > 0);
        assert!(!d[0].to_string().contains("IMG"), "{action}: no file name");
    }
}

#[tokio::test]
async fn pdf_is_served_as_a_download() {
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    let src = pdf("", &[]);
    let (s, b) = upload(&c, &conv_id, "u-agent", "Receipt.PDF", &src).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["media_type"], "application/pdf");
    let (s, h, bytes) = download(&c, &conv_id, b["id"].as_str().unwrap(), "u-mgr").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(bytes, src, "a clean PDF is stored byte-identical");
    let cd = h.get("content-disposition").unwrap().to_str().unwrap();
    assert!(
        cd.starts_with("attachment;") && cd.contains("filename*=UTF-8''Receipt.pdf"),
        "{cd}"
    );
}

#[tokio::test]
async fn refused_uploads_and_roles() {
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    let cases: [(&str, Vec<u8>, &str); 3] = [
        ("notes.txt", b"hello".to_vec(), "unsupported_type"),
        (
            "evil.pdf",
            pdf(
                "/OpenAction 4 0 R",
                &["<< /S /JavaScript /JS (app.alert(1)) >>"],
            ),
            "active_pdf",
        ),
        (
            "broken.pdf",
            b"%PDF-1.4\nnot a pdf".to_vec(),
            "malformed_pdf",
        ),
    ];
    for (name, bytes, reason) in cases {
        let (s, b) = upload(&c, &conv_id, "u-agent", name, &bytes).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {b}");
        assert_eq!(code(&b), "attachment_refused");
        assert_eq!(b["error"]["reason"], reason, "{name}: {b}");
    }
    // an observer may not upload; an unknown conversation is 404; a body without a file is 400
    let (s, b) = upload(&c, &conv_id, "u-obs", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "support_agent");
    let (s, b) = upload(&c, "conv-nope", "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    let (ct, body) = multipart("other", "a.jpg", b"x");
    let req = Request::post(format!("/api/jobs/j1/conversations/{conv_id}/attachments"))
        .header(
            "authorization",
            format!("Bearer {}", person("u-agent", &[])),
        )
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let (s, _, b) = raw(&c, req).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{}", as_json(&b));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation_attachment")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing refused was stored");
}

#[tokio::test]
async fn quota_refusal_is_413() {
    let c = conv::setup_with(|v| {
        v.insert("ATTACHMENT_QUOTA_BYTES".into(), "10".into());
    })
    .await;
    let conv_id = taken(&c, "wc-1").await;
    let (s, b) = upload(&c, &conv_id, "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::PAYLOAD_TOO_LARGE, "attachment_quota"),
        "{b}"
    );
}

#[tokio::test]
async fn other_tenant_other_conversation_and_deleted_are_the_same_404() {
    let c = conv::setup().await;
    let a = taken(&c, "wc-1").await;
    let other = taken(&c, "wc-2").await;
    let (s, b) = upload(&c, &a, "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let att = b["id"].as_str().unwrap().to_string();

    // another conversation of the same job
    let (s, _, bytes) = download(&c, &other, &att, "u-agent").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(code(&as_json(&bytes)), "not_found");
    // a random id, a malformed id
    for id in [Uuid::new_v4().to_string(), "not-a-uuid".to_string()] {
        let (s, _, _) = download(&c, &a, &id, "u-agent").await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{id}");
    }
    // another tenant with its own job `j1` and a support agent
    let beta = Uuid::new_v4();
    insert_tenant(&c.e.owner, beta, "Beta").await;
    insert_user(&c.e.owner, "u-beta", beta, "support_agent").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J1', '[]', 'test')",
    )
    .bind(beta)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, _, _) = download(&c, &a, &att, "u-beta").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "another tenant");
    let (s, b) = delete(&c, &a, &att, "u-beta").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    // deleted (an unsent upload): 204, then 404 for everyone
    let (s, b) = delete(&c, &a, &att, "u-agent").await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, _, _) = download(&c, &a, &att, "u-agent").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = delete(&c, &a, &att, "u-agent").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a second delete");
}

#[tokio::test]
async fn reply_links_attachments_history_shows_refs_and_a_sent_one_cannot_be_deleted() {
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    let (s, a1) = upload(&c, &conv_id, "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::CREATED, "{a1}");
    let (s, a2) = upload(&c, &conv_id, "u-agent", "r.pdf", &pdf("", &[])).await;
    assert_eq!(s, StatusCode::CREATED, "{a2}");
    let ids = [a1["id"].clone(), a2["id"].clone()];

    // an unknown id: the uniform 404, nothing stored, nothing delivered
    let path = format!("/api/jobs/j1/conversations/{conv_id}/messages");
    let send_with = |text: &str, attachments: Value, kind: &str| {
        c.call(
            "POST",
            &path,
            "u-agent",
            json!({"kind": kind, "text": text, "attachments": attachments}),
        )
    };
    let (s, b) = send_with("Here", json!([ids[0], Uuid::new_v4()]), "reply").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(c.edge.delivered().len(), 0);
    assert!(
        c.texts("SELECT text FROM conversation_message WHERE role = 'staff'")
            .await
            .is_empty()
    );
    let (s, b) = send_with("Here", json!([]), "note").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send_with("Here", json!([ids[0]]), "note").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "attachments_on_note")
    );
    let six: Vec<Value> = (0..6).map(|_| json!(Uuid::new_v4())).collect();
    let (s, b) = send_with("Here", json!(six), "reply").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "too_many_attachments")
    );

    let (s, b) = send_with("Here are the files", json!(ids), "reply").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["delivery"], "delivered");
    let refs = b["attachments"].as_array().unwrap();
    assert_eq!(refs.len(), 2, "{b}");
    assert!(
        refs.iter()
            .any(|r| r["id"] == ids[0] && r["media_type"] == "image/jpeg")
    );
    assert!(
        refs.iter()
            .any(|r| r["id"] == ids[1] && r["media_type"] == "application/pdf")
    );
    // the edge got the refs (DeliverMessage.attachments), never bytes
    let sent = c.edge.delivered();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["message"]["attachments"].as_array().unwrap().len(),
        2
    );
    assert!(!sent[0].to_string().contains("sha256"));

    // the transcript shows the refs on the staff message
    let (s, t) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let m = t["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["text"] == "Here are the files")
        .expect("the reply is in the transcript");
    assert_eq!(m["attachments"].as_array().unwrap().len(), 2, "{m}");

    // sent: not deletable, not reusable
    let (s, b) = delete(&c, &conv_id, ids[0].as_str().unwrap(), "u-agent").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "attachment_sent"));
    let (s, b) = send_with("Again", json!([ids[0]]), "reply").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    // and the file is still there
    let (s, _, _) = download(&c, &conv_id, ids[0].as_str().unwrap(), "u-obs").await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn reply_with_attachments_is_refused_on_a_channel_without_files() {
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    sqlx::query("UPDATE conversation SET channel = 'email' WHERE conversation_id = $1")
        .bind(&conv_id)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conv_id}/messages"),
            "u-agent",
            json!({"kind": "reply", "text": "Hi", "attachments": [Uuid::new_v4()]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "attachments_not_supported"
        )
    );
}

/// An attachment row inserted directly, its bytes (`%PDF`) put at the fake artifact_service: `(id, artifact_id)`.
async fn insert_row(
    c: &Ctx,
    conv: &str,
    message: Option<Uuid>,
    age: &str,
    deleted: bool,
) -> (Uuid, Uuid) {
    let artifact = Uuid::new_v4();
    c.artifacts.put(artifact, "application/pdf", b"%PDF");
    let id = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "INSERT INTO conversation_attachment (tenant_id, job_id, conversation_id, message_id, direction, media_type, \
            byte_size, sha256, display_name, artifact_id, created_by, created_at, deleted_at) \
         VALUES ($1, $2, $3, $4, 'staff', 'application/pdf', 4, decode(repeat('00', 32), 'hex'), 'a.pdf', $6, \
                 'u', now() - interval '{age}', CASE WHEN $5 THEN now() - interval '{age}' END) \
         RETURNING id"
    )))
    .bind(c.t)
    .bind(c.job)
    .bind(conv)
    .bind(message)
    .bind(deleted)
    .bind(artifact)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    (id, artifact)
}

async fn bare_conversation(c: &Ctx, id: &str) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id, initiator_sub) \
         VALUES ($1, $2, $3, 'prod', 'staff', 'cust-1', 'u-agent')",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    common::state::put(&c.e.owner, c.t, c.job, id, "name", "Emma").await;
}

async fn tombstones(c: &Ctx) -> Vec<Uuid> {
    let mut v: Vec<Uuid> = sqlx::query_scalar(
        "SELECT artifact_id FROM conversation_attachment_tombstone ORDER BY artifact_id",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    v.sort();
    v
}

fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
    v.sort();
    v
}

#[tokio::test]
async fn deleting_a_conversation_takes_its_attachments_with_it_and_the_worker_deletes_the_bytes() {
    let c = conv::setup().await;
    bare_conversation(&c, "conv-gone").await;
    bare_conversation(&c, "conv-stays").await;
    let (_, gone) = insert_row(&c, "conv-gone", None, "1 minute", false).await;
    let (_, stays) = insert_row(&c, "conv-stays", None, "1 minute", false).await;
    sqlx::query("DELETE FROM conversation WHERE conversation_id = 'conv-gone'")
        .execute(&c.e.owner)
        .await
        .unwrap();
    let left: Vec<String> =
        sqlx::query_scalar("SELECT conversation_id FROM conversation_attachment")
            .fetch_all(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(left, ["conv-stays"]);
    // the cascade queued exactly the deleted row's artifact
    assert_eq!(tombstones(&c).await, [gone]);
    let o = agent_api::attachments::sweep::process_tombstones(&c.e.state)
        .await
        .unwrap();
    assert_eq!((o.deleted, o.failed), (1, 0));
    assert_eq!(c.artifacts.deletes(), [gone]);
    assert!(!c.artifacts.holds(gone) && c.artifacts.holds(stays));
    assert!(tombstones(&c).await.is_empty());
    assert_eq!(
        c.artifacts
            .bad_auth
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn the_sweep_removes_old_unsent_and_deleted_uploads_only_and_queues_their_bytes() {
    let c = conv::setup().await;
    bare_conversation(&c, "conv-s").await;
    let msg = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, message_id, role, author, text, \
                                           refused, at_ms) \
         VALUES ($1, $2, 'conv-s', $3, 'staff', 'x', 'hi', false, 1)",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(msg)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (old_unsent, old_unsent_art) = insert_row(&c, "conv-s", None, "2 hours", false).await;
    let (fresh, _) = insert_row(&c, "conv-s", None, "5 minutes", false).await;
    let (old_linked, linked_art) = insert_row(&c, "conv-s", Some(msg), "3 days", false).await;
    let (old_deleted, old_deleted_art) = insert_row(&c, "conv-s", None, "2 hours", true).await;
    let n = agent_api::attachments::sweep::sweep(&c.e.state.db)
        .await
        .unwrap();
    assert_eq!(n, 2);
    let mut left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM conversation_attachment")
        .fetch_all(&c.e.owner)
        .await
        .unwrap();
    left.sort();
    let mut want = vec![fresh, old_linked];
    want.sort();
    assert_eq!(left, want);
    assert!(![old_unsent, old_deleted].iter().any(|i| left.contains(i)));
    // the sweep (an unsent upload and a staff-deleted one) queued exactly their artifacts
    assert_eq!(
        tombstones(&c).await,
        sorted(vec![old_unsent_art, old_deleted_art])
    );
    let (swept, o) = agent_api::attachments::sweep::tick(&c.e.state)
        .await
        .unwrap();
    assert_eq!((swept, o.deleted, o.failed), (0, 2, 0));
    assert_eq!(
        sorted(c.artifacts.deletes()),
        sorted(vec![old_unsent_art, old_deleted_art])
    );
    assert!(c.artifacts.holds(linked_art), "a sent attachment stays");
    assert!(tombstones(&c).await.is_empty());
}

#[tokio::test]
async fn a_failed_remote_delete_keeps_the_tombstone_and_is_retried_after_the_backoff() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    bare_conversation(&c, "conv-x").await;
    let (_, art) = insert_row(&c, "conv-x", None, "1 minute", false).await;
    sqlx::query("DELETE FROM conversation WHERE conversation_id = 'conv-x'")
        .execute(&c.e.owner)
        .await
        .unwrap();
    c.artifacts.delete_fails.store(true, Ordering::SeqCst);
    let st = &c.e.state;
    let o = agent_api::attachments::sweep::process_tombstones(st)
        .await
        .unwrap();
    assert_eq!((o.deleted, o.failed), (0, 1));
    let (attempts, errored): (i32, bool) = sqlx::query_as(
        "SELECT attempts, last_error_at IS NOT NULL FROM conversation_attachment_tombstone",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!((attempts, errored), (1, true));
    assert!(c.artifacts.holds(art));
    // inside the back-off nothing is tried again
    let o = agent_api::attachments::sweep::process_tombstones(st)
        .await
        .unwrap();
    assert_eq!((o.deleted, o.failed), (0, 0));
    // after it, with the service back, the tombstone is worked off
    sqlx::query(
        "UPDATE conversation_attachment_tombstone SET last_error_at = now() - interval '2 hours'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    c.artifacts.delete_fails.store(false, Ordering::SeqCst);
    let o = agent_api::attachments::sweep::process_tombstones(st)
        .await
        .unwrap();
    assert_eq!((o.deleted, o.failed), (1, 0));
    assert!(!c.artifacts.holds(art) && tombstones(&c).await.is_empty());
}

#[tokio::test]
async fn a_tombstone_for_an_artifact_that_is_gone_counts_as_done_and_one_without_a_licence_stays() {
    let c = conv::setup().await;
    let gone = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO conversation_attachment_tombstone (tenant_id, artifact_id) VALUES ($1, $2)",
    )
    .bind(c.t)
    .bind(gone)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let o = agent_api::attachments::sweep::process_tombstones(&c.e.state)
        .await
        .unwrap();
    assert_eq!((o.deleted, o.failed), (1, 0), "the fake answers 404");
    // no licence: nothing may be sent (never a bare tenant header), the tombstone waits
    sqlx::query(
        "UPDATE tenants SET licence_id = NULL, licence_token_encrypted = NULL WHERE id = $1",
    )
    .bind(c.t)
    .execute(&c.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_attachment_tombstone (tenant_id, artifact_id) VALUES ($1, $2)",
    )
    .bind(c.t)
    .bind(gone)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let before = c.artifacts.deletes().len();
    let o = agent_api::attachments::sweep::process_tombstones(&c.e.state)
        .await
        .unwrap();
    assert_eq!((o.deleted, o.failed), (0, 1));
    assert_eq!(c.artifacts.deletes().len(), before);
    assert_eq!(tombstones(&c).await, [gone]);
}

#[tokio::test]
async fn upload_stores_the_bytes_at_artifact_service_under_the_tenants_licence() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    let src = pdf("", &[]);
    let (s, b) = upload(&c, &conv_id, "u-agent", "../../Secret Name.PDF", &src).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let att: Uuid = b["id"].as_str().unwrap().parse().unwrap();
    // one upload, private, named by the attachment id and never by the display name
    let ups = c.artifacts.uploads();
    assert_eq!(ups.len(), 1);
    let u = &ups[0];
    assert_eq!(u.visibility, "user");
    assert_eq!(u.sensitivity, "secret");
    assert_eq!(
        (u.kind.as_str(), u.content_type.as_str()),
        ("doc", "application/pdf")
    );
    assert_eq!(u.filename, format!("{att}.pdf"));
    assert!(u.tags.contains("conversation_attachment") && u.tags.contains(&format!("att:{att}")));
    assert!(!u.filename.contains("Secret") && !u.tags.contains("Secret"));
    assert_eq!(u.bytes, src);
    // the row names the artifact; no bytes column exists any more
    let artifact: Uuid =
        sqlx::query_scalar("SELECT artifact_id FROM conversation_attachment WHERE id = $1")
            .bind(att)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(artifact, u.artifact);
    let cols: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
          WHERE table_name = 'conversation_attachment' AND column_name = 'bytes'",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(cols, 0);
    // the download reads them back from the service
    let (s, _, bytes) = download(&c, &conv_id, &att.to_string(), "u-obs").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(bytes, src);
    assert_eq!(
        c.artifacts.bad_auth.load(Ordering::SeqCst),
        0,
        "Bearer licence + X-Tenant-Id every time"
    );
    // a 403 from the service is a result: the download fails closed and nothing is retried bare
    c.artifacts.pin(c.t, "another-licence");
    let (s, _, bytes) = download(&c, &conv_id, &att.to_string(), "u-obs").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let b = as_json(&bytes);
    assert_eq!(
        (code(&b), b["error"]["reason"].as_str()),
        ("attachment_refused", Some("storage_unavailable"))
    );
    assert!(c.artifacts.bad_auth.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn artifact_service_refusals_map_to_reasons_and_store_nothing() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    let cases = [
        (Mode::Malware, StatusCode::UNPROCESSABLE_ENTITY, "malware"),
        (
            Mode::ScanDown,
            StatusCode::SERVICE_UNAVAILABLE,
            "scan_unavailable",
        ),
        (
            Mode::Plain422,
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
        ),
        (
            Mode::Plain503,
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
        ),
        (
            Mode::Fail500,
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
        ),
    ];
    for (mode, status, reason) in cases {
        c.artifacts.set_mode(mode);
        let (s, b) = upload(&c, &conv_id, "u-agent", "a.jpg", &jpeg_with_exif()).await;
        assert_eq!(s, status, "{mode:?}: {b}");
        assert_eq!(code(&b), "attachment_refused", "{mode:?}: {b}");
        assert_eq!(b["error"]["reason"], reason, "{mode:?}: {b}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation_attachment")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing refused was stored");
    assert_eq!(c.artifacts.stored(), 0);
    assert_eq!(c.artifacts.bad_auth.load(Ordering::SeqCst), 0);
    // back to normal: accepted
    c.artifacts.set_mode(Mode::Ok);
    let (s, b) = upload(&c, &conv_id, "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn an_unreachable_artifact_service_refuses_with_storage_unavailable() {
    let c = conv::setup_with(|v| {
        v.insert("ARTIFACTS_URL".into(), common::NO_UPSTREAM.into());
    })
    .await;
    let conv_id = taken(&c, "wc-1").await;
    let (s, b) = upload(&c, &conv_id, "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["reason"], "storage_unavailable");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation_attachment")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn a_tenant_without_a_licence_cannot_store_a_file_and_nothing_is_sent() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    sqlx::query(
        "UPDATE tenants SET licence_id = NULL, licence_token_encrypted = NULL WHERE id = $1",
    )
    .bind(c.t)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = upload(&c, &conv_id, "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["reason"], "storage_unavailable");
    assert_eq!(
        c.artifacts.upload_attempts.load(Ordering::SeqCst),
        0,
        "no bare X-Tenant-Id fallback"
    );
}

#[tokio::test]
async fn an_insert_that_fails_after_the_upload_deletes_the_artifact_or_queues_its_tombstone() {
    use agent_api::attachments::store::{
        AttachmentStore, Direction, NewAttachment, PgAttachmentStore,
    };
    use agent_api::attachments::validate;
    use agent_api::error::ApiError;
    let c = conv::setup().await;
    let accepted = validate::accept(&pdf("", &[]), "a.pdf").unwrap();
    let store = PgAttachmentStore::for_state(&c.e.state);
    let put = |conv: &'static str| {
        let (db, t, job, store, accepted) = (
            c.e.state.db.clone(),
            c.t,
            c.job,
            store.clone(),
            accepted.clone(),
        );
        async move {
            let mut tx = agent_api::conversations::store::begin_job(&db, t, job)
                .await
                .unwrap();
            let r = store
                .put(
                    &mut tx,
                    NewAttachment {
                        conversation_id: conv,
                        direction: Direction::Staff,
                        created_by: "u-agent",
                        accepted: &accepted,
                    },
                )
                .await;
            // the savepoint left the transaction usable
            let one: i32 = sqlx::query_scalar("SELECT 1")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(one, 1);
            tx.rollback().await.unwrap();
            r
        }
    };
    // an unknown conversation violates the FK after the upload: the artifact is deleted again
    let err = put("conv-nope").await.unwrap_err();
    assert!(matches!(err, ApiError::Missing(..)), "{err:?}");
    assert_eq!(c.artifacts.uploads().len(), 1);
    assert_eq!(c.artifacts.stored(), 0);
    assert_eq!(c.artifacts.deletes().len(), 1);
    assert!(tombstones(&c).await.is_empty());
    // the service cannot delete either: the tombstone is queued outside the failed transaction
    c.artifacts
        .delete_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let err = put("conv-nope").await.unwrap_err();
    assert!(matches!(err, ApiError::Missing(..)), "{err:?}");
    let queued = tombstones(&c).await;
    assert_eq!(queued.len(), 1);
    assert!(c.artifacts.holds(queued[0]));
}

#[tokio::test]
async fn reply_may_have_no_text_when_it_carries_attachments() {
    let c = conv::setup().await;
    let conv_id = taken(&c, "wc-1").await;
    let (s, a) = upload(&c, &conv_id, "u-agent", "a.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::CREATED, "{a}");
    let path = format!("/api/jobs/j1/conversations/{conv_id}/messages");
    let send_with = |text: &str, attachments: Value, kind: &str| {
        c.call(
            "POST",
            &path,
            "u-agent",
            json!({"kind": kind, "text": text, "attachments": attachments}),
        )
    };
    // no text and no attachments, and a note without text, are still refused
    let (s, b) = send_with("  ", json!([]), "reply").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "empty_message"),
        "{b}"
    );
    let (s, b) = send_with("", json!([]), "note").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "empty_message"),
        "{b}"
    );
    // an attachment-only reply is stored with empty text, delivered, and shown in the transcript
    let (s, b) = send_with("", json!([a["id"]]), "reply").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["text"], "");
    assert_eq!(b["delivery"], "delivered");
    assert_eq!(b["attachments"].as_array().unwrap().len(), 1, "{b}");
    let sent = c.edge.delivered();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["message"]["text"], "");
    assert_eq!(
        sent[0]["message"]["attachments"].as_array().unwrap().len(),
        1
    );
    let (s, t) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let m = t["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["message_id"] == b["message_id"])
        .expect("in the transcript");
    assert_eq!(
        (
            m["text"].as_str(),
            m["attachments"].as_array().map(Vec::len)
        ),
        (Some(""), Some(1))
    );
}

// ---- upload intents (0223): no orphan between the upload and the row ----

async fn intents(c: &Ctx) -> Vec<Uuid> {
    let mut v: Vec<Uuid> = sqlx::query_scalar(
        "SELECT attachment_id FROM conversation_attachment_upload_intent ORDER BY attachment_id",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    v.sort();
    v
}

async fn age_intents(c: &Ctx, age: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE conversation_attachment_upload_intent SET created_at = now() - interval '{age}'"
    )))
    .execute(&c.e.owner)
    .await
    .unwrap();
}

/// What a process that died right after the upload leaves: the committed intent and the tagged artifact, no row.
async fn crashed_upload(c: &Ctx) -> (Uuid, Uuid) {
    use agent_api::attachments::artifacts::ArtifactsClient;
    let accepted = agent_api::attachments::validate::accept(&pdf("", &[]), "a.pdf").unwrap();
    let client = ArtifactsClient::for_state(&c.e.state);
    let lic = client.licence_of(c.t).await.unwrap().unwrap();
    let att = Uuid::new_v4();
    client.open_intent(c.t, att).await.unwrap();
    let artifact = client.upload(&lic, att, &accepted).await.unwrap();
    (att, artifact)
}

#[tokio::test]
async fn a_crash_between_upload_and_row_is_resolved_by_the_sweep_through_the_tag() {
    let c = conv::setup().await;
    let (att, artifact) = crashed_upload(&c).await;
    let (other_att, other_artifact) = crashed_upload(&c).await;
    assert_eq!(intents(&c).await, sorted(vec![att, other_att]));
    assert!(c.artifacts.holds(artifact));
    // fresh intents (an upload may still be running) are left alone, and so are their bytes
    let o = agent_api::attachments::sweep::process_intents(&c.e.state)
        .await
        .unwrap();
    assert_eq!((o.resolved, o.failed), (0, 0));
    assert!(c.artifacts.holds(artifact) && c.artifacts.holds(other_artifact));
    // only the old one is resolved: its artifact is found by the `att:<id>` tag and deleted
    sqlx::query(
        "UPDATE conversation_attachment_upload_intent SET created_at = now() - interval '20 minutes' \
         WHERE attachment_id = $1",
    )
    .bind(att)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let o = agent_api::attachments::sweep::process_intents(&c.e.state)
        .await
        .unwrap();
    assert_eq!((o.resolved, o.failed), (1, 0));
    assert!(!c.artifacts.holds(artifact), "the orphan is deleted");
    assert!(
        c.artifacts.holds(other_artifact),
        "another upload's bytes stay"
    );
    assert_eq!(c.artifacts.deletes(), [artifact]);
    assert_eq!(intents(&c).await, [other_att]);
    assert_eq!(
        c.artifacts
            .bad_auth
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn an_old_intent_without_an_artifact_is_simply_cleared() {
    let c = conv::setup().await;
    let att = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO conversation_attachment_upload_intent (tenant_id, attachment_id, created_at) \
         VALUES ($1, $2, now() - interval '1 hour')",
    )
    .bind(c.t)
    .bind(att)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let o = agent_api::attachments::sweep::process_intents(&c.e.state)
        .await
        .unwrap();
    assert_eq!((o.resolved, o.failed), (1, 0));
    assert!(intents(&c).await.is_empty());
    assert!(c.artifacts.deletes().is_empty());
}

#[tokio::test]
async fn a_failing_lookup_or_delete_keeps_the_intent_and_is_retried_after_the_backoff() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let (att, artifact) = crashed_upload(&c).await;
    age_intents(&c, "1 hour").await;
    let st = &c.e.state;
    c.artifacts.list_fails.store(true, Ordering::SeqCst);
    let o = agent_api::attachments::sweep::process_intents(st)
        .await
        .unwrap();
    assert_eq!((o.resolved, o.failed), (0, 1));
    assert_eq!(intents(&c).await, [att]);
    assert!(c.artifacts.holds(artifact));
    // inside the back-off nothing is tried again
    let o = agent_api::attachments::sweep::process_intents(st)
        .await
        .unwrap();
    assert_eq!((o.resolved, o.failed), (0, 0));
    // after it, with the service back, the intent is resolved (the claim of the first try is kept)
    sqlx::query(
        "UPDATE conversation_attachment_upload_intent SET last_error_at = now() - interval '2 hours'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    c.artifacts.list_fails.store(false, Ordering::SeqCst);
    let o = agent_api::attachments::sweep::process_intents(st)
        .await
        .unwrap();
    assert_eq!((o.resolved, o.failed), (1, 0));
    assert!(!c.artifacts.holds(artifact) && intents(&c).await.is_empty());
}

#[tokio::test]
async fn a_claimed_intent_can_no_longer_be_closed_by_a_late_upload() {
    let c = conv::setup().await;
    let att = Uuid::new_v4();
    let close = || async {
        sqlx::query_scalar::<_, bool>("SELECT agent_attachment_intent_close($1, $2)")
            .bind(c.t)
            .bind(att)
            .fetch_one(&c.e.owner)
            .await
            .unwrap()
    };
    // no intent: nothing to close
    assert!(!close().await);
    sqlx::query("SELECT agent_attachment_intent_open($1, $2)")
        .bind(c.t)
        .bind(att)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let claimed = sqlx::query_scalar::<_, bool>("SELECT agent_attachment_intent_claim($1, $2)")
        .bind(c.t)
        .bind(att)
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert!(claimed);
    assert!(
        !close().await,
        "the sweep owns the bytes now: the put must not keep a row"
    );
    assert_eq!(intents(&c).await, [att]);
}

#[tokio::test]
async fn a_normal_put_leaves_no_intent_and_a_failed_one_leaves_none_either() {
    use agent_api::attachments::store::{
        AttachmentStore, Direction, NewAttachment, PgAttachmentStore,
    };
    let c = conv::setup().await;
    bare_conversation(&c, "conv-i").await;
    let accepted = agent_api::attachments::validate::accept(&pdf("", &[]), "a.pdf").unwrap();
    let store = PgAttachmentStore::for_state(&c.e.state);
    let put = |conv: &'static str, commit: bool| {
        let (db, t, job, store, accepted) = (
            c.e.state.db.clone(),
            c.t,
            c.job,
            store.clone(),
            accepted.clone(),
        );
        async move {
            let mut tx = agent_api::conversations::store::begin_job(&db, t, job)
                .await
                .unwrap();
            let r = store
                .put(
                    &mut tx,
                    NewAttachment {
                        conversation_id: conv,
                        direction: Direction::Staff,
                        created_by: "u-agent",
                        accepted: &accepted,
                    },
                )
                .await;
            if commit {
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            r
        }
    };
    // kept: the row and the bytes exist, the intent went with the row's transaction
    let meta = put("conv-i", true).await.unwrap();
    assert!(intents(&c).await.is_empty());
    assert!(c.artifacts.holds(meta.artifact_id));
    // a refused upload (scanner verdict) stores nothing and leaves no intent
    c.artifacts.set_mode(Mode::Malware);
    assert!(put("conv-i", true).await.is_err());
    assert!(intents(&c).await.is_empty());
    c.artifacts.set_mode(Mode::Ok);
    // an insert that fails after the upload deletes the artifact and the intent
    assert!(put("conv-nope", true).await.is_err());
    assert!(intents(&c).await.is_empty());
    assert_eq!(c.artifacts.stored(), 1, "only the kept attachment's bytes");
    // the request's transaction rolls back after a good put: the close rolls back with it, the intent stays for the
    // sweep, and the sweep deletes the bytes the rolled-back row never named
    let meta2 = put("conv-i", false).await.unwrap();
    assert_eq!(intents(&c).await.len(), 1);
    age_intents(&c, "1 hour").await;
    let o = agent_api::attachments::sweep::process_intents(&c.e.state)
        .await
        .unwrap();
    assert_eq!((o.resolved, o.failed), (1, 0));
    assert!(!c.artifacts.holds(meta2.artifact_id) && c.artifacts.holds(meta.artifact_id));
    assert!(intents(&c).await.is_empty());
}
