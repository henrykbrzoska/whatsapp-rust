// Ported from the Go client `newsletter.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Newsletters (WhatsApp Channels): follow/unfollow, live updates and the
//! message parser shared with the notification handler.
//!
//! Go counterparts: `Go newsletter.go` (`FollowNewsletter`,
//! `UnfollowNewsletter`, `NewsletterSubscribeLiveUpdates`,
//! `NewsletterToggleMute`, `GetNewsletterMessages`, `sendMexIQ`) and
//! `Go notification.go` (`parseNewsletterMessages`).
//!
//! Divergences from Go:
//! * `argo` decoding is not implemented (Go's is currently broken too, it
//!   returns an error): a `result` child with `format="argo"` is rejected
//!   instead of decoded, so only the JSON GraphQL path works;
//! * the macOS/desktop query-ID remapping is not implemented — this port
//!   always presents a web client payload, so the primary IDs are correct.

use serde_json::Value;

use crate::binary::node::{Node, NodeContent};
use crate::types::events::NewsletterMessageData;
use crate::types::jid::Jid;

use super::ack::attr_text;
use super::message::attr_int;
use super::{Client, ClientError, InfoQuery, IqKind};

/// GraphQL query/mutation IDs of the newsletter API (Go's `query…`/`mutation…`
/// constants). They are opaque server-side identifiers.
pub const QUERY_FETCH_NEWSLETTER: &str = "6563316087068696";
pub const QUERY_SUBSCRIBED_NEWSLETTERS: &str = "6388546374527196";
pub const MUTATION_MUTE_NEWSLETTER: &str = "6274038279359549";
pub const MUTATION_UNMUTE_NEWSLETTER: &str = "6068417879924485";
pub const MUTATION_UNFOLLOW_NEWSLETTER: &str = "6392786840836363";
pub const MUTATION_FOLLOW_NEWSLETTER: &str = "9926858900719341";

/// Go `infoQuery{Namespace: "newsletter"}` — the namespace of newsletter IQs.
pub const NEWSLETTER_IQ_NAMESPACE: &str = "newsletter";
/// Go `infoQuery{Namespace: "w:mex"}` — the namespace of the GraphQL bridge.
pub const MEX_IQ_NAMESPACE: &str = "w:mex";

/// Go `parseNewsletterMessages` — the `<message>` children of a channel
/// `messages`/`live_updates` element.
pub fn parse_newsletter_messages(node: &Node) -> Vec<NewsletterMessageData> {
    let mut output = Vec::with_capacity(node.children().len());
    for child in node.children() {
        if child.tag != "message" {
            continue;
        }
        let mut message = NewsletterMessageData {
            server_id: attr_int(child, "server_id").unwrap_or_default(),
            message_id: attr_text(child, "id").unwrap_or_default(),
            message_type: attr_text(child, "type").unwrap_or_default(),
            timestamp: attr_int(child, "t"),
            message: None,
            views_count: 0,
            forwards_count: 0,
            reaction_counts: Default::default(),
        };
        for subchild in child.children() {
            match subchild.tag.as_str() {
                "plaintext" => {
                    if let Some(bytes) = subchild.as_bytes() {
                        message.message = Some(hex::encode(bytes));
                    }
                }
                "views_count" => {
                    message.views_count = attr_int(subchild, "count").unwrap_or_default() as i32;
                }
                "forwards_count" => {
                    message.forwards_count = attr_int(subchild, "count").unwrap_or_default() as i32;
                }
                "reactions" => {
                    let mut map = std::collections::HashMap::new();
                    for reaction in subchild.children() {
                        if let Some(code) = attr_text(reaction, "code") {
                            let count = attr_int(reaction, "count").unwrap_or_default() as i32;
                            map.insert(code, count);
                        }
                    }
                    message.reaction_counts = map;
                }
                _ => {}
            }
        }
        output.push(message);
    }
    output
}

impl Client {
    /// Go `sendMexIQ` — one GraphQL round-trip over `xmlns="w:mex"`.
    ///
    /// Returns the GraphQL `data` object; a non-empty `errors` array is an
    /// error even when the server also sent data.
    pub async fn send_mex_iq(&self, query_id: &str, variables: Value) -> Result<Value, ClientError> {
        let payload = serde_json::json!({ "variables": variables });
        let encoded = serde_json::to_vec(&payload)
            .map_err(|err| ClientError::MessageParse(format!("failed to encode the mex variables: {err}")))?;
        let query = Node::new("query").attr("query_id", query_id).with_content(NodeContent::Bytes(encoded));
        let response = self
            .send_iq(InfoQuery {
                namespace: MEX_IQ_NAMESPACE.into(),
                kind: IqKind::Get,
                to: Some(super::server_jid()),
                content: vec![query],
                ..Default::default()
            })
            .await?;
        let result = response
            .optional_child_by_tag(&["result"])
            .ok_or(ClientError::MissingElement { tag: "result", parent: "mex response" })?;
        let content = result
            .as_bytes()
            .ok_or(ClientError::MissingElement { tag: "result", parent: "mex response with non-bytes content" })?;
        if attr_text(result, "format").as_deref() == Some("argo") {
            // Go returns the same error today; porting the argo decoder is
            // Phase 6 material.
            return Err(ClientError::MessageParse("argo decoding is not supported".into()));
        }
        let parsed: Value = serde_json::from_slice(content)
            .map_err(|err| ClientError::MessageParse(format!("failed to parse the mex response: {err}")))?;
        if let Some(errors) = parsed.get("errors").and_then(Value::as_array).filter(|errors| !errors.is_empty()) {
            return Err(ClientError::MessageParse(format!("graphql error: {errors:?}")));
        }
        Ok(parsed.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Go `NewsletterSubscribeLiveUpdates` — subscribe to a channel's live
    /// updates for the returned duration.
    pub async fn newsletter_subscribe_live_updates(&self, jid: &Jid) -> Result<std::time::Duration, ClientError> {
        let response = self
            .send_iq(InfoQuery {
                namespace: NEWSLETTER_IQ_NAMESPACE.into(),
                kind: IqKind::Set,
                to: Some(jid.clone()),
                content: vec![Node::new("live_updates")],
                ..Default::default()
            })
            .await?;
        let duration = attr_int(&response.child_by_tag(&["live_updates"]), "duration").unwrap_or_default();
        Ok(std::time::Duration::from_secs(u64::try_from(duration).unwrap_or_default()))
    }

    /// Go `FollowNewsletter` — follow (join) a channel.
    pub async fn follow_newsletter(&self, jid: &Jid) -> Result<(), ClientError> {
        self.send_mex_iq(MUTATION_FOLLOW_NEWSLETTER, serde_json::json!({ "newsletter_id": jid.to_string() }))
            .await
            .map(|_| ())
    }

    /// Go `UnfollowNewsletter` — unfollow (leave) a channel.
    pub async fn unfollow_newsletter(&self, jid: &Jid) -> Result<(), ClientError> {
        self.send_mex_iq(MUTATION_UNFOLLOW_NEWSLETTER, serde_json::json!({ "newsletter_id": jid.to_string() }))
            .await
            .map(|_| ())
    }

    /// Go `NewsletterToggleMute` — mute or unmute a channel.
    pub async fn newsletter_toggle_mute(&self, jid: &Jid, mute: bool) -> Result<(), ClientError> {
        let query = if mute { MUTATION_MUTE_NEWSLETTER } else { MUTATION_UNMUTE_NEWSLETTER };
        self.send_mex_iq(query, serde_json::json!({ "newsletter_id": jid.to_string() })).await.map(|_| ())
    }

    /// Go `GetNewsletterMessages` — the most recent `count` messages of a
    /// channel (optionally before a `server_id`).
    pub async fn get_newsletter_messages(
        &self,
        jid: &Jid,
        count: usize,
        before: i64,
    ) -> Result<Vec<NewsletterMessageData>, ClientError> {
        let mut messages = Node::new("messages").attr("type", "jid").attr("jid", jid.clone());
        if count != 0 {
            messages = messages.attr("count", count.to_string());
        }
        if before != 0 {
            messages = messages.attr("before", before.to_string());
        }
        let response = self
            .send_iq(InfoQuery {
                namespace: NEWSLETTER_IQ_NAMESPACE.into(),
                kind: IqKind::Get,
                to: Some(super::server_jid()),
                content: vec![messages],
                ..Default::default()
            })
            .await?;
        let messages = response
            .optional_child_by_tag(&["messages"])
            .ok_or(ClientError::MissingElement { tag: "messages", parent: "newsletter messages response" })?;
        Ok(parse_newsletter_messages(messages))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::jid::Jid;

    fn node(value: &str) -> Node {
        Node::new("live_updates").with_content(NodeContent::Bytes(value.as_bytes().to_vec()))
    }

    #[test]
    fn parse_newsletter_messages_reads_ids_types_and_plaintext() {
        let messages = Node::new("messages").with_content(vec![
            Node::new("message")
                .attr("server_id", "17")
                .attr("id", "ABCDEF")
                .attr("type", "text")
                .attr("t", "1700000000")
                .with_content(vec![
                    Node::new("plaintext").with_content(NodeContent::Bytes(vec![0x0A, 0x02, 0x68, 0x69])),
                ]),
            Node::new("gateway"),
        ]);
        let parsed = parse_newsletter_messages(&messages);
        assert_eq!(parsed.len(), 1, "only <message> children are messages");
        assert_eq!(parsed[0].server_id, 17);
        assert_eq!(parsed[0].message_id, "ABCDEF");
        assert_eq!(parsed[0].message_type, "text");
        assert_eq!(parsed[0].timestamp, Some(1_700_000_000));
        assert_eq!(parsed[0].message.as_deref(), Some("0a026869"));
    }

    #[test]
    fn parse_newsletter_messages_tolerates_a_message_without_plaintext() {
        let parsed = parse_newsletter_messages(&node(""));
        assert!(parsed.is_empty(), "a bytes payload has no <message> children");
        let messages = Node::new("messages").with_content(vec![Node::new("message").attr("server_id", "1")]);
        let parsed = parse_newsletter_messages(&messages);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].message.is_none());
        assert_eq!(parsed[0].timestamp, None);
    }

    #[test]
    fn query_ids_match_the_go_constants() {
        assert_eq!(MUTATION_FOLLOW_NEWSLETTER, "9926858900719341");
        assert_eq!(MUTATION_UNFOLLOW_NEWSLETTER, "6392786840836363");
        assert_eq!(MUTATION_MUTE_NEWSLETTER, "6274038279359549");
        assert_eq!(MUTATION_UNMUTE_NEWSLETTER, "6068417879924485");
        assert_eq!(NEWSLETTER_IQ_NAMESPACE, "newsletter");
        assert_eq!(MEX_IQ_NAMESPACE, "w:mex");
        let _: Jid = Jid::new("120363000000000001", "newsletter");
    }
}
