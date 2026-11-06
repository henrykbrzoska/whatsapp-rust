//! Phase 6 server wiring: protocol client in `AppState`, `POST /logout`,
//! `GET /chats` with live chat settings, contacts / picture / blocklist,
//! `GET /debug/log` and the redacted config.
//!
//! The new endpoints are not in `routes::router` yet (the routing table is
//! owned elsewhere), so [`extra`] mounts them exactly as they are meant to be
//! registered there.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use bebok_server::routes::{groups, presence};
use bebok_server::state::{AppState, SharedState};
use bebok_server::{Config, debug_log, routes};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wa_core::WaEvent;
use wa_core::client::{Client, ClientConfig};
use wa_core::types::jid::Jid;
use wa_store::StoredMessage;

async fn state() -> SharedState {
    let cfg = Config::parse(["--token", "secret", "--diagnostic"].iter().map(|s| s.to_string()), |_| None).unwrap();
    AppState::memory(cfg).await.unwrap()
}

/// The routes `routes/mod.rs` still has to register, mounted the same way
/// (without the auth layer, which `tests/http.rs` covers).
fn extra(state: SharedState) -> Router {
    Router::new()
        .route("/logout", post(groups::logout))
        .route("/contact", get(presence::contact))
        .route("/contacts", get(presence::contacts))
        .route("/picture", get(presence::picture))
        .route("/blocklist", get(presence::blocklist))
        .route("/block", post(presence::block))
        .route("/unblock", post(presence::unblock))
        .route("/debug/log", get(debug_log::get).delete(debug_log::clear))
        .with_state(state)
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

/// Save a paired device so the per-device stores have an owner row.
async fn pair(state: &SharedState) {
    let mut d = state.container.new_device();
    d.id = Some(Jid::new_ad("48111", 0, 1));
    d.lid = Jid::new_ad("948111", 1, 1);
    d.account = Some(Default::default());
    if let Some(account) = d.account.as_mut() {
        account.details = Some(b"details".to_vec());
        account.account_signature_key = Some(vec![1; 32]);
        account.account_signature = Some(vec![2; 64]);
        account.device_signature = Some(vec![3; 64]);
    }
    d.platform = "android".into();
    d.push_name = "Me".into();
    d.save().await.unwrap();
}

fn msg(id: &str, chat: &str, ts: i64) -> StoredMessage {
    serde_json::from_value(json!({
        "id": id, "chat_jid": chat, "sender": "a@s.whatsapp.net", "timestamp": ts, "type": "text", "text": id,
    }))
    .unwrap()
}

#[tokio::test]
async fn status_reports_a_paired_device_before_the_first_connect() {
    let state = state().await;
    let app = routes::router(state.clone());

    // Nothing paired: the UI must offer pairing.
    let (st, v) = call(&app, get_req("/status")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["loggedIn"], false);
    assert_eq!(v["connected"], false);

    // A paired device with no session (an engine that was just restarted):
    // still logged in, so the client reconnects instead of asking for a QR.
    pair(&state).await;
    let (_, v) = call(&app, get_req("/status")).await;
    assert_eq!(v["loggedIn"], true, "{v}");
    assert_eq!(v["connected"], false);
    assert_eq!(v["state"], "disconnected");
}

#[tokio::test]
async fn chats_carry_live_mute_pin_archive_and_names() {
    let state = state().await;
    pair(&state).await;
    state
        .store
        .insert_messages(&[
            msg("a", "1@g.us", 10),
            msg("b", "2@g.us", 30),
            msg("c", "3@g.us", 20),
            msg("d", "48222@s.whatsapp.net", 5),
        ])
        .unwrap();
    let device = state.container.get_first_device().await.unwrap();
    let jid = |s: &str| Jid::parse(s).unwrap();
    device.chat_settings.put_pinned(&jid("1@g.us"), true).await.unwrap();
    device.chat_settings.put_archived(&jid("2@g.us"), true).await.unwrap();
    let muted_until = SystemTime::now() + Duration::from_secs(3600);
    device.chat_settings.put_muted_until(&jid("3@g.us"), Some(muted_until)).await.unwrap();
    device.contacts.put_contact_name(&jid("48222@s.whatsapp.net"), "Bob", "Bob Builder").await.unwrap();

    let app = routes::router(state.clone());
    let (st, v) = call(&app, get_req("/chats")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let rows = v.as_array().unwrap();
    let order: Vec<&str> = rows.iter().map(|r| r["jid"].as_str().unwrap()).collect();
    assert_eq!(order, ["1@g.us", "2@g.us", "3@g.us", "48222@s.whatsapp.net"], "pinned first, then recent");
    assert_eq!(rows[0]["pinned"], true);
    assert_eq!(rows[1]["archived"], true);
    assert_eq!(rows[2]["muted"], true);
    assert!(rows[2]["muted_until"].as_i64().is_some());
    assert_eq!(rows[3]["name"], "Bob Builder");
    assert_eq!(rows[3]["muted"], false);
    assert_eq!(rows[0]["count"], 1);
    assert_eq!(rows[0]["last_text"], "a");

    let (_, v) = call(&app, get_req("/chats?archived=active")).await;
    let active: Vec<&str> = v.as_array().unwrap().iter().map(|r| r["jid"].as_str().unwrap()).collect();
    assert!(!active.contains(&"2@g.us"), "{v}");
    assert_eq!(active.len(), 3);
    let (_, v) = call(&app, get_req("/chats?archived=archived")).await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    let (st, _) = call(&app, get_req("/chats?archived=bogus")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chats_work_before_any_pairing() {
    let state = state().await;
    state.store.insert_messages(&[msg("a", "1@g.us", 1)]).unwrap();
    let (st, v) = call(&routes::router(state), get_req("/chats")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v[0]["jid"], "1@g.us");
    assert_eq!(v[0]["pinned"], false);
    assert_eq!(v[0]["archived"], false);
}

#[tokio::test]
async fn contacts_are_served_from_the_local_store() {
    let state = state().await;
    pair(&state).await;
    let device = state.container.get_first_device().await.unwrap();
    let bob = Jid::parse("48222@s.whatsapp.net").unwrap();
    let ann = Jid::parse("48333@s.whatsapp.net").unwrap();
    device.contacts.put_contact_name(&bob, "Bob", "Bob Builder").await.unwrap();
    device.contacts.put_push_name(&ann, "ann").await.unwrap();
    let app = extra(state);

    let (st, v) = call(&app, get_req("/contact?jid=48222@s.whatsapp.net")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["found"], true);
    assert_eq!(v["fullName"], "Bob Builder");
    assert_eq!(v["displayName"], "Bob Builder");

    let (st, v) = call(&app, get_req("/contact?jid=48999@s.whatsapp.net")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["found"], false);
    assert!(v["displayName"].is_null());

    let (st, _) = call(&app, get_req("/contact?jid=nope")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, v) = call(&app, get_req("/contacts")).await;
    assert_eq!(st, StatusCode::OK);
    let names: Vec<&str> = v.as_array().unwrap().iter().map(|c| c["displayName"].as_str().unwrap()).collect();
    assert_eq!(names, ["ann", "Bob Builder"], "sorted case-insensitively by display name");
}

#[tokio::test]
async fn session_endpoints_answer_503_without_a_client() {
    let app = extra(state().await);
    for req in [
        get_req("/picture?jid=48222@s.whatsapp.net"),
        get_req("/blocklist"),
        post_req("/block", json!({"jid": "48222@s.whatsapp.net"})),
        post_req("/unblock", json!({"jid": "1@lid"})),
    ] {
        let uri = req.uri().to_string();
        let (st, v) = call(&app, req).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(v, json!({ "error": groups::NOT_CONNECTED }), "{uri}");
    }
    // Validation runs before the session lookup.
    let (st, _) = call(&app, post_req("/block", json!({"jid": "1@g.us"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&app, get_req("/picture?jid=bad")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&app, post_req("/block", json!({}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_installed_client_replaces_the_503_gap() {
    let state = state().await;
    let device = state.container.get_first_device().await.unwrap();
    let sink = bebok_server::connection::event_sink(&state);
    state.connection.set_client(Some(Client::new(device, ClientConfig::default(), sink)));
    assert!(groups::session(&state).is_ok(), "session() must hand out the live client");

    // The client exists but has no socket: the protocol call itself reports
    // `NotConnected`, which still maps to 503.
    let (st, v) = call(&extra(state.clone()), get_req("/blocklist")).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    let (st, _) = call(&routes::router(state), get_req("/group/1@g.us/info")).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn connect_creates_the_protocol_client() {
    let cfg = Config::parse(
        ["--token", "secret", "--wa-url", "ws://127.0.0.1:1", "--handshake-timeout", "1"].iter().map(|s| s.to_string()),
        |_| None,
    )
    .unwrap();
    let state = AppState::memory(cfg).await.unwrap();
    assert!(state.connection.client().is_none());
    let (st, _) = call(&routes::router(state.clone()), post_req("/connect", json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while state.connection.client().is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(state.connection.client().is_some(), "the first connect must build the client");
    assert!(groups::session(&state).is_ok());
}

#[tokio::test]
async fn logout_deletes_the_paired_device() {
    let state = state().await;
    let app = extra(state.clone());
    let (st, v) = call(&app, post_req("/logout", json!({}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "nothing paired yet: {v}");
    assert_eq!(v["error"], "not logged in");

    pair(&state).await;
    assert_eq!(state.container.device_jids().await.unwrap().len(), 1);
    let mut events = state.events.subscribe();
    let (st, v) = call(&app, post_req("/logout", json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "logged_out");
    assert_eq!(v["loggedIn"], false);
    assert!(state.container.device_jids().await.unwrap().is_empty(), "the device must be deleted");
    let ev = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap();
    assert!(matches!(ev, WaEvent::LoggedOut { .. }), "{ev:?}");
    assert!(state.connection.client().is_none());
}

#[tokio::test]
async fn logout_with_a_client_drops_it() {
    let state = state().await;
    pair(&state).await;
    let device = state.container.get_first_device().await.unwrap();
    let sink = bebok_server::connection::event_sink(&state);
    state.connection.set_client(Some(Client::new(device, ClientConfig::default(), sink)));
    // `Client::logout` is best effort on the iq (no socket here), then deletes.
    let (st, v) = call(&extra(state.clone()), post_req("/logout", json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(state.connection.client().is_none());
    assert!(state.container.device_jids().await.unwrap().is_empty());
}

#[tokio::test]
async fn debug_log_records_redacted_events_and_clears() {
    let state = state().await;
    state.emit(WaEvent::Qr { codes: vec!["2@pairing-ref,xyz".into()] });
    state.emit(WaEvent::Connected);
    state.debug_log.note("token secret must not leak");
    let app = extra(Arc::clone(&state));

    let (st, v) = call(&app, get_req("/debug/log")).await;
    assert_eq!(st, StatusCode::OK);
    let dump = v.to_string();
    assert!(!dump.contains("pairing-ref"), "{dump}");
    assert!(!dump.contains("secret"), "{dump}");
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["message"], "qr");
    assert_eq!(entries[1]["message"], "connected");
    assert_eq!(entries[2]["kind"], "engine");

    let since = entries[0]["seq"].as_u64().unwrap();
    let (_, v) = call(&app, get_req(&format!("/debug/log?since={since}&limit=1"))).await;
    assert_eq!(v["entries"].as_array().unwrap().len(), 1);
    assert_eq!(v["entries"][0]["kind"], "engine");

    let (st, v) = call(&app, Request::delete("/debug/log").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["cleared"], 3);
    assert!(state.debug_log.is_empty());
}

#[tokio::test]
async fn config_helper_is_redacted() {
    let state = state().await;
    let v = state.config.redacted_json(state.store.has_embedder());
    assert!(!v.to_string().contains("secret"));
    assert_eq!(v["token"], "<redacted>");
    assert_eq!(v["diagnostic"], true);
}
