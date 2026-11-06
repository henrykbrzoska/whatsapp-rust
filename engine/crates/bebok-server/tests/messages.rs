//! `routes::messages` — the endpoints behind the message view
//! (`GET /chats/{jid}/messages`, `POST /send/message`, `POST /receipt/read`).
//!
//! Reading a chat is a pure wa-store query, so it is covered end to end here.
//! Sending needs a live WhatsApp session, which these tests do not have: what
//! they pin is the validation and the `503 {"error":"not connected"}` contract
//! every session-bound endpoint shares.

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

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
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

fn msg(id: &str, chat: &str, ts: i64, from_me: bool) -> StoredMessage {
    serde_json::from_value(json!({
        "id": id, "chat_jid": chat, "sender": "a@s.whatsapp.net", "timestamp": ts,
        "type": "text", "text": id, "from_me": from_me,
    }))
    .unwrap()
}

/// Four messages in one chat plus one in another, to catch a missing filter.
async fn seeded() -> (Router, SharedState) {
    let state = state().await;
    state
        .store
        .insert_messages(&[
            msg("m1", "48111@s.whatsapp.net", 10, false),
            msg("m2", "48111@s.whatsapp.net", 20, true),
            msg("m3", "48111@s.whatsapp.net", 30, false),
            msg("m4", "48111@s.whatsapp.net", 40, true),
            msg("other", "2@g.us", 25, false),
        ])
        .unwrap();
    (routes::router(state.clone()), state)
}

fn ids(page: &Value) -> Vec<&str> {
    page["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn a_chat_opens_on_its_newest_page_and_walks_backwards() {
    let (app, _state) = seeded().await;

    // No cursor: the tail of the chat, oldest-first inside the page.
    let (st, page) = call(&app, get_req("/chats/48111@s.whatsapp.net/messages?limit=2")).await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(ids(&page), ["m3", "m4"]);
    assert_eq!(page["hasMore"], true, "m1 and m2 are still older");
    assert_eq!(page["messages"][1]["from_me"], true, "from_me survives the round trip");

    // The cursor is the oldest row we hold.
    let (_, page) = call(&app, get_req("/chats/48111@s.whatsapp.net/messages?limit=2&before=30&beforeId=m3")).await;
    assert_eq!(ids(&page), ["m1", "m2"]);
    assert_eq!(page["hasMore"], false, "nothing before m1");

    // Past the top: an empty page, not an error.
    let (st, page) = call(&app, get_req("/chats/48111@s.whatsapp.net/messages?before=10&beforeId=m1")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(ids(&page).is_empty());
    assert_eq!(page["hasMore"], false);
}

#[tokio::test]
async fn a_page_holds_only_its_own_chat_and_an_unknown_chat_is_empty() {
    let (app, _state) = seeded().await;
    let (_, page) = call(&app, get_req("/chats/48111@s.whatsapp.net/messages")).await;
    assert_eq!(ids(&page), ["m1", "m2", "m3", "m4"]);

    let (st, page) = call(&app, get_req("/chats/nobody@s.whatsapp.net/messages")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(ids(&page).is_empty());
}

#[tokio::test]
async fn half_a_cursor_is_rejected() {
    let (app, _state) = seeded().await;
    for uri in ["/chats/48111@s.whatsapp.net/messages?before=30", "/chats/48111@s.whatsapp.net/messages?beforeId=m3"] {
        let (st, body) = call(&app, get_req(uri)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].as_str().unwrap().contains("beforeId"), "{body}");
    }
}

#[tokio::test]
async fn deleting_a_chat_still_works_on_the_shared_path() {
    let (app, _state) = seeded().await;
    let req = Request::delete("/chats/48111@s.whatsapp.net/messages")
        .header(header::AUTHORIZATION, "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let (st, body) = call(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["deleted"], 4);
    let (_, page) = call(&app, get_req("/chats/48111@s.whatsapp.net/messages")).await;
    assert!(ids(&page).is_empty());
}

#[tokio::test]
async fn sending_validates_before_it_needs_a_session() {
    let (app, _state) = seeded().await;

    let (st, body) =
        call(&app, post_req("/send/message", json!({ "to": "48111@s.whatsapp.net", "text": "  \n" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("must not be empty"));

    let (st, body) = call(&app, post_req("/send/message", json!({ "to": "48111", "text": "hi" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("user@server"));

    let (st, body) = call(&app, post_req("/send/message", json!({ "to": "48111@s.whatsapp.net" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

    // Valid request, but no `POST /connect` has ever run.
    let (st, body) = call(&app, post_req("/send/message", json!({ "to": "48111@s.whatsapp.net", "text": "hi" }))).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "not connected");
}

#[tokio::test]
async fn read_receipts_validate_then_need_a_session() {
    let (app, _state) = seeded().await;

    let (st, body) = call(&app, post_req("/receipt/read", json!({ "chat": "48111@s.whatsapp.net", "ids": [] }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no message ids"));

    let (st, _) = call(&app, post_req("/receipt/read", json!({ "chat": "nope", "ids": ["m1"] }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, body) =
        call(&app, post_req("/receipt/read", json!({ "chat": "48111@s.whatsapp.net", "ids": ["m1", "m3"] }))).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "not connected");
}

#[tokio::test]
async fn the_page_size_is_clamped_and_never_unbounded() {
    let (app, state) = seeded().await;
    let rows: Vec<StoredMessage> =
        (0..600).map(|i| msg(&format!("b{i:03}"), "bulk@s.whatsapp.net", 1000 + i, false)).collect();
    state.store.insert_messages(&rows).unwrap();

    let (_, page) = call(&app, get_req("/chats/bulk@s.whatsapp.net/messages?limit=100000")).await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 500, "MAX_LIMIT");
    let (_, page) = call(&app, get_req("/chats/bulk@s.whatsapp.net/messages?limit=0")).await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 1, "0 clamps up to 1");
}

#[tokio::test]
async fn the_app_state_sync_endpoint_answers_without_a_session() {
    let (app, _state) = seeded().await;
    // No `POST /connect` has run: the sync reports why instead of 500-ing, so
    // the client can show "connect first" rather than a dead button.
    let (st, body) = call(&app, post_req("/appstate/sync", json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["errors"][0], "not connected");
    assert_eq!(body["contacts"], 0);
    assert_eq!(body["chatSettings"], 0);
}
