// Ported from the Go client `presence.go` and `tctoken.go` (the privacy-token
// lookup used by presence subscriptions), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Presence (`<presence>`) and typing state (`<chatstate>`).
//!
//! Go counterparts: `Go presence.go` (`SendPresence`,
//! `SubscribePresence`, `SendChatPresence`, `handlePresence`,
//! `handleChatState`) and the read side of `Go tctoken.go`
//! (`ensureTCToken`, `resolveTCTokenStorageLID`, the bucket maths). Issuing
//! new tokens (`issuePrivacyToken*`) and the in-memory `tcTokenSenderTS` map
//! belong to the send path and are not needed to subscribe.
//!
//! The node builders and parsers are pure functions so the wire shape is
//! testable offline; the `Client` methods only add store access and the
//! socket write. The handlers return the parsed payload — the caller turns it
//! into a `WaEvent` and passes it to `dispatch_event`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::binary::node::{AttrValue, Node};
use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid};
use crate::types::message::MessageSource;
use crate::types::presence::{ChatPresenceMedia, ChatPresenceState, PresenceState};

use super::ack::attr_text;
use super::message::{attr_int, attr_jid};
use super::{Client, ClientError};

/// Go `tcTokenBucketDuration` — one bucket is 7 days (seconds).
pub const TC_TOKEN_BUCKET_DURATION: u64 = 604_800;
/// Go `tcTokenNumBuckets` — four rolling buckets (~28 days).
pub const TC_TOKEN_NUM_BUCKETS: u64 = 4;

/// Payload of the Go `events.Presence` event (a user went on/offline).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct PresenceData {
    /// The user whose presence this is.
    pub from: Jid,
    /// True when the user is now offline.
    pub unavailable: bool,
    /// When the user was last online (unix seconds); `None` when hidden.
    pub last_seen: Option<i64>,
}

/// Payload of the Go `events.ChatPresence` event (typing / recording state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct ChatPresenceData {
    /// Who is typing where (Go embeds `types.MessageSource`).
    pub source: MessageSource,
    /// The raw state tag: `composing` or `paused` (Go keeps unknown tags too).
    pub state: String,
    /// The raw media attribute: `""` (text) or `audio`.
    pub media: String,
}

impl ChatPresenceData {
    /// The typed state, `None` for an unrecognized tag.
    pub fn state(&self) -> Option<ChatPresenceState> {
        match self.state.as_str() {
            "composing" => Some(ChatPresenceState::Composing),
            "paused" => Some(ChatPresenceState::Paused),
            _ => None,
        }
    }

    /// The typed media kind, `None` for an unrecognized value.
    pub fn media(&self) -> Option<ChatPresenceMedia> {
        match self.media.as_str() {
            "" => Some(ChatPresenceMedia::Text),
            "audio" => Some(ChatPresenceMedia::Audio),
            _ => None,
        }
    }
}

/// Go `currentTCTokenCutoffTimestamp` — the start of the oldest bucket that
/// still counts as valid at `now`.
pub fn tc_token_cutoff(now: SystemTime) -> SystemTime {
    let now_secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let current_bucket = now_secs / TC_TOKEN_BUCKET_DURATION;
    let cutoff_bucket = current_bucket.saturating_sub(TC_TOKEN_NUM_BUCKETS - 1);
    UNIX_EPOCH + Duration::from_secs(cutoff_bucket * TC_TOKEN_BUCKET_DURATION)
}

/// Go `isTCTokenExpired` — `None` (zero time) is always expired.
pub fn is_tc_token_expired(timestamp: Option<SystemTime>, now: SystemTime) -> bool {
    match timestamp {
        None => true,
        Some(ts) => ts == UNIX_EPOCH || ts < tc_token_cutoff(now),
    }
}

/// Go `SubscribePresence`'s request node.
pub fn build_subscribe_presence_node(jid: &Jid, token: Option<&[u8]>) -> Node {
    let node = Node::new("presence").attr("type", "subscribe").attr("to", jid.clone());
    match token {
        Some(token) => node.with_content(vec![Node::new("tctoken").with_content(token.to_vec())]),
        None => node,
    }
}

/// Go `SendPresence`'s node. `push_name` is `None` for Messenger E2EE
/// sessions (no `name` attribute).
pub fn build_presence_node(state: PresenceState, push_name: Option<&str>) -> Node {
    let node = Node::new("presence").attr("type", state.as_str());
    match push_name {
        Some(name) => node.attr("name", name.to_owned()),
        None => node,
    }
}

/// Go `SendChatPresence`'s node. `media` only applies to `composing` and is
/// omitted when it is the text kind (the empty string).
pub fn build_chat_presence_node(jid: &Jid, state: ChatPresenceState, media: ChatPresenceMedia) -> Node {
    let mut child = Node::new(state.as_str());
    if state == ChatPresenceState::Composing && !media.as_str().is_empty() {
        child = child.attr("media", media.as_str());
    }
    Node::new("chatstate").attr("to", jid.clone()).with_content(vec![child])
}

/// Go `handlePresence` — parse an inbound `<presence>` node.
pub fn parse_presence(node: &Node) -> Result<PresenceData, ClientError> {
    let from =
        attr_jid(node, "from").ok_or_else(|| ClientError::MissingAttribute { tag: node.tag.clone(), attr: "from" })?;
    let mut evt = PresenceData { from, ..Default::default() };
    match attr_text(node, "type").as_deref() {
        Some("unavailable") => evt.unavailable = true,
        None | Some("") => {}
        Some(other) => {
            tracing::debug!("unrecognized presence type '{other}' in presence event from {}", evt.from);
        }
    }
    match attr_text(node, "last").as_deref() {
        None | Some("") | Some("deny") => {}
        Some(_) => {
            let last = attr_int(node, "last")
                .ok_or_else(|| ClientError::MissingAttribute { tag: node.tag.clone(), attr: "last" })?;
            // Go `UnixTime`: 0 means the zero time (no value).
            evt.last_seen = (last != 0).then_some(last);
        }
    }
    Ok(evt)
}

/// Go `handleChatState` minus the source parsing — the single child carries
/// the state tag and the optional `media` attribute.
pub fn parse_chat_state_child(node: &Node, source: MessageSource) -> Result<ChatPresenceData, ClientError> {
    let children = node.children();
    if children.len() != 1 {
        return Err(ClientError::MessageParse(format!(
            "unexpected number of children in chat state element ({})",
            children.len()
        )));
    }
    let child = &children[0];
    if child.tag != "composing" && child.tag != "paused" {
        tracing::warn!("unrecognized chat presence state {}", child.tag);
    }
    let media = child.attrs.get("media").and_then(AttrValue::as_str).unwrap_or_default().to_owned();
    Ok(ChatPresenceData { source, state: child.tag.clone(), media })
}

/// Go `shouldSendTCTokenInChatAction` — tokens only go to real users.
pub fn should_send_tc_token_in_chat_action(jid: &Jid) -> bool {
    let jid = jid.to_non_ad();
    (jid.server == DEFAULT_USER_SERVER || jid.server == HIDDEN_USER_SERVER)
        && jid.user != "0" // Go `types.PSAJID.User`
        && !jid.is_bot()
}

impl Client {
    /// Go `resolveTCTokenStorageLID` — tokens are stored under the LID when
    /// one is known for a phone-number JID.
    async fn resolve_tc_token_storage_lid(&self, jid: &Jid) -> Jid {
        let storage = jid.to_non_ad();
        if storage.server != DEFAULT_USER_SERVER {
            return storage;
        }
        let lids = self.device.lock().await.lids.clone();
        match lids.get_lid_for_pn(&storage).await {
            Ok(Some(lid)) if !lid.is_empty() => lid.to_non_ad(),
            Ok(_) => storage,
            Err(err) => {
                tracing::debug!("failed to resolve LID for tctoken JID {storage}: {err}");
                storage
            }
        }
    }

    /// Go `ensureTCToken` — a stored, non-expired privacy token for `jid`.
    pub async fn ensure_tc_token(&self, jid: &Jid) -> Result<Option<Vec<u8>>, ClientError> {
        if self.own_id().await.is_empty() {
            return Err(ClientError::NotLoggedIn);
        }
        let now = SystemTime::now();
        let tokens = self.device.lock().await.privacy_tokens.clone();
        // Go `deleteExpiredPrivacyTokens` (rate-limited background prune);
        // done inline here, failures are only logged.
        if let Err(err) = tokens.delete_expired_privacy_tokens(tc_token_cutoff(now)).await {
            tracing::warn!("failed to delete expired privacy tokens: {err}");
        }
        let storage = self.resolve_tc_token_storage_lid(jid).await;
        let Some(existing) = tokens.get_privacy_token(&storage).await? else {
            return Ok(None);
        };
        if !existing.token.is_empty() && !is_tc_token_expired(Some(existing.timestamp), now) {
            return Ok(Some(existing.token));
        }
        Ok(None)
    }

    /// Go `SendPresence` — tell the server we are (un)available. Call it at
    /// least once after connecting so contacts see our push name.
    pub async fn send_presence(&self, state: PresenceState) -> Result<(), ClientError> {
        let push_name = self.device.lock().await.push_name.clone();
        if push_name.is_empty() {
            // Go `ErrNoPushName` (Messenger sessions are not supported).
            return Err(ClientError::InvalidArgument("can't send presence without a push name set"));
        }
        // NOTE: Go also calls `sendUnifiedSession` and flips `sendActiveReceipts` here; neither exists yet.
        self.send_node(&build_presence_node(state, Some(&push_name))).await
    }

    /// Go `SubscribePresence` — ask for presence updates of `jid`. The server
    /// only delivers them while we are `available` ourselves.
    pub async fn subscribe_presence(&self, jid: &Jid) -> Result<(), ClientError> {
        let token = self.ensure_tc_token(jid).await?;
        if token.is_none() {
            tracing::debug!("trying to subscribe to presence of {jid} without privacy token");
        }
        self.send_node(&build_subscribe_presence_node(jid, token.as_deref())).await
    }

    /// Go `SendChatPresence` — our typing / recording state in `jid`.
    pub async fn send_chat_presence(
        &self,
        jid: &Jid,
        state: ChatPresenceState,
        media: ChatPresenceMedia,
    ) -> Result<(), ClientError> {
        self.send_node(&build_chat_presence_node(jid, state, media)).await
    }

    /// Go `handlePresence` — parse an inbound `<presence>`; `None` (logged)
    /// when it is malformed. The caller dispatches `WaEvent::Presence`.
    pub async fn handle_presence(&self, node: &Node) -> Option<PresenceData> {
        match parse_presence(node) {
            Ok(evt) => Some(evt),
            Err(err) => {
                tracing::warn!("error parsing presence event: {err}");
                None
            }
        }
    }

    /// Go `handleChatState` — parse an inbound `<chatstate>`; `None` (logged)
    /// when it is malformed. The caller dispatches `WaEvent::ChatPresence`.
    pub async fn handle_chat_state(&self, node: &Node) -> Option<ChatPresenceData> {
        let source = match self.parse_message_source(node, true).await {
            Ok(source) => source,
            Err(err) => {
                tracing::warn!("failed to parse chat state update: {err}");
                return None;
            }
        };
        match parse_chat_state_child(node, source) {
            Ok(evt) => Some(evt),
            Err(err) => {
                tracing::warn!("failed to parse chat state update: {err}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    use super::*;
    use crate::binary::node::NodeContent;
    use crate::client::{ClientConfig, EventSink};
    use crate::store::sqlite::Container;
    use crate::store::traits::{PrivacyToken, PrivacyTokenStore};

    fn jid(s: &str) -> Jid {
        Jid::parse(s).unwrap()
    }

    #[derive(Default)]
    struct FakeTokens(StdMutex<Vec<PrivacyToken>>);

    #[async_trait]
    impl PrivacyTokenStore for FakeTokens {
        async fn put_privacy_tokens(&self, tokens: &[PrivacyToken]) -> anyhow::Result<()> {
            self.0.lock().unwrap().extend_from_slice(tokens);
            Ok(())
        }
        async fn get_privacy_token(&self, user: &Jid) -> anyhow::Result<Option<PrivacyToken>> {
            Ok(self.0.lock().unwrap().iter().find(|t| &t.user == user).cloned())
        }
        async fn delete_expired_privacy_tokens(&self, _: SystemTime) -> anyhow::Result<u64> {
            Ok(0)
        }
    }

    async fn client_with(tokens: Arc<FakeTokens>, own: Option<Jid>) -> Arc<Client> {
        let container = Container::open_memory().await.unwrap();
        let mut device = container.new_device();
        device.id = own;
        device.privacy_tokens = tokens;
        let events: EventSink = Arc::new(|_| {});
        Client::new(device, ClientConfig::default(), events)
    }

    #[test]
    fn subscribe_iq_shape_without_token() {
        let node = build_subscribe_presence_node(&jid("123@s.whatsapp.net"), None);
        assert_eq!(node.tag, "presence");
        assert_eq!(node.attrs.get("type").and_then(AttrValue::as_str), Some("subscribe"));
        assert_eq!(node.attrs.get("to").and_then(AttrValue::as_jid), Some(&jid("123@s.whatsapp.net")));
        assert!(node.content.is_empty());
        assert_eq!(node.attrs.len(), 2);
    }

    #[test]
    fn subscribe_iq_shape_with_token() {
        let node = build_subscribe_presence_node(&jid("123@s.whatsapp.net"), Some(&[1, 2, 3]));
        let children = node.children();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].tag, "tctoken");
        assert!(children[0].attrs.is_empty());
        assert!(matches!(&children[0].content, NodeContent::Bytes(b) if b == &[1, 2, 3]));
        // Survives the binary codec unchanged.
        let packed = crate::binary::marshal(&node);
        let decoded = crate::binary::unmarshal(&crate::binary::unpack(&packed).unwrap()).unwrap();
        assert_eq!(decoded.tag, "presence");
        assert_eq!(decoded.children()[0].as_bytes(), Some(&[1u8, 2, 3][..]));
    }

    #[test]
    fn chat_presence_state_strings() {
        let to = jid("123@s.whatsapp.net");
        let cases = [
            (ChatPresenceState::Composing, ChatPresenceMedia::Text, "composing", None),
            (ChatPresenceState::Composing, ChatPresenceMedia::Audio, "composing", Some("audio")),
            (ChatPresenceState::Paused, ChatPresenceMedia::Text, "paused", None),
            // Media is ignored for anything but composing (Go behaviour).
            (ChatPresenceState::Paused, ChatPresenceMedia::Audio, "paused", None),
        ];
        for (state, media, tag, media_attr) in cases {
            let node = build_chat_presence_node(&to, state, media);
            assert_eq!(node.tag, "chatstate");
            assert_eq!(node.attrs.get("to").and_then(AttrValue::as_jid), Some(&to));
            let children = node.children();
            assert_eq!(children.len(), 1);
            assert_eq!(children[0].tag, tag);
            assert_eq!(children[0].attrs.get("media").and_then(AttrValue::as_str), media_attr);
        }
    }

    #[test]
    fn presence_node_carries_push_name() {
        let node = build_presence_node(PresenceState::Available, Some("Alice"));
        assert_eq!(node.attrs.get("type").and_then(AttrValue::as_str), Some("available"));
        assert_eq!(node.attrs.get("name").and_then(AttrValue::as_str), Some("Alice"));
        let node = build_presence_node(PresenceState::Unavailable, None);
        assert_eq!(node.attrs.get("type").and_then(AttrValue::as_str), Some("unavailable"));
        assert!(!node.attrs.contains_key("name"));
    }

    #[test]
    fn presence_event_mapping() {
        let from = jid("123@s.whatsapp.net");
        let online = Node::new("presence").attr("from", from.clone());
        assert_eq!(
            parse_presence(&online).unwrap(),
            PresenceData { from: from.clone(), unavailable: false, last_seen: None }
        );

        let offline =
            Node::new("presence").attr("from", from.clone()).attr("type", "unavailable").attr("last", "1700000000");
        assert_eq!(
            parse_presence(&offline).unwrap(),
            PresenceData { from: from.clone(), unavailable: true, last_seen: Some(1_700_000_000) }
        );

        let hidden = Node::new("presence").attr("from", from.clone()).attr("type", "unavailable").attr("last", "deny");
        assert_eq!(parse_presence(&hidden).unwrap().last_seen, None);

        let unknown_type = Node::new("presence").attr("from", from.clone()).attr("type", "weird");
        assert!(!parse_presence(&unknown_type).unwrap().unavailable);

        assert!(parse_presence(&Node::new("presence")).is_err());
        assert!(parse_presence(&Node::new("presence").attr("from", from).attr("last", "soon")).is_err());
    }

    #[test]
    fn presence_payload_json_is_camel_case() {
        let evt = PresenceData { from: jid("1@s.whatsapp.net"), unavailable: true, last_seen: Some(5) };
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json, serde_json::json!({"from": "1@s.whatsapp.net", "unavailable": true, "lastSeen": 5}));
    }

    #[test]
    fn chat_state_event_mapping() {
        let source =
            MessageSource { chat: jid("1@s.whatsapp.net"), sender: jid("1@s.whatsapp.net"), ..Default::default() };
        let node = Node::new("chatstate").with_content(vec![Node::new("composing").attr("media", "audio")]);
        let evt = parse_chat_state_child(&node, source.clone()).unwrap();
        assert_eq!(evt.state(), Some(ChatPresenceState::Composing));
        assert_eq!(evt.media(), Some(ChatPresenceMedia::Audio));

        let node = Node::new("chatstate").with_content(vec![Node::new("paused")]);
        let evt = parse_chat_state_child(&node, source.clone()).unwrap();
        assert_eq!((evt.state.as_str(), evt.media.as_str()), ("paused", ""));
        assert_eq!(evt.media(), Some(ChatPresenceMedia::Text));

        // Unknown tags still produce an event (Go only warns).
        let node = Node::new("chatstate").with_content(vec![Node::new("recording")]);
        assert_eq!(parse_chat_state_child(&node, source.clone()).unwrap().state(), None);

        assert!(parse_chat_state_child(&Node::new("chatstate"), source).is_err());
    }

    #[tokio::test]
    async fn handle_chat_state_parses_source() {
        let client = client_with(Arc::default(), Some(jid("999:1@s.whatsapp.net"))).await;
        let node =
            Node::new("chatstate").attr("from", jid("123@s.whatsapp.net")).with_content(vec![Node::new("composing")]);
        let evt = client.handle_chat_state(&node).await.unwrap();
        assert_eq!(evt.source.chat, jid("123@s.whatsapp.net"));
        assert_eq!(evt.source.sender, jid("123@s.whatsapp.net"));
        assert!(!evt.source.is_from_me);
        assert_eq!(evt.state(), Some(ChatPresenceState::Composing));
    }

    #[test]
    fn tc_token_buckets() {
        let now = UNIX_EPOCH + Duration::from_secs(TC_TOKEN_BUCKET_DURATION * 10 + 123);
        assert_eq!(tc_token_cutoff(now), UNIX_EPOCH + Duration::from_secs(TC_TOKEN_BUCKET_DURATION * 7));
        assert!(is_tc_token_expired(None, now));
        assert!(is_tc_token_expired(Some(UNIX_EPOCH + Duration::from_secs(TC_TOKEN_BUCKET_DURATION * 7 - 1)), now));
        assert!(!is_tc_token_expired(Some(UNIX_EPOCH + Duration::from_secs(TC_TOKEN_BUCKET_DURATION * 7)), now));
    }

    #[test]
    fn tc_token_chat_action_targets() {
        assert!(should_send_tc_token_in_chat_action(&jid("123:4@s.whatsapp.net")));
        assert!(should_send_tc_token_in_chat_action(&jid("123@lid")));
        assert!(!should_send_tc_token_in_chat_action(&jid("0@s.whatsapp.net")));
        assert!(!should_send_tc_token_in_chat_action(&jid("123-456@g.us")));
        assert!(!should_send_tc_token_in_chat_action(&jid("13135550000@s.whatsapp.net")));
    }

    #[tokio::test]
    async fn ensure_tc_token_requires_login() {
        let client = client_with(Arc::default(), None).await;
        assert!(matches!(client.ensure_tc_token(&jid("1@s.whatsapp.net")).await, Err(ClientError::NotLoggedIn)));
    }

    #[tokio::test]
    async fn ensure_tc_token_returns_fresh_tokens_only() {
        let tokens = Arc::new(FakeTokens::default());
        tokens
            .put_privacy_tokens(&[
                PrivacyToken {
                    user: jid("1@s.whatsapp.net"),
                    token: vec![9, 9],
                    timestamp: SystemTime::now(),
                    sender_timestamp: None,
                },
                PrivacyToken {
                    user: jid("2@s.whatsapp.net"),
                    token: vec![7],
                    timestamp: UNIX_EPOCH + Duration::from_secs(1),
                    sender_timestamp: None,
                },
            ])
            .await
            .unwrap();
        let client = client_with(tokens, Some(jid("999:1@s.whatsapp.net"))).await;
        // The device suffix is stripped before the lookup.
        assert_eq!(client.ensure_tc_token(&jid("1:3@s.whatsapp.net")).await.unwrap(), Some(vec![9, 9]));
        assert_eq!(client.ensure_tc_token(&jid("2@s.whatsapp.net")).await.unwrap(), None);
        assert_eq!(client.ensure_tc_token(&jid("3@s.whatsapp.net")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn send_presence_requires_push_name() {
        let client = client_with(Arc::default(), Some(jid("999:1@s.whatsapp.net"))).await;
        assert!(matches!(client.send_presence(PresenceState::Available).await, Err(ClientError::InvalidArgument(_))));
    }
}
