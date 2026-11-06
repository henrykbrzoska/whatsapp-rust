//! `routes::history` — asking the phone for older messages
//! (`POST /history/fetch`) and serving attachments (`GET /media/{id}`,
//! `GET /media/{id}/info`).
//!
//! Both halves need a live WhatsApp session for the part that talks to the
//! network, which these tests do not have. What they pin is everything around
//! it: the validation, the `503 {"error":"not connected"}` contract, and the
//! cache hit — a blob already in `media_blobs` is served **without** a
//! session, which is what makes a chat with photos readable offline.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bebok_server::state::{AppState, SharedState};
use bebok_server::{Config, routes};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wa_store::StoredMessage;

async fn state() -> SharedState {
    let cfg = Config::parse(["--token", "secret", "--diagnostic"].iter().map(|s| s.to_string()), |_| None).unwrap();
    AppState::memory(cfg).await.unwrap()
}

/// The response with its body still raw — a media response is not JSON.
async fn raw(app: &Router, req: Request<Body>) -> (StatusCode, Option<String>, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let mime = res.headers().get(header::CONTENT_TYPE).map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, mime, bytes)
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let (status, _, bytes) = raw(app, req).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn get_req(uri: &str) -> Request<Body> {
    Request::get(uri).header(header::AUTHORIZATION, "Bearer secret").body(Body::empty()).unwrap()
}

fn post_req(uri: &str, body: Value) -> Request<Body> {
    Request::post(uri)
        .header(header::AUTHORIZATION, "Bearer secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn msg(id: &str, chat: &str, kind: &str, raw_json: Option<&str>) -> StoredMessage {
    serde_json::from_value(json!({
        "id": id, "chat_jid": chat, "sender": "a@s.whatsapp.net", "timestamp": 10,
        "type": kind, "text": null, "raw_json": raw_json, "from_me": false,
    }))
    .unwrap()
}

#[tokio::test]
async fn fetching_history_without_a_session_is_a_503() {
    let state = state().await;
    state.store.insert_messages(&[msg("m1", "48111@s.whatsapp.net", "text", None)]).unwrap();
    let app = routes::router(state);

    let (st, body) = call(&app, post_req("/history/fetch", json!({ "chat": "48111@s.whatsapp.net" }))).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "not connected");

    // The same for "every chat" — the session is checked before the store.
    let (st, _) = call(&app, post_req("/history/fetch", json!({}))).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_bad_jid_is_rejected_before_anything_else() {
    let app = routes::router(state().await);
    for chat in ["not a jid", "@", "48111@"] {
        let (st, body) = call(&app, post_req("/history/fetch", json!({ "chat": chat }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{chat}: {body}");
        assert!(body["error"].as_str().unwrap().contains("jid"), "{body}");
    }
}

#[tokio::test]
async fn a_malformed_body_is_a_400_not_a_panic() {
    let app = routes::router(state().await);
    let req = Request::post("/history/fetch")
        .header(header::AUTHORIZATION, "Bearer secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let (st, _) = call(&app, req).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unknown_message_has_no_media_and_no_info() {
    let app = routes::router(state().await);

    let (st, body) = call(&app, get_req("/media/nope")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].as_str().unwrap().contains("unknown message"), "{body}");

    let (st, body) = call(&app, get_req("/media/nope/info")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn a_message_the_engine_kept_no_payload_for_cannot_be_downloaded() {
    let state = state().await;
    state.store.insert_messages(&[msg("m1", "48111@s.whatsapp.net", "image", None)]).unwrap();
    let app = routes::router(state);

    let (st, body) = call(&app, get_req("/media/m1")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no stored payload"), "{body}");

    // The info endpoint answers instead of failing, so the UI can show a label.
    let (st, info) = call(&app, get_req("/media/m1/info")).await;
    assert_eq!(st, StatusCode::OK, "{info}");
    assert_eq!(info["kind"], "image");
    assert_eq!(info["available"], false, "nothing to download");
    assert_eq!(info["cached"], false);
    assert_eq!(info["mimeType"], Value::Null);
}

#[tokio::test]
async fn a_payload_without_an_attachment_is_reported_as_such() {
    let state = state().await;
    // A valid protobuf that carries no media field (a `conversation` message).
    let payload = hex::encode([0x0a, 0x02, b'h', b'i']);
    state.store.insert_messages(&[msg("m1", "48111@s.whatsapp.net", "text", Some(&payload))]).unwrap();
    let app = routes::router(state);

    let (st, body) = call(&app, get_req("/media/m1")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no attachment"), "{body}");

    let (_, info) = call(&app, get_req("/media/m1/info")).await;
    assert_eq!(info["available"], false);
}

#[tokio::test]
async fn a_cached_blob_is_served_without_a_session() {
    let state = state().await;
    state.store.insert_messages(&[msg("m1", "48111@s.whatsapp.net", "image", None)]).unwrap();
    // What a finished download leaves behind.
    state.store.put_media_blob("m1", "image/jpeg", &[0xff, 0xd8, 0xff, 0xe0]).unwrap();
    let app = routes::router(state.clone());

    let (st, mime, bytes) = raw(&app, get_req("/media/m1")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(mime.as_deref(), Some("image/jpeg"), "the sender's MIME type, so the browser can render it");
    assert_eq!(bytes, vec![0xff, 0xd8, 0xff, 0xe0]);
    assert_eq!(state.store.count_media_blobs().unwrap(), 1, "a cache hit downloads nothing");

    let (_, info) = call(&app, get_req("/media/m1/info")).await;
    assert_eq!(info["cached"], true);
}

#[tokio::test]
async fn the_media_endpoints_need_the_capability_token() {
    let state = state().await;
    state.store.put_media_blob("m1", "image/jpeg", &[1, 2, 3]).unwrap();
    let app = routes::router(state);

    let res = app.clone().oneshot(Request::get("/media/m1").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // An `<img src>` cannot send a header, so the token may ride in the query.
    let (st, _, bytes) = raw(&app, Request::get("/media/m1?token=secret").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bytes, vec![1, 2, 3]);
}
