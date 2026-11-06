// Ported from the Go client `types/receipt.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Go `types.ReceiptType` + `events.Receipt` — one receipt (`<receipt>`) node
//! as the engine understands it.
//!
//! Go keeps the type in a `string`-backed type and the per-receipt payload in
//! `events.Receipt`; the Rust port keeps a plain [`String`] for the type plus
//! the constants below (the wire value is what reaches the UI, so a new
//! receipt type from the server never fails to parse).

use serde::{Deserialize, Serialize};
use specta::Type;

use super::jid::Jid;
use super::message::MessageID;

/// `"delivered"` — the recipient's device received the message (one grey tick).
pub const RECEIPT_TYPE_DELIVERED: &str = "delivered";
/// `"sender"` — our own other devices acknowledge a message we sent.
pub const RECEIPT_TYPE_SENDER: &str = "sender";
/// `"retry"` — the recipient could not decrypt; re-encrypt (see `client/retry.rs`).
pub const RECEIPT_TYPE_RETRY: &str = "retry";
/// `"read"` — the recipient read the message (blue ticks).
pub const RECEIPT_TYPE_READ: &str = "read";
/// `"read-self"` — our read receipt mirrored to our other devices.
pub const RECEIPT_TYPE_READ_SELF: &str = "read-self";
/// `"played"` — a voice message was played by the recipient.
pub const RECEIPT_TYPE_PLAYED: &str = "played";
/// `"played-self"` — our played receipt mirrored to our other devices.
pub const RECEIPT_TYPE_PLAYED_SELF: &str = "played-self";
/// `"server-error"` — the server could not deliver; also used for media retry
/// requests (`client/mediaretry.rs`).
pub const RECEIPT_TYPE_SERVER_ERROR: &str = "server-error";
/// `"inactive"` — a delivery receipt from a client that is not in the
/// foreground (not rendered as ticks by the official apps).
pub const RECEIPT_TYPE_INACTIVE: &str = "inactive";
/// `"peer_msg"` — a peer (own-phone) message acknowledgement.
pub const RECEIPT_TYPE_PEER_MSG: &str = "peer_msg";

/// Every receipt type this engine knows about (`handle_receipt`'s dispatch).
/// Unknown types are logged at debug level and otherwise ignored, exactly like
/// Go's `default` branch.
pub const KNOWN_RECEIPT_TYPES: &[&str] = &[
    RECEIPT_TYPE_DELIVERED,
    RECEIPT_TYPE_SENDER,
    RECEIPT_TYPE_RETRY,
    RECEIPT_TYPE_READ,
    RECEIPT_TYPE_READ_SELF,
    RECEIPT_TYPE_PLAYED,
    RECEIPT_TYPE_PLAYED_SELF,
    RECEIPT_TYPE_SERVER_ERROR,
    RECEIPT_TYPE_INACTIVE,
    RECEIPT_TYPE_PEER_MSG,
];

/// Go `types.ReceiptType` membership check — `false` for a type this engine
/// does not handle (never an error).
pub fn is_known_receipt_type(receipt_type: &str) -> bool {
    KNOWN_RECEIPT_TYPES.contains(&receipt_type)
}

/// `true` for the receipt types that mean "the user saw this message"
/// (blue ticks / played): `read`, `read-self`, `played`, `played-self`.
pub fn is_read_receipt(receipt_type: &str) -> bool {
    matches!(receipt_type, RECEIPT_TYPE_READ | RECEIPT_TYPE_READ_SELF | RECEIPT_TYPE_PLAYED | RECEIPT_TYPE_PLAYED_SELF)
}

/// Go `events.Receipt` — one parsed `<receipt>` node.
///
/// Go embeds `types.MessageSource`; the Rust port flattens the fields the UI
/// actually needs (chat, sender, `is_from_me`, `is_group`) and keeps the
/// delivery-relevant ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct Receipt {
    /// The chat the receipt belongs to (the `from` of the node, or the `jid`
    /// of a grouped receipt's parent).
    pub chat: Jid,
    /// Who sent the receipt (the `participant`, or the chat for a DM).
    pub sender: Jid,
    /// Go `MessageSource.SenderAlt` — not carried by receipts, kept for parity.
    pub sender_alt: Jid,
    /// Go `Receipt.MessageSender` — the `recipient` attribute: the sender of
    /// the message the receipt acknowledges (group chats).
    pub message_sender: Jid,
    /// The receipt came from one of our own devices.
    pub is_from_me: bool,
    /// The chat is a group.
    pub is_group: bool,
    /// One message ID for a plain receipt, several for a `<list>` grouped one.
    pub message_ids: Vec<MessageID>,
    /// The server timestamp (`t`), in unix seconds.
    pub timestamp: i64,
    /// The `type` attribute — one of [`KNOWN_RECEIPT_TYPES`], or something the
    /// server invented (kept verbatim).
    pub receipt_type: String,
}

impl Receipt {
    /// The first message ID, or `""` (`Go` indexes `MessageIDs[0]`).
    pub fn first_message_id(&self) -> &str {
        self.message_ids.first().map(MessageID::as_str).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_types_and_read_flags() {
        assert!(is_known_receipt_type(RECEIPT_TYPE_RETRY));
        assert!(!is_known_receipt_type("brand-new"));
        assert!(is_read_receipt(RECEIPT_TYPE_READ_SELF));
        assert!(!is_read_receipt(RECEIPT_TYPE_DELIVERED));
    }

    #[test]
    fn receipt_serializes_with_camel_case() {
        let receipt = Receipt {
            chat: Jid::new("1234", super::super::jid::DEFAULT_USER_SERVER),
            sender: Jid::new("1234", super::super::jid::DEFAULT_USER_SERVER),
            message_ids: vec![MessageID::new("3EB0AA")],
            timestamp: 42,
            receipt_type: RECEIPT_TYPE_READ.to_owned(),
            ..Default::default()
        };
        let json = serde_json::to_value(&receipt).unwrap();
        assert_eq!(json["messageIds"][0], "3EB0AA");
        assert_eq!(json["receiptType"], "read");
        assert_eq!(json["timestamp"], 42);
        assert_eq!(receipt.first_message_id(), "3EB0AA");
    }
}
