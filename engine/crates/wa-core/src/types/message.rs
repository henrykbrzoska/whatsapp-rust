// Ported from the Go client `types/message.go` and
// `types/events/events.go` (`Message.UnwrapRaw`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Port of `Go types/message.go` plus the message event payload
//! (`events.Message` + `UnwrapRaw`).
//!
//! [`MessageInfo`] is the metadata of one incoming message; [`Message`] pairs
//! it with the protobuf body and the flags `UnwrapRaw` derived from the
//! wrapping (`DeviceSent`, `Ephemeral`, `ViewOnce`, …). Neither type is
//! serialized to the UI directly — `WaEvent` carries the flags and the
//! hex-encoded protobufs (`API-WHATSAPP.md` §2).

use super::jid::Jid;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::{fmt, str::FromStr};

/// The generated `waE2E.Message` — the protobuf body of a WhatsApp message.
pub type WaMessage = wa_proto::wa_web_protobufs_e2e::Message;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageID(String);

impl MessageID {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl From<&str> for MessageID {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}
impl fmt::Display for MessageID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for MessageID {
    type Err = std::convert::Infallible;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(value))
    }
}

// `MessageID` is a newtype over `String`; on the wire it is a plain string.
impl Type for MessageID {
    fn inline(opts: specta::DefOpts, generics: &[specta::DataType]) -> Result<specta::DataType, specta::ExportError> {
        <String as Type>::inline(opts, generics)
    }
}

/// Go `types.MessageServerID`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Type)]
pub struct MessageServerID(pub i64);

/// Go `types.AddressingMode` — how the message addresses the sender.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[specta(rename_all = "lowercase")]
pub enum AddressingMode {
    /// No `addressing_mode` attribute (PN addressing, i.e. the default).
    #[default]
    #[serde(rename = "")]
    Unset,
    /// `pn` — the message is addressed by phone number.
    #[serde(rename = "pn")]
    Pn,
    /// `lid` — the message is addressed by LID.
    #[serde(rename = "lid")]
    Lid,
}

impl AddressingMode {
    pub fn parse(value: &str) -> Self {
        match value {
            "pn" => Self::Pn,
            "lid" => Self::Lid,
            _ => Self::Unset,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unset => "",
            Self::Pn => "pn",
            Self::Lid => "lid",
        }
    }
}

/// Go `types.DeviceSentMeta` — set on a direct message that another one of our
/// own devices sent (the `device_sent_message` wrapper).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct DeviceSentMeta {
    /// The destination user; should match the message's `recipient`.
    pub destination_jid: String,
    pub phash: String,
}

/// Go `types.MsgBotInfo` — the `<bot>` child of a `<message>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct MsgBotInfo {
    /// `edit` attribute (`first`/`inner`/`last`/`full`); empty when absent.
    pub edit_type: String,
    pub edit_target_id: MessageID,
    pub edit_sender_timestamp_ms: i64,
    pub client_thread_id: String,
}

/// Go `types.MsgMetaInfo` — the `<meta>` child of a `<message>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct MsgMetaInfo {
    pub target_id: MessageID,
    pub target_sender: Jid,
    pub target_chat: Jid,
    pub deprecated_lid_session: Option<bool>,
    pub thread_message_id: MessageID,
    pub thread_message_sender_jid: Jid,
}

/// Go `types.MessageSource` — who sent what where. Kept flat inside
/// [`MessageInfo`] (the Go struct embeds it; the Rust wire type does not nest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct MessageSource {
    /// The chat the message arrived in (the group for group chats).
    pub chat: Jid,
    /// The user who sent the message.
    pub sender: Jid,
    /// The sender's alternative address (LID for a PN message and vice versa).
    pub sender_alt: Jid,
    /// The recipient's alternative address, for DMs.
    pub recipient_alt: Jid,
    pub is_from_me: bool,
    /// True for group and broadcast-list chats.
    pub is_group: bool,
    pub addressing_mode: AddressingMode,
}

impl MessageSource {
    /// Go `MessageSource.SourceString` — log-friendly, never prints a body.
    pub fn source_string(&self) -> String {
        if self.sender != self.chat { format!("{} in {}", self.sender, self.chat) } else { self.chat.to_string() }
    }
}

/// Port of `types.MessageInfo` — metadata of one incoming message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct MessageInfo {
    pub chat: Jid,
    pub sender: Jid,
    pub sender_alt: Jid,
    pub recipient_alt: Jid,
    pub is_from_me: bool,
    pub is_group: bool,
    pub addressing_mode: AddressingMode,
    pub id: MessageID,
    pub server_id: MessageServerID,
    /// Go `Type` — `text`, `media`, `peer_msg`, … (the `type` attribute).
    #[serde(rename = "type")]
    pub message_type: String,
    pub push_name: String,
    /// Unix seconds (Go `time.Time`).
    pub timestamp: i64,
    pub category: String,
    pub multicast: bool,
    pub media_type: String,
    /// Go `types.EditAttribute` — `1` message edit, `7` sender revoke, `8`
    /// admin revoke, …
    pub edit: String,
    pub msg_bot_info: MsgBotInfo,
    pub msg_meta_info: MsgMetaInfo,
    pub is_newsletter_status: bool,
    pub device_sent_meta: Option<DeviceSentMeta>,
    pub unavailable_request_id: String,
}

impl MessageInfo {
    /// The message source half of this info.
    pub fn source(&self) -> MessageSource {
        MessageSource {
            chat: self.chat.clone(),
            sender: self.sender.clone(),
            sender_alt: self.sender_alt.clone(),
            recipient_alt: self.recipient_alt.clone(),
            is_from_me: self.is_from_me,
            is_group: self.is_group,
            addressing_mode: self.addressing_mode,
        }
    }

    /// Go `MessageSource.SourceString` — log-friendly, never prints a body.
    pub fn source_string(&self) -> String {
        self.source().source_string()
    }
}

/// A plain-text (`conversation`) message — what a composer sends.
///
/// Go builds this inline (`&waE2E.Message{Conversation: proto.String(text)}`);
/// here it is a helper so `bebok-server` does not need `wa-proto`.
pub fn text_message(text: &str) -> WaMessage {
    WaMessage { conversation: Some(text.to_owned()), ..Default::default() }
}

/// Go `events.Message` — one decrypted message plus the wrapper flags
/// [`Message::unwrap_raw`] derived.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message {
    pub info: MessageInfo,
    /// The message after [`Message::unwrap_raw`] removed the wrappers.
    pub message: Option<WaMessage>,
    /// The message exactly as it arrived (wrappers intact).
    pub raw_message: Option<WaMessage>,
    pub is_ephemeral: bool,
    pub is_view_once: bool,
    pub is_view_once_v2: bool,
    pub is_view_once_v2_extension: bool,
    pub is_document_with_caption: bool,
    pub is_lottie_sticker: bool,
    pub is_bot_invoke: bool,
    pub is_edit: bool,
    /// How many retries it took to decrypt this message (Go `RetryCount`).
    pub retry_count: i32,
}

/// `FutureProofMessage.message` — the boxed inner message of a wrapper.
fn future_proof_inner(wrapper: &Option<Box<wa_proto::wa_web_protobufs_e2e::FutureProofMessage>>) -> Option<WaMessage> {
    wrapper.as_ref().and_then(|w| w.message.as_deref().cloned())
}

impl Message {
    /// Go `events.Message.UnwrapRaw` — peel the wrappers off the raw message.
    ///
    /// The order is part of the protocol (a direct message can be wrapped in
    /// `DeviceSent` *and* `Ephemeral`): DeviceSent → BotInvoke → Ephemeral →
    /// ViewOnce → ViewOnceV2 → ViewOnceV2Extension → LottieSticker →
    /// DocumentWithCaption → Edited. Afterwards the `message_context_info` of
    /// the raw message is back-filled when the unwrapped one has none.
    pub fn unwrap_raw(&mut self) {
        let Some(raw) = self.raw_message.clone() else { return };
        let mut msg = raw.clone();

        if let Some(device_sent) = msg.device_sent_message.as_ref().filter(|sent| sent.message.is_some()) {
            self.info.device_sent_meta = Some(DeviceSentMeta {
                destination_jid: device_sent.destination_jid.clone().unwrap_or_default(),
                phash: device_sent.phash.clone().unwrap_or_default(),
            });
            if let Some(inner) = device_sent.message.as_deref().cloned() {
                msg = inner;
            }
        }
        if let Some(inner) = future_proof_inner(&msg.bot_invoke_message) {
            msg = inner;
            self.is_bot_invoke = true;
        }
        if let Some(inner) = future_proof_inner(&msg.ephemeral_message) {
            msg = inner;
            self.is_ephemeral = true;
        }
        if let Some(inner) = future_proof_inner(&msg.view_once_message) {
            msg = inner;
            self.is_view_once = true;
        }
        if let Some(inner) = future_proof_inner(&msg.view_once_message_v2) {
            msg = inner;
            self.is_view_once = true;
            self.is_view_once_v2 = true;
        }
        if let Some(inner) = future_proof_inner(&msg.view_once_message_v2_extension) {
            msg = inner;
            self.is_view_once = true;
            self.is_view_once_v2 = true;
            self.is_view_once_v2_extension = true;
        }
        if let Some(inner) = future_proof_inner(&msg.lottie_sticker_message) {
            msg = inner;
            self.is_lottie_sticker = true;
        }
        if let Some(inner) = future_proof_inner(&msg.document_with_caption_message) {
            msg = inner;
            self.is_document_with_caption = true;
        }
        if let Some(inner) = future_proof_inner(&msg.edited_message) {
            msg = inner;
            self.is_edit = true;
        }
        if msg.message_context_info.is_none() && raw.message_context_info.is_some() {
            msg.message_context_info = raw.message_context_info.clone();
        }
        self.message = Some(msg);
    }

    /// The plain-text body of the unwrapped message, for the store and the UI.
    /// Only the text-ish fields are mapped here; media captions are handled by
    /// the media path (TODO(Phase 5)).
    pub fn text(&self) -> Option<String> {
        let msg = self.message.as_ref()?;
        if let Some(text) = msg.conversation.as_ref() {
            return Some(text.clone());
        }
        let extended = msg.extended_text_message.as_ref()?;
        extended.text.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_proto::wa_web_protobufs_e2e::{DeviceSentMessage, FutureProofMessage};

    fn conversation(text: &str) -> WaMessage {
        text_message(text)
    }

    fn future_proof(msg: WaMessage) -> Option<Box<FutureProofMessage>> {
        Some(Box::new(FutureProofMessage { message: Some(Box::new(msg)) }))
    }

    fn event(raw: WaMessage) -> Message {
        Message { raw_message: Some(raw), ..Default::default() }
    }

    #[test]
    fn message_id_roundtrip() {
        let id: MessageID = "abc".parse().unwrap();
        assert_eq!(id.as_str(), "abc");
        assert_eq!(id.to_string(), "abc");
    }

    #[test]
    fn message_info_serde_roundtrip() {
        let x = MessageInfo {
            chat: Jid::new("1", "s.whatsapp.net"),
            sender: Jid::new("2", "s.whatsapp.net"),
            sender_alt: Jid::new("3", "lid"),
            id: MessageID::from("id"),
            is_from_me: true,
            timestamp: 12,
            push_name: "Al".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&x).unwrap();
        assert!(json.contains("\"isFromMe\":true"), "{json}");
        assert!(json.contains("\"pushName\":\"Al\""), "{json}");
        let y: MessageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(x, y);
    }

    #[test]
    fn addressing_mode_wire_values() {
        assert_eq!(AddressingMode::parse("pn"), AddressingMode::Pn);
        assert_eq!(AddressingMode::parse("lid"), AddressingMode::Lid);
        assert_eq!(AddressingMode::parse(""), AddressingMode::Unset);
        assert_eq!(AddressingMode::parse("nope"), AddressingMode::Unset);
        assert_eq!(AddressingMode::Lid.as_str(), "lid");
        assert_eq!(serde_json::to_string(&AddressingMode::Pn).unwrap(), "\"pn\"");
        assert_eq!(serde_json::to_string(&AddressingMode::Unset).unwrap(), "\"\"");
    }

    #[test]
    fn unwrap_plain_message() {
        let mut evt = event(conversation("hello"));
        evt.unwrap_raw();
        assert_eq!(evt.text().as_deref(), Some("hello"));
        assert!(!evt.is_ephemeral && !evt.is_edit && !evt.is_view_once);
    }

    #[test]
    fn unwrap_device_sent_sets_meta() {
        let inner = conversation("from my phone");
        let raw = WaMessage {
            device_sent_message: Some(Box::new(DeviceSentMessage {
                destination_jid: Some("48123456789@s.whatsapp.net".into()),
                message: Some(Box::new(inner)),
                phash: Some("phash-1".into()),
            })),
            ..Default::default()
        };
        let mut evt = event(raw);
        evt.unwrap_raw();
        assert_eq!(evt.text().as_deref(), Some("from my phone"));
        let meta = evt.info.device_sent_meta.expect("device sent meta");
        assert_eq!(meta.destination_jid, "48123456789@s.whatsapp.net");
        assert_eq!(meta.phash, "phash-1");
    }

    #[test]
    fn unwrap_nested_device_sent_then_ephemeral() {
        let raw = WaMessage {
            device_sent_message: Some(Box::new(DeviceSentMessage {
                destination_jid: Some("1@s.whatsapp.net".into()),
                message: Some(Box::new(WaMessage {
                    ephemeral_message: future_proof(conversation("wrapped")),
                    ..Default::default()
                })),
                phash: None,
            })),
            ..Default::default()
        };
        let mut evt = event(raw);
        evt.unwrap_raw();
        assert!(evt.is_ephemeral);
        assert!(evt.info.device_sent_meta.is_some());
        assert_eq!(evt.text().as_deref(), Some("wrapped"));
    }

    /// One wrapper case: the raw message, the flag it must set and its name.
    struct WrapperCase {
        raw: WaMessage,
        sets_flag: fn(&Message) -> bool,
        name: &'static str,
    }

    #[test]
    fn unwrap_every_wrapper_flag() {
        let cases = vec![
            WrapperCase {
                raw: WaMessage { ephemeral_message: future_proof(conversation("a")), ..Default::default() },
                sets_flag: |m: &Message| m.is_ephemeral,
                name: "ephemeral",
            },
            WrapperCase {
                raw: WaMessage { view_once_message: future_proof(conversation("b")), ..Default::default() },
                sets_flag: |m: &Message| m.is_view_once && !m.is_view_once_v2,
                name: "view once",
            },
            WrapperCase {
                raw: WaMessage { view_once_message_v2: future_proof(conversation("c")), ..Default::default() },
                sets_flag: |m: &Message| m.is_view_once && m.is_view_once_v2 && !m.is_view_once_v2_extension,
                name: "view once v2",
            },
            WrapperCase {
                raw: WaMessage {
                    view_once_message_v2_extension: future_proof(conversation("d")),
                    ..Default::default()
                },
                sets_flag: |m: &Message| m.is_view_once_v2_extension,
                name: "view once v2 extension",
            },
            WrapperCase {
                raw: WaMessage { lottie_sticker_message: future_proof(conversation("e")), ..Default::default() },
                sets_flag: |m: &Message| m.is_lottie_sticker,
                name: "lottie sticker",
            },
            WrapperCase {
                raw: WaMessage { document_with_caption_message: future_proof(conversation("f")), ..Default::default() },
                sets_flag: |m: &Message| m.is_document_with_caption,
                name: "document with caption",
            },
            WrapperCase {
                raw: WaMessage { edited_message: future_proof(conversation("g")), ..Default::default() },
                sets_flag: |m: &Message| m.is_edit,
                name: "edited",
            },
            WrapperCase {
                raw: WaMessage { bot_invoke_message: future_proof(conversation("h")), ..Default::default() },
                sets_flag: |m: &Message| m.is_bot_invoke,
                name: "bot invoke",
            },
        ];
        for case in cases {
            let mut evt = event(case.raw);
            evt.unwrap_raw();
            assert!((case.sets_flag)(&evt), "{} must set its flag", case.name);
            assert!(evt.text().is_some(), "{} must unwrap to the inner message", case.name);
        }
    }

    #[test]
    fn unwrap_backfills_message_context_info() {
        use wa_proto::wa_web_protobufs_e2e::MessageContextInfo;
        let raw = WaMessage {
            ephemeral_message: future_proof(conversation("ctx")),
            message_context_info: Some(MessageContextInfo::default()),
            ..Default::default()
        };
        let mut evt = event(raw);
        evt.unwrap_raw();
        assert!(evt.message.as_ref().expect("message").message_context_info.is_some());
    }
}
