// Ported from the Go client `notification.go` (and the notification half of
// `group.go`), © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `<notification>` routing and the handlers this phase owns.
//!
//! Go counterparts: `Go notification.go` (`handleNotification`,
//! `parseGroupNotification`, `handlePictureNotification`,
//! `handleNewsletterNotification`, `handleAccountSyncNotification`).
//!
//! Only the notification types this phase ports are routed; every other type
//! is logged and skipped. The handler queue entry point is
//! [`handle_notification`], which `Client::handle_node` must call for
//! `<notification>` nodes.

use std::sync::Arc;

use crate::WaEvent;
use crate::binary::node::Node;
use crate::store::traits::LidMapping;
use crate::types::events::{GroupInfoEventData, JoinedGroupData, NewsletterMessageData};
use crate::types::jid::Jid;

use super::Client;
use super::ack::attr_text;
use super::group::{parse_group_change, parse_group_create};
use super::message::attr_int;
use super::newsletter::parse_newsletter_messages;

impl Client {
    /// Go `handleNotification` — route one `<notification>` node.
    ///
    /// The Go signature also returns the "handled, do not defer the ack" flag;
    /// this port acks through [`Client::maybe_deferred_ack`] once the node has
    /// been routed.
    pub async fn handle_notification(self: &Arc<Self>, node: &Node) {
        let Some(notif_type) = attr_text(node, "type") else {
            tracing::debug!("ignoring a notification without a type attribute");
            return;
        };
        match notif_type.as_str() {
            "w:gp2" => self.handle_group_notification(node).await,
            "picture" => self.handle_picture_notification(node),
            "newsletter" => self.handle_newsletter_notification(node),
            "account_sync" => self.handle_account_sync_notification(node).await,
            other => tracing::debug!("unhandled notification type {other}"),
        }
        self.maybe_deferred_ack(node).await;
    }

    /// Go `parseGroupNotification` + the `w:gp2` arm of `handleNotification`.
    ///
    /// A `<create>` child means we joined (or created) a group and yields
    /// [`WaEvent::JoinedGroup`]; anything else is a metadata change and yields
    /// [`WaEvent::GroupInfo`].
    pub async fn handle_group_notification(self: &Arc<Self>, node: &Node) {
        // Go: a `create` notification is exactly one `<create>` child.
        let children = node.children();
        let parsed = match children {
            [only] if only.tag == "create" => parse_group_create(node, only).map(NotificationPayload::Joined),
            _ => parse_group_change(node).map(|(evt, lid_pairs)| NotificationPayload::Changed(evt, lid_pairs)),
        };
        let payload = match parsed {
            Ok(payload) => payload,
            Err(err) => {
                tracing::error!("failed to parse a group notification: {err}");
                return;
            }
        };
        match payload {
            NotificationPayload::Joined(joined) => {
                // The notification carries the whole group: cache it.
                self.cache_group_info_from_event(&joined.info).await;
                self.dispatch_event(WaEvent::JoinedGroup { info: joined });
            }
            NotificationPayload::Changed(evt, lid_pairs) => {
                if !lid_pairs.is_empty() {
                    let lids = self.device.lock().await.lids.clone();
                    if let Err(err) = lids.put_many_lid_mappings(&lid_pairs).await {
                        tracing::warn!("failed to store LID mappings from a group notification: {err}");
                    }
                }
                self.group_cache.lock().await.apply_group_change(&evt);
                self.dispatch_event(WaEvent::GroupInfo { info: evt });
            }
        }
    }

    /// Cache a group whose full info arrived through a notification
    /// (`JoinedGroup`) — the notification path cannot use
    /// `Client::store_group_info`'s `&GroupInfoData`, it has the same shape.
    async fn cache_group_info_from_event(&self, info: &crate::types::events::GroupInfoData) {
        self.store_group_info(info).await;
    }

    /// Go `handlePictureNotification` — one [`WaEvent::Picture`] per
    /// `add`/`set`/`delete` child.
    pub fn handle_picture_notification(&self, node: &Node) {
        let timestamp = attr_int(node, "t");
        for child in node.children() {
            let remove = match child.tag.as_str() {
                "delete" => true,
                "add" | "set" => false,
                _ => continue,
            };
            let Some(jid) = child.attrs.get("jid").and_then(|value| value.as_jid()).cloned() else {
                tracing::debug!("ignoring a picture notification child without a jid");
                continue;
            };
            self.dispatch_event(WaEvent::Picture {
                jid,
                author: child.attrs.get("author").and_then(|value| value.as_jid()).cloned().unwrap_or_default(),
                timestamp,
                remove,
                picture_id: attr_text(child, "id").unwrap_or_default(),
            });
        }
    }

    /// Go `handleNewsletterNotification` — the `<live_updates>` messages of a
    /// channel notification.
    pub fn handle_newsletter_notification(&self, node: &Node) {
        let Some(id) = node.attrs.get("from").and_then(|value| value.as_jid()).cloned() else {
            tracing::debug!("ignoring a newsletter notification without a from attribute");
            return;
        };
        // Go: `live_updates` (with a nested `messages`) or the node's own
        // `message` children.
        let live_updates = node.optional_child_by_tag(&["live_updates"]).unwrap_or(node);
        let messages_node = live_updates.optional_child_by_tag(&["messages"]).unwrap_or(live_updates);
        let messages: Vec<NewsletterMessageData> = parse_newsletter_messages(messages_node);
        self.dispatch_event(WaEvent::NewsletterLiveUpdate { id, time: attr_int(node, "t"), messages });
    }

    /// Go `handleAccountSyncNotification` — the `<privacy>` child is routed to
    /// `client/privacysettings.rs`; the rest is logged.
    pub async fn handle_account_sync_notification(self: &Arc<Self>, node: &Node) {
        let own = self.own_id().await.to_non_ad();
        for child in node.children() {
            match child.tag.as_str() {
                "privacy" => match self.handle_privacy_settings_notification(child).await {
                    Some(settings) => self.dispatch_event(WaEvent::PrivacySettings { settings }),
                    None => tracing::debug!("privacy settings notification produced no payload"),
                },
                "picture" => self.dispatch_event(WaEvent::Picture {
                    jid: own.clone(),
                    // Go sends no author for our own picture.
                    author: Jid::default(),
                    timestamp: attr_int(node, "t"),
                    remove: false,
                    picture_id: String::new(),
                }),
                other => {
                    // NOTE: `devices` (own device list) and `blocklist` live in their own phases.
                    tracing::debug!("unhandled account sync item {other}");
                }
            }
        }
    }
}

/// The two possible shapes of a `w:gp2` notification.
enum NotificationPayload {
    Joined(JoinedGroupData),
    Changed(GroupInfoEventData, Vec<LidMapping>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::node::Node;

    fn jid(value: &str) -> Jid {
        Jid::parse(value).unwrap()
    }

    #[test]
    fn group_notification_parses_a_create_as_a_join() {
        let node =
            Node::new("notification")
                .attr("type", "w:gp2")
                .attr("from", jid("120363000000000001@g.us"))
                .attr("notify", "Rust port")
                .attr("participant", jid("491234567890@s.whatsapp.net"))
                .attr("addressing_mode", "lid")
                .with_content(vec![Node::new("create").attr("type", "new").attr("reason", "invite").with_content(
                    vec![Node::new("group").attr("id", "120363000000000001").attr("subject", "Rust port")],
                )]);
        let group_node = node.child_by_tag(&["create"]);
        let joined = parse_group_create(&node, &group_node).expect("a joined group");
        assert_eq!(joined.join_type, "new");
        assert_eq!(joined.reason, "invite");
        assert_eq!(joined.info.jid, jid("120363000000000001@g.us"));
        assert_eq!(joined.info.name, "Rust port");
        assert_eq!(joined.info.addressing_mode, "lid", "the addressing mode lives on the notification");
    }

    #[test]
    fn newsletter_notification_reads_the_live_update_messages() {
        let node = Node::new("notification")
            .attr("type", "newsletter")
            .attr("from", jid("120363000000000001@newsletter"))
            .with_content(vec![Node::new("live_updates").with_content(vec![Node::new("messages").with_content(
                vec![Node::new("message").attr("server_id", "5").attr("type", "reaction").attr("id", "ABC")],
            )])]);
        let live_updates = node.optional_child_by_tag(&["live_updates"]).unwrap_or(&node);
        let messages = live_updates.optional_child_by_tag(&["messages"]).unwrap_or(live_updates);
        let parsed = parse_newsletter_messages(messages);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].message_id, "ABC");
        assert_eq!(parsed[0].message_type, "reaction");
    }

    #[test]
    fn notification_without_a_type_is_ignored() {
        // The router returns before touching anything else.
        let node = Node::new("notification");
        assert!(attr_text(&node, "type").is_none());
    }
}
