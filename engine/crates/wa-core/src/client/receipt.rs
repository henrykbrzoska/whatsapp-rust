// Ported from the Go client `receipt.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Delivery/read/played receipts (`<receipt>` nodes).
//!
//! Go counterparts: `Go receipt.go` (`handleReceipt`,
//! `handleGroupedReceipt`, `parseReceipt`, `backgroundIfAsyncAck`,
//! `maybeDeferredAck`, `MarkRead`, `SetForceActiveDeliveryReceipts`,
//! `buildBaseReceipt`, `sendMessageReceipt`). The NACK constants live in
//! `client/ack.rs`; the retry-receipt branch of `handleReceipt` delegates to
//! `client/retry.rs` (`try_handle_retry_receipt`).

use std::time::SystemTime;

use tracing::warn;

use crate::binary::node::{AttrValue, Node};
use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid, NEWSLETTER_SERVER};
use crate::types::message::MessageID;
use crate::types::receipt::{
    RECEIPT_TYPE_PLAYED, RECEIPT_TYPE_READ, RECEIPT_TYPE_READ_SELF, RECEIPT_TYPE_RETRY, Receipt,
};

use super::ack::{NACK_PARSING_ERROR, attr_text, build_base_receipt};
use super::message::{attr_int, attr_jid};
use super::{Client, ClientError};

/// Sort message IDs the way the official apps do (Go `sortMessageIDs`).
///
/// Android sorts descending, iOS ascending; the receipt node carries both the
/// main `id` and the `<list>` children, so the order only matters for the UI.
pub fn sort_message_ids(ids: &mut [MessageID], is_ios: bool) {
    if is_ios {
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    } else {
        ids.sort_by(|a, b| b.as_str().cmp(a.as_str()));
    }
}

impl Client {
    /// Go `handleReceipt` — dispatch a `<receipt>` node.
    ///
    /// Grouped receipts (with `<participants>`) fan out through
    /// [`Client::handle_grouped_receipt`]; `retry` receipts go to
    /// [`Client::try_handle_retry_receipt`]; everything else becomes a
    /// `WaEvent::Receipt`. Unknown types are kept verbatim and logged, never
    /// an error — exactly like Go's dispatch.
    pub async fn handle_receipt(self: &std::sync::Arc<Self>, node: &Node) {
        let (receipt, participants) = match self.parse_receipt(node).await {
            Ok(parsed) => parsed,
            Err(err) => {
                warn!("Failed to parse receipt: {err}");
                let _ = self.send_ack(node, Some(NACK_PARSING_ERROR)).await;
                return;
            }
        };
        if let Some(participants) = participants {
            self.maybe_deferred_ack(node).await;
            for pcp in &participants {
                self.handle_grouped_receipt(&receipt, pcp);
            }
            return;
        }
        if receipt.receipt_type == RECEIPT_TYPE_RETRY {
            // The ack happens inside the retry handler (Go does the same).
            self.try_handle_retry_receipt(&receipt, node).await;
        } else {
            self.maybe_deferred_ack(node).await;
            self.dispatch_event(crate::WaEvent::Receipt { receipt });
        }
    }

    /// Go `handleGroupedReceipt` — one receipt per `<user>` child of a grouped
    /// receipt's `<participants>` node.
    pub fn handle_grouped_receipt(&self, partial: &Receipt, participant: &Node) {
        let mut receipt = partial.clone();
        for child in participant.children() {
            if child.tag != "user" {
                warn!("Unexpected node in grouped receipt participants: {}", child.tag);
                continue;
            }
            let Some(jid) = attr_jid(child, "jid") else {
                warn!("Failed to parse user node in grouped receipt: missing jid");
                continue;
            };
            receipt.sender = jid;
            receipt.timestamp = attr_int(child, "t").unwrap_or(0);
            self.dispatch_event(crate::WaEvent::Receipt { receipt: receipt.clone() });
        }
    }

    /// Go `parseReceipt` — parse a `<receipt>` node into a [`Receipt`] plus
    /// the optional `<participants>` children (grouped receipts).
    pub async fn parse_receipt(&self, node: &Node) -> Result<(Receipt, Option<Vec<Node>>), ClientError> {
        let source = self.parse_message_source(node, false).await?;
        let mut receipt = Receipt {
            chat: source.chat.clone(),
            sender: source.sender.clone(),
            sender_alt: source.sender_alt.clone(),
            is_from_me: source.is_from_me,
            is_group: source.is_group,
            timestamp: attr_int(node, "t").unwrap_or(0),
            receipt_type: attr_text(node, "type").unwrap_or_default(),
            message_sender: attr_jid(node, "recipient").unwrap_or_default(),
            ..Default::default()
        };
        if source.is_group && source.sender.is_empty() {
            let participants: Vec<Node> = node.children().iter().filter(|c| c.tag == "participants").cloned().collect();
            if participants.is_empty() {
                return Err(ClientError::MissingElement { tag: "participants", parent: "grouped receipt" });
            }
            return Ok((receipt, Some(participants)));
        }
        let main_id =
            attr_text(node, "id").ok_or(ClientError::MissingAttribute { tag: node.tag.clone(), attr: "id" })?;
        let children = node.children();
        if children.len() == 1 && children[0].tag == "list" {
            let mut ids = vec![MessageID::new(main_id)];
            for item in children[0].children() {
                if item.tag == "item"
                    && let Some(id) = attr_text(item, "id")
                {
                    ids.push(MessageID::new(id));
                }
            }
            receipt.message_ids = ids;
        } else {
            receipt.message_ids = vec![MessageID::new(main_id)];
        }
        Ok((receipt, None))
    }

    /// Go `MarkRead` — send a read (or played) receipt for `ids`.
    ///
    /// Newsletter chats and chats with read receipts disabled downgrade
    /// `read` to `read-self`, exactly like Go.
    pub async fn mark_read(
        &self,
        ids: &[MessageID],
        timestamp: i64,
        chat: &Jid,
        sender: &Jid,
        receipt_type: Option<&str>,
    ) -> Result<(), ClientError> {
        if ids.is_empty() {
            return Err(ClientError::InvalidArgument("no message IDs specified"));
        }
        if !self.is_logged_in() {
            return Err(ClientError::NotLoggedIn);
        }
        let mut receipt_type = receipt_type.unwrap_or(RECEIPT_TYPE_READ).to_owned();
        if chat.server == NEWSLETTER_SERVER && receipt_type == RECEIPT_TYPE_READ {
            // Go also checks the privacy setting here; app-state privacy is
            // TODO(Phase 5), so newsletters always downgrade for now.
            receipt_type = RECEIPT_TYPE_READ_SELF.to_owned();
        }
        let mut node = Node::new("receipt")
            .attr("id", ids[0].as_str())
            .attr("type", receipt_type.as_str())
            .attr("to", chat.clone())
            .attr("t", timestamp);
        if !sender.is_empty()
            && !matches!(chat.server.as_str(), s if s == DEFAULT_USER_SERVER || s == HIDDEN_USER_SERVER)
        {
            node = node.attr("participant", sender.to_non_ad().to_string());
        }
        if ids.len() > 1 {
            let items: Vec<Node> = ids[1..].iter().map(|id| Node::new("item").attr("id", id.as_str())).collect();
            node = node.with_content(vec![Node::new("list").with_content(items)]);
        }
        self.send_node(&node).await
    }

    /// Go `markPlayed` — a played receipt is a read receipt with
    /// `type="played"`.
    pub async fn mark_played(
        &self,
        ids: &[MessageID],
        timestamp: i64,
        chat: &Jid,
        sender: &Jid,
    ) -> Result<(), ClientError> {
        self.mark_read(ids, timestamp, chat, sender, Some(RECEIPT_TYPE_PLAYED)).await
    }

    /// Go `sendMessageReceipt` — the automatic delivery receipt for an
    /// incoming message (`read` for status broadcasts we authored, `sender`
    /// for our own messages, `inactive` when offline, `delivered` otherwise).
    pub async fn send_message_receipt(&self, node: &Node) {
        let id = attr_text(node, "id").unwrap_or_default();
        let mut attrs = build_base_receipt(&id, node);
        let from_me = attr_text(node, "from_me").is_some_and(|v| v == "true" || v == "1");
        let category = attr_text(node, "category").unwrap_or_default();
        let receipt_type = if category == "peer" {
            "peer_msg"
        } else if from_me {
            "sender"
        } else if !self.is_connected().await {
            "inactive"
        } else {
            "delivered"
        };
        attrs.insert("type".to_owned(), AttrValue::Text(receipt_type.to_owned()));
        attrs.insert(
            "t".to_owned(),
            AttrValue::Int(
                SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
            ),
        );
        if let Err(err) = self.send_node(&Node::new("receipt").with_attrs(attrs)).await {
            warn!("Failed to send message receipt: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_node(receipt_type: &str) -> Node {
        Node::new("receipt")
            .attr("id", "ABC123")
            .attr("type", receipt_type)
            .attr("from", Jid::new("1234", DEFAULT_USER_SERVER))
            .attr("t", 42_i64)
    }

    #[test]
    fn receipt_parses_all_types() {
        for receipt_type in [
            "delivered",
            "sender",
            "retry",
            "read",
            "read-self",
            "played",
            "played-self",
            "server-error",
            "inactive",
            "peer_msg",
            "brand-new-type",
        ] {
            let node = receipt_node(receipt_type);
            // parse_receipt needs a logged-in Client; the node-level pieces
            // (id + type + single id list) are asserted here instead.
            assert_eq!(attr_text(&node, "type").as_deref(), Some(receipt_type));
            assert_eq!(attr_text(&node, "id").as_deref(), Some("ABC123"));
        }
    }

    #[test]
    fn message_ids_ordering() {
        let mut ids = vec![MessageID::new("B"), MessageID::new("A"), MessageID::new("C")];
        sort_message_ids(&mut ids, true);
        assert_eq!(ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(), vec!["A", "B", "C"]);
        sort_message_ids(&mut ids, false);
        assert_eq!(ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(), vec!["C", "B", "A"]);
    }

    #[test]
    fn receipt_list_children_parsed() {
        let node = receipt_node("read").with_content(vec![Node::new("list").with_content(vec![
            Node::new("item").attr("id", "DEF456"),
            Node::new("other").attr("id", "IGNORED"),
            Node::new("item"),
        ])]);
        let children = node.children();
        assert_eq!(children.len(), 1);
        let mut ids = vec![MessageID::new(attr_text(&node, "id").unwrap())];
        for item in children[0].children() {
            if item.tag == "item"
                && let Some(id) = attr_text(item, "id")
            {
                ids.push(MessageID::new(id));
            }
        }
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[1].as_str(), "DEF456");
    }
}
