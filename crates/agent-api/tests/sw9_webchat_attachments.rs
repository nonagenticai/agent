//! SW-9 customer attachments at core's internal door (crate::conversations::webchat_attachments): a customer upload
//! is validated and re-encoded, refused with a closed reason, limited to ten per conversation per hour, scoped to the
//! session's own conversation, linked to the inbound message in its transaction, and served back only to that
//! conversation. Real Postgres 18. Bytes are only ever read through the store's `get` (here: through the route).
use crate::common;

use std::io::Cursor;

use agent_api::attachments::store::{AttachmentStore, Direction, NewAttachment, PgAttachmentStore};
use agent_api::attachments::validate;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::conv::{self, Ctx, EDGE_KEY, code};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const B: &str = "SW9BOUNDARY";

fn png() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(8, 6, image::Rgb([200, 30, 30]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A JPEG that carries an EXIF segment (with a marker string) right after SOI.
fn jpeg_with_exif() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(8, 6, image::Rgb([20, 120, 200]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    let plain = out.into_inner();
    let exif = b"Exif\0\0GPS-MARKER-52.37N";
    let len = u16::try_from(exif.len() + 2).unwrap().to_be_bytes();
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xE1, len[0], len[1]];
    j.extend_from_slice(exif);
    j.extend_from_slice(&plain[2..]);
    j
}

fn upload_body(fields: &[(&str, String)], name: &str, file: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    for (n, v) in fields {
        b.extend_from_slice(
            format!("--{B}\r\nContent-Disposition: form-data; name=\"{n}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    b.extend_from_slice(
        format!(
            "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(file);
    b.extend_from_slice(format!("\r\n--{B}--\r\n").as_bytes());
    b
}

async fn raw(c: &Ctx, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let r = c.e.app.clone().oneshot(req).await.unwrap();
    let (s, h) = (r.status(), r.headers().clone());
    (
        s,
        h,
        r.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

fn json_of(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

/// A web chat conversation with a live session: `(core conversation id, session token)`. The conversation is opened
/// by a "talk to a person" message (no runtime needed), which also ties the session to it.
async fn open(c: &Ctx, ext: &str) -> (String, String) {
    let (s, b) = c.inbound(ext, "m0", "I want to talk to a human").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let conv = b["conversation_id"].as_str().unwrap().to_string();
    let token = c.sessions.lock().unwrap().get(ext).unwrap().1.clone();
    (conv, token)
}

async fn upload_as(
    c: &Ctx,
    conv: &str,
    token: &str,
    name: &str,
    file: &[u8],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let fields = [
        ("tenant", c.t.to_string()),
        ("job_id", c.job.to_string()),
        ("binding_id", c.wc.to_string()),
        ("conversation", conv.to_string()),
        ("session_token", token.to_string()),
    ];
    let req = Request::post("/api/internal/channel/webchat/attachments")
        .header("authorization", format!("Bearer {EDGE_KEY}"))
        .header("content-type", format!("multipart/form-data; boundary={B}"))
        .body(Body::from(upload_body(&fields, name, file)))
        .unwrap();
    raw(c, req).await
}

fn fetch_path(c: &Ctx, conv: &str, att: &str) -> String {
    format!(
        "/api/internal/channel/attachments/{att}?tenant={}&job_id={}&binding_id={}&conversation={conv}",
        c.t, c.job, c.wc
    )
}

async fn fetch(c: &Ctx, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    raw(
        c,
        Request::get(path)
            .header("authorization", format!("Bearer {EDGE_KEY}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn a_customer_upload_is_reencoded_stored_and_served_back_to_its_conversation_only() {
    let c = conv::setup().await;
    let (conv_id, token) = open(&c, "wc-att-1").await;
    let (s, _, b) = upload_as(&c, &conv_id, &token, "IMG_1.jpg", &jpeg_with_exif()).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let r = json_of(&b);
    assert_eq!(r["media_type"], "image/jpeg");
    assert_eq!(r["display_name"], "IMG_1.jpg");
    assert_eq!(r["width"], 8);
    assert!(r.get("sha256").is_none() && r.get("created_by").is_none());
    let att = r["id"].as_str().unwrap().to_string();

    // served with its headers, and what comes back is the re-encoded image: no EXIF
    let (s, h, bytes) = fetch(&c, &fetch_path(&c, &conv_id, &att)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["content-type"], "image/jpeg");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["cache-control"], "private, no-store");
    assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
    assert!(!bytes.windows(4).any(|w| w == b"Exif"), "metadata stripped");
    assert!(!bytes.windows(10).any(|w| w == b"GPS-MARKER"));
    use base64::Engine as _;
    let name = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(h["x-attachment-name"].to_str().unwrap())
        .unwrap();
    assert_eq!(name, b"IMG_1.jpg");

    // the key gates both routes
    let (s, _, _) = raw(
        &c,
        Request::get(fetch_path(&c, &conv_id, &att))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // another conversation, another binding, another tenant, a made-up id: the same uniform 404
    let (other_conv, _) = open(&c, "wc-att-2").await;
    for path in [
        fetch_path(&c, &other_conv, &att),
        fetch_path(&c, &conv_id, &Uuid::new_v4().to_string()),
        format!(
            "/api/internal/channel/attachments/{att}?tenant={}&job_id={}&binding_id={}&conversation={conv_id}",
            Uuid::new_v4(),
            c.job,
            c.wc
        ),
        format!(
            "/api/internal/channel/attachments/{att}?tenant={}&job_id={}&binding_id={}&conversation={conv_id}",
            c.t,
            c.job,
            Uuid::new_v4()
        ),
        fetch_path(&c, &conv_id, "not-a-uuid"),
    ] {
        let (s, _, b) = fetch(&c, &path).await;
        assert_eq!(
            (s, code(&json_of(&b)).to_string()),
            (StatusCode::NOT_FOUND, "not_found".into()),
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_staff_draft_is_not_the_customers_to_read_and_a_file_needs_the_conversations_own_session()
{
    let c = conv::setup().await;
    let (conv_id, token) = open(&c, "wc-att-1").await;
    let (other_conv, other_token) = open(&c, "wc-att-2").await;
    // a staff upload still in the composer
    let accepted = validate::accept(&png(), "draft.png").unwrap();
    let mut tx = agent_api::conversations::store::begin_job(&c.e.state.db, c.t, c.job)
        .await
        .unwrap();
    let draft = PgAttachmentStore::for_state(&c.e.state)
        .put(
            &mut tx,
            NewAttachment {
                conversation_id: &conv_id,
                direction: Direction::Staff,
                created_by: "u-agent",
                accepted: &accepted,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (s, _, _) = fetch(&c, &fetch_path(&c, &conv_id, &draft.id.to_string())).await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "unsent staff files are not served"
    );

    // no session / a made-up one / another conversation's session / another's conversation id
    for (conv, tok) in [
        (conv_id.as_str(), "nope"),
        (
            conv_id.as_str(),
            "0199aaaa-0000-7000-8000-000000000009.AAAA",
        ),
        (conv_id.as_str(), other_token.as_str()),
        (other_conv.as_str(), token.as_str()),
    ] {
        let (s, _, b) = upload_as(&c, conv, tok, "a.png", &png()).await;
        assert_eq!(
            (s, code(&json_of(&b)).to_string()),
            (StatusCode::UNAUTHORIZED, "session_invalid".into()),
            "{conv} {tok}"
        );
    }
    assert!(
        c.texts("SELECT id::text FROM conversation_attachment WHERE direction = 'customer'")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_refused_file_says_why_in_a_closed_word() {
    let c = conv::setup().await;
    let (conv_id, token) = open(&c, "wc-att-1").await;
    let active_pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /OpenAction << /S /JavaScript /JS (app.alert(1)) >> >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n";
    for (name, bytes, reason) in [
        ("notes.txt", b"hello".to_vec(), "unsupported_type"),
        (
            "x.jpg",
            b"<html>not an image</html>".to_vec(),
            "unsupported_type",
        ),
        ("a.pdf", active_pdf.to_vec(), "active_pdf"),
    ] {
        let (s, _, b) = upload_as(&c, &conv_id, &token, name, &bytes).await;
        let v = json_of(&b);
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {v}");
        assert_eq!(code(&v), "attachment_refused");
        assert_eq!(v["error"]["reason"], reason, "{name}");
    }
    assert!(
        c.texts("SELECT id::text FROM conversation_attachment")
            .await
            .is_empty(),
        "nothing refused is stored"
    );
}

#[tokio::test]
async fn at_most_ten_uploads_per_conversation_per_hour() {
    let c = conv::setup().await;
    let (conv_id, token) = open(&c, "wc-att-1").await;
    for i in 0..10 {
        let (s, _, b) = upload_as(&c, &conv_id, &token, "a.png", &png()).await;
        assert_eq!(
            s,
            StatusCode::CREATED,
            "{i}: {}",
            String::from_utf8_lossy(&b)
        );
    }
    let (s, h, b) = upload_as(&c, &conv_id, &token, "a.png", &png()).await;
    let v = json_of(&b);
    assert_eq!(
        (s, code(&v)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    assert!(
        v["error"]["retry_after_s"].as_u64().unwrap() >= 1,
        "{v} {h:?}"
    );
    // another conversation has its own window
    let (other, other_token) = open(&c, "wc-att-2").await;
    let (s, _, _) = upload_as(&c, &other, &other_token, "a.png", &png()).await;
    assert_eq!(s, StatusCode::CREATED);
}

fn inbound_body(c: &Ctx, ext: &str, msg: &str, text: &str, atts: Value) -> Value {
    let (sid, token, ip) = c.sessions.lock().unwrap().get(ext).cloned().unwrap();
    json!({
        "kind": "web_chat", "external_id": "wk_test", "binding_id": c.wc,
        "tenant": c.t, "job_id": c.job, "job_key": "j1",
        "conversation": {"channel": "web_chat", "id": ext, "route": {}},
        "message_id": msg, "party": {"id": format!("wc:{sid}"), "name": "Femke", "verified": false},
        "text": text, "session_token": token, "ip_hash": ip, "attachments": atts,
    })
}

#[tokio::test]
async fn the_ids_a_message_carries_are_linked_to_it_in_its_transaction() {
    let c = conv::setup().await;
    let (conv_id, token) = open(&c, "wc-att-1").await;
    let (other_conv, other_token) = open(&c, "wc-att-2").await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        let (s, _, b) = upload_as(&c, &conv_id, &token, "a.png", &png()).await;
        assert_eq!(s, StatusCode::CREATED);
        ids.push(json_of(&b)["id"].as_str().unwrap().to_string());
    }
    let (s, _, b) = upload_as(&c, &other_conv, &other_token, "b.png", &png()).await;
    assert_eq!(s, StatusCode::CREATED);
    let foreign = json_of(&b)["id"].as_str().unwrap().to_string();

    // more than three, another conversation's file, a made-up id: refused before anything is stored
    let (s, b) = c
        .internal(
            "/api/internal/channel/inbound",
            inbound_body(
                &c,
                "wc-att-1",
                "m1",
                "I want to talk to a human, m1",
                json!(ids),
            ),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "too_many_attachments")
    );
    for bad in [json!([foreign]), json!([Uuid::new_v4()])] {
        let (s, b) = c
            .internal(
                "/api/internal/channel/inbound",
                inbound_body(&c, "wc-att-1", "m2", "I want to talk to a human, m2", bad),
            )
            .await;
        assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "not_found"));
    }
    assert!(
        c.texts(
            "SELECT message_id::text FROM conversation_attachment WHERE message_id IS NOT NULL"
        )
        .await
        .is_empty()
    );
    assert_eq!(
        c.texts("SELECT 1::text FROM conversation_message WHERE external_id IN ('m1', 'm2')")
            .await
            .len(),
        0,
        "the refused messages were not stored"
    );

    // two files linked to the message that carries them
    let (s, b) = c
        .internal(
            "/api/internal/channel/inbound",
            inbound_body(
                &c,
                "wc-att-1",
                "m3",
                "I want to talk to a human (photo)",
                json!(&ids[..2]),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let linked = c
        .texts(
            "SELECT a.id::text FROM conversation_attachment a JOIN conversation_message m \
             ON m.message_id = a.message_id WHERE m.external_id = 'm3' ORDER BY a.created_at",
        )
        .await;
    assert_eq!(linked, ids[..2].to_vec());
    // already linked: not again; the other two are still unsent
    let (s, b) = c
        .internal(
            "/api/internal/channel/inbound",
            inbound_body(
                &c,
                "wc-att-1",
                "m4",
                "I want to talk to a human, m4",
                json!([ids[0]]),
            ),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "not_found"));

    // the history the widget reads carries the refs of the message
    let (s, b) = c
        .internal_get(&format!(
            "/api/internal/channel/conversations/{conv_id}/messages?tenant={}&job_id={}&binding_id={}",
            c.t, c.job, c.wc
        ))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let with_files: Vec<&Value> = b["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m.get("attachments").is_some())
        .collect();
    assert_eq!(with_files.len(), 1, "{b}");
    assert_eq!(with_files[0]["attachments"].as_array().unwrap().len(), 2);
    assert!(with_files[0]["attachments"][0].get("url").is_none());
}

#[tokio::test]
async fn the_generic_upload_serves_a_channel_without_a_web_chat_session() {
    let c = conv::setup().await;
    let (conv_id, _) = open(&c, "wc-att-1").await;
    // the one wire definition: crate::channels::attachments_wire::InboundUploadMeta
    let meta = json!({"tenant": c.t, "job_id": c.job, "binding_id": c.wc, "kind": "web_chat",
                      "conversation": {"channel": "web_chat", "id": "wc-att-1", "route": {}},
                      "message_id": "tg-1", "party_id": "wc:test", "declared_name": "scan.png"});
    let body = |meta: &Value| {
        let mut b =
            format!("--{B}\r\nContent-Disposition: form-data; name=\"meta\"\r\n\r\n{meta}\r\n")
                .into_bytes();
        b.extend_from_slice(
            format!(
                "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x\"\r\n\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(&png());
        b.extend_from_slice(format!("\r\n--{B}--\r\n").as_bytes());
        b
    };
    let post = |b: Vec<u8>| {
        Request::post("/api/internal/channel/attachments")
            .header("authorization", format!("Bearer {EDGE_KEY}"))
            .header("content-type", format!("multipart/form-data; boundary={B}"))
            .body(Body::from(b))
            .unwrap()
    };
    let (s, _, b) = raw(&c, post(body(&meta))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let att = json_of(&b)["id"].as_str().unwrap().to_string();
    assert_eq!(json_of(&b)["display_name"], "scan.png");
    // a conversation that does not exist yet, and a tenant that is not the binding's
    let mut unknown = meta.clone();
    unknown["conversation"]["id"] = json!("nobody");
    let (s, _, b) = raw(&c, post(body(&unknown))).await;
    assert_eq!(
        (s, code(&json_of(&b)).to_string()),
        (StatusCode::NOT_FOUND, "conversation_unknown".into())
    );
    let mut foreign = meta.clone();
    foreign["tenant"] = json!(Uuid::new_v4());
    let (s, _, _) = raw(&c, post(body(&foreign))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // read back by the channel's conversation id (no binding named), or by core's own id
    for conv in ["wc-att-1", conv_id.as_str()] {
        let (s, h, _) = fetch(
            &c,
            &format!(
                "/api/internal/channel/attachments/{att}?tenant={}&job_id={}&conversation={conv}",
                c.t, c.job
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{conv}");
        assert_eq!(h["content-type"], "image/png");
    }
}
