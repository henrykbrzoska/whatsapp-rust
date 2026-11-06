// Ported from the Go client `retry.go` (the incoming retry-receipt side), © Tulir
// Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Retry receipts — a recipient could not decrypt a message we sent and asks
//! us to encrypt it again (`<receipt type="retry"><retry count=…/></receipt>`).
//!
//! The engine answers from the [`RecentMessages`] cache: the last
//! [`RECENT_MESSAGES_SIZE`] sent payloads are kept in memory so a retry does
//! not need the message DB, and the answer is a fresh `<message>` with the same
//! ID carrying a new ciphertext for that one device.
//!
//! Three Go rules are preserved:
//!
//! * the retry count from the receipt selects the strategy: `<keys>` present →
//!   rebuild the session from the bundled pre-key; count ≥ 2 →
//!   [`Client::should_recreate_session`] may drop and re-fetch; otherwise the
//!   existing session is reused;
//! * an internal per-`(sender, message)` counter drops a retry bomb after
//!   [`MAX_INCOMING_RETRIES`];
//! * a retry for a message we never sent triggers the "ask the primary phone
//!   for the message" flow ([`Client::delayed_request_message_from_phone`]).

use std::num::NonZeroUsize;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use lru::LruCache;
use prost::Message as ProstMessage;
use tokio_util::sync::CancellationToken;

use crate::binary::node::{AttrValue, Attrs, Node};
use crate::crypto::signal::address::{SenderKeyName, SignalAddress};
use crate::crypto::signal::groups::GroupSessionBuilder;
use crate::types::jid::Jid;
use crate::types::message::{MessageID, MessageInfo, WaMessage};
use crate::types::receipt::Receipt;
use wa_proto::wa_web_protobufs_e2e as wa_e2e;

use super::ack::attr_text;
use super::send::{SendRequestExtra, get_media_type_from_message, get_message_content, get_type_from_message};
use super::session::{is_auth_error_disconnect, is_disconnect_node};
use super::{Client, ClientError};

/// How many sent messages are kept in memory for retry receipts.
pub const RECENT_MESSAGES_SIZE: usize = 512;

/// How often we answer retry receipts for the same message before giving up
/// (Go's `incomingRetryRequestCounter` limit).
pub const MAX_INCOMING_RETRIES: i32 = 10;

/// Go `RequestFromPhoneDelay` — how long to wait for the sender to resend a
/// message before asking the primary phone for it.
pub const REQUEST_FROM_PHONE_DELAY: Duration = Duration::from_secs(5);

/// Go `recreateSessionTimeout` — at most one session recreation per JID per
/// hour.
pub const RECREATE_SESSION_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Go `retryFrame`: how long to wait for the reconnect before giving up on a
/// request that was interrupted by a disconnect.
pub const RETRY_FRAME_RECONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Go `addRecentMessage`: how often the store-backed retry buffer is pruned
/// (`DeleteOldOutgoingEvents` at most every 12 hours).
pub const RETRY_STORE_CLEAR_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// The `format` tag of a `<message>` payload in the store-backed retry buffer
/// (Go `"wa"`; `"fb"` — the future-proof/armadillo variant — is not sent by
/// this engine yet).
pub const RETRY_STORE_FORMAT_WA: &str = "wa";

/// The key of a [`RecentMessages`] entry: the chat plus the message ID.
type RecentMessageKey = (String, String);

/// Go `RecentMessage` — the payload, kept so it can be re-encrypted verbatim.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecentMessage {
    /// `None` for a future-proof (`fb`) message, which this engine does not
    /// send yet.
    pub wa: Option<WaMessage>,
}

impl RecentMessage {
    /// Go `RecentMessage.IsEmpty`.
    pub fn is_empty(&self) -> bool {
        self.wa.is_none()
    }
}

/// Go `recentMessagesMap` + its ring buffer — an LRU of the last
/// [`RECENT_MESSAGES_SIZE`] sent messages.
#[derive(Debug)]
pub struct RecentMessages {
    cache: LruCache<RecentMessageKey, RecentMessage>,
}

impl Default for RecentMessages {
    fn default() -> Self {
        Self::new()
    }
}

impl RecentMessages {
    /// An empty cache holding [`RECENT_MESSAGES_SIZE`] messages.
    pub fn new() -> Self {
        let capacity = NonZeroUsize::new(RECENT_MESSAGES_SIZE).expect("RECENT_MESSAGES_SIZE is not zero");
        Self { cache: LruCache::new(capacity) }
    }

    /// Remember a sent message, evicting the least recently used one.
    pub fn insert(&mut self, chat: &Jid, id: &MessageID, message: WaMessage) {
        self.cache.put((chat.to_string(), id.as_str().to_owned()), RecentMessage { wa: Some(message) });
    }

    /// The cached message for `chat`/`id`.
    pub fn get(&mut self, chat: &Jid, id: &MessageID) -> Option<RecentMessage> {
        self.cache.get(&(chat.to_string(), id.as_str().to_owned())).cloned()
    }

    /// How many messages are cached (never more than [`RECENT_MESSAGES_SIZE`]).
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Drop every cached message.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

impl Client {
    // -----------------------------------------------------------------------
    // The recent-message cache
    // -----------------------------------------------------------------------

    /// Go `Client.UseRetryMessageStore` — keep the sent-message cache in the
    /// store (`wa_retry_buffer`) instead of only in memory, so a retry receipt
    /// can still be answered after a restart.
    pub fn use_retry_message_store(&self) -> bool {
        self.use_retry_message_store.load(Ordering::SeqCst)
    }

    /// Enables the store-backed retry buffer (Go `UseRetryMessageStore`).
    pub fn set_use_retry_message_store(&self, value: bool) {
        self.use_retry_message_store.store(value, Ordering::SeqCst);
    }

    /// Go `addRecentMessage` — remember a message we just sent.
    ///
    /// With [`Client::use_retry_message_store`] the marshalled payload also
    /// goes into the store's outgoing-event buffer (pruned at most every
    /// [`RETRY_STORE_CLEAR_INTERVAL`]); the in-memory LRU stays as the fast
    /// path, exactly like Go keeps both.
    pub async fn add_recent_message(&self, to: &Jid, id: &MessageID, message: &WaMessage) {
        if self.use_retry_message_store() {
            let plaintext = message.encode_to_vec();
            let store = self.device_store().await;
            let event_buffer = store.device.event_buffer.clone();
            match event_buffer.add_outgoing_event(to, id.as_str(), RETRY_STORE_FORMAT_WA, &plaintext).await {
                Ok(()) => {
                    let mut last_clear = self.last_retry_store_clear.lock().await;
                    if last_clear.is_none_or(|previous: Instant| previous.elapsed() > RETRY_STORE_CLEAR_INTERVAL) {
                        if let Err(err) = event_buffer.delete_old_outgoing_events().await {
                            tracing::warn!("failed to clear old messages from the retry store: {err}");
                        }
                        *last_clear = Some(Instant::now());
                    }
                }
                Err(err) => tracing::warn!("failed to add {} to the retry store: {err}", id.as_str()),
            }
        }
        self.recent_messages.lock().await.insert(to, id, message.clone());
    }

    /// Go `getRecentMessage`.
    pub async fn get_recent_message(&self, to: &Jid, id: &MessageID) -> Option<RecentMessage> {
        self.recent_messages.lock().await.get(to, id)
    }

    /// How many sent messages are currently cached in memory.
    pub async fn recent_message_count(&self) -> usize {
        self.recent_messages.lock().await.len()
    }

    /// Go `parseRecentMessage` — decode a payload read back from the retry
    /// store.
    ///
    /// Only the `"wa"` format is produced by this engine; an unknown tag (or
    /// the unsupported `"fb"` one) is an error, never a silent empty message.
    pub fn parse_recent_message(format: &str, buf: &[u8]) -> Result<RecentMessage, ClientError> {
        match format {
            RETRY_STORE_FORMAT_WA => {
                let wa = WaMessage::decode(buf).map_err(|err| ClientError::Store(anyhow::anyhow!(err)))?;
                Ok(RecentMessage { wa: Some(wa) })
            }
            other => Err(ClientError::Store(anyhow::anyhow!("unknown format in retry store: {other}"))),
        }
    }

    /// Go `getMessageForRetry` — the message to re-encrypt for a receipt.
    ///
    /// Lookup order (Go's exactly): the in-memory cache for the chat, the same
    /// cache with the alternate (LID ↔ PN) JID, then — when
    /// [`Client::use_retry_message_store`] is on — the store-backed retry
    /// buffer.
    ///
    /// Returns `Ok(None)` when the message is genuinely unknown; a LID store or
    /// retry store failure is an error.
    pub async fn get_message_for_retry(
        &self,
        receipt: &Receipt,
        message_id: &MessageID,
    ) -> Result<Option<RecentMessage>, ClientError> {
        if let Some(message) = self.get_recent_message(&receipt.chat, message_id).await {
            tracing::debug!("found {} inside recent messages for retry", message_id.as_str());
            return Ok(Some(message));
        }
        let lids = self.device.lock().await.lids.clone();
        let alternate = if receipt.chat.is_lid() {
            lids.get_pn_for_lid(&receipt.chat).await.map_err(|err| ClientError::Signal(err.to_string()))?
        } else {
            lids.get_lid_for_pn(&receipt.chat).await.map_err(|err| ClientError::Signal(err.to_string()))?
        };
        let alternate = alternate.filter(|alternate| !alternate.is_empty());
        if let Some(alternate) = &alternate
            && let Some(message) = self.get_recent_message(alternate, message_id).await
        {
            tracing::debug!("found {} with the alternate chat JID for retry", message_id.as_str());
            return Ok(Some(message));
        }
        if self.use_retry_message_store() {
            let store = self.device_store().await;
            let event_buffer = store.device.event_buffer.clone();
            let alt_chat = alternate.unwrap_or_default();
            return match event_buffer.get_outgoing_event(&receipt.chat, &alt_chat, message_id.as_str()).await {
                Ok(Some((format, buf))) => {
                    tracing::debug!("found {} in the retry store", message_id.as_str());
                    Self::parse_recent_message(&format, &buf).map(Some)
                }
                Ok(None) => Ok(None),
                Err(err) => Err(ClientError::Store(err)),
            };
        }
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Retry bookkeeping
    // -----------------------------------------------------------------------

    /// Go `shouldRecreateSession` — drop the Signal session with `jid` when the
    /// receipt asks for the second time and the last recreation is older than
    /// [`RECREATE_SESSION_TIMEOUT`].
    ///
    /// Returns `Some(reason)` when the session should be rebuilt.
    pub async fn should_recreate_session(&self, retry_count: i32, jid: &Jid) -> Option<String> {
        let store = self.device_store().await;
        let address = crate::crypto::signal::address::SignalAddress::from_jid(jid);
        let has_session = match store.device.sessions.has_session(&address.to_string()).await {
            Ok(has_session) => has_session,
            Err(err) => {
                tracing::warn!("failed to check the session with {jid}: {err}");
                return None;
            }
        };
        let mut history = self.session_recreate_history.lock().await;
        if !has_session {
            history.insert(jid.clone(), Instant::now());
            return Some("we don't have a Signal session with them".to_owned());
        }
        if retry_count < 2 {
            return None;
        }
        let recreate = history.get(jid).is_none_or(|previous| previous.elapsed() > RECREATE_SESSION_TIMEOUT);
        if recreate {
            history.insert(jid.clone(), Instant::now());
            return Some("retry count > 1 and over an hour since the last recreation".to_owned());
        }
        None
    }

    /// Go `tryHandleRetryReceipt` — acknowledge the receipt, answer it and emit
    /// the outcome as an event.
    pub async fn try_handle_retry_receipt(self: &std::sync::Arc<Self>, receipt: &Receipt, node: &Node) {
        self.maybe_deferred_ack(node).await;
        let message_id = receipt.message_ids.first().cloned().unwrap_or_default();
        let result = self.handle_retry_receipt(receipt, node).await;
        match result {
            Ok(()) => {
                self.dispatch_event(crate::WaEvent::RetryReceiptResent {
                    chat: receipt.chat.to_string(),
                    sender: receipt.sender.to_string(),
                    message_id: message_id.as_str().to_owned(),
                    retry_count: self.retry_count_from_node(node),
                });
            }
            Err(err) => {
                tracing::warn!("failed to handle retry receipt for {}: {err}", message_id.as_str());
                self.dispatch_event(crate::WaEvent::RetryReceiptFailed {
                    chat: receipt.chat.to_string(),
                    sender: receipt.sender.to_string(),
                    message_id: message_id.as_str().to_owned(),
                    retry_count: self.retry_count_from_node(node),
                    error: err.to_string(),
                });
            }
        }
    }

    /// The `count` attribute of the `<retry>` child (0 when absent).
    fn retry_count_from_node(&self, node: &Node) -> i32 {
        node.optional_child_by_tag(&["retry"])
            .and_then(|retry| retry.attrs.get("count"))
            .and_then(|value| value.to_string().parse().ok())
            .unwrap_or_default()
    }

    /// Go `handleRetryReceipt` — re-encrypt the cached message for the device
    /// that asked and send it.
    pub async fn handle_retry_receipt(&self, receipt: &Receipt, node: &Node) -> Result<(), ClientError> {
        let Some(retry_child) = node.optional_child_by_tag(&["retry"]) else {
            return Err(ClientError::Signal("retry receipt without a <retry> child".to_owned()));
        };
        let message_id = MessageID::new(attr_text(retry_child, "id").unwrap_or_default());
        if message_id.is_empty() {
            return Err(ClientError::Signal("retry receipt without a message id".to_owned()));
        }
        let timestamp: i64 = attr_text(retry_child, "t").and_then(|value| value.parse().ok()).unwrap_or_default();
        let retry_count: i32 = attr_text(retry_child, "count").and_then(|value| value.parse().ok()).unwrap_or(1);

        let mut message = match self.get_message_for_retry(receipt, &message_id).await? {
            Some(message) => message,
            None => {
                tracing::debug!("couldn't find {} to answer the retry receipt", message_id.as_str());
                return Err(ClientError::NoSession(format!("cached message {} not found", message_id.as_str())));
            }
        };
        let Some(mut wa_message) = message.wa.take() else {
            return Err(ClientError::Signal("cached message has no payload".to_owned()));
        };

        let retry_key = (receipt.sender.to_string(), message_id.as_str().to_owned());
        let internal_counter = {
            let mut counters = self.incoming_retry_counter.lock().await;
            let counter = counters.entry(retry_key).or_insert(0);
            *counter += 1;
            *counter
        };
        if internal_counter >= MAX_INCOMING_RETRIES {
            tracing::warn!(
                "dropping retry request from {} for {}: internal retry counter is {internal_counter}",
                receipt.sender,
                message_id.as_str()
            );
            return Ok(());
        }

        // A group retry must carry a fresh sender-key distribution message.
        if receipt.is_group {
            let own_lid = self.own_lid().await;
            let sender = if own_lid.is_empty() { self.own_id().await } else { own_lid };
            let name = SenderKeyName::new(receipt.chat.to_string(), SignalAddress::from_jid(&sender));
            let store = self.device_store().await;
            match GroupSessionBuilder::new(&store).create(&name).await {
                Ok(skd) => {
                    wa_message.sender_key_distribution_message = Some(wa_e2e::SenderKeyDistributionMessage {
                        group_id: Some(receipt.chat.to_string()),
                        axolotl_sender_key_distribution_message: Some(skd.serialized.clone()),
                    });
                }
                Err(err) => tracing::warn!("failed to create the SKDM for the retry of {message_id}: {err}"),
            }
        } else if receipt.is_from_me {
            wa_message = WaMessage {
                device_sent_message: Some(Box::new(wa_e2e::DeviceSentMessage {
                    destination_jid: Some(receipt.chat.to_string()),
                    message: Some(Box::new(wa_message)),
                    phash: None,
                })),
                ..Default::default()
            };
        }
        let plaintext = wa_message.encode_to_vec();

        let has_keys = node.optional_child_by_tag(&["keys"]).is_some();
        let mut bundle = None;
        if has_keys {
            bundle = Some(super::send::node_to_pre_key_bundle(u32::from(receipt.sender.device), node)?);
        } else if let Some(reason) = self.should_recreate_session(retry_count, &receipt.sender).await {
            tracing::debug!("fetching pre-keys for {} to answer the retry: {reason}", receipt.sender);
            let bundles = self.fetch_pre_key_bundles(std::slice::from_ref(&receipt.sender)).await?;
            bundle = bundles.get(&receipt.sender).cloned();
            if bundle.is_none() {
                return Err(ClientError::NoSession(format!("no pre-key bundle for {}", receipt.sender)));
            }
        }

        let media_type = get_media_type_from_message(&wa_message);
        let mut enc_attrs = Attrs::new();
        if !media_type.is_empty() {
            enc_attrs.insert("mediatype".to_owned(), AttrValue::Text(media_type.to_owned()));
        }
        let identity = self.encryption_identity(&receipt.sender).await;
        let (mut encrypted, include_device_identity) =
            self.encrypt_message_for_device(&plaintext, &identity, bundle.as_ref(), &enc_attrs, None).await?;
        encrypted.attrs.insert("count".to_owned(), AttrValue::Int(i64::from(retry_count)));

        let mut attrs = Attrs::new();
        if let Some(from) = node.attrs.get("from") {
            attrs.insert("to".to_owned(), from.clone());
        }
        attrs.insert("type".to_owned(), AttrValue::Text(get_type_from_message(&wa_message).to_owned()));
        attrs.insert("id".to_owned(), AttrValue::Text(message_id.as_str().to_owned()));
        attrs.insert("t".to_owned(), AttrValue::Int(timestamp));
        if !receipt.is_group {
            attrs.insert("device_fanout".to_owned(), AttrValue::Bool(false));
        }
        for key in ["participant", "recipient", "edit"] {
            if let Some(value) = node.attrs.get(key) {
                attrs.insert(key.to_owned(), value.clone());
            }
        }

        let device_identity = if include_device_identity { self.make_device_identity_node().await } else { None };
        let content = get_message_content(
            encrypted,
            &wa_message,
            &attrs,
            include_device_identity,
            &Default::default(),
            device_identity,
        );
        let payload = Node::new("message").with_attrs(attrs).with_content(content);
        self.send_node(&payload).await?;
        tracing::debug!("sent retry #{retry_count} for {}/{}", receipt.chat, message_id.as_str());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Asking the primary phone for a message
    // -----------------------------------------------------------------------

    /// Go `delayedRequestMessageFromPhone` — wait
    /// [`REQUEST_FROM_PHONE_DELAY`] for the sender to resend the message, then
    /// ask the primary phone for it.
    ///
    /// A second request for the same message ID is ignored; the pending request
    /// can be cancelled with [`Client::cancel_delayed_request_from_phone`].
    pub async fn delayed_request_message_from_phone(self: &std::sync::Arc<Self>, info: &MessageInfo) {
        let token = CancellationToken::new();
        {
            let mut pending = self.pending_phone_rerequests.lock().await;
            if pending.contains_key(info.id.as_str()) {
                tracing::debug!("already waiting for {} before requesting it", info.id.as_str());
                return;
            }
            pending.insert(info.id.as_str().to_owned(), token.clone());
        }
        let client = std::sync::Arc::clone(self);
        let info = info.clone();
        tokio::spawn(async move {
            let cancelled = tokio::select! {
                _ = tokio::time::sleep(REQUEST_FROM_PHONE_DELAY) => false,
                _ = token.cancelled() => true,
            };
            if cancelled {
                tracing::debug!("cancelled the delayed request for {} from the phone", info.id.as_str());
            } else {
                client.immediate_request_message_from_phone(&info).await;
            }
            client.pending_phone_rerequests.lock().await.remove(info.id.as_str());
        });
    }

    /// Go `immediateRequestMessageFromPhone` — send a peer
    /// `PLACEHOLDER_MESSAGE_RESEND_REQUEST` for `info`.
    pub async fn immediate_request_message_from_phone(&self, info: &MessageInfo) {
        let message = self.build_unavailable_message_request(&info.chat, &info.sender, info.id.as_str()).await;
        let extra = SendRequestExtra { peer: true, ..Default::default() };
        match self.send_message(&info.chat, &message, extra).await {
            Ok(response) => tracing::debug!("requested {} from the phone ({})", info.id.as_str(), response.id.as_str()),
            Err(err) => tracing::warn!("failed to request {} from the phone: {err}", info.id.as_str()),
        }
    }

    /// Go `cancelDelayedRequestFromPhone` — cancel a pending request for
    /// `message_id`.
    pub async fn cancel_delayed_request_from_phone(&self, message_id: &MessageID) {
        let pending = self.pending_phone_rerequests.lock().await;
        if let Some(token) = pending.get(message_id.as_str()) {
            token.cancel();
        }
    }

    /// Go `clearDelayedMessageRequests` — cancel every pending request.
    pub async fn clear_delayed_message_requests(&self) {
        let pending = self.pending_phone_rerequests.lock().await;
        for token in pending.values() {
            token.cancel();
        }
    }

    // -----------------------------------------------------------------------
    // retryFrame — re-sending a request that a disconnect interrupted
    // -----------------------------------------------------------------------

    /// Go `retryFrame` (`request.go`) — a request was interrupted by a
    /// websocket disconnect, so wait up to [`RETRY_FRAME_RECONNECT_TIMEOUT`]
    /// for the reconnect, re-register the waiter and re-send the very same
    /// marshalled bytes.
    ///
    /// Two rules are load-bearing:
    ///
    /// * an auth disconnect (`401`, `conflict type=replaced`/`device_removed`)
    ///   is never retried — the session is gone;
    /// * the returned node is **not** validated here. Go returns the raw
    ///   response and lets the caller decide (`sendIQ` rejects a non-`iq`
    ///   response, `sendMessage` reads the `<ack>` attributes). Retrying a
    ///   `<message>` therefore never runs the `iq` check.
    ///
    /// `timeout == Duration::ZERO` waits forever for the response (Go's
    /// `timeoutChan` left at `nil`).
    pub async fn retry_frame(
        &self,
        what: &'static str,
        id: &str,
        data: &[u8],
        orig: &Node,
        timeout: Duration,
    ) -> Result<Node, ClientError> {
        if is_auth_error_disconnect(orig) {
            tracing::debug!("{id} ({what}) interrupted by an auth disconnect, not retrying");
            return Err(ClientError::Disconnected { what, id: id.to_owned() });
        }
        tracing::debug!("{id} ({what}) interrupted by a disconnect, waiting for a reconnect to retry");
        if !self.wait_for_connection(RETRY_FRAME_RECONNECT_TIMEOUT).await {
            tracing::debug!(
                "socket did not reconnect within {} s of failed {what} ({id})",
                RETRY_FRAME_RECONNECT_TIMEOUT.as_secs()
            );
            return Err(ClientError::Disconnected { what, id: id.to_owned() });
        }
        let socket = self.socket.lock().await.clone().ok_or(ClientError::NotConnected)?;
        let waiter = self.register_waiter(id).await;
        if let Err(err) = socket.send_frame(data).await {
            self.cancel_waiter(id).await;
            return Err(ClientError::Noise(err));
        }
        let response = match timeout {
            Duration::ZERO => waiter.await,
            timeout => tokio::time::timeout(timeout, waiter)
                .await
                .map_err(|_| ClientError::RequestTimeout { what, id: id.to_owned() })?,
        };
        let response = response.map_err(|_| ClientError::Disconnected { what, id: id.to_owned() })?;
        if is_disconnect_node(&response) {
            tracing::debug!("retrying {what} ({id}) was interrupted again, not retrying anymore");
            return Err(ClientError::Disconnected { what, id: id.to_owned() });
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::client::{ClientConfig, EventSink};
    use crate::store::sqlite::Container;
    use crate::types::jid::DEFAULT_USER_SERVER;

    fn seen_events() -> EventSink {
        Arc::new(|_: crate::WaEvent| {})
    }

    async fn client(use_store: bool) -> (Arc<Client>, Container) {
        let container = Container::open_memory().await.expect("in-memory container");
        let mut device = container.new_device();
        device.id = Some(Jid::new_ad("48111111111", 0, 0));
        device.account = Some(wa_proto::wa_adv::AdvSignedDeviceIdentity {
            details: Some(b"details".to_vec()),
            account_signature_key: Some(vec![1; 32]),
            account_signature: Some(vec![2; 64]),
            device_signature: Some(vec![3; 64]),
        });
        device.save().await.expect("save device");
        let client = Client::new(device, ClientConfig::default(), seen_events());
        client.set_use_retry_message_store(use_store);
        (client, container)
    }

    fn chat() -> Jid {
        Jid::new("491234567890", DEFAULT_USER_SERVER)
    }

    fn message(body: &str) -> WaMessage {
        WaMessage { conversation: Some(body.to_owned()), ..Default::default() }
    }

    fn receipt(chat: Jid) -> Receipt {
        Receipt { chat, ..Default::default() }
    }

    #[tokio::test]
    async fn the_in_memory_cache_keeps_the_last512_messages() {
        let (client, _container) = client(false).await;
        assert_eq!(client.recent_message_count().await, 0);
        client.add_recent_message(&chat(), &MessageID::new("A"), &message("hi")).await;
        let cached = client.get_recent_message(&chat(), &MessageID::new("A")).await.expect("cached");
        assert_eq!(cached.wa.as_ref().and_then(|wa| wa.conversation.as_deref()), Some("hi"));
        assert!(client.get_recent_message(&chat(), &MessageID::new("nope")).await.is_none());
        // The capacity is Go's 512-entry cache.
        assert_eq!(RECENT_MESSAGES_SIZE, 512);
        for i in 0..RECENT_MESSAGES_SIZE + 10 {
            client.add_recent_message(&chat(), &MessageID::new(format!("M{i}")), &message("x")).await;
        }
        assert_eq!(client.recent_message_count().await, RECENT_MESSAGES_SIZE);
    }

    #[tokio::test]
    async fn the_store_answers_a_retry_the_memory_cache_lost() {
        let (client, _container) = client(true).await;
        assert!(client.use_retry_message_store());
        let id = MessageID::new("STORE1");
        client.add_recent_message(&chat(), &id, &message("from the store")).await;
        // Drop the in-memory copy: only the store can answer now.
        client.recent_messages.lock().await.clear();
        assert!(client.get_recent_message(&chat(), &id).await.is_none());
        let found = client.get_message_for_retry(&receipt(chat()), &id).await.expect("lookup").expect("found");
        assert_eq!(found.wa.and_then(|wa| wa.conversation).as_deref(), Some("from the store"));
        // An unknown message stays unknown.
        assert!(
            client.get_message_for_retry(&receipt(chat()), &MessageID::new("other")).await.expect("lookup").is_none()
        );
    }

    #[tokio::test]
    async fn without_the_store_an_evicted_message_is_gone() {
        let (client, _container) = client(false).await;
        let id = MessageID::new("MEM1");
        client.add_recent_message(&chat(), &id, &message("only memory")).await;
        client.recent_messages.lock().await.clear();
        assert!(!client.use_retry_message_store());
        assert!(client.get_message_for_retry(&receipt(chat()), &id).await.expect("lookup").is_none());
    }

    #[test]
    fn parse_recent_message_round_trips_the_wa_format() {
        let encoded = message("round trip").encode_to_vec();
        let parsed = Client::parse_recent_message(RETRY_STORE_FORMAT_WA, &encoded).expect("parsed");
        assert_eq!(parsed.wa.and_then(|wa| wa.conversation).as_deref(), Some("round trip"));
        // The armadillo ("fb") format is not produced or accepted here.
        assert!(Client::parse_recent_message("fb", &encoded).is_err());
        assert!(Client::parse_recent_message(RETRY_STORE_FORMAT_WA, b"not a protobuf").is_err());
    }

    #[tokio::test]
    async fn retry_frame_never_retries_an_auth_disconnect() {
        let (client, _container) = client(false).await;
        let auth = Node::new("stream:error").attr("code", "401");
        let err = client.retry_frame("info query", "abc.1-1", b"", &auth, Duration::ZERO).await.expect_err("auth");
        assert!(matches!(err, ClientError::Disconnected { .. }));
        // A plain disconnect (no socket to wait for) fails as disconnected too.
        let err = client
            .retry_frame("message send", "abc.1-2", b"", &Node::new("xmlstreamend"), Duration::from_millis(1))
            .await
            .expect_err("disconnected");
        assert!(matches!(err, ClientError::Disconnected { .. }));
    }

    #[tokio::test]
    async fn retry_bookkeeping_matches_go() {
        let (client, _container) = client(false).await;
        let receipt = Receipt { sender: Jid::new("491234567890", DEFAULT_USER_SERVER), ..Default::default() };
        // No Signal session in the store → "recreate" on the first retry.
        let reason = client.should_recreate_session(1, &receipt.sender).await.expect("recreate");
        assert!(reason.contains("don't have a Signal session"));
        // Without a session in the store Go keeps asking for a recreation, so
        // a missing session is not once-per-hour limited.
        assert!(client.should_recreate_session(1, &receipt.sender).await.is_some());
        assert_eq!(MAX_INCOMING_RETRIES, 10);
        assert_eq!(RETRY_FRAME_RECONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(RECREATE_SESSION_TIMEOUT, Duration::from_secs(3600));
    }
}
