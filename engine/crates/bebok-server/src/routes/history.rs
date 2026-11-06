// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Fetching older history, and serving attachments.
//!
//! | Method | Path                   | Handler        |
//! |--------|------------------------|----------------|
//! | POST   | `/history/fetch`       | [`fetch`]      |
//! | GET    | `/media/{id}`          | [`media`]      |
//!
//! **Fetching** is asynchronous by nature: the engine sends a peer message
//! asking our own phone for the messages before the oldest one we hold, and
//! the phone answers minutes later with an encrypted blob, which
//! [`crate::history_import`] stores. So [`fetch`] reports what it *asked for*,
//! and the UI learns the result from the `history_imported` event.
//!
//! **Media** is served from the local blob cache, downloading on a miss. The
//! cache is a convenience, not storage: [`MEDIA_BLOB_TTL_SECS`] old blobs are
//! purged on every request (and hourly by [`spawn_media_purge`]), so a chat
//! with a hundred photos does not quietly become a hundred megabytes on disk.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use wa_core::client::download::downloadable_from_bytes;
use wa_core::types::message::{MessageID, MessageInfo};

use super::chats::blocking;
use super::error;
use super::groups::{parse_body, parse_jid, session};
use crate::state::SharedState;

/// How long a decrypted attachment stays in the local cache.
pub const MEDIA_BLOB_TTL_SECS: i64 = 3600;
/// How often the background purge runs.
const MEDIA_PURGE_INTERVAL: Duration = Duration::from_secs(600);
/// Messages to ask for when the caller does not say.
const DEFAULT_HISTORY_COUNT: i32 = 200;
/// The phone refuses much larger on-demand requests.
const MAX_HISTORY_COUNT: i32 = 500;

/// Body of `POST /history/fetch`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchBody {
    /// One chat, or every known chat when absent.
    #[serde(default)]
    pub chat: Option<String>,
    /// How many messages to ask for per chat.
    #[serde(default)]
    pub count: Option<i32>,
}

/// One chat the request covered.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FetchedChat {
    pub chat: String,
    /// The message the phone was asked to go back from.
    pub oldest_id: String,
    pub oldest_timestamp: i64,
    /// The ID of the peer request, for matching it in the debug log.
    pub request_id: String,
}

/// `POST /history/fetch` answer.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FetchReport {
    /// How many messages were asked for per chat.
    pub count: i32,
    /// The chats a request went out for.
    pub requested: Vec<FetchedChat>,
    /// Chats that were skipped, with the reason (no messages to anchor on,
    /// or the request failed).
    pub skipped: Vec<String>,
}

/// `POST /history/fetch` — ask the phone for the messages before the oldest
/// one this engine holds, for one chat or for every chat.
///
/// The answer arrives later as a history-sync blob; this only reports the
/// requests that went out.
pub async fn fetch(
    State(s): State<SharedState>,
    body: Result<Json<FetchBody>, JsonRejection>,
) -> Result<Json<FetchReport>, Response> {
    let body = parse_body(body)?;
    let count = body.count.unwrap_or(DEFAULT_HISTORY_COUNT).clamp(1, MAX_HISTORY_COUNT);
    // The request is validated before the session is looked at, so a typo in
    // the JID reads as a 400 and not as "not connected".
    let named = match body.chat.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(chat) => {
            parse_jid(chat)?;
            Some(chat.to_owned())
        }
        None => None,
    };
    let client = session(&s)?;

    let chats: Vec<String> = match named {
        Some(chat) => vec![chat],
        None => blocking(&s.store, |st| st.chat_jids()).await?,
    };

    let mut report = FetchReport { count, ..Default::default() };
    for chat in chats {
        // The request has to name a message to go back from.
        let oldest = {
            let chat = chat.clone();
            blocking(&s.store, move |st| st.page(&chat, None, None, None, 1)).await?
        };
        let Some(oldest) = oldest.into_iter().next() else {
            report.skipped.push(format!("{chat}: no messages to ask from"));
            continue;
        };
        let Ok(jid) = wa_core::types::jid::Jid::parse(&chat) else {
            report.skipped.push(format!("{chat}: not a valid jid"));
            continue;
        };
        let info = MessageInfo {
            chat: jid,
            id: MessageID::new(oldest.id.clone()),
            timestamp: oldest.timestamp,
            is_from_me: oldest.from_me,
            ..Default::default()
        };
        match client.request_history(&info, count).await {
            Ok(request_id) => report.requested.push(FetchedChat {
                chat: chat.clone(),
                oldest_id: oldest.id,
                oldest_timestamp: oldest.timestamp,
                request_id: request_id.to_string(),
            }),
            Err(err) => report.skipped.push(format!("{chat}: {err}")),
        }
    }
    s.debug_log.note(format!(
        "history fetch: asked for {count} message(s) in {} chat(s), {} skipped",
        report.requested.len(),
        report.skipped.len()
    ));
    Ok(Json(report))
}

/// `GET /media/{message_id}` — the decrypted bytes of one attachment.
///
/// Served from the blob cache when it is warm, downloaded and cached
/// otherwise. The response carries the sender's MIME type, so the browser can
/// render it in an `<img>`, `<video>` or `<audio>` element directly.
pub async fn media(State(s): State<SharedState>, Path(id): Path<String>) -> Result<Response, Response> {
    // Opportunistic cleanup: keeps the cache bounded without a scheduler.
    purge_expired(&s).await;

    if let Some((mime, bytes)) = {
        let id = id.clone();
        blocking(&s.store, move |st| st.get_media_blob(&id)).await?
    } {
        return Ok(serve(mime, bytes));
    }

    let stored = {
        let id = id.clone();
        blocking(&s.store, move |st| st.get_message(&id)).await?
    }
    .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("unknown message {id}")))?;

    let raw = stored
        .raw_json
        .as_deref()
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "this message has no stored payload to download"))?;
    let bytes = hex::decode(raw).map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("bad payload: {e}")))?;
    let (downloadable, media_type) = downloadable_from_bytes(&bytes)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "this message carries no attachment"))?;

    let client = session(&s)?;
    let data = client
        .download_media(&downloadable, media_type)
        .await
        .map_err(|err| error(StatusCode::BAD_GATEWAY, format!("media download failed: {err}")))?;

    let mime = downloadable.mime_type.clone();
    {
        let (id, mime, data) = (id.clone(), mime.clone(), data.clone());
        blocking(&s.store, move |st| st.put_media_blob(&id, &mime, &data)).await?;
    }
    Ok(serve(mime, data))
}

/// `GET /media/{id}/info` — what the engine knows about an attachment without
/// downloading it. Lets the UI decide between an inline image and a link.
pub async fn media_info(State(s): State<SharedState>, Path(id): Path<String>) -> Result<Json<MediaInfo>, Response> {
    let stored = {
        let id = id.clone();
        blocking(&s.store, move |st| st.get_message(&id)).await?
    }
    .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("unknown message {id}")))?;
    let cached = {
        let id = id.clone();
        blocking(&s.store, move |st| st.get_media_blob(&id)).await?.is_some()
    };
    let media = stored
        .raw_json
        .as_deref()
        .and_then(|raw| hex::decode(raw).ok())
        .and_then(|bytes| downloadable_from_bytes(&bytes));
    Ok(Json(MediaInfo {
        id,
        kind: stored.kind,
        available: media.is_some(),
        cached,
        mime_type: media.as_ref().map(|(d, _)| d.mime_type.clone()),
        size: media.as_ref().map(|(d, _)| d.file_length),
    }))
}

/// `GET /media/{id}/info` answer.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub id: String,
    /// The stored message kind (`image`, `video`, …).
    pub kind: String,
    /// `true` when the message carries a downloadable attachment.
    pub available: bool,
    /// `true` when the bytes are already in the local cache.
    pub cached: bool,
    pub mime_type: Option<String>,
    /// Declared plaintext size in bytes, when the sender gave one.
    pub size: Option<u64>,
}

fn serve(mime: String, bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            // The bytes never change for a message id, but they are private
            // and short-lived locally.
            (header::CACHE_CONTROL, "private, max-age=3600".to_owned()),
        ],
        Body::from(bytes),
    )
        .into_response()
}

/// Drop cached attachments older than [`MEDIA_BLOB_TTL_SECS`].
async fn purge_expired(s: &SharedState) {
    let store = Arc::clone(&s.store);
    let purged = tokio::task::spawn_blocking(move || store.purge_media_blobs(MEDIA_BLOB_TTL_SECS)).await;
    match purged {
        Ok(Ok(n)) if n > 0 => tracing::debug!("purged {n} cached attachment(s)"),
        Ok(Err(err)) => tracing::warn!("media cache purge failed: {err}"),
        _ => {}
    }
}

/// Background purge, so a long-running engine does not keep an hour-old cache
/// around just because nobody opened a chat.
pub fn spawn_media_purge(app: &SharedState) {
    let weak = Arc::downgrade(app);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(MEDIA_PURGE_INTERVAL).await;
            let Some(app) = weak.upgrade() else { break };
            purge_expired(&app).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_requested_count_is_clamped() {
        let clamp = |c: Option<i32>| c.unwrap_or(DEFAULT_HISTORY_COUNT).clamp(1, MAX_HISTORY_COUNT);
        assert_eq!(clamp(None), DEFAULT_HISTORY_COUNT);
        assert_eq!(clamp(Some(0)), 1);
        assert_eq!(clamp(Some(-5)), 1);
        assert_eq!(clamp(Some(10_000)), MAX_HISTORY_COUNT);
        assert_eq!(clamp(Some(50)), 50);
    }

    #[test]
    fn the_body_accepts_both_shapes() {
        let all: FetchBody = serde_json::from_str("{}").expect("empty body = every chat");
        assert!(all.chat.is_none() && all.count.is_none());
        let one: FetchBody =
            serde_json::from_value(serde_json::json!({ "chat": "1@g.us", "count": 50 })).expect("one chat");
        assert_eq!((one.chat.as_deref(), one.count), (Some("1@g.us"), Some(50)));
    }
}
