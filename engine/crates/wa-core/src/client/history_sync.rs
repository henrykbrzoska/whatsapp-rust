// Ported from the Go client `message.go` (the history-sync half of
// `handleProtocolMessage`), © Tulir Asokan, MPL-2.0 — see
// THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! History sync notifications — the phone hands the companion a blob to
//! download by sending a `ProtocolMessage` that carries a
//! [`wa_e2e::HistorySyncNotification`].
//!
//! Go counterparts: the `HistorySyncNotification` arm of
//! `Go message.go::handleProtocolMessage` (queue the notification,
//! download it in `handleHistorySyncNotificationLoop`, dispatch
//! `events.HistorySync`, ack with a `hist_sync` receipt) and
//! `SendProtocolMessageReceipt`.
//!
//! The blob is an encrypted attachment like any other, so
//! [`Client::download_history_blob`] fetches it through
//! [`download`](super::download) and decodes it with
//! [`history_blob`](super::history_blob). Writing the result into wa-store is
//! the server's job (it owns the database).
//!
//! `WaEvent` lives in `types/events.rs` and is owned by the main thread, so
//! this module defines its own [`HistorySyncNotificationEvent`] and fans it out
//! over a `tokio::sync::broadcast` channel
//! ([`Client::history_sync_events`]). A cross-crate `WaEvent::HistorySync`
//! variant can replace the channel later without touching this parser.

use crate::binary::node::Node;
use crate::client::download::{Downloadable, MediaType};
use crate::client::history_blob::{DecodedHistory, decode_history_blob};
use crate::types::message::{MessageID, MessageInfo, WaMessage};
use wa_proto::wa_web_protobufs_e2e as wa_e2e;

use super::Client;

/// The blob of a history-sync notification, as a download job.
pub fn downloadable_of(notification: &wa_e2e::HistorySyncNotification) -> Downloadable {
    Downloadable {
        direct_path: notification.direct_path.clone().unwrap_or_default(),
        media_key: notification.media_key.clone().unwrap_or_default(),
        file_sha256: notification.file_sha256.clone().unwrap_or_default(),
        file_enc_sha256: notification.file_enc_sha256.clone().unwrap_or_default(),
        file_length: notification.file_length.unwrap_or_default(),
        mime_type: "application/octet-stream".to_owned(),
    }
}

/// Go `types.ReceiptTypeHistorySync` — the acknowledgement sent for a
/// `ProtocolMessage` that carries a history-sync notification.
pub const RECEIPT_TYPE_HISTORY_SYNC: &str = "hist_sync";

/// Go's `historySyncNotifications` channel holds 32 pending notifications.
pub const HISTORY_SYNC_CHANNEL_CAPACITY: usize = 32;

/// The client-local `HistorySync` event (see the module docs for why it is not
/// a `WaEvent` variant yet).
#[derive(Debug, Clone, PartialEq)]
pub struct HistorySyncNotificationEvent {
    /// The ID of the `ProtocolMessage` that carried the notification.
    pub message_id: MessageID,
    /// The chat the protocol message arrived in (normally our own chat).
    pub chat: crate::types::jid::Jid,
    /// The device that sent the protocol message (normally our own phone).
    pub sender: crate::types::jid::Jid,
    /// `ProtocolMessage.timestamp_ms` — when the phone created the notification.
    pub conversation_timestamp: i64,
    /// The notification to download (`direct_path`, `media_key`, `sync_type`…).
    pub notification: wa_e2e::HistorySyncNotification,
}

impl HistorySyncNotificationEvent {
    /// The blob's direct download path (empty when the phone did not send one).
    pub fn direct_path(&self) -> &str {
        self.notification.direct_path.as_deref().unwrap_or_default()
    }

    /// The sync type as its protobuf enum (`INITIAL_BOOTSTRAP`, `RECENT`, …).
    pub fn sync_type(&self) -> wa_e2e::HistorySyncType {
        self.notification.sync_type()
    }

    /// Whether this is a chunk of the initial history bootstrap.
    pub fn is_initial_bootstrap(&self) -> bool {
        self.sync_type() == wa_e2e::HistorySyncType::InitialBootstrap
    }
}

/// Go `handleProtocolMessage`'s first arm — pull the history-sync notification
/// out of an unwrapped message.
///
/// Go only looks at protocol messages *we* sent (`if !info.IsFromMe { return }`),
/// because the notification arrives from our own phone; anything else yields
/// `None`.
pub fn history_sync_notification(info: &MessageInfo, message: &WaMessage) -> Option<HistorySyncNotificationEvent> {
    if !info.is_from_me {
        return None;
    }
    let protocol = message.protocol_message.as_deref()?;
    let notification = protocol.history_sync_notification.clone()?;
    Some(HistorySyncNotificationEvent {
        message_id: info.id.clone(),
        chat: info.chat.clone(),
        sender: info.sender.clone(),
        conversation_timestamp: protocol.timestamp_ms.unwrap_or_default(),
        notification,
    })
}

impl Client {
    /// Download the blob a notification points at and decode it.
    ///
    /// The phone answers both the initial sync and an on-demand request with
    /// an encrypted blob; this turns one into chats and messages the caller
    /// can store.
    pub async fn download_history_blob(
        &self,
        event: &HistorySyncNotificationEvent,
    ) -> Result<DecodedHistory, super::ClientError> {
        let media = downloadable_of(&event.notification);
        let bytes = self
            .download_media(&media, MediaType::History)
            .await
            .map_err(|err| super::ClientError::Media(format!("history sync download failed: {err}")))?;
        let own = self.own_id().await;
        decode_history_blob(&bytes, &own.to_string()).map_err(super::ClientError::Media)
    }

    /// Ask the phone for the messages before `oldest`, in one chat.
    ///
    /// The request is a peer message to our own devices; the answer arrives
    /// later as another [`HistorySyncNotificationEvent`] (`ON_DEMAND`), which
    /// is why this returns as soon as the request is acknowledged.
    pub async fn request_history(&self, oldest: &MessageInfo, count: i32) -> Result<MessageID, super::ClientError> {
        let own = self.own_id().await;
        if own.is_empty() {
            return Err(super::ClientError::NotLoggedIn);
        }
        let message = self.build_history_sync_request(oldest, count).await;
        let response = self
            .send_message(
                &own.to_non_ad(),
                &message,
                crate::client::send::SendRequestExtra { peer: true, ..Default::default() },
            )
            .await?;
        Ok(response.id)
    }

    /// Subscribe to [`HistorySyncNotificationEvent`]s. Every call gets an
    /// independent receiver; a notification emitted while nobody listens is
    /// dropped (the phone re-sends on the next connect).
    pub fn history_sync_events(&self) -> tokio::sync::broadcast::Receiver<HistorySyncNotificationEvent> {
        self.history_sync_tx.subscribe()
    }

    /// How many listeners currently wait for history-sync notifications.
    pub fn history_sync_receiver_count(&self) -> usize {
        self.history_sync_tx.receiver_count()
    }

    /// Emit a [`HistorySyncNotificationEvent`] to every subscriber.
    pub(crate) fn dispatch_history_sync_notification(&self, event: HistorySyncNotificationEvent) {
        if self.history_sync_tx.send(event).is_err() {
            tracing::debug!("history sync notification received with no listener attached");
        }
    }

    /// The history-sync arm of Go `handleProtocolMessage`: ack the protocol
    /// message and emit the notification.
    ///
    /// Called from the decrypted-message path; never fails — a failed ack is
    /// logged, like Go's `go func() { … }` goroutine.
    pub(crate) async fn maybe_handle_history_sync(&self, info: &MessageInfo, message: &WaMessage) {
        let Some(event) = history_sync_notification(info, message) else {
            return;
        };
        tracing::debug!(
            "history sync notification {} of type {:?} from {}",
            event.message_id.as_str(),
            event.sync_type(),
            event.sender
        );
        if let Err(err) = self.send_protocol_message_receipt(&info.id, RECEIPT_TYPE_HISTORY_SYNC).await {
            tracing::warn!("failed to send the history sync receipt for {}: {err}", info.id.as_str());
        }
        self.dispatch_history_sync_notification(event);
    }

    /// Go `SendProtocolMessageReceipt` — a bare `<receipt>` acknowledging a
    /// protocol message (empty ID = nothing to do).
    pub async fn send_protocol_message_receipt(
        &self,
        id: &MessageID,
        receipt_type: &str,
    ) -> Result<(), super::ClientError> {
        if id.is_empty() {
            return Ok(());
        }
        let own_id = self.own_id().await.to_non_ad();
        let node = Node::new("receipt").attr("id", id.as_str()).attr("type", receipt_type).attr("to", own_id);
        self.send_node(&node).await
    }
}

#[cfg(test)]
mod tests {
    use prost::Message as _;

    use super::*;
    use crate::types::jid::Jid;

    fn protocol_message(notification: Option<wa_e2e::HistorySyncNotification>) -> WaMessage {
        WaMessage {
            protocol_message: Some(Box::new(wa_e2e::ProtocolMessage {
                history_sync_notification: notification,
                timestamp_ms: Some(1_700_000_000),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn info(is_from_me: bool) -> MessageInfo {
        MessageInfo {
            id: MessageID::new("3EB0HISTORY"),
            chat: Jid::new("48111111111", crate::types::jid::DEFAULT_USER_SERVER),
            sender: Jid::new("48122222222", crate::types::jid::DEFAULT_USER_SERVER),
            is_from_me,
            ..Default::default()
        }
    }

    fn notification() -> wa_e2e::HistorySyncNotification {
        wa_e2e::HistorySyncNotification {
            file_sha256: Some(vec![7u8; 32]),
            file_length: Some(4096),
            media_key: Some(vec![9u8; 32]),
            direct_path: Some("/v/t62.7117-24/hist".to_owned()),
            sync_type: Some(wa_e2e::HistorySyncType::Recent as i32),
            ..Default::default()
        }
    }

    #[test]
    fn notification_is_extracted_with_its_metadata() {
        let event = history_sync_notification(&info(true), &protocol_message(Some(notification()))).expect("an event");
        assert_eq!(event.message_id.as_str(), "3EB0HISTORY");
        assert_eq!(event.sender.to_string(), "48122222222@s.whatsapp.net");
        assert_eq!(event.conversation_timestamp, 1_700_000_000);
        assert_eq!(event.direct_path(), "/v/t62.7117-24/hist");
        assert_eq!(event.sync_type(), wa_e2e::HistorySyncType::Recent);
        assert!(!event.is_initial_bootstrap());
        assert_eq!(event.notification.file_length, Some(4096));
    }

    #[test]
    fn initial_bootstrap_is_recognised() {
        let mut notif = notification();
        notif.sync_type = Some(wa_e2e::HistorySyncType::InitialBootstrap as i32);
        let event = history_sync_notification(&info(true), &protocol_message(Some(notif))).expect("an event");
        assert!(event.is_initial_bootstrap());
    }

    #[test]
    fn notifications_not_from_us_are_ignored() {
        assert!(history_sync_notification(&info(false), &protocol_message(Some(notification()))).is_none());
    }

    #[test]
    fn protocol_message_without_a_notification_is_ignored() {
        assert!(history_sync_notification(&info(true), &protocol_message(None)).is_none());
        assert!(history_sync_notification(&info(true), &WaMessage::default()).is_none());
    }

    #[test]
    fn missing_direct_path_reads_as_empty() {
        let mut notif = notification();
        notif.direct_path = None;
        let event = history_sync_notification(&info(true), &protocol_message(Some(notif))).expect("an event");
        assert!(event.direct_path().is_empty());
    }

    #[tokio::test]
    async fn the_client_emits_the_notification_on_its_channel() {
        use crate::client::{Client, ClientConfig, EventSink};
        use crate::store::sqlite::Container;

        let events: EventSink = std::sync::Arc::new(|_: crate::WaEvent| {});
        let container = Container::open_memory().await.expect("in-memory container");
        let client = Client::new(container.new_device(), ClientConfig::default(), events);
        let mut rx = client.history_sync_events();
        assert_eq!(client.history_sync_receiver_count(), 1);

        client.maybe_handle_history_sync(&info(true), &protocol_message(Some(notification()))).await;
        let event = rx.try_recv().expect("a notification");
        assert_eq!(event.message_id.as_str(), "3EB0HISTORY");
        assert_eq!(event.direct_path(), "/v/t62.7117-24/hist");
        assert!(rx.try_recv().is_err(), "exactly one notification per protocol message");

        // Not from us / no notification: nothing is emitted.
        client.maybe_handle_history_sync(&info(false), &protocol_message(Some(notification()))).await;
        client.maybe_handle_history_sync(&info(true), &protocol_message(None)).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_notification_survives_a_protobuf_round_trip() {
        let raw = protocol_message(Some(notification()));
        let encoded = raw.encode_to_vec();
        let decoded = WaMessage::decode(encoded.as_slice()).expect("decode");
        let event = history_sync_notification(&info(true), &decoded).expect("an event");
        assert_eq!(event.notification.file_sha256, Some(vec![7u8; 32]));
        assert_eq!(event.notification.media_key, Some(vec![9u8; 32]));
    }
}
