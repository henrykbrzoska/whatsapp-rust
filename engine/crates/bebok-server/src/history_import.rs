// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! History-sync import: blob → `wa-store`.
//!
//! The phone announces history with a `HistorySyncNotification` and the
//! messages themselves sit in an encrypted blob on the media CDN. `wa-core`
//! downloads and decodes it ([`Client::download_history_blob`]); this module
//! owns the database side, which the protocol crate deliberately does not:
//!
//! * messages go in with [`Store::insert_messages_if_new`], so a live message
//!   (decrypted here, with its device and push name) always beats the history
//!   copy of the same ID;
//! * the raw message protobuf is kept as hex in `raw_json`, which is what
//!   makes an attachment downloadable later (`GET /media/{id}`);
//! * a conversation's name is written into the contact store's push-name slot,
//!   so the chat list can show "Kasia" for a chat that is not in the address
//!   book — the address book still wins when it has an entry.
//!
//! Both the automatic sync (at pairing) and `POST /history/fetch` land here.

use std::sync::Arc;

use wa_core::client::history_sync::HistorySyncNotificationEvent;
use wa_core::types::jid::Jid;
use wa_store::StoredMessage;

use crate::state::SharedState;

/// What one imported blob contained.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    /// Messages in the blob.
    pub messages: usize,
    /// Messages that were not in the database yet.
    pub stored: usize,
    /// Chats the blob described.
    pub conversations: usize,
    /// Chats that got a name out of it.
    pub named: usize,
    /// `true` when the phone said this was the oldest chunk it has.
    pub end_of_history: bool,
}

/// Download, decode and store one history-sync blob.
pub async fn import(app: &SharedState, event: &HistorySyncNotificationEvent) -> anyhow::Result<ImportReport> {
    let client = app.connection.client().ok_or_else(|| anyhow::anyhow!("not connected"))?;
    let decoded = client.download_history_blob(event).await?;

    let mut report = ImportReport {
        messages: decoded.messages.len(),
        conversations: decoded.conversations.len(),
        end_of_history: decoded.conversations.iter().any(|c| c.end_of_history),
        ..Default::default()
    };

    // The phone answers in phone-number form, while live messages arrive
    // LID-addressed — the same conversation under two JIDs. Without this the
    // import opens a parallel chat and the open thread looks untouched.
    let device = crate::routes::groups::current_device_or_first(app).await;
    let canonical = Canonicalise { device: device.clone() };

    let mut rows: Vec<StoredMessage> = Vec::with_capacity(decoded.messages.len());
    for m in decoded.messages.iter().filter(|m| !m.id.is_empty() && !m.chat.is_empty()) {
        rows.push(StoredMessage {
            id: m.id.clone(),
            chat_jid: canonical.jid(&m.chat).await,
            sender: canonical.jid(&m.sender).await,
            timestamp: m.timestamp,
            kind: m.message_type.clone(),
            text: m.text.clone(),
            // Hex, like the `message`/`raw` fields of `WaEvent::Message`.
            raw_json: m.raw.as_ref().map(hex::encode),
            from_me: m.from_me,
            push_name: m.push_name.clone(),
        });
    }
    if !rows.is_empty() {
        let store = Arc::clone(&app.store);
        report.stored = tokio::task::spawn_blocking(move || store.insert_messages_if_new(&rows)).await??;
    }

    // Chat names: the blob is the only place a never-messaged chat gets one.
    if let Some(device) = device {
        for conversation in &decoded.conversations {
            let Some(name) = conversation.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else { continue };
            let Ok(jid) = Jid::parse(&canonical.jid(&conversation.id).await) else { continue };
            match device.contacts.put_push_name(&jid.to_non_ad(), name).await {
                Ok(_) => report.named += 1,
                Err(err) => tracing::warn!("could not store the name of {}: {err}", conversation.id),
            }
        }
    }

    // The blob may have arrived in PN form for a chat we know by LID.
    if let Err(err) = merge_split_chats(app).await {
        tracing::warn!("chat merge after the import failed: {err}");
    }

    app.debug_log.note(format!(
        "history sync imported: {} message(s) ({} new), {} chat(s), {} named{}",
        report.messages,
        report.stored,
        report.conversations,
        report.named,
        if report.end_of_history { ", end of history" } else { "" }
    ));
    Ok(report)
}

/// Rewrites a phone-number JID to the LID the engine knows for it.
///
/// The LID map is filled from the `_alt` attributes of live messages and from
/// our own identities on connect (`Client::store_lid_pn_mapping`), so a chat
/// we have ever heard from resolves. Anything else — a group, a newsletter, a
/// PN with no known LID — is handed back untouched; guessing would scatter the
/// history further.
struct Canonicalise {
    device: Option<wa_core::store::Device>,
}

impl Canonicalise {
    async fn jid(&self, raw: &str) -> String {
        let Some(device) = &self.device else { return raw.to_owned() };
        let Ok(jid) = Jid::parse(raw) else { return raw.to_owned() };
        if jid.server != wa_core::types::jid::DEFAULT_USER_SERVER {
            return raw.to_owned();
        }
        match device.lids.get_lid_for_pn(&jid.to_non_ad()).await {
            Ok(Some(lid)) => lid.with_device(jid.device).to_string(),
            _ => raw.to_owned(),
        }
    }
}

/// Merge chats that are the same conversation under two JIDs.
///
/// A PN-keyed chat whose LID the engine knows is moved into the LID chat —
/// the one live messages land in and the one the chat list shows a name for.
/// Runs after every import and once at startup, because the LID map fills up
/// from live traffic: a pair we did not know yesterday can be known today.
///
/// Returns the merges it made, as `(from, to, moved)`.
pub async fn merge_split_chats(app: &SharedState) -> anyhow::Result<Vec<(String, String, usize)>> {
    let Some(device) = crate::routes::groups::current_device_or_first(app).await else { return Ok(Vec::new()) };

    let store = Arc::clone(&app.store);
    let chats = tokio::task::spawn_blocking(move || store.chat_jids_stored()).await??;

    // Every chat addressed by phone number is a merge candidate.
    let mut candidates: Vec<(String, Jid)> = Vec::new();
    for chat in chats {
        let Ok(jid) = Jid::parse(&chat) else { continue };
        if jid.server == wa_core::types::jid::DEFAULT_USER_SERVER {
            candidates.push((chat, jid.to_non_ad()));
        }
    }

    // The ones the LID map does not know yet are worth one `usync` round trip:
    // the answer carries the LID and `get_user_info` stores the mapping. Only
    // then is "we do not know this contact's LID" the truth.
    let mut unknown = Vec::new();
    for (_, pn) in &candidates {
        if matches!(device.lids.get_lid_for_pn(pn).await, Ok(None)) {
            unknown.push(pn.clone());
        }
    }
    if !unknown.is_empty() {
        match app.connection.client() {
            Some(client) => {
                if let Err(err) = client.get_user_info(&unknown).await {
                    tracing::debug!("could not look up the LIDs of {} chat(s): {err}", unknown.len());
                }
            }
            None => tracing::debug!("no session: {} chat(s) keep their phone-number jid", unknown.len()),
        }
    }

    let mut plan: Vec<(String, String)> = Vec::new();
    for (chat, pn) in candidates {
        if let Ok(Some(lid)) = device.lids.get_lid_for_pn(&pn).await {
            let target = lid.to_non_ad().to_string();
            if target != chat {
                plan.push((chat, target));
            }
        }
    }
    if plan.is_empty() {
        return Ok(Vec::new());
    }

    let store = Arc::clone(&app.store);
    let merged = tokio::task::spawn_blocking(move || {
        let mut done = Vec::new();
        for (from, to) in plan {
            match store.merge_chat(&from, &to) {
                Ok(moved) => done.push((from, to, moved)),
                Err(err) => tracing::warn!("could not merge {from} into {to}: {err}"),
            }
        }
        done
    })
    .await?;
    for (from, to, moved) in &merged {
        let note = format!("merged {moved} message(s) from {from} into {to} (same contact, PN and LID)");
        tracing::info!("{note}");
        app.debug_log.note(note);
    }
    Ok(merged)
}

/// Run [`merge_split_chats`] in the background, logging failures.
pub fn spawn_chat_merge(app: &SharedState) {
    let app = Arc::clone(app);
    tokio::spawn(async move {
        if let Err(err) = merge_split_chats(&app).await {
            tracing::warn!("chat merge failed: {err}");
        }
    });
}

/// The stanza kinds that say nothing about the attachment a row carries.
const STANZA_KINDS: [&str; 2] = ["media", "text"];

/// Re-derive the attachment kind of rows that were stored under a stanza kind.
///
/// An incoming `<message type="media">` says only "this is an attachment", so
/// rows written before the kind was taken from the payload sit in the database
/// as `media` — and the thread renders `📎 media` instead of the photo. The
/// payload is still there, so the kind can be recovered without the network.
///
/// Runs once at startup. Returns how many rows were corrected; a row whose
/// payload holds no attachment (a plain text message) is left alone.
pub fn repair_message_kinds(store: &wa_store::Store) -> anyhow::Result<usize> {
    let mut fixed = 0;
    for (id, raw) in store.messages_with_payload(&STANZA_KINDS)? {
        let Some(kind) =
            hex::decode(&raw).ok().and_then(|bytes| wa_core::client::history_blob::media_kind_from_bytes(&bytes))
        else {
            continue;
        };
        if store.set_message_kind(&id, kind)? {
            fixed += 1;
        }
    }
    Ok(fixed)
}

/// Run [`repair_message_kinds`] on the blocking pool, logging the outcome.
pub fn spawn_message_kind_repair(app: &SharedState) {
    let store = Arc::clone(&app.store);
    let log = Arc::clone(&app.debug_log);
    tokio::task::spawn_blocking(move || match repair_message_kinds(&store) {
        Ok(0) => {}
        Ok(fixed) => {
            let note = format!("recovered the attachment kind of {fixed} stored message(s)");
            tracing::info!("{note}");
            log.note(note);
        }
        Err(err) => tracing::warn!("could not repair the stored message kinds: {err}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::state::AppState;

    #[tokio::test]
    async fn importing_without_a_session_fails_cleanly() {
        let cfg = Config::parse(["--no-auth".to_string()], |_| None).unwrap();
        let app = AppState::memory(cfg).await.unwrap();
        let event = HistorySyncNotificationEvent {
            message_id: wa_core::types::message::MessageID::new("H1".to_owned()),
            chat: Jid::parse("48111@s.whatsapp.net").unwrap(),
            sender: Jid::parse("48111@s.whatsapp.net").unwrap(),
            conversation_timestamp: 0,
            notification: Default::default(),
        };
        let err = import(&app, &event).await.expect_err("no session");
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    /// A hex `waE2E.Message` with one attachment field set.
    fn payload(kind: &str) -> String {
        use prost::Message as _;
        use wa_proto::wa_web_protobufs_e2e as wa_e2e;
        let mut msg = wa_e2e::Message::default();
        match kind {
            "image" => msg.image_message = Some(Box::new(wa_e2e::ImageMessage::default())),
            "sticker" => msg.sticker_message = Some(Box::new(wa_e2e::StickerMessage::default())),
            "video" => msg.video_message = Some(Box::new(wa_e2e::VideoMessage::default())),
            "gif" => {
                msg.video_message =
                    Some(Box::new(wa_e2e::VideoMessage { gif_playback: Some(true), ..Default::default() }))
            }
            "audio" => msg.audio_message = Some(Box::new(wa_e2e::AudioMessage::default())),
            "ptt" => msg.audio_message = Some(Box::new(wa_e2e::AudioMessage { ptt: Some(true), ..Default::default() })),
            "document" => msg.document_message = Some(Box::new(wa_e2e::DocumentMessage::default())),
            _ => msg.conversation = Some("hello".to_owned()),
        }
        hex::encode(msg.encode_to_vec())
    }

    fn row(id: &str, kind: &str, raw: Option<String>) -> StoredMessage {
        StoredMessage {
            id: id.to_owned(),
            chat_jid: "1@s.whatsapp.net".to_owned(),
            sender: "1@s.whatsapp.net".to_owned(),
            timestamp: 10,
            kind: kind.to_owned(),
            text: None,
            raw_json: raw,
            from_me: false,
            push_name: None,
        }
    }

    /// Without a device (and so without a LID map) nothing is merged — and
    /// nothing is lost either.
    #[tokio::test]
    async fn without_a_device_no_chat_is_merged() {
        let cfg = Config::parse(["--no-auth".to_string()], |_| None).unwrap();
        let app = AppState::memory(cfg).await.unwrap();
        app.store.insert_messages(&[row("m1", "text", None)]).unwrap();

        assert!(merge_split_chats(&app).await.unwrap().is_empty());
        assert_eq!(app.store.chat_jids_stored().unwrap(), ["1@s.whatsapp.net"], "the chat is untouched");
    }

    /// The thread renders by kind, so a row stored under the stanza kind
    /// (`type="media"`) shows `📎 media` instead of the photo. The payload
    /// still names the real kind.
    #[tokio::test]
    async fn the_stored_kind_is_recovered_from_the_payload() {
        let cfg = Config::parse(["--no-auth".to_string()], |_| None).unwrap();
        let app = AppState::memory(cfg).await.unwrap();
        let rows: Vec<StoredMessage> = [
            ("m-image", "image"),
            ("m-sticker", "sticker"),
            ("m-video", "video"),
            ("m-gif", "gif"),
            ("m-audio", "audio"),
            ("m-ptt", "ptt"),
            ("m-doc", "document"),
        ]
        .iter()
        .map(|(id, kind)| row(id, "media", Some(payload(kind))))
        .collect();
        app.store.insert_messages(&rows).unwrap();
        // A text message with a payload, and a media row whose payload is gone.
        app.store
            .insert_messages(&[row("m-text", "text", Some(payload("text"))), row("m-lost", "media", None)])
            .unwrap();

        assert_eq!(repair_message_kinds(&app.store).unwrap(), 7, "one per attachment row");

        let kind = |id: &str| app.store.get_message(id).unwrap().expect("row").kind;
        assert_eq!(kind("m-image"), "image");
        assert_eq!(kind("m-sticker"), "sticker");
        assert_eq!(kind("m-video"), "video");
        assert_eq!(kind("m-gif"), "gif", "a gif is an mp4 that loops");
        assert_eq!(kind("m-audio"), "audio");
        assert_eq!(kind("m-ptt"), "ptt", "a voice note is labelled differently");
        assert_eq!(kind("m-doc"), "document");
        assert_eq!(kind("m-text"), "text", "a payload without an attachment is left alone");
        assert_eq!(kind("m-lost"), "media", "no payload, nothing to recover");

        // Idempotent: a second run has nothing left to do.
        assert_eq!(repair_message_kinds(&app.store).unwrap(), 0);
    }

    #[tokio::test]
    async fn the_repair_keeps_the_search_index_in_step() {
        let cfg = Config::parse(["--no-auth".to_string()], |_| None).unwrap();
        let app = AppState::memory(cfg).await.unwrap();
        let mut with_caption = row("m-cap", "media", Some(payload("image")));
        with_caption.text = Some("sunset over the lake".to_owned());
        app.store.insert_messages(&[with_caption]).unwrap();

        repair_message_kinds(&app.store).unwrap();

        let hits = app.store.search("sunset", None, 10, wa_store::SearchMode::Keyword).unwrap();
        assert_eq!(hits.len(), 1, "the FTS row survived the update");
        assert_eq!(hits[0].message.kind, "image");
    }
}
