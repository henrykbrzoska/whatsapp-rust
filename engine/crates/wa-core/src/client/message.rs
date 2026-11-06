// Ported from the Go client `message.go` (incoming message handling), © Tulir
// Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The incoming message path — Phase 4 Task C skeleton.
//!
//! Ports the front half of `Go message.go`:
//!
//! * [`Client::parse_message_source`] / [`Client::parse_message_info`] — the
//!   `<message>` attributes become a [`MessageInfo`] (chat, sender, id,
//!   timestamp, `is_from_me`, `is_group`, category, push name, …).
//! * [`Client::handle_message_node`] — routes `message`/`appdata`
//!   (`handleEncryptedMessage`) and `status` (`handleUnencryptedMessage`).
//! * [`Client::decrypt_messages`] / [`Client::decrypt_enc_node`] — the `<enc>`
//!   loop. `pkmsg`/`msg` go through [`SessionCipher`] and `skmsg` through
//!   [`GroupCipher`], both over a [`DeviceStore`] built from the client's
//!   device; `msmsg` (bot messages) needs the message-secret layer and is still
//!   reported as undecryptable (TODO Phase 5). A failure emits
//!   [`WaEvent::UndecryptableMessage`] and sends a retry receipt, exactly like
//!   Go's error branch; a success decodes the `Message` protobuf, processes a
//!   sender-key distribution message and emits [`WaEvent::Message`].
//! * [`Client::handle_plaintext_message`] — newsletter/status messages whose
//!   `<plaintext>` child is already decoded.

use prost::Message as ProstMessage;
use wa_proto::wa_web_protobufs_e2e::Message as WaMessage;

use crate::WaEvent;
use crate::binary::node::{Node, NodeContent};
use crate::crypto::keys::unpad_message;
use crate::crypto::signal::{
    DeviceStore, GroupCipher, GroupSessionBuilder, PreKeySignalMessage, SenderKeyDistributionMessage, SenderKeyName,
    SessionCipher, SessionlessSignalStore, SignalAddress, SignalError, SignalMessage, SignalStore,
};
use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid};
use crate::types::message::{
    AddressingMode, Message, MessageID, MessageInfo, MessageServerID, MessageSource, MsgBotInfo, MsgMetaInfo,
};

use super::ack::{NACK_PARSING_ERROR, NACK_UNRECOGNIZED_STANZA, attr_text};
use super::{Client, ClientError};

/// Why an `<enc>` child could not be decrypted.
///
/// Signal failures are mapped by [`DecryptError::from_signal`]; the variant
/// decides the `decrypt_fail_mode` and whether the message is retried.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecryptError {
    /// `pkmsg`/`msg` — no pairwise session with the sender
    /// (Go `signalerror.ErrNoSessionForUser`).
    #[error("failed to decrypt {enc_type} message: {reason}")]
    NoSession { enc_type: &'static str, reason: String },
    /// `skmsg` — no sender key for the group sender
    /// (Go `signalerror.ErrNoSenderKeyForUser`).
    #[error("failed to decrypt group message: {0}")]
    NoSenderKey(String),
    /// The message key is already consumed (old counter or replay). Go ignores
    /// these without a retry receipt (`signalerror.ErrOldCounter`).
    #[error("failed to decrypt {enc_type} message: {reason}")]
    OldCounter { enc_type: &'static str, reason: String },
    /// Any other Signal / parsing / unpadding failure.
    #[error("failed to decrypt {enc_type} message: {reason}")]
    Decrypt { enc_type: &'static str, reason: String },
    /// `msmsg` — bot messages need the message-secret layer (Phase 5).
    #[error("bot (msmsg) messages need the message secret layer (TODO Phase 5)")]
    BotMessageNeedsSecret,
    /// An `<enc type="…">` WhatsApp may add later.
    #[error("unhandled encrypted message type {0}")]
    UnhandledEncType(String),
}

impl DecryptError {
    /// Map a Signal-layer error for an `<enc type=enc_type>` child.
    pub fn from_signal(enc_type: &'static str, err: SignalError) -> Self {
        let reason = err.to_string();
        match err {
            SignalError::NoSession(_) => Self::NoSession { enc_type, reason },
            SignalError::NoSenderKeyForUser(_) => Self::NoSenderKey(reason),
            SignalError::OldCounter { .. } | SignalError::DuplicateMessage(_) => Self::OldCounter { enc_type, reason },
            _ => Self::Decrypt { enc_type, reason },
        }
    }

    /// The `decrypt_fail_mode` reported on [`WaEvent::UndecryptableMessage`]
    /// (Go `events.DecryptFailMode`).
    pub fn fail_mode(&self) -> &'static str {
        match self {
            Self::NoSession { .. } => "no_session",
            Self::NoSenderKey(_) => "sender_key",
            Self::OldCounter { .. }
            | Self::Decrypt { .. }
            | Self::BotMessageNeedsSecret
            | Self::UnhandledEncType(_) => "decrypt",
        }
    }
}

/// Go `decryptMessages`' `senderEncryptionJID`: a PN sender whose LID is known
/// (the `participant_lid`/`sender_lid` alt) encrypts with the LID session.
///
/// TODO(Phase 5): `Store.LIDs.GetLIDForPN` fallback + `migrateSessionStore`.
fn sender_encryption_jid(info: &MessageInfo) -> Jid {
    if info.sender.server == DEFAULT_USER_SERVER
        && !info.sender.is_bot()
        && info.sender_alt.server == HIDDEN_USER_SERVER
    {
        info.sender_alt.clone()
    } else {
        info.sender.clone()
    }
}

/// One attribute as a JID, whether it arrived as a JID token or as text.
pub(crate) fn attr_jid(node: &Node, key: &str) -> Option<Jid> {
    node.attrs.get(key).and_then(|value| match value {
        crate::binary::node::AttrValue::Jid(jid) => Some(jid.clone()),
        other => Jid::parse(&other.to_string()).ok(),
    })
}

/// One attribute as an integer (`t`, `server_id`, `count`, …).
pub(crate) fn attr_int(node: &Node, key: &str) -> Option<i64> {
    match node.attrs.get(key)? {
        crate::binary::node::AttrValue::Int(value) => Some(*value),
        crate::binary::node::AttrValue::Uint(value) => i64::try_from(*value).ok(),
        other => other.to_string().parse().ok(),
    }
}

/// A required attribute; its absence is a message-parse failure.
fn required_attr(node: &Node, key: &str) -> Result<String, ClientError> {
    attr_text(node, key).ok_or_else(|| ClientError::MessageParse(format!("missing `{key}` attribute")))
}

/// Go `handleHostedServer` — hosted JIDs are reported as their plain servers.
fn unhost(jid: Jid) -> Jid {
    match jid.server.as_str() {
        crate::types::jid::HOSTED_SERVER => jid.with_server(crate::types::jid::DEFAULT_USER_SERVER),
        crate::types::jid::HOSTED_LID_SERVER => jid.with_server(crate::types::jid::HIDDEN_USER_SERVER),
        _ => jid,
    }
}

/// Build the [`WaEvent::Message`] payload: the unwrapped message, the raw one
/// and the wrapper flags. Both protobufs travel hex-encoded (see
/// [`super::web_message::message_to_json`] for why).
pub(crate) fn message_event(message: &Message, retry_count: i32) -> WaEvent {
    WaEvent::Message {
        info: message.info.clone(),
        message: message.message.as_ref().map(|msg| hex::encode(msg.encode_to_vec())),
        raw: message.raw_message.as_ref().map(|msg| hex::encode(msg.encode_to_vec())),
        text: message.text(),
        is_ephemeral: message.is_ephemeral,
        is_view_once: message.is_view_once,
        is_edit: message.is_edit,
        retry_count,
    }
}

impl Client {
    /// Go `nodeHandlers["message"|"status"|"appdata"]` — the entry point the
    /// node loop calls for message-class nodes. Returns `false` for a tag this
    /// module does not own.
    pub async fn handle_message_node(&self, node: &Node) -> bool {
        match node.tag.as_str() {
            "message" | "appdata" => {
                self.handle_encrypted_message(node).await;
                true
            }
            "status" => {
                self.handle_unencrypted_message(node).await;
                true
            }
            other => {
                tracing::warn!("no message handler for node {other}");
                false
            }
        }
    }

    /// Go `parseMessageSource` — who sent what where.
    ///
    /// `require_participant` mirrors Go's flag: the message path always requires
    /// the `<participant>` of a group message.
    pub async fn parse_message_source(
        &self,
        node: &Node,
        require_participant: bool,
    ) -> Result<MessageSource, ClientError> {
        let (own_id, own_lid) = {
            let device = self.device.lock().await;
            (device.get_jid(), device.get_lid())
        };
        if own_id.is_empty() {
            return Err(ClientError::NotLoggedIn);
        }
        let from =
            attr_jid(node, "from").ok_or_else(|| ClientError::MessageParse("missing `from` attribute".into()))?;
        let addressing_mode = AddressingMode::parse(&attr_text(node, "addressing_mode").unwrap_or_default());
        let mut source = MessageSource { addressing_mode, ..Default::default() };

        if from.is_group() || from.server == crate::types::jid::BROADCAST_SERVER {
            source.is_group = true;
            source.chat = from.clone();
            source.sender = if require_participant {
                attr_jid(node, "participant")
                    .ok_or_else(|| ClientError::MessageParse("missing `participant` attribute".into()))?
            } else {
                attr_jid(node, "participant").unwrap_or_default()
            };
            source.sender_alt = attr_jid(node, "participant_pn").unwrap_or_default();
            if source.sender_alt.is_empty() && addressing_mode != AddressingMode::Lid {
                source.sender_alt = attr_jid(node, "participant_lid").unwrap_or_default();
            }
            if source.sender.user == own_id.user || source.sender.user == own_lid.user {
                source.is_from_me = true;
            }
            if from.server == crate::types::jid::BROADCAST_SERVER {
                // Go also records `BroadcastListOwner` and, for our own messages,
                // the `<participants>` recipients. TODO(Phase 5): the broadcast
                // list fields are not part of `MessageSource` yet.
                tracing::debug!("broadcast list message in {} (recipient/participants TODO Phase 5)", source.chat);
            }
        } else if from.is_newsletter() {
            source.chat = from.clone();
            source.sender = from;
        } else if from.user == own_id.user || from.user == own_lid.user {
            let from = unhost(from);
            source.is_from_me = true;
            source.sender = from.clone();
            source.chat = attr_jid(node, "recipient").unwrap_or_else(|| from.to_non_ad());
            if source.chat.is_lid() {
                source.recipient_alt = attr_jid(node, "peer_recipient_pn").unwrap_or_default();
            } else {
                source.recipient_alt = attr_jid(node, "peer_recipient_lid").unwrap_or_default();
            }
        } else if from.is_bot() {
            source.sender = from.clone();
            let target_chat = node.optional_child_by_tag(&["meta"]).and_then(|meta| attr_jid(meta, "target_chat_jid"));
            source.chat = target_chat.map(|jid| jid.to_non_ad()).unwrap_or_else(|| from.clone());
        } else {
            let from = unhost(from);
            source.chat = from.to_non_ad();
            source.sender = from.clone();
            if source.sender.is_lid() {
                source.sender_alt = attr_jid(node, "sender_pn").unwrap_or_default();
            } else {
                source.sender_alt = attr_jid(node, "sender_lid").unwrap_or_default();
            }
        }

        if !source.sender_alt.is_empty() && source.sender_alt.device == 0 {
            source.sender_alt.device = source.sender.device;
        }
        Ok(source)
    }

    /// Go `StoreLIDPNMapping` — remember that these two JIDs are the same
    /// person.
    ///
    /// The order does not matter: whichever is the `@lid` and whichever the
    /// `@s.whatsapp.net` is sorted out here, and a pair that is not exactly
    /// one of each (two LIDs, a group, an empty JID) is ignored. The device
    /// index is dropped — the map is per account, not per device.
    pub async fn store_lid_pn_mapping(&self, first: &Jid, second: &Jid) {
        let (lid, pn) = match (first.server.as_str(), second.server.as_str()) {
            (HIDDEN_USER_SERVER, DEFAULT_USER_SERVER) => (first, second),
            (DEFAULT_USER_SERVER, HIDDEN_USER_SERVER) => (second, first),
            _ => return,
        };
        if lid.user.is_empty() || pn.user.is_empty() {
            return;
        }
        let lids = self.device.lock().await.lids.clone();
        if let Err(err) = lids.put_lid_mapping(&lid.to_non_ad(), &pn.to_non_ad()).await {
            tracing::warn!("failed to store the LID mapping {lid} -> {pn}: {err}");
        }
    }

    /// Go `parseMessageInfo` — the attributes plus the `<multicast>`, `<bot>`
    /// and `<meta>` children of a `<message>`/`<status>` node.
    pub async fn parse_message_info(&self, node: &Node) -> Result<MessageInfo, ClientError> {
        let source = self.parse_message_source(node, true).await?;
        let mut info = MessageInfo {
            chat: source.chat,
            sender: source.sender,
            sender_alt: source.sender_alt,
            recipient_alt: source.recipient_alt,
            is_from_me: source.is_from_me,
            is_group: source.is_group,
            addressing_mode: source.addressing_mode,
            id: MessageID::new(required_attr(node, "id")?),
            server_id: MessageServerID(attr_int(node, "server_id").unwrap_or(0)),
            timestamp: attr_int(node, "t").unwrap_or(0),
            push_name: attr_text(node, "notify").unwrap_or_default(),
            category: attr_text(node, "category").unwrap_or_default(),
            message_type: attr_text(node, "type").unwrap_or_default(),
            edit: attr_text(node, "edit").unwrap_or_default(),
            ..Default::default()
        };
        // Go `handleDecryptedMessage` does this for every message: the `_alt`
        // attributes are the only place a companion ever sees the two halves
        // of an identity together, and the map is what lets a PN-addressed
        // history blob join its LID-addressed chat.
        self.store_lid_pn_mapping(&info.sender_alt, &info.sender).await;
        self.store_lid_pn_mapping(&info.recipient_alt, &info.chat).await;

        for child in node.children() {
            match child.tag.as_str() {
                "multicast" => info.multicast = true,
                "bot" => info.msg_bot_info = parse_msg_bot_info(child),
                "meta" => info.msg_meta_info = parse_msg_meta_info(child),
                "verified_name" | "franking" | "trace" => {
                    // Go parses `verified_name` into the info and ignores the
                    // other two. TODO(Phase 5): verified-name update events.
                }
                _ => {
                    if let Some(media_type) = attr_text(child, "mediatype") {
                        info.media_type = media_type;
                    }
                }
            }
        }
        self.update_push_name(&info).await;
        Ok(info)
    }

    /// Go `updatePushName` — remember the `notify` name of an incoming message
    /// so the UI can show "Ann" instead of `48111@s.whatsapp.net`.
    ///
    /// The address book (app-state contact sync) wins over this, which is why
    /// the name goes into the contact row's push-name slot; [`ContactInfo`]
    /// resolution order is address book → business → push name. A store
    /// failure is logged, never fatal: the message itself still arrives.
    ///
    /// [`ContactInfo`]: crate::store::traits::ContactInfo
    async fn update_push_name(&self, info: &MessageInfo) {
        let name = info.push_name.trim();
        // WhatsApp sends `-` for "no name"; our own messages carry our name.
        if name.is_empty() || name == "-" || info.is_from_me || info.sender.is_empty() {
            return;
        }
        let user = info.sender.to_non_ad();
        let device = self.device().await;
        if let Err(err) = device.contacts.put_push_name(&user, name).await {
            tracing::warn!("could not store push name for {user}: {err}");
        }
    }

    /// Go `handleEncryptedMessage` — E2E (or newsletter) `<message>`/`<appdata>`.
    pub async fn handle_encrypted_message(&self, node: &Node) {
        let info = match self.parse_message_info(node).await {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!("failed to parse message: {err}");
                self.nack(node, Some(NACK_PARSING_ERROR)).await;
                return;
            }
        };
        tracing::debug!("handling message {} from {}", info.id, info.source_string());
        // Go also stores the LID↔PN mapping and refreshes push names here.
        // TODO(Phase 5): `StoreLIDPNMapping`, `updatePushName`, `updateBusinessName`.

        if info.sender.is_newsletter() {
            self.handle_plaintext_message(&info, node).await;
            self.maybe_deferred_ack(node).await;
            return;
        }
        self.decrypt_messages(&info, node).await;
    }

    /// Go `handleUnencryptedMessage` — `<status>` nodes. Only newsletters carry
    /// plaintext; everything else is acked and dropped.
    pub async fn handle_unencrypted_message(&self, node: &Node) {
        let mut info = match self.parse_message_info(node).await {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!("failed to parse unencrypted message: {err}");
                self.nack(node, Some(NACK_PARSING_ERROR)).await;
                return;
            }
        };
        if !info.sender.is_newsletter() {
            self.nack(node, None).await;
            return;
        }
        info.is_newsletter_status = true;
        self.handle_plaintext_message(&info, node).await;
        self.maybe_deferred_ack(node).await;
    }

    /// Go `handlePlaintextMessage` — a `<plaintext>` child we can decode
    /// directly (newsletter status updates).
    pub async fn handle_plaintext_message(&self, info: &MessageInfo, node: &Node) -> bool {
        let Some(plaintext) = node.optional_child_by_tag(&["plaintext"]) else {
            return false;
        };
        let NodeContent::Bytes(body) = &plaintext.content else {
            tracing::warn!("plaintext message from {} doesn't have byte content", info.source_string());
            return false;
        };
        let raw = match WaMessage::decode(body.as_slice()) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!("failed to unmarshal plaintext message from {}: {err}", info.source_string());
                return false;
            }
        };
        let mut message = Message { info: info.clone(), raw_message: Some(raw), ..Default::default() };
        message.unwrap_raw();
        let event = message_event(&message, 0);
        self.dispatch_event(event);
        true
    }

    /// Go `decryptMessages` — walk the `<enc>` children and decrypt each one.
    ///
    /// A decrypted `<enc>` becomes a [`WaEvent::Message`]; a failure warns,
    /// sends a retry receipt (NACK 495 for `msmsg`) and emits
    /// [`WaEvent::UndecryptableMessage`]; an old counter / replay is ignored.
    /// A node with no `<enc>` child is either an `<unavailable>` message
    /// (reported as undecryptable) or unhandleable (NACK 488).
    pub async fn decrypt_messages(&self, info: &MessageInfo, node: &Node) {
        let enc_nodes: Vec<Node> = node.children_by_tag("enc").cloned().collect();
        if enc_nodes.is_empty() {
            match node.optional_child_by_tag(&["unavailable"]) {
                Some(unavailable) => {
                    let unavailable_type = attr_text(unavailable, "type").unwrap_or_default();
                    tracing::warn!(
                        "unavailable message {} from {} (type: {unavailable_type:?})",
                        info.id,
                        info.source_string()
                    );
                    // TODO(Phase 5): `immediateRequestMessageFromPhone`.
                    self.nack(node, None).await;
                    self.dispatch_event(WaEvent::UndecryptableMessage {
                        info: info.clone(),
                        is_unavailable: true,
                        unavailable_type,
                        decrypt_fail_mode: String::new(),
                    });
                }
                None => {
                    tracing::warn!("message {} from {} has no encrypted content", info.id, info.source_string());
                    self.nack(node, Some(NACK_UNRECOGNIZED_STANZA)).await;
                }
            }
            return;
        }

        // Go sends the ack only after every `<enc>` decrypted; the error branch
        // below returns early, so a failure never acks.
        let mut contains_direct_msg = false;
        for enc in &enc_nodes {
            let enc_type = attr_text(enc, "type").unwrap_or_default();
            contains_direct_msg |= matches!(enc_type.as_str(), "pkmsg" | "msg");
            let err = match self.decrypt_enc_node(info, enc, &enc_type).await {
                Ok(plaintext) => {
                    self.handle_decrypted_plaintext(info, enc, &plaintext).await;
                    continue;
                }
                Err(DecryptError::OldCounter { reason, .. }) => {
                    tracing::warn!("ignoring message {} from {}: {reason}", info.id, info.source_string());
                    continue;
                }
                Err(err) => err,
            };
            tracing::warn!("error decrypting message {} from {}: {err}", info.id, info.source_string());
            let decrypt_fail_mode = err.fail_mode().to_owned();
            let is_unavailable =
                enc_type == "skmsg" && !contains_direct_msg && matches!(err, DecryptError::NoSenderKey(_));
            // `msmsg` failures are answered with NACK 495 (no message secret);
            // everything else asks the sender to re-encrypt.
            if matches!(err, DecryptError::BotMessageNeedsSecret) {
                self.nack(node, Some(super::ack::NACK_MISSING_MESSAGE_SECRET)).await;
            } else if let Err(send_err) = self.send_retry_receipt_with(node, info, is_unavailable).await {
                tracing::warn!("failed to send retry receipt for {}: {send_err}", info.id);
            }
            self.dispatch_event(WaEvent::UndecryptableMessage {
                info: info.clone(),
                is_unavailable,
                unavailable_type: String::new(),
                decrypt_fail_mode,
            });
            return;
        }
        // Every `<enc>` decrypted (or was ignored): ack the node the way Go's
        // success path does.
        self.nack(node, None).await;
    }

    /// The per-`<enc>` decrypt, routed like Go's `decryptMessages`:
    /// `decryptDM` for `pkmsg`/`msg`, `decryptGroupMsg` for `skmsg`,
    /// `decryptBotMessage` for `msmsg` (message-secret layer).
    pub(crate) async fn decrypt_enc_node(
        &self,
        info: &MessageInfo,
        enc: &Node,
        enc_type: &str,
    ) -> Result<Vec<u8>, DecryptError> {
        let static_type: &'static str = match enc_type {
            "pkmsg" => "pkmsg",
            "msg" => "msg",
            "skmsg" if info.is_group => "skmsg",
            "msmsg" if info.sender.is_bot() => "msmsg",
            other => {
                tracing::warn!("unhandled encrypted message (type {other}) from {}", info.source_string());
                return Err(DecryptError::UnhandledEncType(other.to_owned()));
            }
        };
        let NodeContent::Bytes(content) = &enc.content else {
            return Err(DecryptError::Decrypt {
                enc_type: static_type,
                reason: "message content is not a byte slice".into(),
            });
        };
        let from = sender_encryption_jid(info);
        let plaintext = if static_type == "msmsg" {
            self.decrypt_bot_message(info, content)
                .await
                .map_err(|reason| DecryptError::Decrypt { enc_type: static_type, reason })?
        } else {
            let store = DeviceStore::new(self.device().await);
            match static_type {
                "pkmsg" if attr_text(enc, "state").as_deref() == Some("false") => {
                    tracing::debug!("not storing session from stateless prekey message from {from}");
                    self.decrypt_dm(&SessionlessSignalStore(store), &from, content, true).await
                }
                "pkmsg" | "msg" => self.decrypt_dm(&store, &from, content, static_type == "pkmsg").await,
                _ => {
                    let name = SenderKeyName::new(info.chat.to_string(), SignalAddress::from_jid(&from));
                    GroupCipher::new(&store, name).decrypt(content).await
                }
            }
            .map_err(|err| DecryptError::from_signal(static_type, err))?
        };
        let version = attr_int(enc, "v").and_then(|v| i32::try_from(v).ok()).unwrap_or(0);
        unpad_message(&plaintext, version).map_err(|err| DecryptError::Decrypt {
            enc_type: static_type,
            reason: format!("failed to unpad message: {err}"),
        })
    }

    /// Go `decryptDM` — a pairwise `pkmsg`/`msg`. With auto-trust on, an
    /// untrusted identity on a `pkmsg` is cleared and the decrypt retried
    /// (Go `clearUntrustedIdentity`).
    async fn decrypt_dm<S: SignalStore>(
        &self,
        store: &S,
        from: &Jid,
        content: &[u8],
        is_pre_key: bool,
    ) -> Result<Vec<u8>, SignalError> {
        let address = SignalAddress::from_jid(from);
        let cipher = SessionCipher::new(store, address.clone());
        if !is_pre_key {
            return cipher.decrypt(&SignalMessage::deserialize(content)?).await;
        }
        let message = PreKeySignalMessage::deserialize(content)?;
        match cipher.decrypt_message(&message).await {
            Err(err) if err.is_untrusted_identity() && self.is_auto_trust_identity() => {
                tracing::warn!(
                    "got {err} while trying to decrypt prekey message from {from}, clearing stored identity and retrying"
                );
                store.delete_identity(&address).await?;
                store.delete_session(&address).await?;
                // TODO(Phase 5): dispatch `IdentityChange` like Go's `clearUntrustedIdentity`.
                cipher.decrypt_message(&message).await
            }
            other => other,
        }
    }

    /// The decrypted half of Go's `decryptMessages` loop + `handleDecryptedMessage`:
    /// decode the protobuf (`v="2"`), process a sender-key distribution
    /// message and emit [`WaEvent::Message`].
    async fn handle_decrypted_plaintext(&self, info: &MessageInfo, enc: &Node, plaintext: &[u8]) {
        let retry_count = attr_int(enc, "count").and_then(|c| i32::try_from(c).ok()).unwrap_or(0);
        match attr_int(enc, "v").unwrap_or(0) {
            2 => {}
            3 => {
                // TODO(Phase 5): `handleDecryptedArmadillo` (FB/Armadillo messages).
                tracing::warn!("armadillo (v3) message {} from {} is not handled yet", info.id, info.source_string());
                return;
            }
            other => {
                tracing::warn!("unknown version {other} in decrypted message from {}", info.source_string());
                return;
            }
        }
        let raw = match WaMessage::decode(plaintext) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!("error unmarshaling decrypted message from {}: {err}", info.source_string());
                return;
            }
        };
        let mut message = Message { info: info.clone(), raw_message: Some(raw), retry_count, ..Default::default() };
        message.unwrap_raw();
        if let Some(skdm) = message
            .raw_message
            .as_ref()
            .map(|raw| raw.device_sent_message.as_ref().and_then(|d| d.message.as_deref()).unwrap_or(raw))
            .and_then(|msg| msg.sender_key_distribution_message.as_ref())
            .and_then(|skdm| skdm.axolotl_sender_key_distribution_message.as_deref())
        {
            if info.is_group {
                self.handle_sender_key_distribution_message(&info.chat, &sender_encryption_jid(info), skdm).await;
            } else {
                tracing::warn!("got sender key distribution message in non-group chat from {}", info.sender);
            }
        }
        self.dispatch_event(message_event(&message, retry_count));
        // Go `handleProtocolMessage` runs on the *unwrapped* message and only
        // for protocol messages we sent; the history-sync arm acks the
        // protocol message and emits the notification (no blob download and no
        // store backfill). LID migration and placeholder resends are also handled here.
        if let Some(unwrapped) = &message.message {
            self.maybe_handle_history_sync(info, unwrapped).await;
            self.handle_protocol_message(info, unwrapped).await;
        }
    }

    /// Go `handleSenderKeyDistributionMessage` — store the sender's group chain.
    async fn handle_sender_key_distribution_message(&self, chat: &Jid, from: &Jid, axolotl_skdm: &[u8]) {
        let name = SenderKeyName::new(chat.to_string(), SignalAddress::from_jid(from));
        let skdm = match SenderKeyDistributionMessage::deserialize(axolotl_skdm) {
            Ok(skdm) => skdm,
            Err(err) => {
                tracing::error!("failed to parse sender key distribution message from {from} for {chat}: {err}");
                return;
            }
        };
        let store = DeviceStore::new(self.device().await);
        match GroupSessionBuilder::new(&store).process(&name, &skdm).await {
            Ok(()) => tracing::debug!("processed sender key distribution message from {from} in {chat}"),
            Err(err) => {
                tracing::error!("failed to process sender key distribution message from {from} for {chat}: {err}")
            }
        }
    }

    /// Go `decryptBotMessage` — decrypt an `msmsg` (bot) message with the message secret.
    async fn decrypt_bot_message(&self, info: &MessageInfo, content: &[u8]) -> Result<Vec<u8>, String> {
        use prost::Message as ProstMessage;
        let ms_msg = wa_proto::wa_web_protobufs_e2e::MessageSecretMessage::decode(content)
            .map_err(|e| format!("failed to unmarshal MessageSecretMessage: {e}"))?;
        let enc_iv = ms_msg.enc_iv.as_deref().ok_or("missing enc_iv in MessageSecretMessage")?;
        let enc_payload = ms_msg.enc_payload.as_deref().ok_or("missing enc_payload in MessageSecretMessage")?;
        let target_sender = if info.msg_meta_info.target_sender.is_empty() {
            if info.sender.server == crate::types::jid::BOT_SERVER {
                self.device.lock().await.lid.clone()
            } else {
                self.device.lock().await.get_jid()
            }
        } else {
            info.msg_meta_info.target_sender.clone()
        };
        let target_id = info.msg_meta_info.target_id.as_str();
        if target_id.is_empty() {
            return Err("missing target_id for bot message".into());
        }
        let device = self.device.lock().await;
        let (secret, real_sender) = device
            .msg_secrets
            .get_message_secret(&info.chat, &target_sender, target_id)
            .await
            .map_err(|e| format!("failed to get message secret for {target_id}: {e}"))?
            .ok_or_else(|| format!("message secret for {target_id} not found"))?;
        drop(device);
        let decrypt_id = if matches!(info.msg_bot_info.edit_type.as_str(), "inner" | "last")
            && !info.msg_bot_info.edit_target_id.is_empty()
        {
            info.msg_bot_info.edit_target_id.as_str()
        } else {
            info.id.as_str()
        };
        let try_decrypt = |msg_id: &str| {
            crate::crypto::msgsecret::decrypt_bot_message(
                &secret,
                enc_iv,
                enc_payload,
                msg_id,
                &real_sender.to_string(),
                &info.sender.to_string(),
            )
            .map_err(|e| format!("failed to decrypt secret message: {e}"))
        };
        match try_decrypt(decrypt_id) {
            Ok(pt) => Ok(pt),
            Err(err) if decrypt_id != info.id.as_str() => match try_decrypt(info.id.as_str()) {
                Ok(pt) => {
                    tracing::debug!(
                        "decrypted bot message {} using own ID instead of edit target ID {decrypt_id}",
                        info.id.as_str()
                    );
                    Ok(pt)
                }
                Err(_) => Err(err),
            },
            Err(err) => Err(err),
        }
    }

    /// Go `storeLIDSyncMessage` — decode a `LIDMigrationMappingSyncPayload` and store PN→LID mappings.
    pub(crate) async fn store_lid_sync_message(&self, payload: &[u8]) {
        use prost::Message as ProstMessage;
        let decoded =
            match wa_proto::wa_web_protobuf_lid_migration_sync_payload::LidMigrationMappingSyncPayload::decode(payload)
            {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!("failed to unmarshal LID migration sync payload: {err}");
                    return;
                }
            };
        let mut pairs: Vec<crate::store::traits::LidMapping> = Vec::new();
        for m in &decoded.pn_to_lid_mappings {
            let pn = m.pn;
            let lid = m.assigned_lid;
            if pn == 0 || lid == 0 {
                continue;
            }
            let Ok(pn_jid) = crate::types::jid::Jid::parse(&format!("{pn}@s.whatsapp.net")) else { continue };
            let Ok(lid_jid) = crate::types::jid::Jid::parse(&format!("{lid}@lid")) else { continue };
            pairs.push(crate::store::traits::LidMapping { lid: lid_jid, pn: pn_jid });
        }
        if !pairs.is_empty() {
            let lids = self.device.lock().await.lids.clone();
            if let Err(err) = lids.put_many_lid_mappings(&pairs).await {
                tracing::warn!("failed to store PN-LID mappings from sync message: {err}");
            } else {
                tracing::debug!("stored {} PN-LID mappings from sync message", pairs.len());
            }
        }
        if let Some(ts) = decoded.chat_db_migration_timestamp {
            let mut device = self.device.lock().await;
            device.lid_migration_timestamp = ts as i64;
            let mut to_save = device.clone();
            drop(device);
            if let Err(err) = to_save.save().await {
                tracing::warn!("failed to save chat DB LID migration timestamp {ts}: {err}");
            }
        }
    }

    /// Go `handlePlaceholderResendResponse` — decode `WebMessageInfo` bytes and dispatch messages.
    pub(crate) async fn handle_placeholder_resend_response(
        &self,
        msg: &wa_proto::wa_web_protobufs_e2e::PeerDataOperationRequestResponseMessage,
    ) -> bool {
        let req_id = msg.stanza_id.clone().unwrap_or_default();
        let parts = &msg.peer_data_operation_result;
        tracing::debug!("handling response to placeholder resend request {req_id} with {} items", parts.len());
        let ok = true;
        for (i, part) in parts.iter().enumerate() {
            let Some(resp) = part.placeholder_message_resend_response.as_ref() else {
                tracing::warn!("missing response in item #{} of response to {req_id}", i + 1);
                continue;
            };
            let Some(bytes) = resp.web_message_info_bytes.as_deref() else {
                tracing::warn!("missing web_message_info_bytes in item #{} of response to {req_id}", i + 1);
                continue;
            };
            let web_msg = match wa_proto::wa_web_protobufs_web::WebMessageInfo::decode(bytes) {
                Ok(m) => m,
                Err(err) => {
                    tracing::warn!("failed to unmarshal web message in item #{} of response to {req_id}: {err}", i + 1);
                    continue;
                }
            };
            let mut msg_evt = self.parse_web_message(&web_msg).await;
            msg_evt.info.unavailable_request_id = req_id.clone();
            let evt = message_event(&msg_evt, 0);
            self.dispatch_event(evt);
            // Go returns false when dispatch fails; here dispatch is sync so always ok.
        }
        ok
    }

    /// Go `handleProtocolMessage` — LID migration, placeholder resends, app-state key shares.
    pub(crate) async fn handle_protocol_message(
        &self,
        info: &MessageInfo,
        message: &wa_proto::wa_web_protobufs_e2e::Message,
    ) -> bool {
        let Some(protocol) = message.protocol_message.as_deref() else { return true };
        if !info.is_from_me {
            return true;
        }
        if let Some(lid_sync) = protocol.lid_migration_mapping_sync_message.as_ref()
            && let Some(payload) = lid_sync.encoded_mapping_payload.as_deref()
        {
            self.store_lid_sync_message(payload).await;
        }
        if info.sender.device == 0
            && let Some(peer_resp) = protocol.peer_data_operation_request_response_message.as_ref()
        {
            match peer_resp.peer_data_operation_request_type {
                Some(t)
                    if t == wa_proto::wa_web_protobufs_e2e::PeerDataOperationRequestType::PlaceholderMessageResend
                        as i32 =>
                {
                    return self.handle_placeholder_resend_response(peer_resp).await;
                }
                _ => {}
            }
        }
        true
    }

    /// Ack a node, logging (never propagating) a failure.
    async fn nack(&self, node: &Node, error: Option<i32>) {
        if let Err(err) = self.send_ack(node, error).await {
            tracing::debug!("ack for {} {} failed: {err}", node.tag, node.id_string());
        }
    }
}

/// Go `parseMsgBotInfo` — the `<bot>` child.
fn parse_msg_bot_info(bot: &Node) -> MsgBotInfo {
    let edit_type = attr_text(bot, "edit").unwrap_or_default();
    let is_edit = matches!(edit_type.as_str(), "inner" | "last" | "full");
    MsgBotInfo {
        edit_target_id: if is_edit {
            MessageID::new(attr_text(bot, "edit_target_id").unwrap_or_default())
        } else {
            MessageID::default()
        },
        edit_sender_timestamp_ms: if is_edit { attr_int(bot, "sender_timestamp_ms").unwrap_or(0) } else { 0 },
        edit_type,
        client_thread_id: attr_text(bot, "client_thread_id").unwrap_or_default(),
    }
}

/// Go `parseMsgMetaInfo` — the attributes of the `<meta>` child.
fn parse_msg_meta_info(meta: &Node) -> MsgMetaInfo {
    MsgMetaInfo {
        target_id: MessageID::new(attr_text(meta, "target_id").unwrap_or_default()),
        target_sender: attr_jid(meta, "target_sender_jid").unwrap_or_default(),
        target_chat: attr_jid(meta, "target_chat_jid").unwrap_or_default(),
        deprecated_lid_session: match meta.attrs.get("deprecated_lid_session") {
            Some(crate::binary::node::AttrValue::Bool(value)) => Some(*value),
            Some(other) => Some(other.to_string() == "true"),
            None => None,
        },
        thread_message_id: MessageID::new(attr_text(meta, "thread_msg_id").unwrap_or_default()),
        thread_message_sender_jid: attr_jid(meta, "thread_msg_sender_jid").unwrap_or_default(),
    }
}

/// Helper used in log lines: the node's `id` attribute, if any.
trait NodeId {
    fn id_string(&self) -> String;
}

impl NodeId for Node {
    fn id_string(&self) -> String {
        attr_text(self, "id").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_node() -> Node {
        Node::new("message")
            .attr("id", "3EB0")
            .attr("from", "120363@g.us")
            .attr("participant", "48123456789:7@s.whatsapp.net")
            .attr("participant_pn", "48123456789@s.whatsapp.net")
            .attr("t", "1700000000")
            .attr("notify", "Al")
            .attr("category", "peer")
    }

    #[test]
    fn bot_info_parses_edits_only_for_edit_types() {
        let inner = parse_msg_bot_info(
            &Node::new("bot").attr("edit", "inner").attr("edit_target_id", "T1").attr("sender_timestamp_ms", "1700"),
        );
        assert_eq!(inner.edit_type, "inner");
        assert_eq!(inner.edit_target_id.as_str(), "T1");
        assert_eq!(inner.edit_sender_timestamp_ms, 1700);

        let first = parse_msg_bot_info(&Node::new("bot").attr("edit", "first").attr("edit_target_id", "T1"));
        assert_eq!(first.edit_type, "first");
        assert!(first.edit_target_id.is_empty(), "non-edit types carry no target id");
    }

    #[test]
    fn meta_info_parses_every_attribute() {
        let meta = Node::new("meta")
            .attr("target_id", "TID")
            .attr("target_sender_jid", "1@s.whatsapp.net")
            .attr("target_chat_jid", "2@s.whatsapp.net")
            .attr("deprecated_lid_session", true)
            .attr("thread_msg_id", "THREAD")
            .attr("thread_msg_sender_jid", "3@lid");
        let info = parse_msg_meta_info(&meta);
        assert_eq!(info.target_id.as_str(), "TID");
        assert_eq!(info.target_sender.to_string(), "1@s.whatsapp.net");
        assert_eq!(info.target_chat.to_string(), "2@s.whatsapp.net");
        assert_eq!(info.deprecated_lid_session, Some(true));
        assert_eq!(info.thread_message_id.as_str(), "THREAD");
        assert_eq!(info.thread_message_sender_jid.to_string(), "3@lid");
    }

    #[test]
    fn attr_helpers_read_every_attr_variant() {
        let node = group_node().attr("server_id", 42i64).attr("peer", "9@s.whatsapp.net".parse::<Jid>().unwrap());
        assert_eq!(attr_text(&node, "id").as_deref(), Some("3EB0"));
        assert_eq!(attr_int(&node, "t"), Some(1_700_000_000));
        assert_eq!(attr_int(&node, "server_id"), Some(42));
        assert_eq!(attr_jid(&node, "peer").map(|j| j.to_string()), Some("9@s.whatsapp.net".to_owned()));
        assert_eq!(attr_jid(&node, "participant").map(|j| j.device), Some(7));
        assert!(attr_jid(&node, "missing").is_none());
    }

    #[test]
    fn decrypt_error_maps_signal_errors() {
        let mode = |err| DecryptError::from_signal("pkmsg", err).fail_mode();
        assert_eq!(mode(SignalError::NoSession("a:0".into())), "no_session");
        assert_eq!(mode(SignalError::NoSenderKeyForUser("g".into())), "sender_key");
        assert_eq!(mode(SignalError::BadMac), "decrypt");
        assert_eq!(mode(SignalError::UntrustedIdentity("a:0".into())), "decrypt");
        assert!(matches!(
            DecryptError::from_signal("msg", SignalError::DuplicateMessage(3)),
            DecryptError::OldCounter { enc_type: "msg", .. }
        ));
        assert!(matches!(
            DecryptError::from_signal("msg", SignalError::OldCounter { current: 5, received: 1 }),
            DecryptError::OldCounter { .. }
        ));
        assert_eq!(DecryptError::BotMessageNeedsSecret.fail_mode(), "decrypt");
        assert_eq!(DecryptError::UnhandledEncType("nope".into()).fail_mode(), "decrypt");
    }

    mod signal_path {
        use std::sync::Arc;

        use super::*;
        use crate::client::{ClientConfig, EventSink};
        use crate::crypto::keys::pad_message;
        use crate::crypto::signal::store::memory::MemoryStore;
        use crate::crypto::signal::{PreKeyBundle, SessionBuilder};
        use crate::store::sqlite::Container;

        type Seen = Arc<std::sync::Mutex<Vec<WaEvent>>>;

        const ALICE: &str = "4911";
        const GROUP: &str = "120363000000000001@g.us";

        /// A paired, saved client (the in-memory SQLite store backs Signal).
        async fn client() -> (Arc<Client>, Container, Seen) {
            let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            let events: EventSink = Arc::new(move |event| sink.lock().expect("events lock").push(event));
            let container = Container::open_memory().await.expect("memory");
            let mut device = container.new_device();
            device.id = Some(Jid::new_ad("48111111111", 0, 0));
            device.account = Some(wa_proto::wa_adv::AdvSignedDeviceIdentity {
                details: Some(b"details".to_vec()),
                account_signature_key: Some(vec![1; 32]),
                account_signature: Some(vec![2; 64]),
                device_signature: Some(vec![3; 64]),
            });
            device.save().await.expect("save device");
            (Client::new(device, ClientConfig::default(), events), container, seen)
        }

        async fn bundle_of(client: &Client) -> PreKeyBundle {
            let device = client.device().await;
            let one_time = device.pre_keys.get_or_gen_pre_keys(1).await.expect("prekeys").remove(0);
            PreKeyBundle {
                registration_id: device.registration_id,
                device_id: 0,
                pre_key_id: Some(one_time.key_id),
                pre_key_public: Some(one_time.key_pair.public),
                signed_pre_key_id: device.signed_pre_key.key_id,
                signed_pre_key_public: device.signed_pre_key.key_pair.public,
                signed_pre_key_signature: device.signed_pre_key.signature.expect("signed"),
                identity_key: device.identity_key.public,
            }
        }

        fn bob_address() -> SignalAddress {
            SignalAddress::new("48111111111", 0)
        }

        fn alice_address() -> SignalAddress {
            SignalAddress::new(ALICE, 0)
        }

        fn text(body: &str) -> Vec<u8> {
            pad_message(&WaMessage { conversation: Some(body.into()), ..Default::default() }.encode_to_vec())
        }

        fn enc(enc_type: &str, bytes: &[u8]) -> Node {
            Node::new("enc").attr("type", enc_type.to_owned()).attr("v", "2").with_content(bytes.to_vec())
        }

        fn dm_info() -> MessageInfo {
            MessageInfo {
                chat: Jid::new(ALICE, crate::types::jid::DEFAULT_USER_SERVER),
                sender: Jid::new(ALICE, crate::types::jid::DEFAULT_USER_SERVER),
                id: MessageID::new("DM1"),
                ..Default::default()
            }
        }

        fn conversation(plaintext: &[u8]) -> Option<String> {
            WaMessage::decode(plaintext).expect("protobuf").conversation
        }

        fn texts(seen: &Seen) -> Vec<String> {
            seen.lock()
                .expect("events lock")
                .iter()
                .filter_map(|event| match event {
                    WaEvent::Message { text, .. } => text.clone(),
                    _ => None,
                })
                .collect()
        }

        /// Alice → client `pkmsg`, client → Alice reply, Alice → client `msg`.
        #[tokio::test]
        async fn pkmsg_then_msg_round_trip_through_decrypt_enc_node() {
            let (client, _container, _seen) = client().await;
            let alice = MemoryStore::new(4242);
            let bundle = bundle_of(&client).await;
            let one_time_id = bundle.pre_key_id.expect("one-time key");
            SessionBuilder::new(&alice, bob_address()).build_session(&bundle).await.expect("build session");

            let first = SessionCipher::new(&alice, bob_address()).encrypt(&text("hello")).await.expect("encrypt");
            assert_eq!(first.enc_type(), "pkmsg");
            let plaintext = client
                .decrypt_enc_node(&dm_info(), &enc("pkmsg", first.serialize()), "pkmsg")
                .await
                .expect("decrypt pkmsg");
            assert_eq!(conversation(&plaintext).as_deref(), Some("hello"));
            let device = client.device().await;
            assert!(device.sessions.has_session(&alice_address().to_string()).await.expect("has session"));
            assert!(device.pre_keys.get_pre_key(one_time_id).await.expect("get").is_none(), "one-time key used");

            // Replaying the same pkmsg is an old counter, not a retry.
            let replay = client.decrypt_enc_node(&dm_info(), &enc("pkmsg", first.serialize()), "pkmsg").await;
            assert!(matches!(replay, Err(DecryptError::OldCounter { .. })), "{replay:?}");

            // The client answers, which completes Alice's session.
            let reply = SessionCipher::new(&DeviceStore::new(device), alice_address())
                .encrypt(&pad_message(b"ack"))
                .await
                .expect("reply");
            SessionCipher::new(&alice, bob_address()).decrypt_bytes(reply.serialize()).await.expect("alice decrypt");

            let second = SessionCipher::new(&alice, bob_address()).encrypt(&text("again")).await.expect("encrypt");
            assert_eq!(second.enc_type(), "msg");
            let plaintext =
                client.decrypt_enc_node(&dm_info(), &enc("msg", second.serialize()), "msg").await.expect("msg");
            assert_eq!(conversation(&plaintext).as_deref(), Some("again"));

            // A different device has no session for Alice.
            let (stranger, _c, _s) = super::signal_path::client().await;
            let err = stranger.decrypt_enc_node(&dm_info(), &enc("msg", second.serialize()), "msg").await;
            assert_eq!(err.as_ref().map_err(DecryptError::fail_mode), Err("no_session"));
        }

        #[tokio::test]
        async fn stateless_pkmsg_stores_no_session() {
            let (client, _container, _seen) = client().await;
            let alice = MemoryStore::new(4243);
            SessionBuilder::new(&alice, bob_address()).build_session(&bundle_of(&client).await).await.expect("build");
            let ciphertext = SessionCipher::new(&alice, bob_address()).encrypt(&text("once")).await.expect("encrypt");
            let node = enc("pkmsg", ciphertext.serialize()).attr("state", "false");
            let plaintext = client.decrypt_enc_node(&dm_info(), &node, "pkmsg").await.expect("decrypt");
            assert_eq!(conversation(&plaintext).as_deref(), Some("once"));
            let device = client.device().await;
            assert!(!device.sessions.has_session(&alice_address().to_string()).await.expect("has session"));
        }

        /// A group message node carrying the SKDM (`pkmsg`) and the group
        /// text (`skmsg`) — the way WhatsApp delivers a first group message.
        #[tokio::test]
        async fn group_message_with_skdm_decrypts_end_to_end() {
            let (client, _container, seen) = client().await;
            let alice = MemoryStore::new(4244);
            SessionBuilder::new(&alice, bob_address()).build_session(&bundle_of(&client).await).await.expect("build");

            let name = SenderKeyName::new(GROUP, alice_address());
            let skdm = GroupSessionBuilder::new(&alice).create(&name).await.expect("create skdm");
            let carrier = WaMessage {
                sender_key_distribution_message: Some(wa_proto::wa_web_protobufs_e2e::SenderKeyDistributionMessage {
                    group_id: Some(GROUP.into()),
                    axolotl_sender_key_distribution_message: Some(skdm.serialized.clone()),
                }),
                ..Default::default()
            };
            let pkmsg = SessionCipher::new(&alice, bob_address())
                .encrypt(&pad_message(&carrier.encode_to_vec()))
                .await
                .expect("encrypt skdm");
            let skmsg = GroupCipher::new(&alice, name).encrypt(&text("hi group")).await.expect("group encrypt");

            let node = Node::new("message")
                .attr("id", "GRP1")
                .attr("from", GROUP)
                .attr("participant", format!("{ALICE}@s.whatsapp.net"))
                .attr("t", "1700000000")
                .with_content(vec![enc("pkmsg", pkmsg.serialize()), enc("skmsg", &skmsg.serialized)]);
            assert!(client.handle_message_node(&node).await);

            assert_eq!(texts(&seen), ["hi group"]);
            assert!(
                !seen.lock().expect("events lock").iter().any(|e| matches!(e, WaEvent::UndecryptableMessage { .. })),
                "nothing is undecryptable"
            );
        }

        #[tokio::test]
        async fn skmsg_without_sender_key_is_unavailable() {
            let (client, _container, seen) = client().await;
            let message = crate::crypto::signal::SenderKeyMessage::new(1, 0, vec![0; 16], &[3u8; 32]);
            let node = Node::new("message")
                .attr("id", "GRP2")
                .attr("from", GROUP)
                .attr("participant", format!("{ALICE}@s.whatsapp.net"))
                .attr("t", "1700000000")
                .with_content(vec![enc("skmsg", &message.serialized)]);
            client.handle_message_node(&node).await;
            let events = seen.lock().expect("events lock").clone();
            let [WaEvent::UndecryptableMessage { decrypt_fail_mode, is_unavailable, .. }] = events.as_slice() else {
                panic!("expected one UndecryptableMessage, got {events:?}");
            };
            assert_eq!(decrypt_fail_mode, "sender_key");
            assert!(is_unavailable, "skmsg-only with no sender key is reported unavailable (Go)");
        }

        #[tokio::test]
        async fn msmsg_warns_and_needs_the_message_secret() {
            let (client, _container, seen) = client().await;
            let info = MessageInfo {
                sender: Jid::new("867051314767696", crate::types::jid::BOT_SERVER),
                id: MessageID::new("BOT1"),
                ..Default::default()
            };
            let err = client.decrypt_enc_node(&info, &enc("msmsg", &[1, 2, 3]), "msmsg").await;
            assert!(matches!(err, Err(DecryptError::Decrypt { enc_type: "msmsg", .. })));

            let node = Node::new("message")
                .attr("id", "BOT2")
                .attr("from", "867051314767696@bot")
                .attr("t", "1700000000")
                .with_content(vec![enc("msmsg", &[1, 2, 3])]);
            client.handle_message_node(&node).await;
            let events = seen.lock().expect("events lock").clone();
            let [WaEvent::UndecryptableMessage { decrypt_fail_mode, .. }] = events.as_slice() else {
                panic!("expected one UndecryptableMessage, got {events:?}");
            };
            assert_eq!(decrypt_fail_mode, "decrypt");
        }

        #[tokio::test]
        async fn sender_encryption_jid_prefers_the_lid_alt() {
            let mut info = dm_info();
            assert_eq!(sender_encryption_jid(&info), info.sender);
            info.sender_alt = Jid::new("99", crate::types::jid::HIDDEN_USER_SERVER);
            assert_eq!(sender_encryption_jid(&info).to_string(), "99@lid");
        }
    }
}
