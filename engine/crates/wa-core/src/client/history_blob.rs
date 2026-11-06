// Ported from the Go client `history.go` / `message.go` (`handleHistorySyncNotificationLoop`,
// the decode half: zlib inflate + `proto.Unmarshal(HistorySync)` + conversation walk),
// © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Decoding of a history-sync blob into plain message records (GAP A4, decode half).
//!
//! The phone uploads the history blob encrypted and announces it with a
//! [`wa_e2e::HistorySyncNotification`] (see [`super::history_sync`]). Once the
//! encrypted blob has been fetched and decrypted, its payload is a zlib stream
//! of a `HistorySync` protobuf. This module turns that payload into
//! [`HistoryMessage`] records that `wa-store` can persist.
//!
//! Scope: **pure decoding only**. Fetching the blob (media download and
//! `mediaconn`) is GAP A2/A3 and is not ported yet, so nothing calls this
//! function from the live pipeline today.

use std::io::Read;

use prost::Message as ProstMessage;
use wa_proto::wa_web_protobufs_e2e::Message as WaMessage;
use wa_proto::wa_web_protobufs_history_sync::HistorySync;
use wa_proto::wa_web_protobufs_web::WebMessageInfo;

/// One message recovered from a history-sync blob, in store-friendly form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    /// Chat JID (`key.remoteJID`, or the enclosing conversation ID as fallback).
    pub chat: String,
    /// Message ID (`key.ID`).
    pub id: String,
    /// Author JID: our own JID for `fromMe`, otherwise the participant (groups)
    /// or the chat JID (1:1).
    pub sender: String,
    pub from_me: bool,
    /// Unix seconds (`WebMessageInfo.messageTimestamp`, 0 when absent).
    pub timestamp: i64,
    /// `text`, a media kind (`image`, `video`, `audio`, `document`,
    /// `sticker`) or `other`.
    pub message_type: String,
    pub text: Option<String>,
    /// `WebMessageInfo.pushName` — the sender's own display name. For a chat
    /// that is not in the address book this is the only name there is.
    pub push_name: Option<String>,
    /// The message protobuf as it arrived, kept so an attachment can be
    /// downloaded later (it holds `directPath`, `mediaKey` and the hashes).
    pub raw: Option<Vec<u8>>,
}

/// One chat as the history blob describes it: its name and flags, even when
/// the blob carries no messages for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryConversation {
    /// Chat JID.
    pub id: String,
    /// The display name the phone has for this chat (contact or group subject).
    pub name: Option<String>,
    /// Unix seconds of the last message the phone knows about.
    pub last_timestamp: i64,
    pub unread: u32,
    pub archived: bool,
    /// Pin order (`0` = not pinned).
    pub pinned: u32,
    /// Unix seconds the mute ends at, when muted.
    pub mute_end_time: Option<i64>,
    /// `true` when the phone says this is the oldest chunk it will send.
    pub end_of_history: bool,
}

/// What one history-sync blob contained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodedHistory {
    /// Every chat in the blob, with its name and flags.
    pub conversations: Vec<HistoryConversation>,
    /// Every message that carried a key, in blob order.
    pub messages: Vec<HistoryMessage>,
}

/// Inflate (zlib) and decode a history-sync payload, returning every message
/// that carries a message key. Entries without a key are skipped.
///
/// `own_jid` is our own JID; it is used as the sender of `fromMe` messages.
pub fn decode_history_blob(compressed: &[u8], own_jid: &str) -> Result<DecodedHistory, String> {
    let mut plain = Vec::new();
    flate2::read::ZlibDecoder::new(compressed)
        .read_to_end(&mut plain)
        .map_err(|err| format!("failed to inflate history sync blob: {err}"))?;
    let sync =
        HistorySync::decode(plain.as_slice()).map_err(|err| format!("failed to unmarshal HistorySync: {err}"))?;

    let mut out = DecodedHistory::default();
    for conversation in &sync.conversations {
        out.conversations.push(HistoryConversation {
            id: conversation.id.clone(),
            name: conversation.name.clone().filter(|n| !n.trim().is_empty()),
            last_timestamp: conversation.last_msg_timestamp.unwrap_or(0) as i64,
            unread: conversation.unread_count.unwrap_or(0),
            archived: conversation.archived.unwrap_or(false),
            pinned: conversation.pinned.unwrap_or(0),
            mute_end_time: conversation.mute_end_time.map(|t| t as i64).filter(|t| *t > 0),
            end_of_history: conversation.end_of_history_transfer.unwrap_or(false),
        });
        for entry in &conversation.messages {
            let Some(web) = entry.message.as_ref() else { continue };
            out.messages.push(record_from_web(web, &conversation.id, own_jid));
        }
    }
    Ok(out)
}

fn record_from_web(web: &WebMessageInfo, conversation_id: &str, own_jid: &str) -> HistoryMessage {
    let key = &web.key;
    let chat = key.remote_jid.clone().unwrap_or_else(|| conversation_id.to_owned());
    let from_me = key.from_me.unwrap_or(false);
    let sender = if from_me { own_jid.to_owned() } else { key.participant.clone().unwrap_or_else(|| chat.clone()) };
    let body = web.message.as_ref();
    let text = body.and_then(text_of);
    let message_type = match (body.and_then(media_kind_of), &text) {
        (Some(kind), _) => kind.to_owned(),
        (None, Some(_)) => "text".to_owned(),
        (None, None) => "other".to_owned(),
    };
    HistoryMessage {
        chat,
        id: key.id.clone().unwrap_or_default(),
        sender,
        from_me,
        timestamp: web.message_timestamp.unwrap_or(0) as i64,
        message_type,
        text,
        push_name: web.push_name.clone().filter(|n| !n.trim().is_empty()),
        raw: body.map(|msg| msg.encode_to_vec()),
    }
}

/// The attachment kind a message carries, if any. The caption of a media
/// message is its `text`, so both can be present.
///
/// A GIF and a voice note get their own kind (`gif`, `ptt`) even though they
/// travel as a video and an audio message: the UI plays a GIF looping and
/// muted, and labels a voice note differently.
pub fn media_kind_of(msg: &WaMessage) -> Option<&'static str> {
    if msg.image_message.is_some() {
        Some("image")
    } else if let Some(video) = msg.video_message.as_ref() {
        Some(if video.gif_playback.unwrap_or(false) { "gif" } else { "video" })
    } else if let Some(audio) = msg.audio_message.as_ref() {
        Some(if audio.ptt.unwrap_or(false) { "ptt" } else { "audio" })
    } else if msg.document_message.is_some() {
        Some("document")
    } else if msg.sticker_message.is_some() {
        Some("sticker")
    } else {
        None
    }
}

/// [`media_kind_of`] for a stored payload (the hex-decoded `waE2E.Message`).
///
/// The incoming stanza only says `type="media"`, which is not enough to pick
/// an `<img>` over a `<video>`; the payload is. Protobuf decoding stays in
/// wa-core, so the server passes the bytes and gets the kind back.
pub fn media_kind_from_bytes(raw: &[u8]) -> Option<&'static str> {
    media_kind_of(&WaMessage::decode(raw).ok()?)
}

/// Plain-text body of a message (`conversation` or `extendedTextMessage.text`).
fn text_of(msg: &WaMessage) -> Option<String> {
    msg.conversation
        .clone()
        .or_else(|| msg.extended_text_message.as_ref().and_then(|ext| ext.text.clone()))
        // A media caption is the body a chat list and a search index want.
        .or_else(|| msg.image_message.as_ref().and_then(|m| m.caption.clone()))
        .or_else(|| msg.video_message.as_ref().and_then(|m| m.caption.clone()))
        .or_else(|| msg.document_message.as_ref().and_then(|m| m.caption.clone()))
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_proto::wa_common::MessageKey;
    use wa_proto::wa_web_protobufs_e2e::{ExtendedTextMessage, Message as WaMessage};
    use wa_proto::wa_web_protobufs_history_sync::{Conversation, HistorySyncMsg};

    const OWN: &str = "15550001111@s.whatsapp.net";

    fn web(id: &str, remote: &str, from_me: bool, participant: Option<&str>, text: Option<&str>) -> WebMessageInfo {
        WebMessageInfo {
            key: MessageKey {
                remote_jid: Some(remote.to_owned()),
                from_me: Some(from_me),
                id: Some(id.to_owned()),
                participant: participant.map(str::to_owned),
            },
            message: text.map(|t| WaMessage { conversation: Some(t.to_owned()), ..Default::default() }),
            message_timestamp: Some(1_700_000_123),
            ..Default::default()
        }
    }

    fn blob(conversations: Vec<Conversation>) -> Vec<u8> {
        use flate2::{Compression, write::ZlibEncoder};
        use std::io::Write;
        let sync = HistorySync { conversations, ..Default::default() };
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&sync.encode_to_vec()).unwrap();
        enc.finish().unwrap()
    }

    fn conversation(id: &str, msgs: Vec<WebMessageInfo>) -> Conversation {
        Conversation {
            id: id.to_owned(),
            messages: msgs.into_iter().map(|m| HistorySyncMsg { message: Some(m), msg_order_id: None }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn decodes_direct_and_group_messages_with_senders() {
        let direct = "15550002222@s.whatsapp.net";
        let group = "120363000000000000@g.us";
        let bytes = blob(vec![
            conversation(
                direct,
                vec![web("A1", direct, false, None, Some("hi")), web("A2", direct, true, None, Some("hello"))],
            ),
            conversation(group, vec![web("G1", group, false, Some("15550003333@s.whatsapp.net"), Some("group hi"))]),
        ]);

        let decoded = decode_history_blob(&bytes, OWN).expect("decodes");
        assert_eq!(decoded.messages.len(), 3);

        assert_eq!(decoded.messages[0].id, "A1");
        assert_eq!(decoded.messages[0].sender, direct);
        assert!(!decoded.messages[0].from_me);
        assert_eq!(decoded.messages[0].text.as_deref(), Some("hi"));
        assert_eq!(decoded.messages[0].timestamp, 1_700_000_123);

        assert_eq!(decoded.messages[1].sender, OWN, "fromMe messages are authored by us");
        assert!(decoded.messages[1].from_me);

        assert_eq!(decoded.messages[2].chat, group);
        assert_eq!(decoded.messages[2].sender, "15550003333@s.whatsapp.net", "group sender is the participant");
    }

    #[test]
    fn skips_entries_without_message_and_falls_back_to_conversation_id() {
        let chat = "15550002222@s.whatsapp.net";
        let mut no_body = conversation(chat, vec![]);
        no_body.messages.push(HistorySyncMsg { message: None, msg_order_id: None });
        // A message whose key has no remoteJID falls back to the conversation ID.
        let mut keyless_remote = web("K1", "", false, None, None);
        keyless_remote.key.remote_jid = None;
        no_body.messages.push(HistorySyncMsg { message: Some(keyless_remote), msg_order_id: None });

        let decoded = decode_history_blob(&blob(vec![no_body]), OWN).expect("decodes");
        assert_eq!(decoded.messages.len(), 1);
        assert_eq!(decoded.messages[0].chat, chat);
        assert_eq!(decoded.messages[0].message_type, "other");
        assert_eq!(decoded.messages[0].text, None);
    }

    #[test]
    fn extended_text_body_is_read() {
        let chat = "15550002222@s.whatsapp.net";
        let mut m = web("E1", chat, false, None, None);
        m.message = Some(WaMessage {
            extended_text_message: Some(Box::new(ExtendedTextMessage {
                text: Some("linked".into()),
                ..Default::default()
            })),
            ..Default::default()
        });
        let decoded = decode_history_blob(&blob(vec![conversation(chat, vec![m])]), OWN).expect("decodes");
        assert_eq!(decoded.messages[0].text.as_deref(), Some("linked"));
        assert_eq!(decoded.messages[0].message_type, "text");
    }

    #[test]
    fn conversations_carry_their_name_and_flags() {
        use wa_proto::wa_web_protobufs_history_sync::Conversation;
        let chat = "192680906768485@lid";
        let mut c = conversation(chat, vec![web("N1", chat, false, None, Some("hi"))]);
        c.name = Some("Kasia".into());
        c.last_msg_timestamp = Some(1_700_000_500);
        c.unread_count = Some(3);
        c.archived = Some(true);
        c.pinned = Some(1);
        c.mute_end_time = Some(1_800_000_000);
        c.end_of_history_transfer = Some(true);
        // A chat the blob names but sends no messages for still shows up.
        let empty = Conversation { id: "120363@g.us".to_owned(), name: Some("Family".into()), ..Default::default() };

        let decoded = decode_history_blob(&blob(vec![c, empty]), OWN).expect("decodes");
        assert_eq!(decoded.conversations.len(), 2);
        let first = &decoded.conversations[0];
        assert_eq!((first.id.as_str(), first.name.as_deref()), (chat, Some("Kasia")));
        assert_eq!((first.last_timestamp, first.unread, first.pinned), (1_700_000_500, 3, 1));
        assert!(first.archived && first.end_of_history);
        assert_eq!(first.mute_end_time, Some(1_800_000_000));
        assert_eq!(decoded.conversations[1].name.as_deref(), Some("Family"));
        assert_eq!(decoded.messages.len(), 1, "the named-but-empty chat adds no messages");
    }

    #[test]
    fn a_message_keeps_its_push_name_and_raw_payload() {
        let chat = "192680906768485@lid";
        let mut m = web("P1", chat, false, None, Some("hi"));
        m.push_name = Some("Kasia".into());
        let mut mine = web("P2", chat, true, None, Some("mine"));
        mine.push_name = Some("   ".into());

        let decoded = decode_history_blob(&blob(vec![conversation(chat, vec![m, mine])]), OWN).expect("decodes");
        assert_eq!(decoded.messages[0].push_name.as_deref(), Some("Kasia"));
        assert_eq!(decoded.messages[1].push_name, None, "a blank push name is dropped");
        // The raw protobuf is kept so an attachment can be downloaded later.
        let raw = decoded.messages[0].raw.as_ref().expect("raw payload");
        assert_eq!(WaMessage::decode(raw.as_slice()).unwrap().conversation.as_deref(), Some("hi"));
    }

    #[test]
    fn media_messages_report_their_kind_and_caption() {
        use wa_proto::wa_web_protobufs_e2e::{DocumentMessage, ImageMessage, StickerMessage};
        let chat = "192680906768485@lid";

        let mut image = web("M1", chat, false, None, None);
        image.message = Some(WaMessage {
            image_message: Some(Box::new(ImageMessage {
                caption: Some("at the lake".into()),
                direct_path: Some("/v/t62/abc?ccb=11".into()),
                media_key: Some(vec![1; 32]),
                ..Default::default()
            })),
            ..Default::default()
        });
        let mut sticker = web("M2", chat, false, None, None);
        sticker.message =
            Some(WaMessage { sticker_message: Some(Box::new(StickerMessage::default())), ..Default::default() });
        let mut doc = web("M3", chat, false, None, None);
        doc.message = Some(WaMessage {
            document_message: Some(Box::new(DocumentMessage { caption: Some("spec".into()), ..Default::default() })),
            ..Default::default()
        });

        let decoded = decode_history_blob(&blob(vec![conversation(chat, vec![image, sticker, doc])]), OWN).unwrap();
        let kinds: Vec<&str> = decoded.messages.iter().map(|m| m.message_type.as_str()).collect();
        assert_eq!(kinds, ["image", "sticker", "document"]);
        assert_eq!(decoded.messages[0].text.as_deref(), Some("at the lake"), "a caption is the body");
        assert_eq!(decoded.messages[1].text, None);
        assert_eq!(decoded.messages[2].text.as_deref(), Some("spec"));
    }

    #[test]
    fn rejects_non_zlib_input() {
        let err = decode_history_blob(b"not zlib at all", OWN).unwrap_err();
        assert!(err.contains("inflate"), "{err}");
    }

    /// A GIF travels as an mp4 and a voice note as an audio message; the UI
    /// loops the first and labels the second, so they need their own kinds.
    #[test]
    fn a_gif_and_a_voice_note_get_their_own_kind() {
        let video = |gif: bool| WaMessage {
            video_message: Some(Box::new(wa_proto::wa_web_protobufs_e2e::VideoMessage {
                gif_playback: Some(gif),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(media_kind_of(&video(true)), Some("gif"));
        assert_eq!(media_kind_of(&video(false)), Some("video"));

        let audio = |ptt: bool| WaMessage {
            audio_message: Some(Box::new(wa_proto::wa_web_protobufs_e2e::AudioMessage {
                ptt: Some(ptt),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(media_kind_of(&audio(true)), Some("ptt"));
        assert_eq!(media_kind_of(&audio(false)), Some("audio"));

        assert_eq!(media_kind_of(&WaMessage { conversation: Some("hi".into()), ..Default::default() }), None);
    }

    /// The server only has the stored hex payload, and protobuf decoding stays
    /// in wa-core.
    #[test]
    fn the_kind_can_be_read_back_from_a_stored_payload() {
        let msg = WaMessage {
            image_message: Some(Box::new(wa_proto::wa_web_protobufs_e2e::ImageMessage::default())),
            ..Default::default()
        };
        assert_eq!(media_kind_from_bytes(&msg.encode_to_vec()), Some("image"));
        assert_eq!(media_kind_from_bytes(&[0xff, 0xff, 0xff, 0xff]), None, "not a protobuf");
        assert_eq!(media_kind_from_bytes(&[]), None, "an empty payload decodes to an empty message");
    }
}
