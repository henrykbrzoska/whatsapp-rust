// Ported from the Go client `send.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Sending messages — the port of `Go send.go`.
//!
//! The node shape is Go's exactly:
//!
//! ```text
//! <message id=… to=… type=text [phash] [edit] [peer_recipient_pn]>
//!   <participants>
//!     <to jid=<device>>
//!       <enc v="2" type="msg|pkmsg">…</enc>
//!     </to>
//!     …
//!   </participants>
//!   <device-identity>…</device-identity>     (when any device needed a pkmsg)
//!   [<meta …/>] [<bot>…</bot>] [<biz>…</biz>]
//!   <enc v="2" type="skmsg">…</enc>          (group send only)
//! </message>
//! ```
//!
//! Two Go behaviours matter for correctness and are kept:
//!
//! * **per-device failures never abort a send.** A device whose pre-key bundle
//!   cannot be fetched or whose session cannot be built is collected into
//!   [`DeviceFailure`] and the message still goes out to everyone else (Go
//!   logs `Failed to encrypt …` and `continue`s).
//! * **every plaintext is padded before encryption** ([`pad_message`]): the
//!   Signal ciphertext of a WhatsApp message carries Go's 1..=15 byte
//!   ISO-7816-4 padding *inside* libsignal's PKCS#7 block, so a receiver
//!   running the Go client can unpad it ([`unpad_message`]).
//!
//! Out of scope for this phase (documented at the call site, never silently
//! dropped): the PN→LID rewrite of the destination (`GetUserInfo`), the
//! `tctoken`/`cstoken` privacy tokens (`ensureTCToken`) and the message-secret
//! / reporting-token machinery of bots and newsletters.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use prost::Message as ProstMessage;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;

use crate::binary::node::{AttrValue, Attrs, Node};
use crate::crypto::signal::address::{SenderKeyName, SignalAddress};
use crate::crypto::signal::groups::{GroupCipher, GroupSessionBuilder};
use crate::crypto::signal::session::{CiphertextMessage, SessionBuilder, SessionCipher};
use crate::crypto::signal::store::{DeviceStore, PreKeyBundle};
use crate::types::jid::{
    BOT_SERVER, DEFAULT_USER_SERVER, GROUP_SERVER, HIDDEN_USER_SERVER, HOSTED_LID_SERVER, HOSTED_SERVER, Jid,
    NEWSLETTER_SERVER,
};
use crate::types::message::{MessageID, MessageInfo, MessageServerID, MsgMetaInfo, WaMessage};
use wa_proto::wa_common::MessageKey;
use wa_proto::wa_web_protobufs_e2e as wa_e2e;

use super::ack::attr_text;
use super::session::{InfoQuery, IqKind};
use super::{Client, ClientError};

/// Go `WebMessageIDPrefix` — every message ID this client generates starts
/// with it.
pub const WEB_MESSAGE_ID_PREFIX: &str = "3EB0";

/// The `v` attribute of every `<enc>` node (Go `encryptMessageForDevice`).
/// The server answers `<error code="479">` (`smax-invalid`) without it.
pub const ENC_VERSION: &str = "2";

/// Go `RemoveReactionText` — an empty reaction text removes the reaction.
pub const REMOVE_REACTION_TEXT: &str = "";

/// Go `EditWindow` — how long after sending a message may still be edited.
pub const EDIT_WINDOW: Duration = Duration::from_secs(20 * 60);

/// Disappearing-message timers (Go's `DisappearingTimer*` constants), in
/// seconds — `0` = off.
pub const DISAPPEARING_TIMER_OFF: u32 = 0;
/// 24 hours.
pub const DISAPPEARING_TIMER_24_HOURS: u32 = 24 * 60 * 60;
/// 7 days.
pub const DISAPPEARING_TIMER_7_DAYS: u32 = 7 * 24 * 60 * 60;
/// 90 days.
pub const DISAPPEARING_TIMER_90_DAYS: u32 = 90 * 24 * 60 * 60;

/// Go `types.EditAttribute` values that go into `<message edit=…>`.
pub const EDIT_ATTRIBUTE_MESSAGE_EDIT: &str = "1";
/// A pinned message.
pub const EDIT_ATTRIBUTE_PIN_IN_CHAT: &str = "2";
/// An admin edit in a newsletter.
pub const EDIT_ATTRIBUTE_ADMIN_EDIT: &str = "3";
/// The sender revoked the message.
pub const EDIT_ATTRIBUTE_SENDER_REVOKE: &str = "7";
/// A group admin revoked someone else's message.
pub const EDIT_ATTRIBUTE_ADMIN_REVOKE: &str = "8";

/// How long the pre-key bundle lookup may take before the affected devices are
/// given up on (Go uses the `sendIQ` default; a send must not hang on it).
const PRE_KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Go `MessageDebugTimings`, in milliseconds (the UI shows them in its debug
/// drawer).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct SendTimings {
    /// Time spent waiting in the send queue.
    pub queue: u64,
    /// Time spent marshalling the protobuf payloads.
    pub marshal: u64,
    /// Time spent resolving the device list.
    pub get_devices: u64,
    /// Time spent encrypting for the devices.
    pub encrypt: u64,
    /// Time spent writing the node to the socket.
    pub send: u64,
    /// Time spent waiting for the server's acknowledgement.
    pub response: u64,
    /// Time spent retrying the frame after a reconnect.
    pub retry: u64,
}

/// Go `SendResponse` — what [`Client::send_message`] returns once the server
/// acknowledged the message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct SendResponse {
    /// The message ID used for the send (generated when the caller gave none).
    pub id: MessageID,
    /// The server timestamp, in unix seconds (0 when the server sent none).
    pub timestamp: i64,
    /// The server-specified ID — only newsletters send one.
    pub server_id: MessageServerID,
    /// The identity the message was sent with (PN or LID).
    pub sender: Jid,
    /// The chat the message actually went to.
    pub chat: Jid,
    /// `true` when the response was lost to a timeout — the message still went
    /// out (Go's `ErrMessageTimedOut` debug path).
    pub debug_timeout: bool,
    /// Devices the message could not be encrypted for (Go only logs these).
    /// A non-empty list means those devices will not see the message, which
    /// is worth surfacing: it is the difference between "sent" and "sent to
    /// one of five devices".
    pub skipped_devices: Vec<SkippedDevice>,
    /// Per-step timings, in milliseconds.
    pub timings: SendTimings,
}

/// One device [`Client::send_message`] could not encrypt for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct SkippedDevice {
    /// The device that was skipped.
    pub device: String,
    /// Why it was skipped (a Signal session or pre-key failure).
    pub error: String,
}

/// Go `SendRequestExtra` — the optional parameters of a send.
#[derive(Debug, Clone, Default)]
pub struct SendRequestExtra {
    /// The message ID to use; generated when absent.
    pub id: Option<MessageID>,
    /// JID of the inline bot to invoke.
    pub inline_bot_jid: Option<Jid>,
    /// Send as a peer (own-device) message instead of a chat message.
    pub peer: bool,
    /// How long to wait for the server response;
    /// [`crate::client::ClientConfig::request_timeout`] when `None`.
    pub timeout: Option<Duration>,
    /// The upload handle of the attached media (newsletter sends only).
    pub media_handle: String,
    /// Extra `<meta>` attributes (threads, deprecated LID sessions).
    pub meta: Option<MsgMetaInfo>,
    /// Extra top-level children, appended verbatim.
    pub additional_nodes: Option<Vec<Node>>,
    /// Check the group's send permissions before sending.
    pub check_permissions: bool,
    /// **Not in Go**: an explicit device list for the fan-out. `None` = ask the
    /// server (`usync`, like Go's `GetUserDevices`). Tests and the retry path
    /// use it to skip the round-trip.
    pub devices: Option<Vec<Jid>>,
}

/// One device that could not be encrypted for. Collected, never fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFailure {
    /// The device that was skipped.
    pub device: Jid,
    /// Why it was skipped.
    pub error: String,
}

/// A `<message>` node plus everything the caller needs to log/diagnose it.
#[derive(Debug, Clone)]
pub struct PreparedMessage {
    /// The `<message>` node.
    pub node: Node,
    /// Every device the message was addressed to (the `phash` input).
    pub devices: Vec<Jid>,
    /// `participantListHashV2(devices)`. Go puts this on the node for group
    /// sends only; a DM just compares it with the hash the server acks
    /// (`send.go:895` computes it without touching `node.Attrs`). Sending it
    /// on a DM makes the server reject the stanza with `smax-invalid` (479).
    pub phash: Option<String>,
    /// Devices that were skipped because encryption failed.
    pub failures: Vec<DeviceFailure>,
    /// At least one device received a `pkmsg` → include `<device-identity>`.
    pub include_device_identity: bool,
}

/// The result of handing a `<message>` to the socket.
#[derive(Debug, Clone, Default)]
pub struct SendOutcome {
    /// The participant-list hash (`phash`) that went into the node.
    pub phash: Option<String>,
    /// The node that was sent, kept for `retry_frame` after a reconnect.
    pub node: Node,
    /// The marshalled node, kept so a reconnect can re-send it byte for byte.
    pub data: Vec<u8>,
    /// The devices the node was addressed to.
    pub devices: Vec<Jid>,
    /// The devices that were skipped.
    pub failures: Vec<DeviceFailure>,
    /// Per-step timings.
    pub timings: SendTimings,
}

/// Go `nodeExtraParams` — the optional children/attributes of a `<message>`.
#[derive(Debug, Clone, Default)]
pub struct NodeExtraParams {
    /// `<bot>` child (inline bots).
    pub bot_node: Option<Node>,
    /// `<meta>` child (threads, LID sessions).
    pub meta_node: Option<Node>,
    /// Extra children appended verbatim.
    pub additional_nodes: Option<Vec<Node>>,
    /// `addressing_mode` attribute (`lid`/`pn`), empty = not set.
    pub addressing_mode: String,
    /// `peer_recipient_pn` attribute (LID-addressed chats).
    pub peer_recipient_pn: Option<Jid>,
}

// ---------------------------------------------------------------------------
// Pure builders and lookups
// ---------------------------------------------------------------------------

/// Go `getTypeFromMessage` — the `<message type=…>` attribute.
pub fn get_type_from_message(message: &WaMessage) -> &'static str {
    if let Some(inner) = wrapped_messages(message).into_iter().next() {
        return get_type_from_message(inner);
    }
    if message.reaction_message.is_some() || message.enc_reaction_message.is_some() {
        return "reaction";
    }
    if message.poll_creation_message.is_some() || message.poll_update_message.is_some() {
        return "poll";
    }
    if !get_media_type_from_message(message).is_empty() {
        return "media";
    }
    "text"
}

/// Go `getMediaTypeFromMessage` — the `mediatype` attribute, `""` when the
/// message is not media.
pub fn get_media_type_from_message(message: &WaMessage) -> &'static str {
    if let Some(inner) = wrapped_messages(message).into_iter().next() {
        return get_media_type_from_message(inner);
    }
    if let Some(extended) = &message.extended_text_message
        && extended.title.is_some()
    {
        return "url";
    }
    if message.image_message.is_some() {
        return "image";
    }
    if message.sticker_message.is_some() {
        return "sticker";
    }
    if message.document_message.is_some() {
        return "document";
    }
    if let Some(audio) = &message.audio_message {
        return if audio.ptt.unwrap_or(false) { "ptt" } else { "audio" };
    }
    if let Some(video) = &message.video_message {
        return if video.gif_playback.unwrap_or(false) { "gif" } else { "video" };
    }
    if message.contact_message.is_some() {
        return "vcard";
    }
    if message.contacts_array_message.is_some() {
        return "contact_array";
    }
    if message.list_message.is_some() {
        return "list";
    }
    if message.list_response_message.is_some() {
        return "list_response";
    }
    if message.buttons_response_message.is_some() {
        return "buttons_response";
    }
    if message.order_message.is_some() {
        return "order";
    }
    if message.product_message.is_some() {
        return "product";
    }
    if message.interactive_response_message.is_some() {
        return "native_flow_response";
    }
    ""
}

/// The messages a wrapper (`ViewOnce`, `Ephemeral`, …) carries — Go recurses
/// through each of them before looking at the payload itself.
fn wrapped_messages(message: &WaMessage) -> Vec<&WaMessage> {
    [
        message.view_once_message.as_ref().and_then(|m| m.message.as_deref()),
        message.view_once_message_v2.as_ref().and_then(|m| m.message.as_deref()),
        message.view_once_message_v2_extension.as_ref().and_then(|m| m.message.as_deref()),
        message.lottie_sticker_message.as_ref().and_then(|m| m.message.as_deref()),
        message.ephemeral_message.as_ref().and_then(|m| m.message.as_deref()),
        message.document_with_caption_message.as_ref().and_then(|m| m.message.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Go `getButtonTypeFromMessage` — the `<biz>` child tag, `""` for none.
pub fn get_button_type_from_message(message: &WaMessage) -> &'static str {
    let wrapped = [
        message.view_once_message.as_ref().and_then(|m| m.message.as_deref()),
        message.view_once_message_v2.as_ref().and_then(|m| m.message.as_deref()),
        message.ephemeral_message.as_ref().and_then(|m| m.message.as_deref()),
    ]
    .into_iter()
    .flatten()
    .next();
    if let Some(inner) = wrapped {
        return get_button_type_from_message(inner);
    }
    if message.buttons_message.is_some() {
        return "buttons";
    }
    if message.buttons_response_message.is_some() {
        return "buttons_response";
    }
    if message.list_message.is_some() {
        return "list";
    }
    if message.list_response_message.is_some() {
        return "list_response";
    }
    if message.interactive_response_message.is_some() {
        return "interactive_response";
    }
    ""
}

/// Go `getEditAttribute` — `None` when the message carries no edit marker.
pub fn get_edit_attribute(message: &WaMessage) -> Option<&'static str> {
    if let Some(inner) = wrapped_messages(message).into_iter().next() {
        return get_edit_attribute(inner);
    }
    if let Some(protocol) = &message.protocol_message
        && protocol.key.is_some()
    {
        match protocol.r#type() {
            wa_e2e::protocol_message::Type::Revoke => {
                let from_me = protocol.key.as_ref().and_then(|key| key.from_me).unwrap_or(false);
                return Some(if from_me { EDIT_ATTRIBUTE_SENDER_REVOKE } else { EDIT_ATTRIBUTE_ADMIN_REVOKE });
            }
            wa_e2e::protocol_message::Type::MessageEdit if protocol.edited_message.is_some() => {
                return Some(EDIT_ATTRIBUTE_MESSAGE_EDIT);
            }
            _ => {}
        }
    }
    if let Some(reaction) = &message.reaction_message
        && reaction.text.as_deref() == Some(REMOVE_REACTION_TEXT)
    {
        return Some(EDIT_ATTRIBUTE_SENDER_REVOKE);
    }
    if let Some(keep) = &message.keep_in_chat_message
        && keep.key.as_ref().and_then(|key| key.from_me).unwrap_or(false)
        && keep.keep_type() == wa_e2e::KeepType::UndoKeepForAll
    {
        return Some(EDIT_ATTRIBUTE_SENDER_REVOKE);
    }
    if message.pin_in_chat_message.is_some() {
        return Some(EDIT_ATTRIBUTE_PIN_IN_CHAT);
    }
    None
}

/// Go `padMessage` — 1..=15 identical bytes of ISO-7816-4 padding.
pub fn pad_message(plaintext: &[u8]) -> Vec<u8> {
    let mut pad = [0u8; 1];
    rand::thread_rng().fill_bytes(&mut pad);
    pad[0] &= 0xf;
    if pad[0] == 0 {
        pad[0] = 0xf;
    }
    let mut out = Vec::with_capacity(plaintext.len() + usize::from(pad[0]));
    out.extend_from_slice(plaintext);
    out.extend(std::iter::repeat_n(pad[0], usize::from(pad[0])));
    out
}

/// Go `unpadMessage` — the receiving half of [`pad_message`]. Version 3
/// messages are not padded at all.
pub fn unpad_message(plaintext: &[u8], version: i32) -> Result<Vec<u8>, ClientError> {
    if version == 3 {
        return Ok(plaintext.to_vec());
    }
    let Some(&last) = plaintext.last() else {
        return Err(ClientError::Signal("plaintext is empty".to_owned()));
    };
    if last == 0 || plaintext.len() < usize::from(last) {
        return Err(ClientError::Signal("plaintext doesn't have expected padding".to_owned()));
    }
    let expected = vec![last; usize::from(last)];
    if !plaintext.ends_with(&expected) {
        return Err(ClientError::Signal("plaintext doesn't have expected padding".to_owned()));
    }
    Ok(plaintext[..plaintext.len() - usize::from(last)].to_vec())
}

/// Go `participantListHashV2` — `2:<base64raw(sha256(sorted AD JIDs)[..6])>`.
pub fn participant_list_hash_v2(participants: &[Jid]) -> String {
    let mut ad_strings: Vec<String> = participants.iter().map(Jid::ad_string).collect();
    ad_strings.sort();
    let hash = Sha256::digest(ad_strings.concat().as_bytes());
    format!("2:{}", base64_raw_std(&hash[..6]))
}

/// `base64.RawStdEncoding` (no padding) — Go's encoder for the `phash`.
fn base64_raw_std(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(2) * 3);
    for chunk in bytes.chunks(3) {
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[usize::from(chunk[0] >> 2)] as char);
        out.push(ALPHABET[usize::from(((chunk[0] & 0x03) << 4) | (second >> 4))] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[usize::from(third & 0x3f)] as char);
        }
    }
    out
}

/// Go `marshalMessage` — the payload for the recipients plus, for 1:1 chats,
/// the `DeviceSentMessage` payload for our own other devices.
pub fn marshal_message(to: &Jid, message: &WaMessage) -> Result<(Vec<u8>, Option<Vec<u8>>), ClientError> {
    let plaintext = message.encode_to_vec();
    if to.server == GROUP_SERVER || to.server == NEWSLETTER_SERVER {
        return Ok((plaintext, None));
    }
    let dsm = WaMessage {
        device_sent_message: Some(Box::new(wa_e2e::DeviceSentMessage {
            destination_jid: Some(to.to_string()),
            message: Some(Box::new(message.clone())),
            phash: None,
        })),
        message_context_info: message.message_context_info.clone(),
        ..Default::default()
    };
    Ok((plaintext, Some(dsm.encode_to_vec())))
}

/// Go `getMessageContent` — the base child of a `<message>` node plus the
/// per-feature nodes Go appends.
pub(crate) fn get_message_content(
    base_node: Node,
    message: &WaMessage,
    attrs: &Attrs,
    include_identity: bool,
    extra: &NodeExtraParams,
    device_identity: Option<Node>,
) -> Vec<Node> {
    let mut content = vec![base_node];
    if include_identity && let Some(identity) = device_identity {
        content.push(identity);
    }
    if attrs.get("type").and_then(AttrValue::as_str) == Some("poll") {
        let poll_type = if message.poll_update_message.is_some() { "vote" } else { "creation" };
        content.push(Node::new("meta").attr("polltype", poll_type));
    }
    if let Some(bot) = &extra.bot_node {
        content.push(bot.clone());
    }
    if let Some(meta) = &extra.meta_node {
        content.push(meta.clone());
    }
    if let Some(additional) = &extra.additional_nodes {
        content.extend(additional.iter().cloned());
    }
    let button_type = get_button_type_from_message(message);
    if !button_type.is_empty() {
        content.push(Node::new("biz").with_content(vec![Node::new(button_type)]));
    }
    content
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn attr_jid(node: &Node, key: &str) -> Option<Jid> {
    node.attrs.get(key).and_then(|value| value.as_jid().cloned())
}

fn attr_int(node: &Node, key: &str) -> Option<i64> {
    node.attrs.get(key).and_then(|value| match value {
        AttrValue::Int(number) => Some(*number),
        AttrValue::Uint(number) => i64::try_from(*number).ok(),
        AttrValue::Text(text) => text.parse().ok(),
        _ => None,
    })
}

impl Client {
    // -----------------------------------------------------------------------
    // Own identity
    // -----------------------------------------------------------------------

    /// Go `getOwnID` — our AD-JID, empty when not paired.
    pub async fn own_id(&self) -> Jid {
        self.device.lock().await.id.clone().unwrap_or_default()
    }

    /// Go `getOwnLID` — our LID, empty when the server never sent one.
    pub async fn own_lid(&self) -> Jid {
        self.device.lock().await.lid.clone()
    }

    /// Go `GenerateMessageID` —
    /// `"3EB0" + upper(hex(sha256(unix || user || "@c.us" || 16 random)[..9]))`.
    pub async fn generate_message_id(&self) -> MessageID {
        let own_id = self.own_id().await;
        let mut data = Vec::with_capacity(8 + own_id.user.len() + 5 + 16);
        data.extend_from_slice(&(now_unix() as u64).to_be_bytes());
        if !own_id.is_empty() {
            data.extend_from_slice(own_id.user.as_bytes());
            data.extend_from_slice(b"@c.us");
        }
        let mut random = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut random);
        data.extend_from_slice(&random);
        let hash = Sha256::digest(&data);
        MessageID::new(format!("{WEB_MESSAGE_ID_PREFIX}{}", hex::encode_upper(&hash[..9])))
    }

    /// Go `makeDeviceIdentityNode` — our ADV account, sent to the devices that
    /// received a `pkmsg`.
    pub async fn make_device_identity_node(&self) -> Option<Node> {
        let account = self.device.lock().await.account.clone()?;
        Some(Node::new("device-identity").with_content(account.encode_to_vec()))
    }

    /// A `DeviceStore` snapshot — every Signal call goes through it.
    pub(crate) async fn device_store(&self) -> DeviceStore {
        let device = self.device.lock().await.clone();
        DeviceStore::new(device)
    }

    // -----------------------------------------------------------------------
    // Message-key builders (revokes, reactions, edits, requests)
    // -----------------------------------------------------------------------

    /// Go `BuildMessageKey` — the `MessageKey` a revoke/edit/reaction refers
    /// to. An empty `sender` (or ourselves) means "our own message".
    pub async fn build_message_key(&self, chat: &Jid, sender: &Jid, id: &MessageID) -> MessageKey {
        let own_id = self.own_id().await;
        let own_lid = self.own_lid().await;
        let mut key = MessageKey {
            from_me: Some(true),
            id: Some(id.as_str().to_owned()),
            remote_jid: Some(chat.to_string()),
            participant: None,
        };
        let is_own = sender.is_empty()
            || (!own_id.is_empty() && sender.user == own_id.user)
            || (!own_lid.is_empty() && sender.user == own_lid.user);
        if !is_own {
            key.from_me = Some(false);
            if !matches!(chat.server.as_str(), DEFAULT_USER_SERVER | HIDDEN_USER_SERVER) {
                key.participant = Some(sender.to_non_ad().to_string());
            }
        }
        key
    }

    /// Go `BuildRevoke` — a revoke (`protocolMessage type=REVOKE`) message.
    pub async fn build_revoke(&self, chat: &Jid, sender: &Jid, id: &MessageID) -> WaMessage {
        WaMessage {
            protocol_message: Some(Box::new(wa_e2e::ProtocolMessage {
                r#type: Some(wa_e2e::protocol_message::Type::Revoke as i32),
                key: Some(self.build_message_key(chat, sender, id).await),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    /// Go `RevokeMessage` — build a revoke and send it.
    pub async fn revoke_message(&self, chat: &Jid, id: &MessageID) -> Result<SendResponse, ClientError> {
        let message = self.build_revoke(chat, &Jid::default(), id).await;
        self.send_message(chat, &message, SendRequestExtra::default()).await
    }

    /// Go `BuildReaction` — a reaction to `id`; [`REMOVE_REACTION_TEXT`]
    /// removes it.
    pub async fn build_reaction(&self, chat: &Jid, sender: &Jid, id: &MessageID, reaction: &str) -> WaMessage {
        WaMessage {
            reaction_message: Some(wa_e2e::ReactionMessage {
                key: Some(self.build_message_key(chat, sender, id).await),
                text: Some(reaction.to_owned()),
                grouping_key: None,
                sender_timestamp_ms: Some(now_millis()),
            }),
            ..Default::default()
        }
    }

    /// Go `BuildEdit` — replace the content of one of our messages.
    pub async fn build_edit(&self, chat: &Jid, id: &MessageID, new_content: WaMessage) -> WaMessage {
        WaMessage {
            edited_message: Some(Box::new(wa_e2e::FutureProofMessage {
                message: Some(Box::new(WaMessage {
                    protocol_message: Some(Box::new(wa_e2e::ProtocolMessage {
                        key: Some(self.build_message_key(chat, &Jid::default(), id).await),
                        r#type: Some(wa_e2e::protocol_message::Type::MessageEdit as i32),
                        edited_message: Some(Box::new(new_content)),
                        timestamp_ms: Some(now_millis()),
                        ..Default::default()
                    })),
                    ..Default::default()
                })),
            })),
            ..Default::default()
        }
    }

    /// Go `BuildUnavailableMessageRequest` — ask the primary phone for a copy
    /// of a message we could not decrypt (send it with
    /// [`Client::send_peer_message`]).
    pub async fn build_unavailable_message_request(&self, chat: &Jid, sender: &Jid, id: &str) -> WaMessage {
        WaMessage {
            protocol_message: Some(Box::new(wa_e2e::ProtocolMessage {
                r#type: Some(wa_e2e::protocol_message::Type::PeerDataOperationRequestMessage as i32),
                peer_data_operation_request_message: Some(wa_e2e::PeerDataOperationRequestMessage {
                    peer_data_operation_request_type: Some(
                        wa_e2e::PeerDataOperationRequestType::PlaceholderMessageResend as i32,
                    ),
                    placeholder_message_resend_request: vec![
                        wa_e2e::peer_data_operation_request_message::PlaceholderMessageResendRequest {
                            message_key: Some(self.build_message_key(chat, sender, &MessageID::new(id)).await),
                        },
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    /// Go `BuildHistorySyncRequest` — ask the primary phone for the `count`
    /// messages before `last_known`.
    pub async fn build_history_sync_request(&self, last_known: &MessageInfo, count: i32) -> WaMessage {
        WaMessage {
            protocol_message: Some(Box::new(wa_e2e::ProtocolMessage {
                r#type: Some(wa_e2e::protocol_message::Type::PeerDataOperationRequestMessage as i32),
                peer_data_operation_request_message: Some(wa_e2e::PeerDataOperationRequestMessage {
                    peer_data_operation_request_type: Some(
                        wa_e2e::PeerDataOperationRequestType::HistorySyncOnDemand as i32,
                    ),
                    history_sync_on_demand_request: Some(
                        wa_e2e::peer_data_operation_request_message::HistorySyncOnDemandRequest {
                            chat_jid: Some(last_known.chat.to_string()),
                            oldest_msg_id: Some(last_known.id.as_str().to_owned()),
                            oldest_msg_from_me: Some(last_known.is_from_me),
                            on_demand_msg_count: Some(count),
                            // Despite the field name this carries *seconds* (Go does the same).
                            oldest_msg_timestamp_ms: Some(last_known.timestamp),
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    /// Go `requestAppStateKeys` — ask the primary phone for the app-state
    /// sync keys a patch references but this device does not have.
    ///
    /// Without those keys every mutation of every collection stays unapplied
    /// (`AppStateSkip::MissingKeys`), which is what an address book that never
    /// fills looks like. The answer arrives as an `AppStateSyncKeyShare`
    /// protocol message, which [`Client::handle_app_state_sync_key_share`]
    /// persists.
    pub fn build_app_state_key_request(key_ids: &[Vec<u8>]) -> WaMessage {
        WaMessage {
            protocol_message: Some(Box::new(wa_e2e::ProtocolMessage {
                r#type: Some(wa_e2e::protocol_message::Type::AppStateSyncKeyRequest as i32),
                app_state_sync_key_request: Some(wa_e2e::AppStateSyncKeyRequest {
                    key_i_ds: key_ids.iter().map(|id| wa_e2e::AppStateSyncKeyId { key_id: Some(id.clone()) }).collect(),
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    /// Send [`Client::build_app_state_key_request`] to our own devices.
    /// Returns the ID of the request, or `None` when there is nothing to ask.
    pub async fn request_app_state_keys(&self, key_ids: &[Vec<u8>]) -> Result<Option<MessageID>, ClientError> {
        if key_ids.is_empty() {
            return Ok(None);
        }
        let own = self.own_id().await;
        if own.is_empty() {
            return Err(ClientError::NotLoggedIn);
        }
        let message = Self::build_app_state_key_request(key_ids);
        let response = self
            .send_message(&own.to_non_ad(), &message, SendRequestExtra { peer: true, ..Default::default() })
            .await?;
        Ok(Some(response.id))
    }

    /// Go `SetDisappearingTimer` — set/clear the disappearing timer of a chat.
    ///
    /// 1:1 chats take an `EPHEMERAL_SETTING` protocol message, groups a
    /// `<iq xmlns="w:g2"><ephemeral expiration=…/></iq>` and newsletters a
    /// `<iq xmlns="w:mex">` mutation (not ported — `TODO(Phase 5)`).
    pub async fn set_disappearing_timer(
        &self,
        chat: &Jid,
        timer_seconds: u32,
        setting_ts: i64,
    ) -> Result<Option<SendResponse>, ClientError> {
        let setting_ts = if setting_ts == 0 { now_unix() } else { setting_ts };
        match chat.server.as_str() {
            DEFAULT_USER_SERVER | HIDDEN_USER_SERVER => {
                let message = WaMessage {
                    protocol_message: Some(Box::new(wa_e2e::ProtocolMessage {
                        r#type: Some(wa_e2e::protocol_message::Type::EphemeralSetting as i32),
                        ephemeral_expiration: Some(timer_seconds),
                        ephemeral_setting_timestamp: Some(setting_ts),
                        ..Default::default()
                    })),
                    ..Default::default()
                };
                Ok(Some(self.send_message(chat, &message, SendRequestExtra::default()).await?))
            }
            GROUP_SERVER => {
                let child = if timer_seconds == DISAPPEARING_TIMER_OFF {
                    Node::new("not_ephemeral")
                } else {
                    Node::new("ephemeral").attr("expiration", i64::from(timer_seconds))
                };
                self.send_iq(InfoQuery {
                    namespace: "w:g2".to_owned(),
                    kind: IqKind::Set,
                    to: Some(chat.clone()),
                    content: vec![child],
                    ..Default::default()
                })
                .await?;
                Ok(None)
            }
            other => Err(ClientError::UnknownServer(other.to_owned())),
        }
    }
}

impl Client {
    // -----------------------------------------------------------------------
    // The send entry point
    // -----------------------------------------------------------------------

    /// Go `SendMessage` — send `message` to `to` and wait for the server's
    /// acknowledgement.
    ///
    /// Dispatch mirrors Go: a peer message (`extra.peer`), a newsletter, a
    /// group (fan-out over the participants plus the `skmsg`) or a 1:1 chat
    /// (fan-out over `to` and our own devices).
    pub async fn send_message(
        &self,
        to: &Jid,
        message: &WaMessage,
        mut extra: SendRequestExtra,
    ) -> Result<SendResponse, ClientError> {
        let timings = SendTimings { queue: millis(Instant::now().elapsed()), ..Default::default() };
        let own_id = self.own_id().await;
        if !self.is_logged_in() || own_id.is_empty() {
            return Err(ClientError::NotLoggedIn);
        }
        if to.device > 0 && !extra.peer {
            return Err(ClientError::RecipientAdjId);
        }

        let timeout = extra.timeout.unwrap_or(self.config.request_timeout);
        let mut id = match extra.id.clone() {
            Some(id) if !id.is_empty() => id,
            _ => self.generate_message_id().await,
        };
        extra.id = Some(id.clone());
        let mut id_adjusted_from_server_id = false;

        // Go: an edit/revoke of a newsletter post reuses the server-assigned
        // ID of the post being edited.
        if to.is_newsletter()
            && let Some(protocol) = &message.protocol_message
            && let Some(key) = &protocol.key
            && matches!(
                protocol.r#type(),
                wa_e2e::protocol_message::Type::MessageEdit | wa_e2e::protocol_message::Type::Revoke
            )
            && let Some(server_id) = key.id.as_ref().filter(|id| !id.is_empty())
        {
            id = MessageID::new(server_id.clone());
            extra.id = Some(id.clone());
            id_adjusted_from_server_id = true;
        }

        // Every send is registered for a response before the node goes out: the
        // server may answer before `send_node` returns.
        let waiter = self.register_waiter(id.as_str()).await;
        let dispatch = self.dispatch_send(to, &id, message, &extra).await;
        let mut outcome = match dispatch {
            Ok(outcome) => outcome,
            Err(err) => {
                self.cancel_waiter(id.as_str()).await;
                return Err(err);
            }
        };
        outcome.timings = timings;
        if !extra.peer {
            self.add_recent_message(to, &id, message).await;
        }

        let response = match timeout {
            Duration::ZERO => waiter.await,
            timeout => match tokio::time::timeout(timeout, waiter).await {
                Ok(response) => response,
                Err(_) => return Err(ClientError::MessageTimedOut),
            },
        };
        let response = match response {
            Ok(node) => node,
            Err(_) => return Err(ClientError::MessageTimedOut),
        };
        let retry_started = Instant::now();
        let response = if crate::client::is_disconnect_node(&response) {
            self.retry_frame("message send", id.as_str(), &outcome.data, &outcome.node, timeout).await?
        } else {
            response
        };
        outcome.timings.retry = millis(retry_started.elapsed());

        let mut result = SendResponse {
            id: id.clone(),
            chat: to.clone(),
            sender: own_id,
            timings: outcome.timings,
            ..Default::default()
        };
        if attr_text(&response, "phash").as_deref().is_some_and(|phash| Some(phash) != outcome.phash.as_deref()) {
            tracing::warn!("server returned a different participant hash ({:?}) for {id}", outcome.phash);
        }
        if let Some(server_id) = attr_text(&response, "server_id") {
            if id_adjusted_from_server_id {
                tracing::warn!("newsletter send {id} still returned a server ID ({server_id})");
            }
            result.server_id = MessageServerID(server_id.parse().unwrap_or_default());
        }
        // Go `ag.UnixTime("t")` — the server sends `t` in seconds already.
        result.timestamp = attr_int(&response, "t").unwrap_or_default();
        if let Some(error) = attr_int(&response, "error") {
            return Err(ClientError::ServerReturnedError(i32::try_from(error).unwrap_or_default()));
        }
        if !outcome.failures.is_empty() {
            tracing::warn!("{} devices did not receive {id}: {:?}", outcome.failures.len(), outcome.failures);
            result.skipped_devices = outcome
                .failures
                .iter()
                .map(|f| SkippedDevice { device: f.device.to_string(), error: f.error.clone() })
                .collect();
        }
        Ok(result)
    }

    /// Go `prepareMessageNode` + the `sendDM`/`sendGroup`/`sendPeerMessage`/
    /// `sendNewsletter` dispatch.
    async fn dispatch_send(
        &self,
        to: &Jid,
        id: &MessageID,
        message: &WaMessage,
        extra: &SendRequestExtra,
    ) -> Result<SendOutcome, ClientError> {
        if extra.peer {
            return self.send_peer_message(to, id, message, extra).await;
        }
        if to.is_newsletter() {
            return self.send_newsletter(to, id, Some(message), extra).await;
        }
        if to.is_group() {
            let participants = self.get_group_participants(to).await?;
            return self.send_group(to, id, message, &participants, extra).await;
        }
        self.send_dm(to, id, message, extra).await
    }

    /// Go `sendDM` — encrypt the payload for `to` and our own devices and send
    /// it.
    pub async fn send_dm(
        &self,
        to: &Jid,
        id: &MessageID,
        message: &WaMessage,
        extra: &SendRequestExtra,
    ) -> Result<SendOutcome, ClientError> {
        let own_id = self.own_id().await;
        let mut participants = vec![to.to_non_ad(), own_id.to_non_ad()];
        if let Some(devices) = &extra.devices {
            participants = devices.clone();
        }
        let prepared = self.prepare_message_node(to, id, message, &participants, extra).await?;
        // Go `sendDM`: the hash is computed and compared against the server's
        // ack, but it is *not* an attribute of a DM stanza.
        self.finish_send(prepared).await
    }

    /// Go `sendGroup` — one `skmsg` for the chat plus a `pkmsg`/`msg` per
    /// participant device.
    pub async fn send_group(
        &self,
        to: &Jid,
        id: &MessageID,
        message: &WaMessage,
        participants: &[Jid],
        extra: &SendRequestExtra,
    ) -> Result<SendOutcome, ClientError> {
        let own_id = self.own_id().await;
        let own_lid = self.own_lid().await;
        let sender = if own_lid.is_empty() { own_id } else { own_lid };
        let skd_name = SenderKeyName::new(to.to_string(), SignalAddress::from_jid(&sender));
        let store = self.device_store().await;

        let builder = GroupSessionBuilder::new(&store);
        let sender_key_message = builder.create(&skd_name).await.map_err(|err| ClientError::Signal(err.to_string()))?;
        let skd_message = WaMessage {
            sender_key_distribution_message: Some(wa_e2e::SenderKeyDistributionMessage {
                group_id: Some(to.to_string()),
                axolotl_sender_key_distribution_message: Some(sender_key_message.serialized.clone()),
            }),
            ..Default::default()
        };
        let skd_plaintext = skd_message.encode_to_vec();

        let group_cipher = GroupCipher::new(&store, skd_name);
        let encrypted = group_cipher
            .encrypt(&pad_message(&message.encode_to_vec()))
            .await
            .map_err(|err| ClientError::Signal(err.to_string()))?;

        let mut prepared =
            self.prepare_message_node_with(to, id, message, participants, Some(&skd_plaintext), extra).await?;
        let mut skmsg_attrs = Attrs::new();
        skmsg_attrs.insert("v".to_owned(), AttrValue::Text(ENC_VERSION.to_owned()));
        skmsg_attrs.insert("type".to_owned(), AttrValue::Text("skmsg".to_owned()));
        let mut children = prepared.node.children().to_vec();
        children.push(Node::new("enc").with_attrs(skmsg_attrs).with_content(encrypted.serialized.clone()));
        prepared.node = prepared.node.with_content(children);
        let outcome = self.finish_send(prepared).await?;
        Ok(outcome)
    }

    /// Go `sendPeerMessage` — a message to our own other devices, used for
    /// peer-data requests (`history sync on demand`, `unavailable message`).
    pub async fn send_peer_message(
        &self,
        to: &Jid,
        id: &MessageID,
        message: &WaMessage,
        extra: &SendRequestExtra,
    ) -> Result<SendOutcome, ClientError> {
        if !self.is_logged_in() {
            return Err(ClientError::NotLoggedIn);
        }
        let plaintext = message.encode_to_vec();
        let mut attrs = Attrs::new();
        attrs.insert("id".to_owned(), AttrValue::Text(id.as_str().to_owned()));
        attrs.insert("type".to_owned(), AttrValue::Text("text".to_owned()));
        attrs.insert("category".to_owned(), AttrValue::Text("peer".to_owned()));
        attrs.insert("to".to_owned(), AttrValue::Jid(to.clone()));
        if let Some(protocol) = &message.protocol_message {
            if protocol.r#type() == wa_e2e::protocol_message::Type::AppStateSyncKeyRequest {
                attrs.insert("push_priority".to_owned(), AttrValue::Text("high".to_owned()));
            } else if protocol
                .peer_data_operation_request_message
                .as_ref()
                .and_then(|request| request.peer_data_operation_request_type)
                == Some(wa_e2e::PeerDataOperationRequestType::HistorySyncOnDemand as i32)
            {
                attrs.insert("push_priority".to_owned(), AttrValue::Text("high_force".to_owned()));
                attrs.insert("privacy_sensitive".to_owned(), AttrValue::Text("1".to_owned()));
            }
        }

        // Go `preparePeerMessageNode`: the stanza is addressed to the phone
        // number, but the Signal session with our own devices is the LID one —
        // encrypting for the PN address fails with "no session".
        let identity = self.encryption_identity(to).await;
        let (enc, is_pre_key) =
            self.encrypt_message_for_device(&plaintext, &identity, None, &Attrs::new(), None).await?;
        let mut content = vec![Node::new("meta").attr("appdata", "default"), enc];
        if is_pre_key && let Some(identity) = self.make_device_identity_node().await {
            content.push(identity);
        }
        if let Some(additional) = &extra.additional_nodes {
            content.extend(additional.iter().cloned());
        }
        let node = Node::new("message").with_attrs(attrs).with_content(content);
        let data = self.send_node_and_get_data(&node).await?;
        Ok(SendOutcome { data, node, ..Default::default() })
    }

    /// Go `sendNewsletter` — a post/update to a channel: no Signal encryption,
    /// the payload is the `<plaintext>` child.
    pub async fn send_newsletter(
        &self,
        to: &Jid,
        id: &MessageID,
        message: Option<&WaMessage>,
        extra: &SendRequestExtra,
    ) -> Result<SendOutcome, ClientError> {
        let mut attrs = Attrs::new();
        attrs.insert("id".to_owned(), AttrValue::Text(id.as_str().to_owned()));
        attrs.insert("to".to_owned(), AttrValue::Jid(to.clone()));
        attrs.insert("type".to_owned(), AttrValue::Text("text".to_owned()));
        if !extra.media_handle.is_empty() {
            attrs.insert("media_id".to_owned(), AttrValue::Text(extra.media_handle.clone()));
        }
        let mut children = Vec::new();
        if let Some(message) = message {
            attrs.insert("type".to_owned(), AttrValue::Text(get_type_from_message(message).to_owned()));
            if let Some(edit) = get_edit_attribute(message) {
                attrs.insert("edit".to_owned(), AttrValue::Text(edit.to_owned()));
            }
            children.push(Node::new("plaintext").with_content(message.encode_to_vec()));
        }
        let node = Node::new("message").with_attrs(attrs).with_content(children);
        let data = self.send_node_and_get_data(&node).await?;
        Ok(SendOutcome { data, node, ..Default::default() })
    }

    // -----------------------------------------------------------------------
    // Node preparation and per-device encryption
    // -----------------------------------------------------------------------

    /// Go `prepareMessageNode` — build the `<message>` node for a fan-out.
    ///
    /// `participants` are the chat + the users taking part (for a DM: the peer
    /// and ourselves; for a group: its members). Each participant is expanded
    /// to its devices through [`Client::get_user_devices`] unless
    /// [`SendRequestExtra::devices`] overrides the list.
    pub async fn prepare_message_node(
        &self,
        to: &Jid,
        id: &MessageID,
        message: &WaMessage,
        participants: &[Jid],
        extra: &SendRequestExtra,
    ) -> Result<PreparedMessage, ClientError> {
        self.prepare_message_node_with(to, id, message, participants, None, extra).await
    }

    /// [`Client::prepare_message_node`] with an explicit `skd_plaintext` for
    /// the sender-key distribution message (group sends).
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_message_node_with(
        &self,
        to: &Jid,
        id: &MessageID,
        message: &WaMessage,
        participants: &[Jid],
        skd_plaintext: Option<&[u8]>,
        extra: &SendRequestExtra,
    ) -> Result<PreparedMessage, ClientError> {
        let started = Instant::now();
        let (plaintext, dsm_plaintext) = marshal_message(to, message)?;
        let _ = skd_plaintext;
        let started_devices = Instant::now();
        let mut all_devices = self.resolve_devices(participants, extra).await?;
        if to.is_group() {
            // Group sends never address a hosted device directly.
            all_devices.retain(|device| !matches!(device.server.as_str(), HOSTED_SERVER | HOSTED_LID_SERVER));
        }
        let _devices_time = started_devices.elapsed();

        let mut enc_attrs = Attrs::new();
        enc_attrs.insert("v".to_owned(), AttrValue::Text(ENC_VERSION.to_owned()));
        if dsm_plaintext.is_some() {
            let media_type = get_media_type_from_message(message);
            if !media_type.is_empty() {
                enc_attrs.insert("mediatype".to_owned(), AttrValue::Text(media_type.to_owned()));
            }
        }

        let mut extra_params = NodeExtraParams::default();
        // Go `prepareMessageNode` sets `addressing_mode` only from
        // `extraParams.addressingMode`, which `SendMessage` fills solely for
        // groups (`send.go:317-323`) — never on a DM stanza.
        if to.is_group() && (to.is_lid() || to.is_bot()) {
            extra_params.addressing_mode = "lid".to_owned();
        }
        // Go `SendMessage` (`send.go:335`): a DM to a LID (`HiddenUserServer`)
        // carries `peer_recipient_pn` — the PN behind the LID from the LID
        // map. Without it the server answers `smax-invalid` (479).
        if to.is_lid() {
            let lids = self.device.lock().await.lids.clone();
            match lids.get_pn_for_lid(&to.to_non_ad()).await {
                Ok(Some(pn)) if !pn.is_empty() => {
                    extra_params.peer_recipient_pn = Some(pn.to_non_ad());
                }
                Ok(_) => {}
                Err(err) => tracing::warn!("failed to get peer recipient PN for {to}: {err}"),
            }
        }
        if let Some(meta) = &extra.meta {
            extra_params.meta_node = Some(meta_info_node(meta));
        }
        extra_params.additional_nodes = extra.additional_nodes.clone();

        let (mut enc_nodes, include_identity, failures) =
            self.encrypt_message_for_devices(&all_devices, id, &plaintext, dsm_plaintext.as_deref(), &enc_attrs).await;

        // Go spreads the per-device nodes as `<to jid=…>` children of
        // `<participants>`; a peer message has a single bare `<enc>`.
        let base_node = Node::new("participants").with_content(std::mem::take(&mut enc_nodes));

        let mut attrs = Attrs::new();
        attrs.insert("id".to_owned(), AttrValue::Text(id.as_str().to_owned()));
        attrs.insert("type".to_owned(), AttrValue::Text(get_type_from_message(message).to_owned()));
        attrs.insert("to".to_owned(), AttrValue::Jid(to.clone()));
        if let Some(edit) = get_edit_attribute(message) {
            attrs.insert("edit".to_owned(), AttrValue::Text(edit.to_owned()));
        }
        // Go puts the participant-list hash on a group stanza only
        // (`sendGroup`, send.go:821). On a DM the server answers
        // `smax-invalid` (479) for it, so `sendDM` just keeps the hash to
        // compare against the ack (send.go:895).
        if to.is_group() && !all_devices.is_empty() {
            attrs.insert("phash".to_owned(), AttrValue::Text(participant_list_hash_v2(&all_devices)));
        }
        if !extra_params.addressing_mode.is_empty() {
            attrs.insert("addressing_mode".to_owned(), AttrValue::Text(extra_params.addressing_mode.clone()));
        }
        if let Some(peer_recipient_pn) = &extra_params.peer_recipient_pn {
            attrs.insert("peer_recipient_pn".to_owned(), AttrValue::Jid(peer_recipient_pn.clone()));
        }

        let device_identity = if include_identity { self.make_device_identity_node().await } else { None };
        let content = get_message_content(base_node, message, &attrs, include_identity, &extra_params, device_identity);
        let node = Node::new("message").with_attrs(attrs).with_content(content);
        let _ = started;
        let phash = (!all_devices.is_empty()).then(|| participant_list_hash_v2(&all_devices));
        Ok(PreparedMessage { node, devices: all_devices, phash, failures, include_device_identity: include_identity })
    }

    /// Marshal the prepared node and push it onto the socket.
    async fn finish_send(&self, prepared: PreparedMessage) -> Result<SendOutcome, ClientError> {
        let phash = prepared.phash.clone();
        let data = self.send_node_and_get_data(&prepared.node).await?;
        Ok(SendOutcome {
            phash,
            data,
            node: prepared.node,
            devices: prepared.devices,
            failures: prepared.failures,
            timings: SendTimings::default(),
        })
    }

    /// Go `encryptMessageForDevices` — one `<to>` node per device.
    ///
    /// Per-device failures are collected in the returned [`DeviceFailure`] list
    /// and never abort the fan-out.
    pub async fn encrypt_message_for_devices(
        &self,
        all_devices: &[Jid],
        id: &MessageID,
        msg_plaintext: &[u8],
        dsm_plaintext: Option<&[u8]>,
        enc_attrs: &Attrs,
    ) -> (Vec<Node>, bool, Vec<DeviceFailure>) {
        let store = self.device_store().await;
        // Go `encryptMessageForDevices`: PN devices are migrated to their LID
        // before the session check, so a session stored under the PN is found
        // under the LID encryption identity.
        // Go skips `ownJID`/`ownLID`: this device wrote the message, so there
        // is nobody to encrypt it for and no session with ourselves.
        let all_devices = &fan_out_devices(all_devices, &self.own_id().await, &self.own_lid().await);
        let mut encryption_identities: HashMap<Jid, Jid> = HashMap::with_capacity(all_devices.len());
        for device in all_devices {
            let identity = self.encryption_identity(device).await;
            // A session stored under the old PN address moves with it, so an
            // existing conversation is not restarted (Go `migrateSessionStore`).
            if identity != *device
                && let Err(err) = store.device.sessions.migrate_pn_to_lid(device, &identity).await
            {
                tracing::warn!("failed to migrate session store from {device} to {identity}: {err}");
            }
            encryption_identities.insert(device.clone(), identity);
        }
        let mut needs_bundle = Vec::new();
        let mut session_exists: HashMap<Jid, bool> = HashMap::with_capacity(all_devices.len());
        for device in all_devices {
            if matches!(device.server.as_str(), BOT_SERVER | HOSTED_SERVER | HOSTED_LID_SERVER) {
                // Bots and hosted devices are addressed without a bundle.
                session_exists.insert(device.clone(), true);
                continue;
            }
            let identity = &encryption_identities[device];
            let address = SignalAddress::from_jid(identity).to_string();
            match store.device.sessions.has_session(&address).await {
                Ok(true) => {
                    session_exists.insert(device.clone(), true);
                }
                Ok(false) => {
                    session_exists.insert(device.clone(), false);
                    needs_bundle.push(device.clone());
                }
                Err(err) => {
                    tracing::warn!("failed to check the session for {device}: {err}");
                    session_exists.insert(device.clone(), false);
                }
            }
        }
        let mut bundles = HashMap::new();
        if !needs_bundle.is_empty() {
            match self.fetch_pre_key_bundles(&needs_bundle).await {
                Ok(fetched) => bundles = fetched,
                Err(err) => tracing::warn!("failed to fetch {id}'s pre-key bundles: {err}"),
            }
        }

        let own_id = self.own_id().await;
        let own_lid = self.own_lid().await;
        let mut nodes = Vec::with_capacity(all_devices.len());
        let mut include_identity = false;
        let mut failures = Vec::new();
        for device in all_devices {
            let bundle = bundles.get(device);
            let identity = &encryption_identities[device];
            let has_session = session_exists.get(device).copied().unwrap_or(false) || bundles.contains_key(device);
            match self
                .encrypt_message_for_device_and_wrap_with(
                    msg_plaintext,
                    dsm_plaintext,
                    device,
                    identity,
                    id,
                    bundle,
                    enc_attrs,
                    Some(has_session),
                    &own_id,
                    &own_lid,
                )
                .await
            {
                Ok((node, is_pre_key)) => {
                    include_identity |= is_pre_key;
                    nodes.push(node);
                }
                Err(err) => {
                    tracing::warn!("failed to encrypt {id} for {device}: {err}");
                    failures.push(DeviceFailure { device: device.clone(), error: err.to_string() });
                }
            }
        }
        (nodes, include_identity, failures)
    }

    /// Go `encryptMessageForDeviceAndWrap` — encrypt for one device and wrap it
    /// in the `<to jid=…>` node the server routes by.
    #[allow(clippy::too_many_arguments)]
    pub async fn encrypt_message_for_device_and_wrap(
        &self,
        plaintext: &[u8],
        dsm_plaintext: Option<&[u8]>,
        device: &Jid,
        _id: &MessageID,
        bundle: Option<&PreKeyBundle>,
        enc_attrs: &Attrs,
        session_exists: Option<bool>,
        own_id: &Jid,
        own_lid: &Jid,
    ) -> Result<(Node, bool), ClientError> {
        let identity = self.encryption_identity(device).await;
        self.encrypt_message_for_device_and_wrap_with(
            plaintext,
            dsm_plaintext,
            device,
            &identity,
            _id,
            bundle,
            enc_attrs,
            session_exists,
            own_id,
            own_lid,
        )
        .await
    }

    /// [`Self::encrypt_message_for_device_and_wrap`] with a precomputed
    /// encryption identity (Go's `encryptionIdentities` map).
    #[allow(clippy::too_many_arguments)]
    pub async fn encrypt_message_for_device_and_wrap_with(
        &self,
        plaintext: &[u8],
        dsm_plaintext: Option<&[u8]>,
        device: &Jid,
        identity: &Jid,
        _id: &MessageID,
        bundle: Option<&PreKeyBundle>,
        enc_attrs: &Attrs,
        session_exists: Option<bool>,
        own_id: &Jid,
        own_lid: &Jid,
    ) -> Result<(Node, bool), ClientError> {
        let is_own_device = (!own_id.is_empty() && identity.user == own_id.user)
            || (!own_lid.is_empty() && identity.user == own_lid.user);
        // Our own devices cannot read the `DeviceSentMessage` wrapper they
        // wrote themselves — they get a `pkmsg` and the plain payload instead.
        // Go `encryptMessageForDevice`: an existing session is enough; the
        // bundle is only required to *create* one.
        let payload = match dsm_plaintext {
            Some(dsm) if is_own_device => {
                if bundle.is_none() && !session_exists.unwrap_or(false) {
                    return Err(ClientError::NoSession(format!("{device} (own device without a session)")));
                }
                dsm
            }
            Some(dsm) if bundle.is_some() => dsm,
            _ => plaintext,
        };
        let (enc, is_pre_key) =
            self.encrypt_message_for_device(payload, identity, bundle, enc_attrs, session_exists).await?;
        Ok((Node::new("to").attr("jid", device.clone()).with_content(vec![enc]), is_pre_key))
    }

    /// Go `encryptMessageForDevice` — run the Signal cipher for one address.
    ///
    /// Returns the `<enc>` node and whether a `pkmsg` was produced.
    pub async fn encrypt_message_for_device(
        &self,
        plaintext: &[u8],
        to: &Jid,
        bundle: Option<&PreKeyBundle>,
        extra_attrs: &Attrs,
        session_exists: Option<bool>,
    ) -> Result<(Node, bool), ClientError> {
        let store = self.device_store().await;
        let address = SignalAddress::from_jid(to);
        let builder = SessionBuilder::new(&store, address.clone());
        let cipher = SessionCipher::new(&store, address.clone());
        let mut session_exists = match session_exists {
            Some(exists) => exists,
            None => store
                .device
                .sessions
                .has_session(&address.to_string())
                .await
                .map_err(|err| ClientError::Signal(err.to_string()))?,
        };
        if let Some(bundle) = bundle {
            builder.process_bundle(bundle).await.map_err(|err| ClientError::Signal(err.to_string()))?;
            session_exists = true;
        }
        let padded = pad_message(plaintext);
        let ciphertext = match cipher.encrypt(&padded).await {
            Ok(ciphertext) => ciphertext,
            Err(err) => {
                if session_exists {
                    // Go: the session is broken, drop it and rebuild from the
                    // bundle we already have.
                    tracing::debug!("encrypting for {to} failed ({err}); dropping the session");
                    self.drop_signal_session(to).await?;
                    match bundle {
                        Some(bundle) => {
                            builder.process_bundle(bundle).await.map_err(|err| ClientError::Signal(err.to_string()))?;
                            cipher.encrypt(&padded).await.map_err(|err| ClientError::Signal(err.to_string()))?
                        }
                        None => return Err(ClientError::Signal(err.to_string())),
                    }
                } else {
                    return Err(ClientError::NoSession(to.to_string()));
                }
            }
        };
        let is_pre_key = matches!(ciphertext, CiphertextMessage::PreKey(_));
        let attrs = enc_attrs(ciphertext.enc_type(), extra_attrs);
        Ok((Node::new("enc").with_attrs(attrs).with_content(ciphertext.serialize().to_vec()), is_pre_key))
    }

    /// Drop the Signal session with `device` (Go `store.DeleteSession`).
    pub async fn drop_signal_session(&self, device: &Jid) -> Result<(), ClientError> {
        let address = SignalAddress::from_jid(device);
        let store = self.device_store().await;
        let user = device.to_non_ad();
        store
            .device
            .sessions
            .delete_session(&address.to_string())
            .await
            .map_err(|err| ClientError::Signal(err.to_string()))?;
        store
            .device
            .identities
            .delete_all_identities(&user.signal_address_user())
            .await
            .map_err(|err| ClientError::Signal(err.to_string()))?;
        tracing::debug!("dropped the signal session with {device}");
        Ok(())
    }

    /// The Signal identity a device is encrypted for.
    ///
    /// Sessions live under the LID address, so a phone-number JID has to be
    /// translated before any session lookup (Go `encryptMessageForDevice`'s
    /// LID migration, and `preparePeerMessageNode` for peer messages). The
    /// LID map answers for other people; for **our own** account the device
    /// knows both halves itself, which matters because nothing fills the map
    /// until a message arrives — and our own devices are exactly the ones a
    /// fan-out needs on the very first send.
    ///
    /// The device index is kept, so a JID without one stays the primary phone.
    pub async fn encryption_identity(&self, device: &Jid) -> Jid {
        if device.server != DEFAULT_USER_SERVER {
            return device.clone();
        }
        let (lids, own_id, own_lid) = {
            let store = self.device.lock().await;
            (store.lids.clone(), store.id.clone().unwrap_or_default(), store.lid.clone())
        };
        let own = (!own_lid.is_empty() && own_id.user == device.user).then(|| own_lid.to_non_ad());
        let mapped = match lids.get_lid_for_pn(&device.to_non_ad()).await {
            Ok(lid) => lid,
            Err(err) => {
                tracing::warn!("failed to get the LID of {device}: {err}");
                None
            }
        };
        match mapped.or(own).filter(|lid| !lid.is_empty()) {
            Some(lid) => lid.with_device(device.device),
            None => device.clone(),
        }
    }
}

/// The devices a fan-out actually encrypts for: everything but the device
/// doing the sending, under either of its two addresses (Go
/// `encryptMessageForDevices` skips `ownJID` and `ownLID`).
///
/// Our *other* devices stay in — they are what puts the message on the user's
/// phone and in their other web sessions.
pub(crate) fn fan_out_devices(all: &[Jid], own_id: &Jid, own_lid: &Jid) -> Vec<Jid> {
    all.iter().filter(|jid| !(*jid == own_id || *jid == own_lid)).cloned().collect()
}

/// Go `encryptMessageForDevice`'s `encAttrs`: `{v, type}` first, then the
/// caller's attributes on top (`maps.Copy(encAttrs, extraAttrs)`).
///
/// `v` is not optional. A fan-out sets it in `prepare_message_node`, but a
/// peer message has no such caller — and an `<enc>` without `v` comes back as
/// `<error code="479">` (`smax-invalid`), which is how "Fetch older messages"
/// failed in the field.
pub(crate) fn enc_attrs(enc_type: &str, extra: &Attrs) -> Attrs {
    let mut attrs = Attrs::new();
    attrs.insert("v".to_owned(), AttrValue::Text(ENC_VERSION.to_owned()));
    attrs.insert("type".to_owned(), AttrValue::Text(enc_type.to_owned()));
    for (key, value) in extra {
        attrs.insert(key.clone(), value.clone());
    }
    attrs
}

/// Go `nodeExtraParams.metaNode` — the `<meta>` child from a
/// [`MsgMetaInfo`].
fn meta_info_node(meta: &MsgMetaInfo) -> Node {
    let mut node = Node::new("meta");
    if meta.deprecated_lid_session.is_some() {
        node = node.attr("decrypt-fail", "hidelid");
    }
    if !meta.thread_message_id.is_empty() {
        node = node
            .attr("thread_id", meta.thread_message_id.as_str())
            .attr("thread_msg_sender_jid", meta.thread_message_sender_jid.clone());
    }
    node
}

// ---------------------------------------------------------------------------
// Device and pre-key lookups
// ---------------------------------------------------------------------------

impl Client {
    /// Resolve the fan-out list: [`SendRequestExtra::devices`] when given, else
    /// the server's device list for the participants (plus our own LID).
    async fn resolve_devices(&self, participants: &[Jid], extra: &SendRequestExtra) -> Result<Vec<Jid>, ClientError> {
        if let Some(devices) = &extra.devices {
            return Ok(dedupe_jids(devices.iter().cloned()));
        }
        let own_id = self.own_id().await;
        let own_lid = self.own_lid().await;
        let mut users: Vec<Jid> = participants.iter().map(Jid::to_non_ad).filter(|jid| !jid.is_empty()).collect();
        if !own_lid.is_empty() {
            users.push(own_lid.to_non_ad());
        }
        if !own_id.is_empty() {
            users.push(own_id.to_non_ad());
        }
        let users = dedupe_jids(users);
        self.get_user_devices(&users).await
    }

    /// Go `GetUserDevices` — every device of every given user, via a `usync`
    /// device query.
    ///
    /// A user the server knows nothing about is skipped (Go does the same); the
    /// request itself failing is an error.
    pub async fn get_user_devices(&self, users: &[Jid]) -> Result<Vec<Jid>, ClientError> {
        if users.is_empty() {
            return Ok(Vec::new());
        }
        let mut list = Vec::with_capacity(users.len());
        for user in users {
            list.push(Node::new("user").attr("jid", user.to_non_ad()));
        }
        let query = Node::new("query").with_content(vec![Node::new("devices").attr("version", "2")]);
        let usync = Node::new("usync")
            .attr("sid", self.generate_request_id())
            .attr("mode", "query")
            .attr("last", "true")
            .attr("index", "0")
            .attr("context", "interactive")
            .with_content(vec![query, Node::new("list").with_content(list)]);
        let response = self
            .send_iq(InfoQuery {
                namespace: "usync".to_owned(),
                kind: IqKind::Get,
                to: Some(Jid::server_jid()),
                content: vec![usync],
                ..Default::default()
            })
            .await?;
        let mut devices = Vec::new();
        let Some(usync) = response.optional_child_by_tag(&["usync"]) else {
            return Ok(devices);
        };
        for list in usync.children_by_tag("list") {
            for user in list.children_by_tag("user") {
                let Some(user_jid) = attr_jid(user, "jid") else {
                    continue;
                };
                devices.extend(parse_device_list(&user_jid, user));
            }
        }
        Ok(dedupe_jids(devices))
    }

    /// The participant list of a group.
    ///
    /// `TODO(Phase 5)`: replace with `group.go`'s `getCachedGroupData` (the
    /// cache + the `w:g2` query with `request="interactive"`); until then this
    /// asks the server directly.
    pub async fn get_group_participants(&self, chat: &Jid) -> Result<Vec<Jid>, ClientError> {
        let response = self
            .send_iq(InfoQuery {
                namespace: "w:g2".to_owned(),
                kind: IqKind::Get,
                to: Some(chat.clone()),
                content: vec![Node::new("query")],
                ..Default::default()
            })
            .await?;
        let group = response.child_by_tag(&["group"]);
        let participants: Vec<Jid> =
            group.children_by_tag("participant").filter_map(|node| attr_jid(node, "jid")).collect();
        if participants.is_empty() {
            return Err(ClientError::NoPermissionInGroup);
        }
        Ok(participants)
    }

    /// Go `FetchPreKeys` — fetch the Signal pre-key bundles of `devices`.
    ///
    /// A device without a usable bundle is simply absent from the result (the
    /// caller records it as a [`DeviceFailure`]).
    ///
    /// `TODO(Phase 5)`: `prekeys.rs` owns the outgoing key upload; this is the
    /// incoming half Go keeps in the same file and will move there once Task B
    /// lands (`fetch_pre_keys`).
    pub async fn fetch_pre_key_bundles(&self, devices: &[Jid]) -> Result<HashMap<Jid, PreKeyBundle>, ClientError> {
        // Go `fetchPreKeys`: one `<user jid=… reason="identity">` per device
        // (the full JID, device included) — no `<device id=…>` children.
        let mut content = Vec::with_capacity(devices.len());
        for device in devices {
            content.push(Node::new("user").attr("jid", device.clone()).attr("reason", "identity"));
        }
        let response = self
            .send_iq(InfoQuery {
                namespace: "encrypt".to_owned(),
                kind: IqKind::Get,
                to: Some(Jid::server_jid()),
                content: vec![Node::new("key").with_content(content)],
                timeout: Some(PRE_KEY_FETCH_TIMEOUT),
                ..Default::default()
            })
            .await?;

        let mut bundles = HashMap::new();
        let Some(list) = response.optional_child_by_tag(&["list"]) else {
            return Ok(bundles);
        };
        for user in list.children_by_tag("user") {
            let Some(device_jid) = attr_jid(user, "jid") else {
                continue;
            };
            if user.optional_child_by_tag(&["error"]).is_some() {
                let error = user.child_by_tag(&["error"]);
                tracing::debug!("no pre-key bundle for {device_jid}: {}", error.to_xml_string());
                continue;
            }
            match node_to_pre_key_bundle(u32::from(device_jid.device), user) {
                Ok(bundle) => {
                    bundles.insert(device_jid, bundle);
                }
                Err(err) => tracing::debug!("no pre-key bundle for {device_jid}: {err}"),
            }
        }
        Ok(bundles)
    }
}

/// Go `parseDeviceList` — the `<devices><device-list><device id=…/></…>` of a
/// `usync` user. A `key-index`/`is_hosted` device is still an ordinary device
/// for the send path.
fn parse_device_list(user: &Jid, device_list: &Node) -> Vec<Jid> {
    let Some(list) = device_list.optional_child_by_tag(&["devices", "device-list"]) else {
        return Vec::new();
    };
    list.children_by_tag("device")
        .filter_map(|device| attr_int(device, "id"))
        .filter_map(|id| u16::try_from(id).ok())
        .map(|id| user.with_device(id))
        .collect()
}

/// Go `nodeToPreKeyBundle` — a [`PreKeyBundle`] from a `<user>` (pre-key
/// response) or a retry receipt node.
///
/// The child layout is Go's: `<registration>` (4 bytes big-endian),
/// `<keys>`-or-self for `<identity>`/`<key>`/`<skey>`, and an optional
/// `<error>` that means "no bundle".
pub(crate) fn node_to_pre_key_bundle(device_id: u32, node: &Node) -> Result<PreKeyBundle, ClientError> {
    if let Some(error) = node.optional_child_by_tag(&["error"]) {
        return Err(ClientError::PreKey(format!("server error: {}", error.to_xml_string())));
    }
    let registration_id = node
        .optional_child_by_tag(&["registration"])
        .and_then(|node| node.as_bytes().and_then(uint32_be))
        .ok_or_else(|| ClientError::PreKey("invalid registration ID in prekey response".to_owned()))?;
    let keys_node;
    let keys_node = match node.optional_child_by_tag(&["keys"]) {
        Some(keys) => keys,
        None => {
            keys_node = node.clone();
            &keys_node
        }
    };
    let identity_key = keys_node
        .optional_child_by_tag(&["identity"])
        .and_then(|node| node.as_bytes().and_then(key_bytes))
        .ok_or_else(|| ClientError::PreKey("invalid identity key in prekey response".to_owned()))?;
    let pre_key = keys_node.optional_child_by_tag(&["key"]).map(node_to_pre_key).transpose()?;
    let signed_pre_key_node = keys_node.child_by_tag(&["skey"]);
    let signed_pre_key = node_to_pre_key(&signed_pre_key_node)?;
    Ok(PreKeyBundle {
        registration_id,
        device_id,
        pre_key_id: pre_key.as_ref().map(|key| key.id),
        pre_key_public: pre_key.as_ref().map(|key| key.public),
        signed_pre_key_id: signed_pre_key.id,
        signed_pre_key_public: signed_pre_key.public,
        signed_pre_key_signature: signed_pre_key
            .signature
            .ok_or_else(|| ClientError::PreKey("signed prekey in prekey response has no signature".to_owned()))?,
        identity_key,
    })
}

/// One `<key>`/`<skey>` node of a pre-key response.
pub(crate) struct ParsedPreKey {
    pub id: u32,
    pub public: [u8; 32],
    pub signature: Option<[u8; 64]>,
}

/// Go `nodeToPreKey` — `<id>` is a 3-byte big-endian integer, `<value>` a
/// 32-byte curve25519 public key.
fn node_to_pre_key(node: &Node) -> Result<ParsedPreKey, ClientError> {
    let id = node
        .optional_child_by_tag(&["id"])
        .and_then(|node| node.as_bytes().and_then(uint24_be))
        .ok_or_else(|| ClientError::PreKey("prekey node doesn't contain a valid id".to_owned()))?;
    let public = node
        .optional_child_by_tag(&["value"])
        .and_then(|node| node.as_bytes().and_then(key_bytes))
        .ok_or_else(|| ClientError::PreKey("prekey node doesn't contain a valid value".to_owned()))?;
    let signature = node
        .optional_child_by_tag(&["signature"])
        .map(|node| {
            let bytes =
                node.as_bytes().ok_or_else(|| ClientError::PreKey("prekey signature has no content".to_owned()))?;
            if bytes.len() != 64 {
                return Err(ClientError::PreKey(format!("prekey signature has {} bytes, expected 64", bytes.len())));
            }
            let mut out = [0u8; 64];
            out.copy_from_slice(bytes);
            Ok(out)
        })
        .transpose()?;
    Ok(ParsedPreKey { id, public, signature })
}

/// A 32-byte curve25519 key; a leading `0x05` DJB prefix is stripped.
fn key_bytes(bytes: &[u8]) -> Option<[u8; 32]> {
    let raw = match bytes {
        [5, rest @ ..] if rest.len() == 32 => rest,
        bytes if bytes.len() == 32 => bytes,
        _ => return None,
    };
    let mut out = [0u8; 32];
    out.copy_from_slice(raw);
    Some(out)
}

/// A big-endian `u32` from node content (`<registration>`).
fn uint32_be(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// A big-endian 3-byte integer from node content (`<id>` of a `<key>`).
fn uint24_be(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 3] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
}

/// Drop duplicate JIDs, keeping the first occurrence.
fn dedupe_jids(jids: impl IntoIterator<Item = Jid>) -> Vec<Jid> {
    let mut seen = std::collections::HashSet::new();
    jids.into_iter().filter(|jid| !jid.is_empty() && seen.insert(jid.clone())).collect()
}
