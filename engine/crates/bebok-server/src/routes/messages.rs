// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Message endpoints (`API-WHATSAPP.md` §3.2) — what the message view needs.
//!
//! | Method | Path                        | Handler          |
//! |--------|-----------------------------|------------------|
//! | GET    | `/chats/{jid}/messages`     | [`list`]         |
//! | POST   | `/send/message`             | [`send`]         |
//! | POST   | `/send/media`               | [`send_media`]   |
//! | POST   | `/receipt/read`             | [`mark_read`]    |
//!
//! [`list`] reads the local database (wa-store) only, so a chat opens offline
//! and instantly; it never touches the socket. [`send`], [`send_media`] and
//! [`mark_read`] need a live session (503 otherwise) and go through
//! [`wa_core::client::Client`].
//!
//! A sent message is written into wa-store with `from_me = true` right after
//! the server acknowledges it, so it shows up in the thread, in search and in
//! the export without waiting for a server echo.

// axum handlers answer with `Response`, so the request-validation helpers
// return one too; the large `Err` payload is expected, not a bug.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use wa_core::client::send::SendRequestExtra;
use wa_core::types::message::{MessageID, text_message};
use wa_store::StoredMessage;

use super::chats::blocking;
use super::error;
use super::groups::{client_error, parse_body, parse_jid, session};
use crate::state::SharedState;

/// Default and maximum page size of `GET /chats/{jid}/messages`.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;
/// Largest accepted media upload (64 MiB — WhatsApp's own image/video cap is
/// lower, but the check here is only against abuse).
const MAX_MEDIA_BYTES: usize = 64 * 1024 * 1024;

/// Query of `GET /chats/{jid}/messages`.
///
/// `before`/`beforeId` are the keyset cursor: pass the `timestamp` and `id` of
/// the oldest message you already hold to fetch the page before it. Without a
/// cursor the newest `limit` messages come back.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    pub before: Option<i64>,
    pub before_id: Option<String>,
    pub limit: Option<usize>,
}

/// `GET /chats/{jid}/messages` answer — one page, oldest-first.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    /// The page, ordered `(timestamp, id)` ascending — ready to render.
    pub messages: Vec<StoredMessage>,
    /// `true` when older messages exist before `messages[0]`.
    pub has_more: bool,
}

/// Body of `POST /send/message`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendBody {
    /// The chat to send to, as a `user@server` JID.
    pub to: String,
    /// The message body. Must not be blank.
    pub text: String,
}

/// `POST /send/message` answer: the stored row plus the server timestamp.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub id: String,
    pub timestamp: i64,
    pub chat: String,
    pub sender: String,
    /// The row as it was written to wa-store (`fromMe: true`).
    pub message: StoredMessage,
    /// Devices the message could not be encrypted for. Empty on a clean send;
    /// a non-empty list means the message reached only some of the
    /// recipient's (or our own) devices.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_devices: Vec<wa_core::client::send::SkippedDevice>,
}

/// Body of `POST /receipt/read`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadBody {
    /// The chat the messages are in.
    pub chat: String,
    /// The message IDs to acknowledge.
    #[serde(default)]
    pub ids: Vec<String>,
    /// The sender, for group chats (the `participant` attribute). Optional in
    /// 1:1 chats, where the chat *is* the sender.
    #[serde(default)]
    pub sender: Option<String>,
}

/// The page size a request asks for, clamped to `1..=MAX_LIMIT`.
fn page_limit(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// The keyset cursor of a page request. Both halves are required: `before`
/// alone would silently skip the messages that share that timestamp.
fn parse_cursor(before: Option<i64>, before_id: Option<String>) -> Result<Option<(i64, String)>, Response> {
    match (before, before_id) {
        (Some(ts), Some(id)) if !id.is_empty() => Ok(Some((ts, id))),
        (None, None) => Ok(None),
        _ => Err(error(StatusCode::BAD_REQUEST, "before and beforeId must be given together")),
    }
}

/// `GET /chats/{jid}/messages?before=&beforeId=&limit=` — a backward page of
/// one chat from the local database. Works without a session.
pub async fn list(
    State(s): State<SharedState>,
    Path(jid): Path<String>,
    Query(p): Query<ListParams>,
) -> Result<Json<MessagePage>, Response> {
    let limit = page_limit(p.limit);
    let cursor = parse_cursor(p.before, p.before_id)?;
    let page = blocking(&s.store, {
        let jid = jid.clone();
        let cursor = cursor.clone();
        move |st| {
            let messages = st.page_before(&jid, cursor.as_ref().map(|(ts, id)| (*ts, id.as_str())), limit)?;
            let has_more = match messages.first() {
                Some(first) => st.count_before(&jid, Some((first.timestamp, &first.id)))? > 0,
                // An empty page means there is nothing before the cursor.
                None => false,
            };
            Ok(MessagePage { messages, has_more })
        }
    })
    .await?;
    Ok(Json(page))
}

/// `POST /send/message` — Go `Client::SendMessage` with a `conversation`
/// (plain text) message, then persist the sent row locally.
pub async fn send(
    State(s): State<SharedState>,
    body: Result<Json<SendBody>, JsonRejection>,
) -> Result<Json<SendResult>, Response> {
    let body = parse_body(body)?;
    let text = body.text.trim_end_matches(['\r', '\n']).to_owned();
    if text.trim().is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "message text must not be empty"));
    }
    let to = parse_jid(&body.to)?;
    let client = session(&s)?;
    let response =
        client.send_message(&to, &text_message(&text), SendRequestExtra::default()).await.map_err(client_error)?;

    let row = StoredMessage {
        id: response.id.to_string(),
        chat_jid: response.chat.to_string(),
        sender: response.sender.to_string(),
        timestamp: if response.timestamp > 0 { response.timestamp } else { wa_store::unix_now() },
        kind: "text".to_owned(),
        text: Some(text),
        raw_json: None,
        from_me: true,
        push_name: None,
    };
    let stored = row.clone();
    blocking(&s.store, move |st| st.insert_message(&stored)).await?;

    if !response.skipped_devices.is_empty() {
        // The ring buffer behind `GET /debug/log` is the only diagnostic
        // channel a browser client has.
        crate::debug_log::global_note(format!(
            "send {}: {} device(s) skipped: {}",
            response.id,
            response.skipped_devices.len(),
            response
                .skipped_devices
                .iter()
                .map(|d| format!("{} ({})", d.device, d.error))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Ok(Json(SendResult {
        id: row.id.clone(),
        timestamp: row.timestamp,
        chat: row.chat_jid.clone(),
        sender: row.sender.clone(),
        message: row,
        skipped_devices: response.skipped_devices,
    }))
}

/// `POST /send/media` — Go `Client::Upload` + `Client::SendMessage` with an
/// `ImageMessage` (or a `VideoMessage` with `gif_playback` for GIFs).
///
/// Multipart form fields: `to` (chat JID), `file` (bytes), `mime` (e.g.
/// `image/jpeg`), optional `caption`, optional `kind` (`image` default,
/// `gif`). Dimensions come from the file when the `image` crate can read
/// them; a JPEG thumbnail is generated the same way.
pub async fn send_media(State(s): State<SharedState>, mut multipart: Multipart) -> Result<Json<SendResult>, Response> {
    let mut to: Option<String> = None;
    let mut caption: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut mime: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) =
        multipart.next_field().await.map_err(|err| error(StatusCode::BAD_REQUEST, err.to_string()))?
    {
        let name = field.name().unwrap_or("").to_owned();
        match name.as_str() {
            "to" => to = field.text().await.map_err(|err| error(StatusCode::BAD_REQUEST, err.to_string()))?.into(),
            "caption" => {
                caption = field.text().await.map_err(|err| error(StatusCode::BAD_REQUEST, err.to_string()))?.into();
            }
            "kind" => kind = field.text().await.map_err(|err| error(StatusCode::BAD_REQUEST, err.to_string()))?.into(),
            "mime" => mime = field.text().await.map_err(|err| error(StatusCode::BAD_REQUEST, err.to_string()))?.into(),
            "file" => {
                let data = field.bytes().await.map_err(|err| error(StatusCode::BAD_REQUEST, err.to_string()))?;
                bytes = Some(data.to_vec());
            }
            _ => {}
        }
    }
    let to =
        to.filter(|t| !t.trim().is_empty()).ok_or_else(|| error(StatusCode::BAD_REQUEST, "field 'to' is required"))?;
    let bytes =
        bytes.filter(|b| !b.is_empty()).ok_or_else(|| error(StatusCode::BAD_REQUEST, "field 'file' is required"))?;
    if bytes.len() > MAX_MEDIA_BYTES {
        return Err(error(StatusCode::PAYLOAD_TOO_LARGE, "media file too large"));
    }
    let mime = mime.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| guess_mime(&bytes));
    let media_type = media_type_for(&mime, kind.as_deref());
    let caption = caption.filter(|c| !c.trim().is_empty());

    let to = parse_jid(&to)?;
    let client = session(&s)?;
    let upload = client
        .upload_media(&bytes, &mime, media_type)
        .await
        .map_err(|err| error(StatusCode::BAD_GATEWAY, err.to_string()))?;

    let (width, height, thumbnail) = image_preview(&bytes);
    let dimensions = match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    };
    let message = match media_type {
        wa_core::client::download::MediaType::Video => {
            wa_core::client::upload::video_message(&upload, caption.as_deref(), dimensions, None, thumbnail, true)
        }
        _ => wa_core::client::upload::image_message(&upload, caption.as_deref(), dimensions, thumbnail),
    };
    let response = client.send_message(&to, &message, SendRequestExtra::default()).await.map_err(client_error)?;

    let (kind, text) = match media_type {
        wa_core::client::download::MediaType::Video => ("gif", caption.clone().unwrap_or_default()),
        _ => ("image", caption.clone().unwrap_or_default()),
    };
    let row = StoredMessage {
        id: response.id.to_string(),
        chat_jid: response.chat.to_string(),
        sender: response.sender.to_string(),
        timestamp: if response.timestamp > 0 { response.timestamp } else { wa_store::unix_now() },
        kind: kind.to_owned(),
        text: if text.is_empty() { None } else { Some(text) },
        raw_json: None,
        from_me: true,
        push_name: None,
    };
    let stored = row.clone();
    blocking(&s.store, move |st| st.insert_message(&stored)).await?;

    Ok(Json(SendResult {
        id: row.id.clone(),
        timestamp: row.timestamp,
        chat: row.chat_jid.clone(),
        sender: row.sender.clone(),
        message: row,
        skipped_devices: response.skipped_devices,
    }))
}

/// Guess the MIME type from magic bytes when the client did not send one.
fn guess_mime(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg".to_owned()
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png".to_owned()
    } else if bytes.starts_with(b"GIF8") {
        "image/gif".to_owned()
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
        "image/webp".to_owned()
    } else if bytes.len() > 11 && &bytes[4..12] == b"ftypisom" || bytes.len() > 11 && &bytes[4..12] == b"ftypmp42" {
        "video/mp4".to_owned()
    } else {
        "application/octet-stream".to_owned()
    }
}

/// The media type for an upload: GIFs (by kind or MIME) go through the video
/// keys with `gif_playback`, everything else through the image keys.
fn media_type_for(mime: &str, kind: Option<&str>) -> wa_core::client::download::MediaType {
    use wa_core::client::download::MediaType;
    if matches!(kind, Some(k) if k.eq_ignore_ascii_case("gif")) || mime == "image/gif" || mime.starts_with("video/") {
        MediaType::Video
    } else {
        MediaType::Image
    }
}

/// `(width, height, jpeg thumbnail)` of an image, best effort.
///
/// Reads dimensions from the file headers (JPEG SOF, PNG IHDR, GIF header,
/// WebP VP8/VP8L/VP8X) without pulling in an image crate. The thumbnail is
/// left to the client for now — WhatsApp renders the message without it.
fn image_preview(bytes: &[u8]) -> (Option<u32>, Option<u32>, Option<Vec<u8>>) {
    let (w, h) = image_dimensions(bytes);
    (w, h, None)
}

/// Image dimensions from magic bytes. `None` when the format is unknown.
fn image_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        jpeg_dimensions(bytes)
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        png_dimensions(bytes)
    } else if bytes.starts_with(b"GIF8") {
        gif_dimensions(bytes)
    } else if bytes.starts_with(b"RIFF") {
        webp_dimensions(bytes)
    } else {
        (None, None)
    }
}

fn png_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    if bytes.len() >= 24 && &bytes[12..16] == b"IHDR" {
        let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap_or([0; 4]));
        let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap_or([0; 4]));
        (Some(w), Some(h))
    } else {
        (None, None)
    }
}

fn gif_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    if bytes.len() >= 10 {
        let w = u16::from_le_bytes(bytes[6..8].try_into().unwrap_or([0; 2])) as u32;
        let h = u16::from_le_bytes(bytes[8..10].try_into().unwrap_or([0; 2])) as u32;
        (Some(w), Some(h))
    } else {
        (None, None)
    }
}

fn webp_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    if bytes.len() < 30 || &bytes[8..12] != b"WEBP" {
        return (None, None);
    }
    match &bytes[12..16] {
        b"VP8 " if bytes.len() >= 30 => {
            let w = u16::from_le_bytes(bytes[26..28].try_into().unwrap_or([0; 2])) as u32 & 0x3FFF;
            let h = u16::from_le_bytes(bytes[28..30].try_into().unwrap_or([0; 2])) as u32 & 0x3FFF;
            (Some(w), Some(h))
        }
        b"VP8L" if bytes.len() >= 25 => {
            let b = &bytes[21..25];
            let w = (u32::from(b[0]) | (u32::from(b[1]) & 0x3F) << 8) + 1;
            let h = ((u32::from(b[1]) >> 6) | (u32::from(b[2]) << 2) | ((u32::from(b[3]) & 0x0F) << 10)) + 1;
            (Some(w), Some(h))
        }
        b"VP8X" if bytes.len() >= 30 => {
            let w = (u32::from_le_bytes(bytes[24..28].try_into().unwrap_or([0; 4])) & 0xFFFFFF) + 1;
            let h = (u32::from_le_bytes(bytes[27..31.min(bytes.len())].try_into().unwrap_or([0; 4])) & 0xFFFFFF) + 1;
            (Some(w), Some(h))
        }
        _ => (None, None),
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    // Walk the markers to the first Start-Of-Frame carrying dimensions.
    let mut i = 2;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            break;
        }
        let marker = bytes[i + 1];
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        if i + 4 > bytes.len() {
            break;
        }
        let len = u16::from_be_bytes(bytes[i + 2..i + 4].try_into().unwrap_or([0; 2])) as usize;
        if len < 2 || i + len + 2 > bytes.len() {
            break;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) && len >= 7 {
            let h = u16::from_be_bytes(bytes[i + 5..i + 7].try_into().unwrap_or([0; 2])) as u32;
            let w = u16::from_be_bytes(bytes[i + 7..i + 9].try_into().unwrap_or([0; 2])) as u32;
            return (Some(w), Some(h));
        }
        i += 2 + len;
    }
    (None, None)
}
pub async fn mark_read(
    State(s): State<SharedState>,
    body: Result<Json<ReadBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Response> {
    let body = parse_body(body)?;
    let chat = parse_jid(&body.chat)?;
    let ids: Vec<MessageID> = body
        .ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(|id| MessageID::new(id.to_owned()))
        .collect();
    if ids.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "no message ids given"));
    }
    let sender = match body.sender.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => parse_jid(raw)?,
        None => chat.clone(),
    };
    let client = session(&s)?;
    client.mark_read(&ids, wa_store::unix_now(), &chat, &sender, None).await.map_err(client_error)?;
    Ok(Json(serde_json::json!({ "ok": true, "read": ids.len() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_clamped() {
        assert_eq!(page_limit(None), DEFAULT_LIMIT);
        assert_eq!(page_limit(Some(0)), 1);
        assert_eq!(page_limit(Some(10_000)), MAX_LIMIT);
        assert_eq!(page_limit(Some(25)), 25);
    }

    #[test]
    fn a_cursor_needs_both_halves() {
        assert_eq!(parse_cursor(None, None).ok(), Some(None));
        assert_eq!(parse_cursor(Some(42), Some("ABC".to_owned())).ok(), Some(Some((42, "ABC".to_owned()))));
        assert!(parse_cursor(Some(42), None).is_err());
        assert!(parse_cursor(None, Some("ABC".to_owned())).is_err());
        assert!(parse_cursor(Some(42), Some(String::new())).is_err());
    }

    #[test]
    fn list_params_parse_the_camel_case_query() {
        let p: ListParams =
            serde_json::from_value(serde_json::json!({ "before": 42, "beforeId": "ABC", "limit": 5 })).expect("params");
        assert_eq!((p.before, p.before_id.as_deref(), p.limit), (Some(42), Some("ABC"), Some(5)));
    }

    #[test]
    fn mime_is_guessed_from_magic_bytes() {
        assert_eq!(guess_mime(&[0xFF, 0xD8, 0xFF, 0x00]), "image/jpeg");
        assert_eq!(guess_mime(&[0x89, b'P', b'N', b'G']), "image/png");
        assert_eq!(guess_mime(b"GIF89a...."), "image/gif");
        assert_eq!(guess_mime(b"RIFF\x00\x00\x00\x00WEBP"), "image/webp");
        assert_eq!(guess_mime(b"junk"), "application/octet-stream");
    }

    #[test]
    fn dimensions_come_from_file_headers() {
        // 1x1 PNG: signature + IHDR with width/height.
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&2u32.to_be_bytes());
        assert_eq!(image_dimensions(&png), (Some(1), Some(2)));

        // GIF89a with width 3, height 4.
        let gif = [b'G', b'I', b'F', b'8', b'9', b'a', 3, 0, 4, 0];
        assert_eq!(image_dimensions(&gif), (Some(3), Some(4)));

        assert_eq!(image_dimensions(b"junk"), (None, None));
    }

    #[test]
    fn gifs_go_through_the_video_keys() {
        use wa_core::client::download::MediaType;
        assert_eq!(media_type_for("image/gif", None), MediaType::Video);
        assert_eq!(media_type_for("video/mp4", None), MediaType::Video);
        assert_eq!(media_type_for("image/jpeg", Some("gif")), MediaType::Video);
        assert_eq!(media_type_for("image/jpeg", None), MediaType::Image);
        assert_eq!(media_type_for("image/png", Some("image")), MediaType::Image);
    }
}
