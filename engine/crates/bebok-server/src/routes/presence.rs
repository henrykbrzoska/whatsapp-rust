// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Contacts, presence and privacy endpoints (`API-WHATSAPP.md` §3.4).
//!
//! | Method | Path                 | Handler             |
//! |--------|----------------------|---------------------|
//! | POST   | `/presence/subscribe`| [`subscribe`]       |
//! | POST   | `/presence/send`     | [`send`]            |
//! | POST   | `/presence/chat`     | [`chat`]            |
//! | GET    | `/privacy`           | [`get_privacy`]     |
//! | POST   | `/privacy`           | [`set_privacy`]     |
//! | GET    | `/contact?jid=`      | [`contact`]         |
//! | GET    | `/contacts`          | [`contacts`]        |
//! | GET    | `/picture?jid=`      | [`picture`]         |
//! | GET    | `/blocklist`         | [`blocklist`]       |
//! | POST   | `/block`             | [`block`]           |
//! | POST   | `/unblock`           | [`unblock`]         |
//!
//! `/contact(s)` read the local `ContactStore` and work without a session;
//! the rest need the live client (503 otherwise).
//!
//! The protocol work lives in `wa_core::client::presence` and
//! `wa_core::client::privacysettings`; these handlers only validate, call and
//! map errors (see [`super::groups::client_error`]).

// axum handlers answer with `Response`, so the small request-validation helpers
// return one too; the resulting large `Err` payload is expected, not a bug.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wa_core::binary::node::{AttrValue, Node};
use wa_core::client::privacysettings::{PrivacySetting, PrivacySettingType, PrivacySettings};
use wa_core::client::user::{GetProfilePictureParams, ProfilePictureInfoData};
use wa_core::client::{ClientError, InfoQuery, IqKind};
use wa_core::store::{ContactInfo, LidMapping};
use wa_core::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid};
use wa_core::types::presence::{ChatPresenceMedia, ChatPresenceState, PresenceState};
use wa_store::ContactView;

use super::error;
use super::groups::{client_error, current_device, parse_body, parse_jid, session};
use crate::state::SharedState;

/// Body of `POST /presence/subscribe`.
#[derive(Debug, Clone, Deserialize)]
pub struct PresenceSubscribeBody {
    /// The user whose presence we want.
    pub jid: String,
}

/// Body of `POST /presence/send`: `{"type": "available" | "unavailable"}`.
#[derive(Debug, Clone, Deserialize)]
pub struct PresenceSendBody {
    /// Go `types.Presence` — `available` or `unavailable`.
    #[serde(rename = "type")]
    pub kind: PresenceState,
}

/// Body of `POST /presence/chat`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatPresenceBody {
    /// The chat the typing state belongs to.
    pub chat: String,
    /// `composing` or `paused` (Go `types.ChatPresence`).
    pub state: ChatPresenceState,
    /// `audio` for a voice note, `text`/absent for typing (Go's text kind is
    /// the empty string).
    #[serde(default)]
    pub media: Option<String>,
}

/// Body of `POST /privacy`: one setting to change.
#[derive(Debug, Clone, Deserialize)]
pub struct PrivacySetBody {
    /// The setting name, e.g. `last`, `readreceipts`, `groupadd`.
    pub name: PrivacySettingType,
    /// The new value, e.g. `contacts`, `all`, `none`.
    pub value: PrivacySetting,
}

/// Parse the optional `media` of `POST /presence/chat`. Go's text kind is the
/// empty string, but a caller naturally writes `"text"` (or omits it), so all
/// three spellings are accepted.
fn parse_media(raw: Option<&str>) -> Result<ChatPresenceMedia, Response> {
    match raw.map(str::trim).unwrap_or_default() {
        "" | "text" => Ok(ChatPresenceMedia::Text),
        "audio" => Ok(ChatPresenceMedia::Audio),
        other => Err(error(
            StatusCode::BAD_REQUEST,
            format!("unknown chat presence media {other:?} (expected text or audio)"),
        )),
    }
}

pub async fn subscribe(
    State(s): State<SharedState>,
    body: Result<Json<PresenceSubscribeBody>, JsonRejection>,
) -> Result<Json<Value>, Response> {
    let body = parse_body(body)?;
    let jid = parse_jid(&body.jid)?;
    let client = session(&s)?;
    client.subscribe_presence(&jid).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn send(
    State(s): State<SharedState>,
    body: Result<Json<PresenceSendBody>, JsonRejection>,
) -> Result<Json<Value>, Response> {
    let body = parse_body(body)?;
    let client = session(&s)?;
    client.send_presence(body.kind).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn chat(
    State(s): State<SharedState>,
    body: Result<Json<ChatPresenceBody>, JsonRejection>,
) -> Result<Json<Value>, Response> {
    let body = parse_body(body)?;
    let media = parse_media(body.media.as_deref())?;
    let chat = parse_jid(&body.chat)?;
    let client = session(&s)?;
    client.send_chat_presence(&chat, body.state, media).await.map_err(client_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn get_privacy(State(s): State<SharedState>) -> Result<Json<PrivacySettings>, Response> {
    let client = session(&s)?;
    // `try_fetch` reports the failure instead of falling back to the empty
    // settings (`Client::get_privacy_settings` is the Go "log and ignore" path).
    let settings = client.try_fetch_privacy_settings().await.map_err(client_error)?;
    Ok(Json(settings))
}

pub async fn set_privacy(
    State(s): State<SharedState>,
    body: Result<Json<PrivacySetBody>, JsonRejection>,
) -> Result<Json<PrivacySettings>, Response> {
    let body = parse_body(body)?;
    let client = session(&s)?;
    let settings = client.set_privacy_setting(body.name, body.value).await.map_err(client_error)?;
    Ok(Json(settings))
}

// ---------------------------------------------------------------------------
// Contacts, profile pictures, blocklist (`API-WHATSAPP.md` §3.4)
// ---------------------------------------------------------------------------

/// Query of `GET /contact?jid=`.
#[derive(Debug, Clone, Deserialize)]
pub struct JidParams {
    pub jid: String,
}

/// One contact as `GET /contact` / `GET /contacts` answer it (wa-core
/// `ContactInfo` plus the resolved display name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactResponse {
    pub jid: String,
    /// `false` when the store has no row for the JID (all names empty).
    pub found: bool,
    pub first_name: String,
    pub full_name: String,
    pub push_name: String,
    pub business_name: String,
    pub redacted_phone: String,
    /// Address-book name, then business name, then push name.
    pub display_name: Option<String>,
}

impl ContactResponse {
    pub fn new(jid: &Jid, info: ContactInfo) -> Self {
        let view = ContactView {
            first_name: info.first_name.clone(),
            full_name: info.full_name.clone(),
            push_name: info.push_name.clone(),
            business_name: info.business_name.clone(),
        };
        Self {
            jid: jid.to_string(),
            found: info.found,
            display_name: view.display_name().map(str::to_owned),
            first_name: info.first_name,
            full_name: info.full_name,
            push_name: info.push_name,
            business_name: info.business_name,
            redacted_phone: info.redacted_phone,
        }
    }
}

fn internal(e: impl std::fmt::Display) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// `GET /contact?jid=` — `ContactStore::GetContact` (works offline).
pub async fn contact(
    State(s): State<SharedState>,
    Query(p): Query<JidParams>,
) -> Result<Json<ContactResponse>, Response> {
    let jid = parse_jid(&p.jid)?;
    let device = current_device(&s).await?;
    let info = device.contacts.get_contact(&jid).await.map_err(internal)?;
    Ok(Json(ContactResponse::new(&jid, info)))
}

/// `GET /contacts` — `ContactStore::GetAllContacts`, sorted by display name
/// then JID (works offline).
pub async fn contacts(State(s): State<SharedState>) -> Result<Json<Vec<ContactResponse>>, Response> {
    let device = current_device(&s).await?;
    let all = device.contacts.get_all_contacts().await.map_err(internal)?;
    let mut out: Vec<ContactResponse> = all.into_iter().map(|(jid, info)| ContactResponse::new(&jid, info)).collect();
    out.sort_by(|a, b| {
        let key = |c: &ContactResponse| c.display_name.as_deref().map(str::to_lowercase);
        // Named contacts first, alphabetically; unnamed ones by JID.
        match (key(a), key(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.jid.cmp(&b.jid))
    });
    Ok(Json(out))
}

/// Query of `GET /picture?jid=&preview=&existingId=`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureParams {
    pub jid: String,
    /// The thumbnail instead of the full image.
    #[serde(default)]
    pub preview: bool,
    /// The last known picture ID: an unchanged picture then answers `null`.
    #[serde(default)]
    pub existing_id: String,
}

/// `GET /picture?jid=` — `Client::GetProfilePictureInfo`. `null` when the
/// user/group has no picture (or it is unchanged since `existingId`).
pub async fn picture(
    State(s): State<SharedState>,
    Query(p): Query<PictureParams>,
) -> Result<Json<Option<ProfilePictureInfoData>>, Response> {
    let jid = parse_jid(&p.jid)?;
    let client = session(&s)?;
    let params = GetProfilePictureParams { preview: p.preview, existing_id: p.existing_id, ..Default::default() };
    let info = client.get_profile_picture_info(&jid, &params).await.map_err(client_error)?;
    Ok(Json(info))
}

/// Go `types.BlocklistItem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlocklistItem {
    /// The blocked user (normally a `@lid` JID).
    pub jid: Jid,
    /// The phone-number JID, when the server sent one.
    pub pn: Option<Jid>,
    pub active: bool,
}

/// Go `types.Blocklist` — `GET /blocklist`, `POST /block`, `POST /unblock`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Blocklist {
    pub dhash: String,
    pub addressing_mode: String,
    pub items: Vec<BlocklistItem>,
}

/// Body of `POST /block` and `POST /unblock`.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockBody {
    pub jid: String,
}

fn attr_string(node: &Node, key: &str) -> Option<String> {
    node.attrs.get(key).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))
}

fn attr_jid(node: &Node, key: &str) -> Option<Jid> {
    match node.attrs.get(key)? {
        AttrValue::Jid(jid) => Some(jid.clone()),
        other => Jid::parse(&other.to_string()).ok(),
    }
    .filter(|jid| !jid.is_empty())
}

fn attr_bool(node: &Node, key: &str) -> bool {
    match node.attrs.get(key) {
        Some(AttrValue::Bool(b)) => *b,
        Some(other) => other.to_string() == "true",
        None => false,
    }
}

/// Go `parseBlocklist` — the `<list>` node, plus the LID↔PN mappings of the
/// active entries (stored so later sends resolve the LID).
pub fn parse_blocklist(list: &Node) -> (Blocklist, Vec<LidMapping>) {
    let mut out = Blocklist {
        dhash: attr_string(list, "dhash").unwrap_or_default(),
        addressing_mode: attr_string(list, "addressing_mode").unwrap_or_default(),
        items: Vec::new(),
    };
    let mut mappings = Vec::new();
    for child in list.children() {
        let Some(jid) = attr_jid(child, "jid") else {
            tracing::debug!("ignoring blocklist item without a jid: {}", child.to_xml_string());
            continue;
        };
        let item = BlocklistItem { jid, pn: attr_jid(child, "pn_jid"), active: attr_bool(child, "active") };
        if item.active
            && item.jid.server == HIDDEN_USER_SERVER
            && let Some(pn) = item.pn.as_ref().filter(|pn| pn.server == DEFAULT_USER_SERVER)
        {
            mappings.push(LidMapping { lid: item.jid.clone(), pn: pn.clone() });
        }
        out.items.push(item);
    }
    (out, mappings)
}

/// The `<item>` of a blocklist update (Go `UpdateBlocklist`'s `itemAttrs`).
pub fn build_blocklist_item(lid: &Jid, pn: Option<&Jid>, block: bool) -> Node {
    let mut item = Node::new("item").attr("jid", lid.clone()).attr("action", if block { "block" } else { "unblock" });
    if block && let Some(pn) = pn.filter(|pn| !pn.is_empty()) {
        item = item.attr("pn_jid", pn.clone());
    }
    item
}

fn blocklist_query(kind: IqKind, content: Vec<Node>) -> InfoQuery {
    InfoQuery { namespace: "blocklist".into(), kind, to: Some(Jid::server_jid()), content, ..Default::default() }
}

fn list_child(response: &Node, what: &'static str) -> Result<Node, ClientError> {
    response.optional_child_by_tag(&["list"]).cloned().ok_or(ClientError::MissingElement { tag: "list", parent: what })
}

/// `GET /blocklist` — Go `GetBlocklist`.
pub async fn blocklist(State(s): State<SharedState>) -> Result<Json<Blocklist>, Response> {
    let client = session(&s)?;
    let response = client.send_iq(blocklist_query(IqKind::Get, Vec::new())).await.map_err(client_error)?;
    let list = list_child(&response, "response to blocklist query").map_err(client_error)?;
    let (blocklist, mappings) = parse_blocklist(&list);
    if !mappings.is_empty()
        && let Err(err) = client.device().await.lids.put_many_lid_mappings(&mappings).await
    {
        tracing::warn!("failed to store LID mappings from the blocklist query: {err}");
    }
    Ok(Json(blocklist))
}

/// `POST /block {jid}`.
pub async fn block(
    State(s): State<SharedState>,
    body: Result<Json<BlockBody>, JsonRejection>,
) -> Result<Json<Blocklist>, Response> {
    update_blocklist(&s, body, true).await
}

/// `POST /unblock {jid}`.
pub async fn unblock(
    State(s): State<SharedState>,
    body: Result<Json<BlockBody>, JsonRejection>,
) -> Result<Json<Blocklist>, Response> {
    update_blocklist(&s, body, false).await
}

/// Go `UpdateBlocklist` — the server wants the LID; a phone-number JID is
/// resolved through the LID store, then a usync query.
async fn update_blocklist(
    s: &SharedState,
    body: Result<Json<BlockBody>, JsonRejection>,
    block: bool,
) -> Result<Json<Blocklist>, Response> {
    let body = parse_body(body)?;
    let jid = parse_jid(&body.jid)?;
    if jid.server != DEFAULT_USER_SERVER && jid.server != HIDDEN_USER_SERVER {
        return Err(error(StatusCode::BAD_REQUEST, format!("{jid} is not a user jid")));
    }
    let client = session(s)?;
    let lids = client.device().await.lids;
    let (lid, pn) = if jid.server == DEFAULT_USER_SERVER {
        let known = lids.get_lid_for_pn(&jid).await.map_err(internal)?;
        let lid = match known {
            Some(lid) if !lid.is_empty() => lid,
            _ => {
                let infos = client.get_user_info(std::slice::from_ref(&jid)).await.map_err(client_error)?;
                infos
                    .into_iter()
                    .find(|info| info.jid.user == jid.user && !info.lid.is_empty())
                    .map(|info| info.lid)
                    .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("{jid} is not on WhatsApp (no LID)")))?
            }
        };
        (lid, Some(jid))
    } else {
        let pn = if block { lids.get_pn_for_lid(&jid).await.map_err(internal)? } else { None };
        (jid, pn)
    };
    let item = build_blocklist_item(&lid, pn.as_ref(), block);
    let response = client.send_iq(blocklist_query(IqKind::Set, vec![item])).await.map_err(client_error)?;
    let list = list_child(&response, "response to blocklist update").map_err(client_error)?;
    Ok(Json(parse_blocklist(&list).0))
}

#[cfg(test)]
mod tests {
    #[test]
    fn blocklist_parsing_follows_go_parse_blocklist() {
        let list = Node::new("list").attr("dhash", "123").attr("addressing_mode", "lid").with_content(vec![
            Node::new("item").attr("jid", "1@lid").attr("pn_jid", "48111@s.whatsapp.net").attr("active", "true"),
            Node::new("item").attr("jid", "2@lid"),
            Node::new("item"),
        ]);
        let (bl, mappings) = parse_blocklist(&list);
        assert_eq!(bl.dhash, "123");
        assert_eq!(bl.addressing_mode, "lid");
        assert_eq!(bl.items.len(), 2, "an item without a jid is skipped");
        assert!(bl.items[0].active);
        assert_eq!(bl.items[0].pn.as_ref().map(|j| j.to_string()).as_deref(), Some("48111@s.whatsapp.net"));
        assert!(!bl.items[1].active);
        assert!(bl.items[1].pn.is_none());
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].lid.to_string(), "1@lid");
        let json = serde_json::to_value(&bl).unwrap();
        assert_eq!(json["addressingMode"], "lid");
        assert_eq!(json["items"][0]["jid"], "1@lid");
    }

    #[test]
    fn blocklist_items_carry_the_pn_only_when_blocking() {
        let lid = Jid::parse("1@lid").unwrap();
        let pn = Jid::parse("48111@s.whatsapp.net").unwrap();
        let block = build_blocklist_item(&lid, Some(&pn), true);
        assert_eq!(block.attrs.get("action").and_then(AttrValue::as_str), Some("block"));
        assert!(block.attrs.contains_key("pn_jid"));
        let unblock = build_blocklist_item(&lid, Some(&pn), false);
        assert_eq!(unblock.attrs.get("action").and_then(AttrValue::as_str), Some("unblock"));
        assert!(!unblock.attrs.contains_key("pn_jid"));
    }

    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::routes;

    async fn call(req: Request<Body>) -> (StatusCode, Value) {
        let cfg =
            crate::config::Config::parse(["--no-auth".to_string()], |_| None).expect("the test config must parse");
        let state = crate::state::AppState::memory(cfg).await.expect("in-memory state");
        let res = routes::router(state).oneshot(req).await.expect("the router must answer");
        let status = res.status();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn post(uri: &str, body: Value) -> Request<Body> {
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("a valid request")
    }

    #[test]
    fn presence_bodies_accept_the_go_wire_values() {
        let send: PresenceSendBody = serde_json::from_value(json!({"type": "unavailable"})).expect("a valid body");
        assert_eq!(send.kind, PresenceState::Unavailable);
        assert!(serde_json::from_value::<PresenceSendBody>(json!({"type": "away"})).is_err());

        let chat: ChatPresenceBody =
            serde_json::from_value(json!({"chat": "1@s.whatsapp.net", "state": "composing"})).expect("a valid body");
        assert_eq!(chat.state, ChatPresenceState::Composing);
        assert_eq!(chat.media, None);
        assert!(serde_json::from_value::<ChatPresenceBody>(json!({"chat": "1", "state": "typing"})).is_err());
    }

    #[test]
    fn chat_presence_media_speaks_go_and_json() {
        assert_eq!(parse_media(None).expect("absent is text"), ChatPresenceMedia::Text);
        assert_eq!(parse_media(Some("")).expect("'' is text"), ChatPresenceMedia::Text);
        assert_eq!(parse_media(Some("text")).expect("'text' is text"), ChatPresenceMedia::Text);
        assert_eq!(parse_media(Some("audio")).expect("'audio' is audio"), ChatPresenceMedia::Audio);
        let err = parse_media(Some("video")).expect_err("only text and audio exist");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn privacy_bodies_use_the_wire_names() {
        let body: PrivacySetBody =
            serde_json::from_value(json!({"name": "last", "value": "contacts"})).expect("a valid body");
        assert_eq!(body.name, PrivacySettingType::LastSeen);
        assert_eq!(body.value, PrivacySetting::Contacts);
        assert!(serde_json::from_value::<PrivacySetBody>(json!({"name": "nope", "value": "all"})).is_err());
        assert!(serde_json::from_value::<PrivacySetBody>(json!({"name": "last", "value": "maybe"})).is_err());
    }

    #[tokio::test]
    async fn presence_and_privacy_routes_answer_503_without_a_session() {
        let cases: Vec<(&str, Request<Body>)> = vec![
            ("POST /presence/subscribe", post("/presence/subscribe", json!({"jid": "1@s.whatsapp.net"}))),
            ("POST /presence/send", post("/presence/send", json!({"type": "available"}))),
            (
                "POST /presence/chat",
                post("/presence/chat", json!({"chat": "1@s.whatsapp.net", "state": "composing", "media": "audio"})),
            ),
            ("GET /privacy", Request::get("/privacy").body(Body::empty()).expect("a valid request")),
            ("POST /privacy", post("/privacy", json!({"name": "last", "value": "none"}))),
        ];
        for (name, req) in cases {
            let (status, body) = call(req).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{name}");
            assert_eq!(body, json!({ "error": crate::routes::groups::NOT_CONNECTED }), "{name}");
        }
    }

    #[tokio::test]
    async fn bad_presence_requests_are_400() {
        assert_eq!(call(post("/presence/subscribe", json!({"jid": "nope"}))).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            call(post("/presence/chat", json!({"chat": "1", "state": "composing"}))).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(post("/presence/chat", json!({"chat": "1@s.whatsapp.net", "state": "typing"}))).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(post("/presence/chat", json!({"chat": "1@s.whatsapp.net", "state": "paused", "media": "video"})))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(call(post("/privacy", json!({"name": "nope", "value": "all"}))).await.0, StatusCode::BAD_REQUEST);
    }
}
