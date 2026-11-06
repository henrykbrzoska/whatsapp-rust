// Ported from the Go client `retry.go` (retry receipts), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Retry receipts — ask the sender to re-encrypt a message we could not
//! decrypt (`Go retry.go`: `sendRetryReceipt`,
//! `delayedRequestMessageFromPhone`).
//!
//! The node layout is Go's exactly:
//!
//! ```text
//! <receipt id=<msg id> to=<chat> type="retry" [participant] [recipient]>
//!   <retry count=N id=<msg id> t=<timestamp> v="1"/>
//!   <registration>00 00 12 34</registration>       (4 bytes, big endian)
//!   <keys>                                          (N > 1 or forced)
//!     <type>05</type>
//!     <identity>…</identity>
//!     <key><id/><value/></key>                      (one-time prekey)
//!     <skey><id/><value/><signature/></skey>        (signed prekey)
//!     <device-identity>…</device-identity>          (adv signed identity)
//!   </keys>
//! </receipt>
//! ```
//!
//! Retry receipts are capped at [`MAX_MESSAGE_RETRIES`] per message ID; the
//! counter lives in [`Client::message_retries`] and is reset with the session
//! (Go keeps it in the `Client` too, so a reconnect drops it as well).

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use prost::Message as ProstMessage;

use crate::binary::node::Node;
use crate::types::message::{MessageID, MessageInfo};

use super::ack::{attr_text, build_base_receipt};
use super::{Client, ClientError};

/// Go's hard cap in `sendRetryReceipt`: after this many retries the sender is
/// not asked again ("Not sending any more retry receipts for …").
pub const MAX_MESSAGE_RETRIES: i32 = 5;

/// Go `ecc.DjbType` — the curve id written into the `<keys><type>` node.
pub const CURVE25519_KEY_TYPE: u8 = 0x05;

/// Go `preKeyToNode` — shared with the pre-key upload (`prekeys.go`).
pub use super::prekeys::pre_key_to_node;

impl Client {
    /// Current retry count for a message ID (0 = never asked).
    pub async fn retry_count_for(&self, id: &str) -> i32 {
        self.message_retries.lock().await.get(id).copied().unwrap_or(0)
    }

    /// Drop one message's retry counter (Go `cancelDelayedRequestFromPhone`
    /// leaves the counter alone; this is the explicit reset used by tests and
    /// the re-connect path).
    pub async fn clear_retry_count(&self, id: &str) {
        self.message_retries.lock().await.remove(id);
    }

    /// Go `AutoTrustIdentity` — when `false` (Go's zero value) a new identity
    /// key raises an `IdentityChange` instead of being trusted silently.
    pub fn is_auto_trust_identity(&self) -> bool {
        self.auto_trust_identity.load(Ordering::SeqCst)
    }

    /// Enables/disables silent identity trust. TODO(Phase 4 Task B): the Signal
    /// layer consults this through [`Client::is_auto_trust_identity`].
    pub fn set_auto_trust_identity(&self, value: bool) {
        self.auto_trust_identity.store(value, Ordering::SeqCst);
    }

    /// Go `EnableDecryptedEventBuffer` — whether decrypted plaintexts are kept
    /// so a retry can be answered from the buffer.
    pub fn enable_decrypted_event_buffer(&self) -> bool {
        self.enable_decrypted_event_buffer
    }

    /// Go `sendRetryReceipt(ctx, node, info, forceIncludeIdentity)` — the
    /// variant the message path uses (it already parsed `info`).
    pub async fn send_retry_receipt_with(
        &self,
        node: &Node,
        info: &MessageInfo,
        force_include_identity: bool,
    ) -> Result<(), ClientError> {
        let id = attr_text(node, "id").unwrap_or_else(|| info.id.as_str().to_owned());
        let children = node.children();
        let retry_count_in_msg = if children.len() == 1 && children[0].tag == "enc" {
            children[0].attrs.get("count").and_then(|value| value.to_string().parse::<i32>().ok()).unwrap_or(0)
        } else {
            0
        };

        let retry_count = {
            let mut retries = self.message_retries.lock().await;
            let counter = retries.entry(id.clone()).or_insert(0);
            *counter += 1;
            // A retry response arriving after a restart continues the sender's
            // counter instead of restarting at 1.
            if *counter == 1 && retry_count_in_msg > 0 {
                *counter = retry_count_in_msg + 1;
            }
            *counter
        };
        if retry_count >= MAX_MESSAGE_RETRIES {
            tracing::warn!("not sending any more retry receipts for {id}");
            return Ok(());
        }
        if retry_count == 1 {
            // Go schedules `delayedRequestMessageFromPhone` here (asks the
            // primary phone for the message). TODO(Phase 5): peer-message
            // request path — `SendPeerMessage(BuildUnavailableMessageRequest…)`
            // lands with the send half.
            tracing::debug!("would request unavailable message {id} from the phone (TODO Phase 5)");
        }

        let mut attrs = build_base_receipt(&id, node);
        attrs.insert("type".to_owned(), crate::binary::node::AttrValue::Text("retry".to_owned()));
        if info.message_type == "peer_msg" && info.is_from_me {
            attrs.insert("category".to_owned(), crate::binary::node::AttrValue::Text("peer".to_owned()));
        }

        let registration_id = self.device.lock().await.registration_id;
        let mut content = vec![
            Node::new("retry")
                .attr("count", i64::from(retry_count))
                .attr("id", id.clone())
                .attr("t", attr_text(node, "t").unwrap_or_default())
                .attr("v", 1i64),
            Node::new("registration").with_content(registration_id.to_be_bytes().to_vec()),
        ];

        if retry_count > 1 || force_include_identity {
            match self.build_keys_node().await {
                Ok(keys) => content.push(keys),
                // Go logs and sends the receipt without the keys.
                Err(err) => tracing::error!("failed to build the keys node for retry receipt {id}: {err}"),
            }
        }

        let receipt = Node::new("receipt").with_attrs(attrs).with_content(content);
        self.send_node(&receipt).await
    }

    /// Go `sendRetryReceipt(ctx, node, info, forceIncludeIdentity)` — the
    /// one-argument form used by the message path; `info` is parsed from the
    /// node and a parse failure falls back to the raw attributes.
    pub async fn send_retry_receipt(&self, node: &Node) -> Result<(), ClientError> {
        let info = match self.parse_message_info(node).await {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!("failed to parse message info while building a retry receipt: {err}");
                MessageInfo {
                    id: MessageID::new(attr_text(node, "id").unwrap_or_default()),
                    message_type: attr_text(node, "type").unwrap_or_default(),
                    ..Default::default()
                }
            }
        };
        self.send_retry_receipt_with(node, &info, false).await
    }

    /// The `<keys>` child of a retry receipt: our identity, a fresh one-time
    /// prekey, the signed prekey and the account identity.
    async fn build_keys_node(&self) -> Result<Node, ClientError> {
        let (identity_key, signed_pre_key, account, pre_keys) = {
            let device = self.device.lock().await;
            (device.identity_key.public, device.signed_pre_key.clone(), device.account.clone(), device.pre_keys.clone())
        };
        let mut keys = vec![
            Node::new("type").with_content(vec![CURVE25519_KEY_TYPE]),
            Node::new("identity").with_content(identity_key.to_vec()),
        ];
        // A store that cannot hand out a prekey (uninitialised device) must not
        // cost us the whole keys node — Go logs and keeps going as well.
        match pre_keys.gen_one_pre_key().await {
            Ok(pre_key) => keys.push(pre_key_to_node(&pre_key)),
            Err(err) => tracing::warn!("failed to generate a prekey for the retry receipt: {err}"),
        }
        keys.push(pre_key_to_node(&signed_pre_key));
        if let Some(account) = account {
            keys.push(Node::new("device-identity").with_content(account.encode_to_vec()));
        }
        Ok(Node::new("keys").with_content(keys))
    }
}

/// The retry counter map, kept next to the other `Client` fields.
pub type MessageRetries = HashMap<String, i32>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keys::{KeyPair, PreKey};

    #[test]
    fn keys_node_uses_go_pre_key_layout() {
        let pre_key = PreKey { key_pair: KeyPair::generate(), key_id: 3, signature: None };
        let node = pre_key_to_node(&pre_key);
        assert_eq!(node.tag, "key");
        let signed = PreKey { signature: Some([7u8; 64]), ..pre_key };
        let node = pre_key_to_node(&signed);
        assert_eq!(node.tag, "skey");
        assert_eq!(node.optional_child_by_tag(&["signature"]).and_then(Node::as_bytes), Some(&[7u8; 64][..]));
    }

    #[test]
    fn curve_and_retry_limits() {
        assert_eq!(CURVE25519_KEY_TYPE, 5);
        assert_eq!(MAX_MESSAGE_RETRIES, 5);
    }
}
