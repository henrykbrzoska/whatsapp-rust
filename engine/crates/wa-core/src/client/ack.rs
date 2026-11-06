// Ported from the Go client `receipt.go` (acknowledgements), © Tulir Asokan,
// MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Acknowledgements — the `<ack>` node and the `NACK_*` error codes
//! (`Go receipt.go`: `sendAck`, `maybeDeferredAck`,
//! `buildBaseReceipt`).
//!
//! Every inbound `<message>`/`<status>`/`<appdata>` is answered with exactly
//! one `<ack>`: either a plain ack (success) or one carrying an `error`
//! attribute with a [`nack`] code. Go's `SynchronousAck` mode is not ported
//! yet — [`Client::maybe_deferred_ack`] therefore acks immediately, which is
//! Go's behaviour with `SynchronousAck == false`.

use crate::binary::node::{AttrValue, Attrs, Node};

use super::{Client, ClientError};

/// Go `NackSoftUnknown`-style "no error" marker: a plain ack. Not part of Go's
/// `Nack*` const block, but the value `sendAck` receives for a success.
pub const NACK_SOFT_UNKNOWN: i32 = 0;
/// `NackParsingError` — the `<message>` node's attributes could not be parsed.
pub const NACK_PARSING_ERROR: i32 = 487;
/// `NackUnrecognizedStanza` — no `<enc>` child we could handle.
pub const NACK_UNRECOGNIZED_STANZA: i32 = 488;
/// `NackUnrecognizedStanzaClass` — the node class (tag) is unknown.
pub const NACK_UNRECOGNIZED_STANZA_CLASS: i32 = 489;
/// `NackUnrecognizedStanzaType` — the stanza `type` attribute is unknown.
pub const NACK_UNRECOGNIZED_STANZA_TYPE: i32 = 490;
/// `NackInvalidProtobuf` — the decrypted payload was not a valid protobuf.
pub const NACK_INVALID_PROTOBUF: i32 = 491;
/// `NackInvalidHostedCompanionStanza` — hosted-companion stanza we can't read.
pub const NACK_INVALID_HOSTED_COMPANION_STANZA: i32 = 493;
/// `NackMissingMessageSecret` — a bot (msmsg) message without a message secret.
pub const NACK_MISSING_MESSAGE_SECRET: i32 = 495;
/// `NackSignalErrorOldCounter` — the Signal ratchet counter was already used.
pub const NACK_SIGNAL_ERROR_OLD_COUNTER: i32 = 496;
/// `NackMessageDeletedOnPeer` — the message was deleted before we got it.
pub const NACK_MESSAGE_DELETED_ON_PEER: i32 = 499;
/// `NackUnhandledError` — a generic failure while handling the node.
pub const NACK_UNHANDLED_ERROR: i32 = 500;
/// `NackUnsupportedAdminRevoke` — admin revoke of a message we can't revoke.
pub const NACK_UNSUPPORTED_ADMIN_REVOKE: i32 = 550;
/// `NackUnsupportedLIDGroup` — a LID-addressed group message we can't handle.
pub const NACK_UNSUPPORTED_LID_GROUP: i32 = 551;
/// `NackDBOperationFailed` — the local database rejected the write.
pub const NACK_DB_OPERATION_FAILED: i32 = 552;

/// Go `buildBaseReceipt` — `id`/`to` plus the `recipient`/`participant`
/// passthrough every receipt (ack, delivered, read, retry) starts from.
pub fn build_base_receipt(id: &str, node: &Node) -> Attrs {
    let mut attrs = Attrs::new();
    attrs.insert("id".to_owned(), AttrValue::Text(id.to_owned()));
    if let Some(from) = node.attrs.get("from") {
        attrs.insert("to".to_owned(), from.clone());
    }
    if let Some(recipient) = node.attrs.get("recipient") {
        attrs.insert("recipient".to_owned(), recipient.clone());
    }
    if let Some(participant) = node.attrs.get("participant") {
        attrs.insert("participant".to_owned(), participant.clone());
    }
    attrs
}

/// One attribute as text, whatever its `AttrValue` variant.
pub(crate) fn attr_text(node: &Node, key: &str) -> Option<String> {
    node.attrs.get(key).map(|value| match value {
        AttrValue::Text(text) => text.clone(),
        other => other.to_string(),
    })
}

impl Client {
    /// Go `sendAck` — answer an inbound node with an `<ack>`.
    ///
    /// `error` is a [`nack`] code; `None` (or [`NACK_SOFT_UNKNOWN`]) sends a
    /// plain success ack. A failed send is logged, never propagated: an ack is
    /// best-effort by design (Go does the same).
    pub async fn send_ack(&self, node: &Node, error: Option<i32>) -> Result<(), ClientError> {
        let id = attr_text(node, "id").unwrap_or_default();
        let mut attrs = Attrs::new();
        attrs.insert("class".to_owned(), AttrValue::Text(node.tag.clone()));
        attrs.insert("id".to_owned(), AttrValue::Text(id.clone()));
        if let Some(from) = node.attrs.get("from") {
            attrs.insert("to".to_owned(), from.clone());
        }
        if let Some(participant) = node.attrs.get("participant") {
            attrs.insert("participant".to_owned(), participant.clone());
        }
        if let Some(recipient) = node.attrs.get("recipient") {
            // Go rewrites a bot recipient to its phone-number JID for
            // `<message>` nodes (`types.BotJIDMap`). TODO(Phase 5): port the
            // full bot map — until then the original recipient is echoed back.
            attrs.insert("recipient".to_owned(), recipient.clone());
        }
        if node.tag != "message"
            && let Some(kind) = node.attrs.get("type").cloned()
        {
            attrs.insert("type".to_owned(), kind);
        }
        match error {
            Some(code) if code != NACK_SOFT_UNKNOWN => {
                attrs.insert("error".to_owned(), AttrValue::Int(i64::from(code)));
            }
            _ => {}
        }
        let ack = Node::new("ack").with_attrs(attrs);
        match self.send_node(&ack).await {
            Ok(()) => Ok(()),
            Err(err) => {
                tracing::warn!("failed to send acknowledgement for {} {}: {err}", node.tag, id);
                Err(err)
            }
        }
    }

    /// Go `maybeDeferredAck` — the ack for a node whose handling may fail.
    ///
    /// Go returns a closure and, with `SynchronousAck == false`, sends the ack
    /// right away. Ported without the closure: the ack is sent immediately and
    /// a send failure is logged (Go ignores it too).
    pub async fn maybe_deferred_ack(&self, node: &Node) {
        if let Err(err) = self.send_ack(node, None).await {
            tracing::debug!("deferred ack for {} failed: {err}", node.tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nack_codes_match_go() {
        assert_eq!(NACK_SOFT_UNKNOWN, 0);
        assert_eq!(NACK_PARSING_ERROR, 487);
        assert_eq!(NACK_UNRECOGNIZED_STANZA, 488);
        assert_eq!(NACK_UNRECOGNIZED_STANZA_CLASS, 489);
        assert_eq!(NACK_UNRECOGNIZED_STANZA_TYPE, 490);
        assert_eq!(NACK_INVALID_PROTOBUF, 491);
        assert_eq!(NACK_INVALID_HOSTED_COMPANION_STANZA, 493);
        assert_eq!(NACK_MISSING_MESSAGE_SECRET, 495);
        assert_eq!(NACK_SIGNAL_ERROR_OLD_COUNTER, 496);
        assert_eq!(NACK_MESSAGE_DELETED_ON_PEER, 499);
        assert_eq!(NACK_UNHANDLED_ERROR, 500);
        assert_eq!(NACK_UNSUPPORTED_ADMIN_REVOKE, 550);
        assert_eq!(NACK_UNSUPPORTED_LID_GROUP, 551);
        assert_eq!(NACK_DB_OPERATION_FAILED, 552);
    }

    #[test]
    fn base_receipt_carries_id_to_and_the_participants() {
        let node = Node::new("message")
            .attr("id", "abc")
            .attr("from", "120363@g.us")
            .attr("participant", "48123456789@s.whatsapp.net")
            .attr("recipient", "999@s.whatsapp.net");
        let attrs = build_base_receipt("abc", &node);
        assert_eq!(attrs.get("id").map(|v| v.to_string()), Some("abc".to_owned()));
        assert_eq!(attrs.get("to").map(|v| v.to_string()), Some("120363@g.us".to_owned()));
        assert_eq!(attrs.get("participant").map(|v| v.to_string()), Some("48123456789@s.whatsapp.net".to_owned()));
        assert_eq!(attrs.get("recipient").map(|v| v.to_string()), Some("999@s.whatsapp.net".to_owned()));
        // Nothing extra crept in.
        assert_eq!(attrs.len(), 4);
    }
}
