// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Group and newsletter endpoints (`API-WHATSAPP.md` §3.3 and §3.5).
//!
//! | Method | Path                             | Handler                              |
//! |--------|----------------------------------|--------------------------------------|
//! | POST   | `/group/create`                  | [`create`]                           |
//! | POST   | `/group/join`                    | [`join`]                             |
//! | GET    | `/group/{jid}/info`              | [`info`]                             |
//! | POST   | `/group/{jid}/leave`             | [`leave`]                            |
//! | POST   | `/group/{jid}/participants`      | [`participants`]                     |
//! | POST   | `/group/{jid}/name`              | [`set_name`]                         |
//! | POST   | `/group/{jid}/topic`             | [`set_topic`]                        |
//! | GET    | `/group/{jid}/invite-link?reset` | [`invite_link`]                      |
//! | POST   | `/newsletter/subscribe`          | [`newsletter_subscribe`]             |
//! | POST   | `/newsletter/unsubscribe`        | [`newsletter_unsubscribe`]           |
//! | POST   | `/logout`                        | [`logout`] (session lifecycle)       |
//!
//! The handlers are thin: they validate the request (400), reach the live
//! protocol [`Client`] through [`session`] (503 while there is none) and map a
//! [`ClientError`] to 404/502 via [`client_error`]. All the protocol work lives
//! in `wa_core::client::group` / `wa_core::client::newsletter`.
//!
//! This module also hosts the pieces [`super::presence`] needs (the session
//! accessor, the JSON-body helper and the JID parsers), so `routes/mod.rs`
//! stays a pure routing table.

// axum handlers answer with `Response`, so the small request-validation helpers
// return one too; the resulting large `Err` payload is expected, not a bug.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wa_core::client::{Client, ClientError};
use wa_core::types::events::{GroupInfoData, GroupParticipantData};
use wa_core::types::group::ParticipantChange;
use wa_core::types::jid::{GROUP_SERVER, Jid, NEWSLETTER_SERVER};

use super::error;
use crate::state::SharedState;

/// Body of `POST /group/create`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroupBody {
    /// The group name (subject).
    pub name: String,
    /// The initial participants, as `user@server` JIDs.
    #[serde(default)]
    pub participants: Vec<String>,
}

/// Body of `POST /group/{jid}/participants`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantsBody {
    /// `add`, `remove`, `promote` or `demote`.
    pub action: String,
    /// The participants the action applies to.
    #[serde(default)]
    pub participants: Vec<String>,
}

/// Body of `POST /group/{jid}/name`.
#[derive(Debug, Clone, Deserialize)]
pub struct NameBody {
    /// The new subject.
    pub name: String,
}

/// Body of `POST /group/{jid}/topic`.
#[derive(Debug, Clone, Deserialize)]
pub struct TopicBody {
    /// The new description; an empty string clears it.
    pub topic: String,
}

/// Query of `GET /group/{jid}/invite-link`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InviteLinkParams {
    /// Rotate the link instead of reading the current one.
    #[serde(default)]
    pub reset: bool,
}

/// Body of `POST /group/join`.
#[derive(Debug, Clone, Deserialize)]
pub struct JoinBody {
    /// An invite code or a full `https://chat.whatsapp.com/…` link.
    pub code: String,
}

/// Body of `POST /newsletter/subscribe` and `/newsletter/unsubscribe`.
#[derive(Debug, Clone, Deserialize)]
pub struct NewsletterBody {
    /// A newsletter JID, or a bare newsletter id.
    pub id: String,
}

/// `POST /group/{jid}/participants` answer: the participants as the server
/// reported them, each with an `error` code when its change failed.
#[derive(Debug, Clone, Serialize)]
pub struct ParticipantsResponse {
    pub ok: bool,
    pub participants: Vec<GroupParticipantData>,
}

/// `GET /group/{jid}/invite-link` answer.
#[derive(Debug, Clone, Serialize)]
pub struct InviteLinkResponse {
    pub link: String,
}

/// `POST /group/join` answer: the group that was joined (or whose join request
/// was filed).
#[derive(Debug, Clone, Serialize)]
pub struct JoinResponse {
    pub jid: Jid,
}

/// The `error` text of every answer that has no live protocol session.
pub const NOT_CONNECTED: &str = "not connected";

/// The live protocol client of this engine session.
///
/// The client lives in [`crate::connection::Connection`] (created by the first
/// `POST /connect`, dropped by `POST /logout`). Before that there is no session
/// and every group/presence/privacy/contact endpoint answers
/// `503 {"error":"not connected"}`. A client whose socket dropped is still
/// returned: its calls fail with `ClientError::NotConnected`, which
/// [`client_error`] maps to 503 as well.
pub fn session(state: &SharedState) -> Result<Arc<Client>, Response> {
    state.connection.client().ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, NOT_CONNECTED))
}

/// The device whose local stores (contacts, chat settings) back the read-only
/// endpoints: the live client's device, or — before any `POST /connect` — the
/// container's first device. Works offline; no session needed.
pub async fn current_device(state: &SharedState) -> Result<wa_core::store::Device, Response> {
    match state.connection.client() {
        Some(client) => Ok(client.device().await),
        None => state
            .container
            .get_first_device()
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// [`current_device`] without the HTTP error: `None` when no device can be
/// read at all. For background jobs (history import) that must not fail the
/// whole operation over a missing address book.
pub async fn current_device_or_first(state: &SharedState) -> Option<wa_core::store::Device> {
    current_device(state).await.ok()
}

/// `POST /logout` — Go `Client::Logout`: `remove-companion-device`, disconnect,
/// delete the device from the store. Answers the state afterwards
/// (`logged_out`), or `400 {"error":"not logged in"}` when nothing is paired.
/// The local message database (wa-store) is kept.
pub async fn logout(State(s): State<SharedState>) -> Result<Json<crate::connection::ConnectionStatus>, Response> {
    use crate::connection::LogoutError;
    match s.connection.logout().await {
        Ok(()) => Ok(Json(s.connection.status().await)),
        Err(LogoutError::NotLoggedIn) => Err(error(StatusCode::BAD_REQUEST, "not logged in")),
        Err(LogoutError::Client(err)) => Err(client_error(err)),
    }
}

/// Map a protocol failure onto HTTP: 503 for a dead session, 404 for an
/// unknown group, 400 for a rejected argument, 502 for everything else.
pub fn client_error(err: ClientError) -> Response {
    let detail = err.to_string();
    let status = match &err {
        ClientError::NotConnected | ClientError::NotLoggedIn => StatusCode::SERVICE_UNAVAILABLE,
        ClientError::MissingElement { tag: "group", .. } => StatusCode::NOT_FOUND,
        ClientError::InvalidArgument(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    let message = if status == StatusCode::NOT_FOUND { "unknown group".to_owned() } else { detail };
    error(status, message)
}

/// Unwrap a `Json` body, turning a deserialization failure into a
/// `400 {"error": …}` (axum's own rejection is plain text).
pub fn parse_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, Response> {
    body.map(|Json(value)| value)
        .map_err(|rejection| error(StatusCode::BAD_REQUEST, format!("invalid request body: {rejection}")))
}

/// A `user@server` JID, or a 400. A bare server name is rejected on purpose:
/// `Jid::parse("123")` would read it as a server, not as a user.
pub fn parse_jid(raw: &str) -> Result<Jid, Response> {
    let raw = raw.trim();
    // Both halves have to be there: `Jid::parse` accepts `user@` and `@server`
    // (it has to, for the server-only JIDs the protocol uses internally), but
    // neither can address a chat, and a request carrying one would go out with
    // an empty `to`.
    let (user, server) = raw.split_once('@').unwrap_or_default();
    if user.is_empty() || server.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, format!("invalid jid {raw:?}: expected user@server")));
    }
    Jid::parse(raw).map_err(|e| error(StatusCode::BAD_REQUEST, format!("invalid jid {raw:?}: {e}")))
}

/// A group JID (`…@g.us`), or a 400.
pub fn parse_group_jid(raw: &str) -> Result<Jid, Response> {
    let jid = parse_jid(raw)?;
    if jid.server != GROUP_SERVER {
        return Err(error(StatusCode::BAD_REQUEST, format!("{jid} is not a group jid (expected …@{GROUP_SERVER})")));
    }
    Ok(jid)
}

/// A list of `user@server` JIDs, or a 400 naming the first bad one.
pub fn parse_jids(raw: &[String]) -> Result<Vec<Jid>, Response> {
    raw.iter().map(|jid| parse_jid(jid)).collect()
}

/// A newsletter JID: `id@newsletter`, or a bare id that gets the server
/// appended.
pub fn parse_newsletter_jid(raw: &str) -> Result<Jid, Response> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "expected a newsletter jid or id"));
    }
    if raw.contains('@') { parse_jid(raw) } else { Ok(Jid::new(raw, NEWSLETTER_SERVER)) }
}

/// `add` / `remove` / `promote` / `demote`, or a 400.
pub fn parse_action(raw: &str) -> Result<ParticipantChange, Response> {
    match raw.trim() {
        "add" => Ok(ParticipantChange::Add),
        "remove" => Ok(ParticipantChange::Remove),
        "promote" => Ok(ParticipantChange::Promote),
        "demote" => Ok(ParticipantChange::Demote),
        other => Err(error(
            StatusCode::BAD_REQUEST,
            format!("unknown participant action {other:?} (expected add, remove, promote or demote)"),
        )),
    }
}

pub async fn create(
    State(s): State<SharedState>,
    body: Result<Json<CreateGroupBody>, JsonRejection>,
) -> Result<Json<GroupInfoData>, Response> {
    let body = parse_body(body)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "group name must not be empty"));
    }
    let participants = parse_jids(&body.participants)?;
    let client = session(&s)?;
    let info = client.create_group_named(name, &participants).await.map_err(client_error)?;
    Ok(Json(info))
}

pub async fn info(State(s): State<SharedState>, Path(jid): Path<String>) -> Result<Json<GroupInfoData>, Response> {
    let jid = parse_group_jid(&jid)?;
    let client = session(&s)?;
    let info = client.get_group_info(&jid).await.map_err(client_error)?;
    Ok(Json(info))
}

pub async fn leave(State(s): State<SharedState>, Path(jid): Path<String>) -> Result<Json<Value>, Response> {
    let jid = parse_group_jid(&jid)?;
    let client = session(&s)?;
    client.leave_group(&jid).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn participants(
    State(s): State<SharedState>,
    Path(jid): Path<String>,
    body: Result<Json<ParticipantsBody>, JsonRejection>,
) -> Result<Json<ParticipantsResponse>, Response> {
    let jid = parse_group_jid(&jid)?;
    let body = parse_body(body)?;
    let action = parse_action(&body.action)?;
    let updated = parse_jids(&body.participants)?;
    let client = session(&s)?;
    let participants = client.update_group_participants(&jid, &updated, action).await.map_err(client_error)?;
    Ok(Json(ParticipantsResponse { ok: true, participants }))
}

pub async fn set_name(
    State(s): State<SharedState>,
    Path(jid): Path<String>,
    body: Result<Json<NameBody>, JsonRejection>,
) -> Result<Json<Value>, Response> {
    let jid = parse_group_jid(&jid)?;
    let body = parse_body(body)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "group name must not be empty"));
    }
    let client = session(&s)?;
    client.set_group_name(&jid, name).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn set_topic(
    State(s): State<SharedState>,
    Path(jid): Path<String>,
    body: Result<Json<TopicBody>, JsonRejection>,
) -> Result<Json<Value>, Response> {
    let jid = parse_group_jid(&jid)?;
    let body = parse_body(body)?;
    let client = session(&s)?;
    // Go fetches the current topic id when `previous_id` is empty and mints a
    // fresh id for the new revision; the client does both.
    client.set_group_topic(&jid, "", "", &body.topic).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn invite_link(
    State(s): State<SharedState>,
    Path(jid): Path<String>,
    Query(params): Query<InviteLinkParams>,
) -> Result<Json<InviteLinkResponse>, Response> {
    let jid = parse_group_jid(&jid)?;
    let client = session(&s)?;
    let link = client.get_group_invite_link(&jid, params.reset).await.map_err(client_error)?;
    Ok(Json(InviteLinkResponse { link }))
}

pub async fn join(
    State(s): State<SharedState>,
    body: Result<Json<JoinBody>, JsonRejection>,
) -> Result<Json<JoinResponse>, Response> {
    let body = parse_body(body)?;
    let code = body.code.trim();
    if code.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "invite code must not be empty"));
    }
    let client = session(&s)?;
    let jid = client.join_group_with_link(code).await.map_err(client_error)?;
    Ok(Json(JoinResponse { jid }))
}

pub async fn newsletter_subscribe(
    State(s): State<SharedState>,
    body: Result<Json<NewsletterBody>, JsonRejection>,
) -> Result<Json<Value>, Response> {
    let body = parse_body(body)?;
    let jid = parse_newsletter_jid(&body.id)?;
    let client = session(&s)?;
    client.follow_newsletter(&jid).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn newsletter_unsubscribe(
    State(s): State<SharedState>,
    body: Result<Json<NewsletterBody>, JsonRejection>,
) -> Result<Json<Value>, Response> {
    let body = parse_body(body)?;
    let jid = parse_newsletter_jid(&body.id)?;
    let client = session(&s)?;
    client.unfollow_newsletter(&jid).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::routes;

    async fn app() -> axum::Router {
        let cfg = crate::config::Config::parse(["--no-auth".to_string()], |_| None);
        let cfg = cfg.expect("the test config must parse");
        routes::router(crate::state::AppState::memory(cfg).await.expect("in-memory state"))
    }

    async fn call(req: Request<Body>) -> (StatusCode, Value) {
        let res = app().await.oneshot(req).await.expect("the router must answer");
        let status = res.status();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    fn post(uri: &str, body: Value) -> Request<Body> {
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("a valid request")
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).expect("a valid request")
    }

    #[test]
    fn jids_are_validated_before_any_session_is_needed() {
        let err = parse_jid("not-a-jid").expect_err("a bare word is not a JID");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        let err = parse_jid("").expect_err("an empty string is not a JID");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        let err = parse_group_jid("1@s.whatsapp.net").expect_err("a user JID is not a group JID");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);

        assert_eq!(parse_jid("1@g.us").expect("a group JID").to_string(), "1@g.us");
        assert_eq!(parse_group_jid("1@g.us").expect("a group JID").user, "1");
        assert_eq!(parse_newsletter_jid("123").expect("a bare newsletter id").server, NEWSLETTER_SERVER);
        assert_eq!(parse_newsletter_jid("123@newsletter").expect("a newsletter JID").user, "123");
        assert_eq!(parse_jids(&["1@g.us".to_owned(), "2@g.us".to_owned()]).expect("both parse").len(), 2);
        assert!(parse_jids(&["1@g.us".to_owned(), "nope".to_owned()]).is_err());
    }

    #[test]
    fn actions_cover_the_four_go_constants() {
        assert_eq!(parse_action("add").expect("add"), ParticipantChange::Add);
        assert_eq!(parse_action("remove").expect("remove"), ParticipantChange::Remove);
        assert_eq!(parse_action("promote").expect("promote"), ParticipantChange::Promote);
        assert_eq!(parse_action("demote").expect("demote"), ParticipantChange::Demote);
        assert_eq!(parse_action(" add ").expect("trimmed"), ParticipantChange::Add);
        let err = parse_action("approve").expect_err("only the four member actions are accepted");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn client_errors_map_to_the_documented_statuses() {
        let cases = [
            (ClientError::NotConnected, StatusCode::SERVICE_UNAVAILABLE),
            (ClientError::NotLoggedIn, StatusCode::SERVICE_UNAVAILABLE),
            (ClientError::MissingElement { tag: "group", parent: "test" }, StatusCode::NOT_FOUND),
            (ClientError::InvalidArgument("bad"), StatusCode::BAD_REQUEST),
            (ClientError::MessageTimedOut, StatusCode::BAD_GATEWAY),
        ];
        for (err, expected) in cases {
            assert_eq!(client_error(err).status(), expected);
        }
    }

    #[tokio::test]
    async fn group_routes_answer_503_without_a_session() {
        let cases: Vec<(&str, Request<Body>)> = vec![
            ("POST /group/create", post("/group/create", json!({"name": "x", "participants": ["1@s.whatsapp.net"]}))),
            ("GET /group/{jid}/info", get("/group/1@g.us/info")),
            ("POST /group/{jid}/leave", post("/group/1@g.us/leave", json!({}))),
            (
                "POST /group/{jid}/participants",
                post("/group/1@g.us/participants", json!({"action": "add", "participants": ["1@s.whatsapp.net"]})),
            ),
            ("POST /group/{jid}/name", post("/group/1@g.us/name", json!({"name": "n"}))),
            ("POST /group/{jid}/topic", post("/group/1@g.us/topic", json!({"topic": "t"}))),
            ("GET /group/{jid}/invite-link", get("/group/1@g.us/invite-link")),
            ("GET /group/{jid}/invite-link?reset", get("/group/1@g.us/invite-link?reset=true")),
            ("POST /group/join", post("/group/join", json!({"code": "abc"}))),
            ("POST /newsletter/subscribe", post("/newsletter/subscribe", json!({"id": "123@newsletter"}))),
            ("POST /newsletter/unsubscribe", post("/newsletter/unsubscribe", json!({"id": "123"}))),
        ];
        for (name, req) in cases {
            let (status, body) = call(req).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{name}");
            assert_eq!(body, json!({ "error": NOT_CONNECTED }), "{name}");
        }
    }

    #[tokio::test]
    async fn bad_jids_and_bodies_are_rejected_with_400() {
        let (status, body) = call(get("/group/nope/info")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().is_some_and(|e| e.contains("expected user@server")), "got {body}");
        assert_eq!(call(get("/group/1@s.whatsapp.net/info")).await.0, StatusCode::BAD_REQUEST);

        let (status, _) = call(post("/group/create", json!({"name": "  "}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(post("/group/1@g.us/participants", json!({"action": "kick"}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(post("/group/join", json!({"code": " "}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(post("/newsletter/subscribe", json!({"id": ""}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_malformed_json_body_is_a_400_with_an_error_field() {
        let req = Request::post("/group/create")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{not json"))
            .expect("a valid request");
        let (status, body) = call(req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().is_some_and(|e| e.starts_with("invalid request body")), "got {body}");
    }

    #[test]
    fn invite_link_params_default_to_no_reset() {
        assert!(!InviteLinkParams::default().reset);
        let params: InviteLinkParams = serde_json::from_value(json!({})).expect("an empty query");
        assert!(!params.reset);
    }
}
