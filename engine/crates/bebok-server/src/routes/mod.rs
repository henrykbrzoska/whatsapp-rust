//! Routing table — every HTTP endpoint the engine exposes, in one place.
//!
//! Handlers return `Result<_, axum::response::Response>`; `Response` is a big
//! `Err` variant by construction, so the `result_large_err` lint is silenced
//! for this module (the alternative — boxing every error — buys nothing here).
#![allow(clippy::result_large_err)]
//!
//! | Method | Path                               | Handler                     |
//! |--------|------------------------------------|-----------------------------|
//! | GET    | `/`                                | [`root`] (handshake)        |
//! | GET    | `/version`                         | [`version`]                 |
//! | GET    | `/config`                          | [`config`] (redacted)       |
//! | GET    | `/events`                          | [`sse::events`] (SSE)       |
//! | GET    | `/connected`                       | [`connected`] (session)     |
//! | GET    | `/logged_in`                       | [`logged_in`] (paired?)     |
//! | POST   | `/connect`                         | [`connect`] (handshake)     |
//! | POST   | `/disconnect`                      | [`disconnect`]              |
//! | POST   | `/logout`                          | [`groups::logout`]          |
//! | POST   | `/group/create`, `/group/join`     | [`groups`]                  |
//! | GET    | `/group/{jid}/info`                | [`groups::info`]            |
//! | POST   | `/group/{jid}/leave`               | [`groups::leave`]           |
//! | POST   | `/group/{jid}/participants`        | [`groups::participants`]    |
//! | POST   | `/group/{jid}/name`                | [`groups::set_name`]        |
//! | POST   | `/group/{jid}/topic`               | [`groups::set_topic`]       |
//! | GET    | `/group/{jid}/invite-link`         | [`groups::invite_link`]     |
//! | POST   | `/newsletter/subscribe`            | [`groups::newsletter_subscribe`] |
//! | POST   | `/newsletter/unsubscribe`          | [`groups::newsletter_unsubscribe`] |
//! | POST   | `/presence/subscribe`              | [`presence::subscribe`]     |
//! | POST   | `/presence/send`                   | [`presence::send`]          |
//! | POST   | `/presence/chat`                   | [`presence::chat`]          |
//! | GET    | `/privacy`                         | [`presence::get_privacy`]   |
//! | POST   | `/privacy`                         | [`presence::set_privacy`]   |
//! | GET    | `/contact`, `/contacts`, `/picture` | [`presence::contact`] etc   |
//! | GET    | `/blocklist`                     | [`presence::blocklist`]     |
//! | POST   | `/block`, `/unblock`             | [`presence::block`] etc     |
//! | GET    | `/chats/{jid}/messages`            | [`messages::list`] (page)   |
//! | POST   | `/send/message`                    | [`messages::send`]          |
//! | POST   | `/receipt/read`                    | [`messages::mark_read`]     |
//! | POST   | `/appstate/sync`                   | [`appstate_sync`] (names)   |
//! | POST   | `/history/fetch`                   | [`history::fetch`]          |
//! | GET    | `/media/{id}`, `/media/{id}/info`  | [`history::media`]          |
//! | GET    | `/search`                          | [`chats::search`]           |
//! | GET    | `/code-index/status|/search`        | [`codeintel`]               |
//! | GET    | `/code-ast[/status]`, `/code-graph`, `/code-map` | [`codeintel`] |
//! | GET    | `/chats`                           | [`chats::list`]             |
//! | GET    | `/chats/{jid}/history/export`      | [`chats::export`]           |
//! | GET    | `/chats/{jid}/stats`               | [`chats::stats`]            |
//! | DELETE | `/chats/{jid}/messages`            | [`chats::delete_messages`]  |
//! | POST   | `/debug/seed`                      | [`chats::seed`] (diagnostic)|
//! | GET    | `/debug/log`                       | [`crate::debug_log::get`] (diagnostic)|

pub mod chats;
pub mod codeintel;
pub mod groups;
pub mod history;
pub mod messages;
pub mod presence;
pub mod sse;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde_json::{Value, json};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::connection::ConnectionStatus;
use crate::state::SharedState;

pub fn router(state: SharedState) -> Router {
    let origins: Vec<HeaderValue> = state.config.cors.iter().filter_map(|o| o.parse().ok()).collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .expose_headers([header::CONTENT_DISPOSITION]);

    let api = Router::new()
        .route("/", get(root))
        .route("/version", get(version))
        .route("/config", get(config))
        .route("/events", get(sse::events))
        .route("/connected", get(connected))
        .route("/logged_in", get(logged_in))
        .route("/status", get(status))
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/logout", post(groups::logout))
        .route("/contact", get(presence::contact))
        .route("/contacts", get(presence::contacts))
        .route("/picture", get(presence::picture))
        .route("/blocklist", get(presence::blocklist))
        .route("/block", post(presence::block))
        .route("/unblock", post(presence::unblock))
        .route("/group/create", post(groups::create))
        .route("/group/join", post(groups::join))
        .route("/group/{jid}/info", get(groups::info))
        .route("/group/{jid}/leave", post(groups::leave))
        .route("/group/{jid}/participants", post(groups::participants))
        .route("/group/{jid}/name", post(groups::set_name))
        .route("/group/{jid}/topic", post(groups::set_topic))
        .route("/group/{jid}/invite-link", get(groups::invite_link))
        .route("/newsletter/subscribe", post(groups::newsletter_subscribe))
        .route("/newsletter/unsubscribe", post(groups::newsletter_unsubscribe))
        .route("/presence/subscribe", post(presence::subscribe))
        .route("/presence/send", post(presence::send))
        .route("/presence/chat", post(presence::chat))
        .route("/privacy", get(presence::get_privacy).post(presence::set_privacy))
        .route("/search", get(chats::search))
        .route("/code-index/status", get(codeintel::index_status))
        .route("/code-index/search", get(codeintel::index_search_get).post(codeintel::index_search_post))
        .route("/code-ast/status", get(codeintel::ast_status))
        .route("/code-ast", get(codeintel::ast_search))
        .route("/code-graph", get(codeintel::graph))
        .route("/code-map", get(codeintel::map))
        .route("/chats", get(chats::list))
        .route("/chats/{jid}/messages", get(messages::list).delete(chats::delete_messages))
        .route("/send/message", post(messages::send))
        .route("/send/media", post(messages::send_media))
        .route("/receipt/read", post(messages::mark_read))
        .route("/appstate/sync", post(appstate_sync))
        .route("/history/fetch", post(history::fetch))
        .route("/media/{id}", get(history::media))
        .route("/media/{id}/info", get(history::media_info))
        .route("/chats/{jid}/history/export", get(chats::export))
        .route("/chats/{jid}/stats", get(chats::stats))
        .route("/debug/seed", post(chats::seed))
        .route("/debug/log", get(crate::debug_log::get).delete(crate::debug_log::clear));

    api.layer(middleware::from_fn_with_state(state.clone(), crate::auth::require_token)).layer(cors).with_state(state)
}

pub fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

async fn root() -> Json<Value> {
    Json(json!({ "name": "bebok-server", "version": env!("CARGO_PKG_VERSION") }))
}

async fn version() -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocolRef": wa_core::PROTOCOL_REF,
    }))
}

async fn config(State(s): State<SharedState>) -> Json<Value> {
    Json(s.config.redacted_json(s.store.has_embedder()))
}

/// `POST /connect` — start the session and report its state.
///
/// The dial and the Noise handshake run in the background (`connection.rs`), so
/// this returns at once with the state just entered (`connecting`). Progress is
/// published as `connected` / `connect_failure` on `/events`.
async fn connect(State(s): State<SharedState>) -> Json<ConnectionStatus> {
    Json(s.connection.connect().await)
}

/// `POST /appstate/sync?full=true` — pull the address book and the chat flags
/// from WhatsApp (see [`crate::appstate_sync`]). This is what gives chats and
/// senders their names; the client exposes it as "sync contacts & chats".
async fn appstate_sync(
    State(s): State<SharedState>,
    Query(p): Query<AppStateSyncParams>,
) -> Json<crate::appstate_sync::SyncReport> {
    Json(crate::appstate_sync::sync(&s, p.full).await)
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AppStateSyncParams {
    /// Drop the stored collection versions and ask for a fresh snapshot.
    #[serde(default)]
    full: bool,
}

async fn status(State(s): State<SharedState>) -> Json<ConnectionStatus> {
    Json(s.connection.status().await)
}

async fn connected(State(s): State<SharedState>) -> Json<bool> {
    Json(s.connection.is_connected().await)
}

async fn logged_in(State(s): State<SharedState>) -> Result<Json<bool>, Response> {
    let devices =
        s.container.device_jids().await.map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(!devices.is_empty()))
}

/// `POST /disconnect` — drop the session (Go `Client::Disconnect`).
///
/// Answers the state the session is in afterwards, like `POST /connect`.
async fn disconnect(State(s): State<SharedState>) -> Json<ConnectionStatus> {
    s.connection.disconnect().await;
    Json(s.connection.status().await)
}
