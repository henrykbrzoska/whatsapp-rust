// Ported from the Go client `group.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Groups: creation, info, membership and the in-memory group cache.
//!
//! Go counterparts: `Go group.go` (`CreateGroup`, `GetGroupInfo`,
//! `getCachedGroupData`, `JoinGroupWithLink`, `GetGroupInfoFromLink`,
//! `LeaveGroup`, `UpdateGroupParticipants`, `SetGroupName`, `SetGroupTopic`,
//! `GetGroupInviteLink`, `parseGroupNode`, `parseGroupChange`,
//! `cacheGroupInfo`, `updateGroupParticipantCache`) plus `groupMetaCache`'s
//! `CanSend` (`Go client.go`).
//!
//! Every group operation is one `xmlns="w:g2"` IQ whose child is sent to the
//! group JID (or to the group server for `create`/`invite`). The node builders
//! below are pure functions so they can be asserted on without a socket;
//! [`GroupIqSender`] lets a test drive a round-trip with a fake `send_iq`.
//!
//! Divergences from Go:
//! * the cache has a TTL ([`GROUP_CACHE_TTL`]) instead of living until the next
//!   notification invalidates it — a stale member list must not address sends
//!   forever (a missed notification would otherwise pin it);
//! * `SendMessage` does not call [`GroupMeta::can_send`] yet (Phase 4's send
//!   path predates the cache): the check is exposed for the send path instead.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::binary::node::{Node, NodeContent};
use crate::store::traits::{LidMapping, RedactedPhoneEntry};
use crate::types::events::{
    GroupAnnounceData, GroupDeleteData, GroupEphemeralData, GroupInfoData, GroupInfoEventData, GroupLinkChangeData,
    GroupLinkTargetData, GroupLockedData, GroupNameData, GroupParticipantAddRequestData, GroupParticipantData,
    GroupTopicData, JoinedGroupData,
};
use crate::types::group::ParticipantChange;
use crate::types::jid::{DEFAULT_USER_SERVER, GROUP_SERVER, HIDDEN_USER_SERVER, Jid};
use crate::types::message::WaMessage;

use super::ack::attr_text;
use super::message::{attr_int, attr_jid};
use super::{Client, ClientError, InfoQuery, IqKind};

/// `xmlns` of every group IQ.
pub const GROUP_IQ_NAMESPACE: &str = "w:g2";
/// Go `InviteLinkPrefix` — the prefix of a group invite link.
pub const INVITE_LINK_PREFIX: &str = "https://chat.whatsapp.com/";
/// Go `types.GroupMemberAddModeAdmin`.
pub const MEMBER_ADD_MODE_ADMIN: &str = "admin_add";
/// Go `types.GroupMemberAddModeAllMember`.
pub const MEMBER_ADD_MODE_ALL_MEMBER: &str = "all_member_add";
/// How long a [`GroupCache`] entry stays usable (see the module docs).
pub const GROUP_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Go `types.GroupServerJID` — the `g.us` server JID (`create`/`invite`).
pub fn group_server_jid() -> Jid {
    Jid::new("", GROUP_SERVER)
}

/// Go `stripURLPrefix(code, InviteLinkPrefix)` — accept a bare code or a link.
pub fn strip_invite_prefix(link: &str) -> String {
    let link = link.trim();
    link.strip_prefix(INVITE_LINK_PREFIX).map(str::to_owned).unwrap_or_else(|| link.to_owned())
}

// ---------------------------------------------------------------------------
// group cache (Go `groupMetaCache` + `Client.groupCache`)
// ---------------------------------------------------------------------------

/// Go `groupMetaCache` — the per-group metadata the send path needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupMeta {
    /// `pn` or `lid`: which identity the group is addressed with.
    pub addressing_mode: String,
    /// Go `ReadOnly` — in an announcement group only admins may post.
    pub read_only: bool,
    /// Go `CommunityAnnouncementGroup` — the announce group of a community.
    pub community_announcement_group: bool,
    /// Every member, as the server listed them.
    pub members: Vec<Jid>,
    /// Members with admin rights (regular or super admin).
    pub admins: Vec<Jid>,
}

impl GroupMeta {
    /// Go `groupMetaCache.CanSend` — may `sender` post at all?
    ///
    /// The per-message exceptions (poll updates, reactions, comments, …) are
    /// in [`Self::can_send_message`].
    pub fn can_send(&self, sender: &Jid) -> bool {
        !self.read_only || self.admins.contains(&sender.to_non_ad())
    }

    /// [`Self::can_send`] plus Go's per-message exceptions: reactions, poll
    /// updates, event responses, comments and question responses stay allowed
    /// for everyone, even in an announcement group.
    pub fn can_send_message(&self, sender: &Jid, message: &WaMessage) -> bool {
        if !self.read_only {
            return true;
        }
        if message.poll_update_message.is_some()
            || message.reaction_message.is_some()
            || message.enc_reaction_message.is_some()
            || message.enc_event_response_message.is_some()
            || message.question_response_message.is_some()
            || message.comment_message.is_some()
            || message.enc_comment_message.is_some()
        {
            return true;
        }
        self.admins.contains(&sender.to_non_ad())
    }
}

/// One cache entry plus its insertion timestamp.
#[derive(Debug, Clone)]
struct CachedGroup {
    meta: GroupMeta,
    stored_at: Instant,
}

/// Go `Client.groupCache` with a TTL — the in-memory group metadata cache.
///
/// Keyed by group JID. An entry is dropped when a `w:gp2` notification
/// invalidates it ([`GroupCache::apply_group_change`]) or when it outlives the
/// TTL.
#[derive(Debug, Clone)]
pub struct GroupCache {
    ttl: Duration,
    entries: HashMap<Jid, CachedGroup>,
}

impl Default for GroupCache {
    fn default() -> Self {
        Self::new(GROUP_CACHE_TTL)
    }
}

impl GroupCache {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, entries: HashMap::new() }
    }

    /// The configured entry lifetime.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Number of entries, expired ones included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cached metadata for `jid` at `now`: `None` on a miss or when the
    /// entry is expired.
    pub fn get_at(&self, jid: &Jid, now: Instant) -> Option<&GroupMeta> {
        let entry = self.entries.get(jid)?;
        // `Instant` never goes backwards, so `saturating_duration_since` is exact.
        if now.saturating_duration_since(entry.stored_at) >= self.ttl {
            return None;
        }
        Some(&entry.meta)
    }

    /// [`Self::get_at`] with `Instant::now()`.
    pub fn get(&self, jid: &Jid) -> Option<&GroupMeta> {
        self.get_at(jid, Instant::now())
    }

    /// Store (or replace) the metadata of `jid`, stamped at `now`.
    pub fn insert_at(&mut self, jid: Jid, meta: GroupMeta, now: Instant) {
        self.entries.insert(jid, CachedGroup { meta, stored_at: now });
    }

    /// [`Self::insert_at`] with `Instant::now()`.
    pub fn insert(&mut self, jid: Jid, meta: GroupMeta) {
        self.insert_at(jid, meta, Instant::now());
    }

    /// Drop the entry of `jid`; `true` when there was one.
    pub fn invalidate(&mut self, jid: &Jid) -> bool {
        self.entries.remove(jid).is_some()
    }

    /// Drop every entry (disconnect / logout).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Go `updateGroupParticipantCache` — apply a `w:gp2` notification's
    /// participant/announce changes to the cached entry.
    ///
    /// A notification we cannot apply (no entry, or an expired one) drops the
    /// entry instead of leaving stale members behind: the next [`Client::cached_group_data`]
    /// refetches.
    pub fn apply_group_change(&mut self, evt: &GroupInfoEventData) {
        if evt.join.is_empty()
            && evt.leave.is_empty()
            && evt.promote.is_empty()
            && evt.demote.is_empty()
            && evt.announce.is_none()
        {
            return;
        }
        if self.get(&evt.jid).is_none() {
            self.invalidate(&evt.jid);
            return;
        }
        let Some(entry) = self.entries.get_mut(&evt.jid) else { return };
        if let Some(announce) = &evt.announce {
            entry.meta.read_only = announce.is_announce;
        }
        for jid in &evt.join {
            if !entry.meta.members.contains(jid) {
                entry.meta.members.push(jid.clone());
            }
        }
        for jid in &evt.leave {
            entry.meta.members.retain(|member| member != jid);
            entry.meta.admins.retain(|admin| admin != jid);
        }
        for jid in &evt.promote {
            if !entry.meta.admins.contains(jid) {
                entry.meta.admins.push(jid.clone());
            }
        }
        for jid in &evt.demote {
            entry.meta.admins.retain(|admin| admin != jid);
        }
    }
}

// ---------------------------------------------------------------------------
// IQ building
// ---------------------------------------------------------------------------

/// The `InfoQuery` of one group operation (Go `sendGroupIQ`).
pub fn build_group_query(kind: IqKind, to: &Jid, content: Node) -> InfoQuery {
    InfoQuery {
        namespace: GROUP_IQ_NAMESPACE.into(),
        kind,
        to: Some(to.clone()),
        content: vec![content],
        ..Default::default()
    }
}

/// The transport a group operation needs: one `w:g2` IQ round-trip.
///
/// Implemented for [`Client`]; tests implement it with a fake that records the
/// outgoing node and answers from a canned response (Go's `sendGroupIQ` behind
/// a fake server, without a socket).
#[async_trait]
pub trait GroupIqSender: Send + Sync {
    async fn send_group_iq(&self, kind: IqKind, to: &Jid, content: Node) -> Result<Node, ClientError>;
}

#[async_trait]
impl GroupIqSender for Client {
    async fn send_group_iq(&self, kind: IqKind, to: &Jid, content: Node) -> Result<Node, ClientError> {
        self.send_iq(build_group_query(kind, to, content)).await
    }
}

/// One participant of an `add`/`create`: the JID plus the extra attributes the
/// server wants (the phone-number JID and the privacy token, when we have them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParticipantUpdate {
    /// The participant JID as it goes on the wire.
    pub jid: Jid,
    /// Go's `phone_number` attribute — only for LID-addressed participants.
    pub phone_number: Option<Jid>,
    /// The `<tctoken>` content for this participant, when we have one.
    pub privacy_token: Option<Vec<u8>>,
}

impl ParticipantUpdate {
    /// A participant with no extras (a plain PN address).
    pub fn new(jid: Jid) -> Self {
        Self { jid, ..Default::default() }
    }
}

/// The `<participant>` children of an `add`/`remove`/`promote`/`demote`
/// (Go `UpdateGroupParticipants`' node building).
///
/// Only an `add` may carry the phone-number attribute and the privacy token;
/// Go fetches neither for the other actions.
pub fn build_participants_content(participants: &[ParticipantUpdate], action: ParticipantChange) -> Vec<Node> {
    let is_add = action == ParticipantChange::Add;
    participants
        .iter()
        .map(|participant| {
            let mut node = Node::new("participant").attr("jid", participant.jid.clone());
            if !is_add {
                return node;
            }
            if let Some(phone_number) = &participant.phone_number {
                node = node.attr("phone_number", phone_number.clone());
            }
            match &participant.privacy_token {
                // Go sends the privacy token as a `<privacy>` child here (not
                // `<tctoken>`, which is the usync spelling).
                Some(token) => {
                    node.with_content(vec![Node::new("privacy").with_content(NodeContent::Bytes(token.clone()))])
                }
                None => node,
            }
        })
        .collect()
}

/// Go `CreateGroup`'s request struct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateGroupRequest {
    /// The name (subject) of the new group.
    pub name: String,
    /// The initial participants (their order is preserved on the wire).
    pub participants: Vec<Jid>,
    /// Create the group with disappearing messages enabled.
    pub is_ephemeral: bool,
    /// The disappearing-messages timer in seconds.
    pub disappearing_timer: u32,
    /// Only admins may post.
    pub is_announce: bool,
    /// Only admins may edit the group info.
    pub is_locked: bool,
    /// New members need admin approval.
    pub is_join_approval_required: bool,
    /// [`MEMBER_ADD_MODE_ADMIN`] or [`MEMBER_ADD_MODE_ALL_MEMBER`].
    pub member_add_mode: Option<String>,
    /// Create the group as a community (parent) group.
    pub is_parent: bool,
    /// A community's default approval mode for its sub-groups.
    pub default_membership_approval_mode: Option<String>,
    /// Create the group as a sub-group of this community.
    pub linked_parent_jid: Option<Jid>,
}

/// The `<create>` content (Go `CreateGroup`'s node building), in Go's order:
/// participants, `member_add_mode`, `parent`/`linked_parent`, `locked`,
/// `announcement`, `ephemeral`, `membership_approval_mode`.
pub fn build_create_group_content(req: &CreateGroupRequest, participants: &[ParticipantUpdate]) -> Vec<Node> {
    let mut content = build_participants_content(participants, ParticipantChange::Add);
    // Go defaults the mode to `all_member_add` and always sends the child.
    let mode = req.member_add_mode.clone().unwrap_or_else(|| MEMBER_ADD_MODE_ALL_MEMBER.to_owned());
    content.push(Node::new("member_add_mode").with_content(NodeContent::Bytes(mode.into_bytes())));
    if req.is_parent {
        // Go defaults a community's sub-group approval mode to
        // `request_required`.
        let mode = req.default_membership_approval_mode.clone().unwrap_or_else(|| "request_required".to_owned());
        content.push(Node::new("parent").attr("default_membership_approval_mode", mode));
    } else if let Some(linked_parent_jid) = &req.linked_parent_jid {
        content.push(Node::new("linked_parent").attr("jid", linked_parent_jid.clone()));
    }
    if req.is_locked {
        content.push(Node::new("locked"));
    }
    if req.is_announce {
        content.push(Node::new("announcement"));
    }
    // Go always sends `<ephemeral>`, with `expiration=0` when the group is not
    // ephemeral.
    if req.is_ephemeral {
        content.push(
            Node::new("ephemeral")
                .attr("expiration", req.disappearing_timer.to_string())
                // Go hard-codes `trigger=1` here ("TODO what's this?").
                .attr("trigger", "1"),
        );
    } else {
        content.push(Node::new("ephemeral").attr("expiration", "0"));
    }
    let approval_state = if req.is_join_approval_required { "on" } else { "off" };
    content.push(
        Node::new("membership_approval_mode").with_content(vec![Node::new("group_join").attr("state", approval_state)]),
    );
    content
}

// ---------------------------------------------------------------------------
// node parsing
// ---------------------------------------------------------------------------

/// Go `parseParticipant` — one `<participant>` child of a group node.
pub fn parse_participant(child: &Node) -> GroupParticipantData {
    let participant_type = attr_text(child, "type").unwrap_or_default();
    let mut participant = GroupParticipantData {
        is_admin: participant_type == "admin" || participant_type == "superadmin",
        is_super_admin: participant_type == "superadmin",
        jid: attr_jid(child, "jid").unwrap_or_default(),
        display_name: attr_text(child, "display_name").unwrap_or_default(),
        ..Default::default()
    };
    if participant.jid.server == HIDDEN_USER_SERVER {
        participant.lid = participant.jid.clone();
        participant.phone_number = attr_jid(child, "phone_number").unwrap_or_default();
    } else if participant.jid.server == DEFAULT_USER_SERVER {
        participant.phone_number = participant.jid.clone();
        participant.lid = attr_jid(child, "lid").unwrap_or_default();
    }
    let error_code = attr_int(child, "error").unwrap_or_default();
    if error_code != 0 {
        participant.error = i32::try_from(error_code).unwrap_or_default();
        if let Some(add_request) = child.optional_child_by_tag(&["add_request"]) {
            participant.add_request = Some(GroupParticipantAddRequestData {
                code: attr_text(add_request, "code").unwrap_or_default(),
                expiration: attr_int(add_request, "expiration"),
            });
        }
    }
    participant
}

/// Go `parseParticipantList` — the `(JIDs, LID↔PN mappings)` of a
/// `<add>`/`<remove>`/`<promote>`/`<demote>` element.
pub fn parse_participant_list(node: &Node) -> (Vec<Jid>, Vec<LidMapping>) {
    let mut participants = Vec::new();
    let mut lid_pairs = Vec::new();
    for child in node.children() {
        if child.tag != "participant" {
            continue;
        }
        let Some(jid) = attr_jid(child, "jid") else { continue };
        participants.push(jid.clone());
        if jid.server == HIDDEN_USER_SERVER {
            if let Some(phone_number) = attr_jid(child, "phone_number").filter(|pn| !pn.is_empty()) {
                lid_pairs.push(LidMapping { lid: jid, pn: phone_number });
            }
        } else if jid.server == DEFAULT_USER_SERVER
            && let Some(lid) = attr_jid(child, "lid").filter(|lid| !lid.is_empty())
        {
            lid_pairs.push(LidMapping { lid, pn: jid });
        }
    }
    (participants, lid_pairs)
}

/// Go `parseGroupLinkTargetNode` — a `<group>` child of `sub_groups`/`link`.
pub fn parse_group_link_target(node: &Node) -> GroupLinkTargetData {
    let mut jid = attr_jid(node, "jid").unwrap_or_default();
    if jid.is_empty() {
        jid = Jid::new(attr_text(node, "id").unwrap_or_default(), GROUP_SERVER);
    }
    GroupLinkTargetData {
        jid,
        name: attr_text(node, "subject").unwrap_or_default(),
        name_set_at: attr_int(node, "s_t"),
        is_default_sub_group: node.optional_child_by_tag(&["default_sub_group"]).is_some(),
    }
}

/// Go `parseGroupNode` — the full metadata of one `<group>` node.
pub fn parse_group_node(group_node: &Node) -> GroupInfoData {
    let mut group = GroupInfoData {
        jid: Jid::new(attr_text(group_node, "id").unwrap_or_default(), GROUP_SERVER),
        owner_jid: attr_jid(group_node, "creator").unwrap_or_default(),
        owner_pn: attr_jid(group_node, "creator_pn").unwrap_or_default(),
        name: attr_text(group_node, "subject").unwrap_or_default(),
        name_set_at: attr_int(group_node, "s_t"),
        name_set_by: attr_jid(group_node, "s_o").unwrap_or_default(),
        name_set_by_pn: attr_jid(group_node, "s_o_pn").unwrap_or_default(),
        group_created: attr_int(group_node, "creation"),
        creator_country_code: attr_text(group_node, "creator_country_code").unwrap_or_default(),
        announce_version_id: attr_text(group_node, "a_v_id").unwrap_or_default(),
        participant_version_id: attr_text(group_node, "p_v_id").unwrap_or_default(),
        participant_count: i32::try_from(attr_int(group_node, "size").unwrap_or_default()).unwrap_or_default(),
        addressing_mode: attr_text(group_node, "addressing_mode").unwrap_or_default(),
        ..Default::default()
    };

    for child in group_node.children() {
        match child.tag.as_str() {
            "participant" => group.participants.push(parse_participant(child)),
            "description" => {
                if let Some(body) = child.optional_child_by_tag(&["body"]) {
                    group.topic = String::from_utf8_lossy(body.as_bytes().unwrap_or_default()).into_owned();
                    group.topic_id = attr_text(child, "id").unwrap_or_default();
                    group.topic_set_by = attr_jid(child, "participant").unwrap_or_default();
                    group.topic_set_by_pn = attr_jid(child, "participant_pn").unwrap_or_default();
                    group.topic_set_at = attr_int(child, "t");
                }
            }
            "announcement" => group.is_announce = true,
            "locked" => group.is_locked = true,
            "ephemeral" => {
                group.is_ephemeral = true;
                group.disappearing_timer =
                    u32::try_from(attr_int(child, "expiration").unwrap_or_default()).unwrap_or_default();
            }
            "member_add_mode" => {
                group.member_add_mode = String::from_utf8_lossy(child.as_bytes().unwrap_or_default()).into_owned();
            }
            "linked_parent" => group.linked_parent_jid = attr_jid(child, "jid").unwrap_or_default(),
            "default_sub_group" => group.is_default_sub_group = true,
            "parent" => {
                group.is_parent = true;
                group.default_membership_approval_mode =
                    attr_text(child, "default_membership_approval_mode").unwrap_or_default();
            }
            "incognito" => group.is_incognito = true,
            "membership_approval_mode" => group.is_join_approval_required = true,
            "suspended" => group.suspended = true,
            other => tracing::debug!("unknown element in group node {}: <{other}>", group.jid),
        }
    }
    group
}

/// Go `parseGroupCreate` — a `w:gp2` notification whose only child is
/// `<create>`: we just created the group (or were added to a new one).
pub fn parse_group_create(parent_node: &Node, node: &Node) -> Result<JoinedGroupData, ClientError> {
    let group_node = node
        .optional_child_by_tag(&["group"])
        .ok_or(ClientError::MissingElement { tag: "group", parent: "group create notification" })?;
    let mut info = parse_group_node(group_node);
    if info.addressing_mode.is_empty() {
        info.addressing_mode = attr_text(parent_node, "addressing_mode").unwrap_or_default();
    }
    Ok(JoinedGroupData {
        reason: attr_text(node, "reason").unwrap_or_default(),
        join_type: attr_text(node, "type").unwrap_or_default(),
        create_key: attr_text(node, "key").unwrap_or_default(),
        sender: attr_jid(parent_node, "participant").unwrap_or_default(),
        sender_pn: attr_jid(parent_node, "participant_pn").unwrap_or_default(),
        notify: attr_text(parent_node, "notify").unwrap_or_default(),
        info,
    })
}

/// Go `parseGroupChange` — a `w:gp2` notification carrying change children.
///
/// Returns the event payload plus the LID↔PN pairs the notification revealed
/// (the caller stores them in `LidStore`).
pub fn parse_group_change(node: &Node) -> Result<(GroupInfoEventData, Vec<LidMapping>), ClientError> {
    let jid = attr_jid(node, "from")
        .ok_or(ClientError::MissingElement { tag: "from", parent: "group change notification" })?;
    let mut evt = GroupInfoEventData {
        jid,
        notify: attr_text(node, "notify").unwrap_or_default(),
        sender: attr_jid(node, "participant").unwrap_or_default(),
        sender_pn: attr_jid(node, "participant_pn").unwrap_or_default(),
        timestamp: attr_int(node, "t"),
        ..Default::default()
    };
    let mut lid_pairs = Vec::new();
    for child in node.children() {
        if matches!(child.tag.as_str(), "add" | "remove" | "promote" | "demote") {
            evt.prev_participant_version_id = attr_text(child, "prev_v_id").unwrap_or_default();
            evt.participant_version_id = attr_text(child, "v_id").unwrap_or_default();
        }
        match child.tag.as_str() {
            "add" => {
                evt.join_reason = attr_text(child, "reason").unwrap_or_default();
                let (join, pairs) = parse_participant_list(child);
                evt.join = join;
                lid_pairs = pairs;
            }
            "remove" => {
                let (leave, pairs) = parse_participant_list(child);
                evt.leave = leave;
                lid_pairs = pairs;
            }
            "promote" => {
                let (promote, pairs) = parse_participant_list(child);
                evt.promote = promote;
                lid_pairs = pairs;
            }
            "demote" => {
                let (demote, pairs) = parse_participant_list(child);
                evt.demote = demote;
                lid_pairs = pairs;
            }
            "locked" => evt.locked = Some(GroupLockedData { is_locked: true }),
            "unlocked" => evt.locked = Some(GroupLockedData { is_locked: false }),
            "delete" => {
                evt.delete = Some(GroupDeleteData {
                    deleted: true,
                    delete_reason: attr_text(child, "reason").unwrap_or_default(),
                });
            }
            "subject" => {
                evt.name = Some(GroupNameData {
                    name: attr_text(child, "subject").unwrap_or_default(),
                    name_set_at: attr_int(child, "s_t"),
                    name_set_by: attr_jid(child, "s_o").unwrap_or_default(),
                    name_set_by_pn: attr_jid(child, "s_o_pn").unwrap_or_default(),
                });
            }
            "description" => {
                let deleted = child.optional_child_by_tag(&["delete"]).is_some();
                let topic = if deleted {
                    String::new()
                } else {
                    // Go errors out here when a non-delete description has no
                    // `<body>`: the change is not parseable.
                    let body = child
                        .optional_child_by_tag(&["body"])
                        .ok_or(ClientError::MissingElement { tag: "body", parent: "group description change" })?;
                    String::from_utf8_lossy(body.as_bytes().unwrap_or_default()).into_owned()
                };
                evt.topic = Some(GroupTopicData {
                    topic,
                    topic_id: attr_text(child, "id").unwrap_or_default(),
                    topic_set_at: evt.timestamp,
                    topic_set_by: evt.sender.clone(),
                    topic_set_by_pn: evt.sender_pn.clone(),
                    topic_deleted: deleted,
                });
            }
            "announcement" => {
                evt.announce = Some(GroupAnnounceData {
                    is_announce: true,
                    announce_version_id: attr_text(child, "v_id").unwrap_or_default(),
                });
            }
            "not_announcement" => {
                evt.announce = Some(GroupAnnounceData {
                    is_announce: false,
                    announce_version_id: attr_text(child, "v_id").unwrap_or_default(),
                });
            }
            "invite" => {
                if let Some(code) = attr_text(child, "code") {
                    evt.new_invite_link = Some(format!("{INVITE_LINK_PREFIX}{code}"));
                }
            }
            "ephemeral" => {
                evt.ephemeral = Some(GroupEphemeralData {
                    is_ephemeral: true,
                    disappearing_timer: u32::try_from(attr_int(child, "expiration").unwrap_or_default())
                        .unwrap_or_default(),
                });
            }
            "not_ephemeral" => {
                evt.ephemeral = Some(GroupEphemeralData { is_ephemeral: false, disappearing_timer: 0 });
            }
            "link" => {
                let group_node = child
                    .optional_child_by_tag(&["group"])
                    .ok_or(ClientError::MissingElement { tag: "group", parent: "group link" })?;
                evt.link = Some(GroupLinkChangeData {
                    change_type: attr_text(child, "link_type").unwrap_or_default(),
                    unlink_reason: String::new(),
                    group: parse_group_link_target(group_node),
                });
            }
            "unlink" => {
                let group_node = child
                    .optional_child_by_tag(&["group"])
                    .ok_or(ClientError::MissingElement { tag: "group", parent: "group unlink" })?;
                evt.unlink = Some(GroupLinkChangeData {
                    change_type: attr_text(child, "unlink_type").unwrap_or_default(),
                    unlink_reason: attr_text(child, "unlink_reason").unwrap_or_default(),
                    group: parse_group_link_target(group_node),
                });
            }
            "membership_approval_mode" => evt.membership_approval_mode = true,
            "suspended" => evt.suspended = true,
            "unsuspended" => evt.unsuspended = true,
            other => evt.unknown_changes.push(other.to_owned()),
        }
    }
    Ok((evt, lid_pairs))
}

// ---------------------------------------------------------------------------
// client operations
// ---------------------------------------------------------------------------

impl Client {
    /// Go `cacheGroupInfo`: derive the cache entry and the store writes from a
    /// parsed group, without touching the client.
    fn cache_group_info(info: &GroupInfoData) -> (GroupMeta, Vec<LidMapping>, Vec<RedactedPhoneEntry>) {
        let mut members = Vec::with_capacity(info.participants.len());
        let mut lid_pairs = Vec::new();
        let mut redacted_phones = Vec::new();
        let mut admins = Vec::new();
        for participant in &info.participants {
            members.push(participant.jid.clone());
            if participant.is_admin || participant.is_super_admin {
                admins.push(participant.jid.clone());
            }
            if !participant.phone_number.is_empty() && !participant.lid.is_empty() {
                lid_pairs.push(LidMapping { lid: participant.lid.clone(), pn: participant.phone_number.clone() });
            }
            if !participant.display_name.is_empty() && !participant.lid.is_empty() {
                redacted_phones.push(RedactedPhoneEntry {
                    jid: participant.lid.clone(),
                    redacted_phone: participant.display_name.clone(),
                });
            }
        }
        let meta = GroupMeta {
            addressing_mode: info.addressing_mode.clone(),
            read_only: info.is_announce,
            community_announcement_group: info.is_announce && info.is_default_sub_group,
            members,
            admins,
        };
        (meta, lid_pairs, redacted_phones)
    }

    /// Cache `info` and persist the LID mappings / redacted phones it carried
    /// (Go `cacheGroupInfo` + the `PutMany…` calls of `getGroupInfo`).
    pub(crate) async fn store_group_info(&self, info: &GroupInfoData) {
        let (meta, lid_pairs, redacted_phones) = Self::cache_group_info(info);
        self.group_cache.lock().await.insert(info.jid.clone(), meta);
        let (lids, contacts) = {
            let device = self.device.lock().await;
            (device.lids.clone(), device.contacts.clone())
        };
        if !lid_pairs.is_empty()
            && let Err(err) = lids.put_many_lid_mappings(&lid_pairs).await
        {
            tracing::warn!("failed to store LID mappings for members of {}: {err}", info.jid);
        }
        if !redacted_phones.is_empty()
            && let Err(err) = contacts.put_many_redacted_phones(redacted_phones).await
        {
            tracing::warn!("failed to store redacted phones for members of {}: {err}", info.jid);
        }
    }

    /// Go `getCachedGroupData` — the cached metadata of `jid`, fetched (and
    /// cached) through [`Self::get_group_info`] on a miss or an expired entry.
    pub async fn cached_group_data(&self, jid: &Jid) -> Result<GroupMeta, ClientError> {
        if let Some(meta) = self.group_cache.lock().await.get(jid).cloned() {
            return Ok(meta);
        }
        self.get_group_info(jid).await?;
        self.group_cache
            .lock()
            .await
            .get(jid)
            .cloned()
            .ok_or(ClientError::MissingElement { tag: "group", parent: "group cache after a successful info query" })
    }

    /// Go `GetGroupInfo` — ask the server for `jid`'s metadata, refresh the
    /// cache and return it.
    pub async fn get_group_info(&self, jid: &Jid) -> Result<GroupInfoData, ClientError> {
        let response = self.send_group_iq(IqKind::Get, jid, Node::new("query").attr("request", "interactive")).await?;
        let group_node = response
            .optional_child_by_tag(&["group"])
            .ok_or(ClientError::MissingElement { tag: "group", parent: "response to group info query" })?;
        let info = parse_group_node(group_node);
        self.store_group_info(&info).await;
        Ok(info)
    }

    /// The participants of an `add`/`create` as the wire wants them: LID
    /// participants carry their phone-number JID, and every participant gets
    /// the privacy token when we have a fresh one (Go's `ensureTCToken` loop).
    async fn prepare_participants(&self, participants: &[Jid], is_add: bool) -> Vec<ParticipantUpdate> {
        let lids = self.device.lock().await.lids.clone();
        let mut prepared = Vec::with_capacity(participants.len());
        for participant in participants {
            let jid = participant.to_non_ad();
            if !is_add {
                // Go fills only the JID for remove/promote/demote.
                prepared.push(ParticipantUpdate::new(jid));
                continue;
            }
            let phone_number = if jid.server == HIDDEN_USER_SERVER {
                match lids.get_pn_for_lid(&jid).await {
                    Ok(pn) if pn.as_ref().is_some_and(|pn| !pn.is_empty()) => pn,
                    Ok(_) => None,
                    Err(err) => {
                        tracing::warn!("failed to get the phone number for LID {jid}: {err}");
                        None
                    }
                }
            } else {
                None
            };
            let privacy_token = match self.ensure_tc_token(&jid).await {
                Ok(token) => token,
                Err(err) => {
                    tracing::warn!("failed to get a privacy token for participant {jid}: {err}");
                    None
                }
            };
            prepared.push(ParticipantUpdate { jid, phone_number, privacy_token });
        }
        prepared
    }

    /// Go `CreateGroup` — create a group and return its metadata.
    pub async fn create_group(&self, req: CreateGroupRequest) -> Result<GroupInfoData, ClientError> {
        let participants = self.prepare_participants(&req.participants, true).await;
        let content = build_create_group_content(&req, &participants);
        let mut create = Node::new("create").with_content(content);
        if !req.name.is_empty() {
            create = create.attr("subject", req.name.clone());
        }
        let response = self.send_group_iq(IqKind::Set, &group_server_jid(), create).await?;
        let group_node = response
            .optional_child_by_tag(&["group"])
            .ok_or(ClientError::MissingElement { tag: "group", parent: "response to create group query" })?;
        let info = parse_group_node(group_node);
        self.store_group_info(&info).await;
        Ok(info)
    }

    /// [`Self::create_group`] with just a name and participants — the shape
    /// `API-WHATSAPP.md` §3.3 documents.
    pub async fn create_group_named(&self, name: &str, participants: &[Jid]) -> Result<GroupInfoData, ClientError> {
        self.create_group(CreateGroupRequest {
            name: name.to_owned(),
            participants: participants.to_vec(),
            ..Default::default()
        })
        .await
    }

    /// Go `UpdateGroupParticipants` — add/remove/promote/demote and return the
    /// participants as the server reported them (with an `error` code when a
    /// change failed).
    pub async fn update_group_participants(
        &self,
        jid: &Jid,
        participants: &[Jid],
        action: ParticipantChange,
    ) -> Result<Vec<GroupParticipantData>, ClientError> {
        let prepared = self.prepare_participants(participants, action == ParticipantChange::Add).await;
        let result = update_group_participants_with(self, jid, &prepared, action).await;
        if result.is_ok() {
            // The member list changed: make the next send refetch it.
            self.group_cache.lock().await.invalidate(jid);
        }
        result
    }

    /// Go `SetGroupName` — change the group's subject.
    pub async fn set_group_name(&self, jid: &Jid, name: &str) -> Result<(), ClientError> {
        let content = Node::new("subject").with_content(NodeContent::Bytes(name.as_bytes().to_vec()));
        self.send_group_iq(IqKind::Set, jid, content).await.map(|_| ())
    }

    /// Go `SetGroupTopic` — change (or clear) the group's description.
    ///
    /// The previous topic ID is fetched when `previous_id` is empty (Go does
    /// the same), and `new_id` defaults to a fresh message ID.
    pub async fn set_group_topic(
        &self,
        jid: &Jid,
        previous_id: &str,
        new_id: &str,
        topic: &str,
    ) -> Result<(), ClientError> {
        let previous_id =
            if previous_id.is_empty() { self.get_group_info(jid).await?.topic_id } else { previous_id.to_owned() };
        let new_id =
            if new_id.is_empty() { self.generate_message_id().await.as_str().to_owned() } else { new_id.to_owned() };
        let mut description = Node::new("description").attr("id", new_id);
        if !previous_id.is_empty() {
            description = description.attr("prev", previous_id);
        }
        if topic.is_empty() {
            description = description.attr("delete", "true");
        } else {
            description = description.with_content(NodeContent::Bytes(topic.as_bytes().to_vec()));
        }
        self.send_group_iq(IqKind::Set, jid, description).await.map(|_| ())
    }

    /// Go `SetGroupLocked` — only admins may edit the group info.
    pub async fn set_group_locked(&self, jid: &Jid, locked: bool) -> Result<(), ClientError> {
        let tag = if locked { "locked" } else { "unlocked" };
        self.send_group_iq(IqKind::Set, jid, Node::new(tag)).await.map(|_| ())
    }

    /// Go `SetGroupAnnounce` — only admins may post.
    pub async fn set_group_announce(&self, jid: &Jid, announce: bool) -> Result<(), ClientError> {
        let tag = if announce { "announcement" } else { "not_announcement" };
        let result = self.send_group_iq(IqKind::Set, jid, Node::new(tag)).await.map(|_| ());
        if result.is_ok() {
            self.group_cache.lock().await.invalidate(jid);
        }
        result
    }

    /// Go `SetGroupJoinApprovalMode` — new members need admin approval.
    pub async fn set_group_join_approval_mode(&self, jid: &Jid, mode: bool) -> Result<(), ClientError> {
        let state = if mode { "on" } else { "off" };
        let content =
            Node::new("membership_approval_mode").with_content(vec![Node::new("group_join").attr("state", state)]);
        self.send_group_iq(IqKind::Set, jid, content).await.map(|_| ())
    }

    /// Go `SetGroupMemberAddMode` — who may add members.
    pub async fn set_group_member_add_mode(&self, jid: &Jid, mode: &str) -> Result<(), ClientError> {
        if mode != MEMBER_ADD_MODE_ADMIN && mode != MEMBER_ADD_MODE_ALL_MEMBER {
            return Err(ClientError::InvalidArgument(
                "invalid member add mode, must be 'admin_add' or 'all_member_add'",
            ));
        }
        let content = Node::new("member_add_mode").with_content(NodeContent::Bytes(mode.as_bytes().to_vec()));
        self.send_group_iq(IqKind::Set, jid, content).await.map(|_| ())
    }

    /// Go `GetGroupInviteLink` — fetch (and with `reset`, rotate) the invite
    /// link of `jid`.
    pub async fn get_group_invite_link(&self, jid: &Jid, reset: bool) -> Result<String, ClientError> {
        let kind = if reset { IqKind::Set } else { IqKind::Get };
        let response = self.send_group_iq(kind, jid, Node::new("invite")).await?;
        let code = response
            .optional_child_by_tag(&["invite"])
            .and_then(|invite| attr_text(invite, "code"))
            .ok_or(ClientError::MissingElement { tag: "invite", parent: "response to group invite query" })?;
        Ok(format!("{INVITE_LINK_PREFIX}{code}"))
    }

    /// Go `JoinGroupWithLink` — join through an invite code or link and return
    /// the group JID.
    ///
    /// When the group requires approval the server answers with a
    /// `membership_approval_request` instead of the group node; its `jid` is
    /// returned too, so the caller can tell the two apart by re-reading
    /// [`Self::get_group_info`].
    pub async fn join_group_with_link(&self, code: &str) -> Result<Jid, ClientError> {
        let invite = Node::new("invite").attr("code", strip_invite_prefix(code));
        let response = self.send_group_iq(IqKind::Set, &group_server_jid(), invite).await?;
        if let Some(request) = response.optional_child_by_tag(&["membership_approval_request"]) {
            return Ok(attr_jid(request, "jid").unwrap_or_default());
        }
        let group_node = response
            .optional_child_by_tag(&["group"])
            .ok_or(ClientError::MissingElement { tag: "group", parent: "response to group link join query" })?;
        let jid = attr_jid(group_node, "jid")
            .unwrap_or_else(|| Jid::new(attr_text(group_node, "id").unwrap_or_default(), GROUP_SERVER));
        self.group_cache.lock().await.invalidate(&jid);
        Ok(jid)
    }

    /// Go `GetGroupInfoFromLink` — resolve an invite code without joining.
    pub async fn get_group_info_from_link(&self, code: &str) -> Result<GroupInfoData, ClientError> {
        let invite = Node::new("invite").attr("code", strip_invite_prefix(code));
        let response = self.send_group_iq(IqKind::Get, &group_server_jid(), invite).await?;
        let group_node = response
            .optional_child_by_tag(&["group"])
            .ok_or(ClientError::MissingElement { tag: "group", parent: "response to group link info query" })?;
        Ok(parse_group_node(group_node))
    }

    /// Go `LeaveGroup` — leave `jid`.
    pub async fn leave_group(&self, jid: &Jid) -> Result<(), ClientError> {
        let leave = Node::new("leave").with_content(vec![Node::new("group").attr("id", jid.clone())]);
        let result = self.send_group_iq(IqKind::Set, &group_server_jid(), leave).await.map(|_| ());
        // The cached member list must not survive leaving the group.
        self.group_cache.lock().await.invalidate(jid);
        result
    }

    /// Go `GetGroupInfoFromInvite` — the group an invite *message* points at.
    pub async fn get_group_info_from_invite(
        &self,
        jid: &Jid,
        inviter: &Jid,
        code: &str,
        expiration: i64,
    ) -> Result<GroupInfoData, ClientError> {
        let add_request = Node::new("add_request")
            .attr("code", code)
            .attr("expiration", expiration.to_string())
            .attr("admin", inviter.clone());
        let query = Node::new("query").with_content(vec![add_request]);
        let response = self.send_group_iq(IqKind::Get, jid, query).await?;
        let group_node = response
            .optional_child_by_tag(&["group"])
            .ok_or(ClientError::MissingElement { tag: "group", parent: "response to invite group info query" })?;
        Ok(parse_group_node(group_node))
    }

    /// Go `JoinGroupWithInvite` — accept an invite *message*.
    pub async fn join_group_with_invite(
        &self,
        jid: &Jid,
        inviter: &Jid,
        code: &str,
        expiration: i64,
    ) -> Result<(), ClientError> {
        let accept = Node::new("accept")
            .attr("code", code)
            .attr("expiration", expiration.to_string())
            .attr("admin", inviter.clone());
        let result = self.send_group_iq(IqKind::Set, jid, accept).await.map(|_| ());
        self.group_cache.lock().await.invalidate(jid);
        result
    }
}

/// Go `UpdateGroupParticipants`' request half, against any
/// [`GroupIqSender`] — the fake-`send_iq` entry point of the tests.
pub async fn update_group_participants_with(
    sender: &dyn GroupIqSender,
    group: &Jid,
    participants: &[ParticipantUpdate],
    action: ParticipantChange,
) -> Result<Vec<GroupParticipantData>, ClientError> {
    let content = build_participants_content(participants, action);
    let update = Node::new(action.as_str()).with_content(content);
    let response = sender.send_group_iq(IqKind::Set, group, update).await?;
    let changed = response
        .optional_child_by_tag(&[action.as_str()])
        .ok_or(ClientError::MissingElement { tag: action.as_str(), parent: "response to group participants update" })?;
    Ok(changed.children_by_tag("participant").map(parse_participant).collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::binary::node::Node;
    use crate::types::jid::Jid;

    fn jid(value: &str) -> Jid {
        Jid::parse(value).unwrap()
    }

    fn group_jid() -> Jid {
        jid("120363000000000001@g.us")
    }

    fn own() -> Jid {
        jid("491234567890@s.whatsapp.net")
    }

    fn admin() -> Jid {
        jid("491111111111@s.whatsapp.net")
    }

    fn meta(read_only: bool, admins: Vec<Jid>) -> GroupMeta {
        GroupMeta {
            addressing_mode: "pn".to_owned(),
            read_only,
            community_announcement_group: false,
            members: vec![own(), admin()],
            admins,
        }
    }

    // -- cache ---------------------------------------------------------------

    #[test]
    fn group_cache_misses_until_an_entry_is_inserted() {
        let mut cache = GroupCache::new(Duration::from_secs(60));
        let now = Instant::now();
        assert!(cache.get_at(&group_jid(), now).is_none(), "an empty cache must miss");
        cache.insert_at(group_jid(), meta(false, vec![admin()]), now);
        let hit = cache.get_at(&group_jid(), now).expect("the inserted entry must be a hit");
        assert_eq!(hit.members, vec![own(), admin()]);
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());
    }

    #[test]
    fn group_cache_entry_expires_after_its_ttl() {
        let mut cache = GroupCache::new(Duration::from_secs(60));
        let now = Instant::now();
        cache.insert_at(group_jid(), meta(false, vec![]), now);
        assert!(cache.get_at(&group_jid(), now + Duration::from_secs(59)).is_some(), "still inside the TTL");
        assert!(cache.get_at(&group_jid(), now + Duration::from_secs(60)).is_none(), "an expired entry is a miss");
    }

    #[test]
    fn group_cache_invalidate_drops_the_entry() {
        let mut cache = GroupCache::default();
        cache.insert(group_jid(), meta(false, vec![]));
        assert!(cache.get(&group_jid()).is_some());
        assert!(cache.invalidate(&group_jid()), "the entry existed");
        assert!(cache.get(&group_jid()).is_none());
        assert!(!cache.invalidate(&group_jid()), "invalidating twice is a no-op");
        cache.insert(group_jid(), meta(false, vec![]));
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn group_change_notification_updates_a_cached_entry() {
        let mut cache = GroupCache::default();
        cache.insert(group_jid(), meta(false, vec![]));
        let newbie = jid("492222222222@s.whatsapp.net");
        let evt = GroupInfoEventData {
            jid: group_jid(),
            join: vec![newbie.clone()],
            promote: vec![newbie.clone()],
            ..Default::default()
        };
        cache.apply_group_change(&evt);
        let cached = cache.get(&group_jid()).expect("still cached");
        assert!(cached.members.contains(&newbie), "the join must land in the member list");
        assert!(cached.admins.contains(&newbie), "the promote must land in the admin list");

        cache.apply_group_change(&GroupInfoEventData {
            jid: group_jid(),
            demote: vec![newbie.clone()],
            ..Default::default()
        });
        assert!(!cache.get(&group_jid()).expect("still cached").admins.contains(&newbie));

        cache.apply_group_change(&GroupInfoEventData {
            jid: group_jid(),
            leave: vec![newbie.clone()],
            ..Default::default()
        });
        let cached = cache.get(&group_jid()).expect("still cached");
        assert!(!cached.members.contains(&newbie), "the leave must drop the member");
    }

    #[test]
    fn group_change_for_an_unknown_group_does_not_create_an_entry() {
        let mut cache = GroupCache::default();
        cache.apply_group_change(&GroupInfoEventData { jid: group_jid(), join: vec![own()], ..Default::default() });
        assert!(cache.is_empty(), "Go returns early when the group is not cached");
    }

    #[test]
    fn group_change_can_toggle_the_read_only_flag() {
        let mut cache = GroupCache::default();
        cache.insert(group_jid(), meta(false, vec![]));
        cache.apply_group_change(&GroupInfoEventData {
            jid: group_jid(),
            announce: Some(GroupAnnounceData { is_announce: true, announce_version_id: "2".to_owned() }),
            ..Default::default()
        });
        assert!(cache.get(&group_jid()).expect("cached").read_only);
    }

    // -- CanSend -------------------------------------------------------------

    #[test]
    fn can_send_is_true_for_everyone_in_a_normal_group() {
        let group = meta(false, vec![]);
        assert!(group.can_send(&own()), "a non-announce group accepts a member");
        assert!(group.can_send(&admin()));
    }

    #[test]
    fn can_send_is_admin_only_in_an_announcement_group() {
        let group = meta(true, vec![admin()]);
        assert!(group.can_send(&admin()), "an admin may always post");
        assert!(!group.can_send(&own()), "a regular member may not post");
        // An AD JID is reduced to its user JID before the admin check (Go
        // `sender.ToNonAD()`).
        assert!(group.can_send(&jid("491111111111:12@s.whatsapp.net")));
    }

    #[test]
    fn can_send_message_allows_reactions_and_poll_updates_in_an_announcement_group() {
        let group = meta(true, vec![admin()]);
        assert!(!group.can_send_message(&own(), &WaMessage::default()), "a plain message is rejected");
        let reaction = WaMessage { reaction_message: Some(Default::default()), ..Default::default() };
        assert!(group.can_send_message(&own(), &reaction), "reactions are exempt");
        let poll_update = WaMessage { poll_update_message: Some(Default::default()), ..Default::default() };
        assert!(group.can_send_message(&own(), &poll_update), "poll updates are exempt");
    }

    // -- IQ building ---------------------------------------------------------

    #[test]
    fn group_query_targets_the_group_with_the_g2_namespace() {
        let query = build_group_query(IqKind::Get, &group_jid(), Node::new("query"));
        assert_eq!(query.namespace, "w:g2");
        assert_eq!(query.kind, IqKind::Get);
        assert_eq!(query.to, Some(group_jid()));
        let node = query.to_node("1");
        assert_eq!(attr_text(&node, "xmlns").as_deref(), Some("w:g2"));
        assert_eq!(attr_text(&node, "to").as_deref(), Some("120363000000000001@g.us"));
        assert_eq!(node.children()[0].tag, "query");
    }

    #[test]
    fn participant_content_carries_the_phone_number_and_privacy_token() {
        let lid = jid("999000111222333@lid");
        let pn = jid("491234567890@s.whatsapp.net");
        let participants = [
            ParticipantUpdate {
                jid: lid.clone(),
                phone_number: Some(pn.clone()),
                privacy_token: Some(vec![0xAA, 0xBB]),
            },
            ParticipantUpdate::new(pn.clone()),
        ];
        let nodes = build_participants_content(&participants, ParticipantChange::Add);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].tag, "participant");
        assert_eq!(attr_jid(&nodes[0], "jid").as_ref(), Some(&lid));
        assert_eq!(attr_jid(&nodes[0], "phone_number").as_ref(), Some(&pn));
        assert_eq!(nodes[0].children()[0].tag, "privacy", "the token child is <privacy>");
        assert_eq!(nodes[0].children()[0].as_bytes(), Some(&[0xAA, 0xBB][..]));
        assert_eq!(attr_jid(&nodes[1], "jid").as_ref(), Some(&pn));
        assert!(nodes[1].children().is_empty(), "no token, no child");

        let demote = build_participants_content(&participants, ParticipantChange::Demote);
        assert_eq!(demote[0].tag, "participant");
        assert!(
            attr_jid(&demote[0], "phone_number").is_none() && demote[0].children().is_empty(),
            "only an add carries the phone number and the privacy token"
        );
    }

    /// A fake `sendGroupIQ`: records the outgoing node and answers with a
    /// canned response (Go's `sendGroupIQ` behind a stub).
    struct FakeSender {
        response: Node,
        sent: StdMutex<Vec<(IqKind, Jid, Node)>>,
    }

    impl FakeSender {
        fn new(response: Node) -> Self {
            Self { response, sent: StdMutex::new(Vec::new()) }
        }

        fn last(&self) -> (IqKind, Jid, Node) {
            let sent = self.sent.lock().unwrap_or_else(|err| err.into_inner());
            sent.last().cloned().expect("a group IQ was sent")
        }
    }

    #[async_trait]
    impl GroupIqSender for FakeSender {
        async fn send_group_iq(&self, kind: IqKind, to: &Jid, content: Node) -> Result<Node, ClientError> {
            self.sent.lock().unwrap_or_else(|err| err.into_inner()).push((kind, to.clone(), content));
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn participant_action_iq_round_trip_against_a_fake_send_iq() {
        let lid = jid("999000111222333@lid");
        let pn = jid("491234567890@s.whatsapp.net");
        let response = Node::new("iq").with_content(vec![Node::new("add").with_content(vec![
            Node::new("participant").attr("jid", lid.clone()).attr("phone_number", pn.clone()).attr("type", "admin"),
            Node::new("participant").attr("jid", pn.clone()).attr("error", "403"),
        ])]);
        let sender = FakeSender::new(response);
        let participants = [ParticipantUpdate {
            jid: lid.clone(),
            phone_number: Some(pn.clone()),
            privacy_token: Some(vec![1, 2, 3]),
        }];

        let result = update_group_participants_with(&sender, &group_jid(), &participants, ParticipantChange::Add)
            .await
            .expect("the fake answers with an <add>");

        // The request: one `w:g2` set IQ to the group with an `<add>` child.
        let (kind, to, content) = sender.last();
        assert_eq!(kind, IqKind::Set);
        assert_eq!(to, group_jid());
        assert_eq!(content.tag, "add");
        assert_eq!(content.children()[0].tag, "participant");
        assert_eq!(attr_jid(&content.children()[0], "jid").as_ref(), Some(&lid));
        assert_eq!(attr_jid(&content.children()[0], "phone_number").as_ref(), Some(&pn));
        assert_eq!(content.children()[0].children()[0].tag, "privacy");

        // The response: parsed participants, the error code preserved.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].jid, lid);
        assert_eq!(result[0].lid, lid, "a LID participant is its own LID");
        assert_eq!(result[0].phone_number, pn);
        assert!(result[0].is_admin && !result[0].is_super_admin);
        assert_eq!(result[1].error, 403);
    }

    #[tokio::test]
    async fn participant_action_without_the_expected_response_child_is_an_error() {
        let sender = FakeSender::new(Node::new("iq"));
        let participants = [ParticipantUpdate::new(own())];
        let result =
            update_group_participants_with(&sender, &group_jid(), &participants, ParticipantChange::Remove).await;
        assert!(matches!(result, Err(ClientError::MissingElement { tag: "remove", .. })));
    }

    #[test]
    fn create_group_content_matches_the_go_child_order() {
        let participants = [ParticipantUpdate::new(own())];
        let req = CreateGroupRequest {
            name: "Rust port".to_owned(),
            participants: vec![own()],
            is_ephemeral: true,
            disappearing_timer: 86_400,
            is_announce: true,
            is_locked: true,
            is_join_approval_required: true,
            member_add_mode: Some(MEMBER_ADD_MODE_ADMIN.to_owned()),
            is_parent: false,
            default_membership_approval_mode: None,
            linked_parent_jid: Some(group_jid()),
        };
        let content = build_create_group_content(&req, &participants);
        let tags: Vec<&str> = content.iter().map(|node| node.tag.as_str()).collect();
        assert_eq!(
            tags,
            [
                "participant",
                "member_add_mode",
                "linked_parent",
                "locked",
                "announcement",
                "ephemeral",
                "membership_approval_mode"
            ]
        );
    }

    #[test]
    fn create_group_defaults_member_add_mode_to_all_member_add() {
        let req = CreateGroupRequest { name: "g".to_owned(), ..Default::default() };
        let content = build_create_group_content(&req, &[]);
        let mode = &content[0];
        assert_eq!(mode.tag, "member_add_mode");
        assert_eq!(mode.as_bytes(), Some(MEMBER_ADD_MODE_ALL_MEMBER.as_bytes()));
        // Not ephemeral: Go still sends `<ephemeral expiration="0">`.
        assert_eq!(content[1].tag, "ephemeral");
        assert_eq!(attr_text(&content[1], "expiration").as_deref(), Some("0"));
        assert_eq!(content[2].tag, "membership_approval_mode");
        assert_eq!(attr_text(&content[2].children()[0], "state").as_deref(), Some("off"));
    }

    #[test]
    fn invite_links_and_bare_codes_resolve_to_the_same_code() {
        assert_eq!(strip_invite_prefix("https://chat.whatsapp.com/ABC123"), "ABC123");
        assert_eq!(strip_invite_prefix("ABC123"), "ABC123");
        assert_eq!(strip_invite_prefix("  ABC123  "), "ABC123");
    }

    // -- parsing -------------------------------------------------------------

    #[test]
    fn parse_group_node_reads_the_metadata_children() {
        let node = Node::new("group")
            .attr("id", "120363000000000001")
            .attr("creator", own())
            .attr("subject", "Rust port")
            .attr("s_t", "1700000000")
            .attr("size", "2")
            .attr("addressing_mode", "lid")
            .with_content(vec![
                Node::new("participant").attr("jid", admin()).attr("type", "superadmin"),
                Node::new("participant").attr("jid", jid("999000111222333@lid")).attr("phone_number", own()),
                Node::new("announcement"),
                Node::new("locked"),
                Node::new("ephemeral").attr("expiration", "86400"),
                Node::new("description")
                    .attr("id", "T1")
                    .with_content(vec![Node::new("body").with_content(NodeContent::Bytes(b"hello".to_vec()))]),
            ]);
        let info = parse_group_node(&node);
        assert_eq!(info.jid, group_jid());
        assert_eq!(info.name, "Rust port");
        assert_eq!(info.name_set_at, Some(1_700_000_000));
        assert_eq!(info.participant_count, 2);
        assert_eq!(info.addressing_mode, "lid");
        assert!(info.is_announce && info.is_locked && info.is_ephemeral);
        assert_eq!(info.disappearing_timer, 86_400);
        assert_eq!(info.topic, "hello");
        assert_eq!(info.participants.len(), 2);
        assert!(info.participants[0].is_super_admin);
        assert_eq!(info.participants[1].lid, jid("999000111222333@lid"));
        assert_eq!(info.participants[1].phone_number, own());
    }

    #[test]
    fn parse_group_change_reads_the_participant_actions() {
        let newcomer = jid("492222222222@s.whatsapp.net");
        let node = Node::new("notification")
            .attr("from", group_jid())
            .attr("participant", admin())
            .attr("t", "1700000001")
            .with_content(vec![
                Node::new("add")
                    .attr("v_id", "7")
                    .with_content(vec![Node::new("participant").attr("jid", newcomer.clone())]),
                // Go overwrites both version IDs for every participant action
                // child, so the last one wins; `promote` also carries one here.
                Node::new("promote")
                    .attr("prev_v_id", "6")
                    .attr("v_id", "8")
                    .with_content(vec![Node::new("participant").attr("jid", newcomer.clone())]),
                Node::new("subject").attr("subject", "new name"),
            ]);
        let (evt, lid_pairs) = parse_group_change(&node).expect("a group change");
        assert_eq!(evt.jid, group_jid());
        assert_eq!(evt.sender, admin());
        assert_eq!(evt.timestamp, Some(1_700_000_001));
        assert_eq!(evt.join, vec![newcomer.clone()]);
        assert_eq!(evt.promote, vec![newcomer]);
        assert_eq!(evt.participant_version_id, "8");
        assert_eq!(evt.prev_participant_version_id, "6");
        assert_eq!(evt.name.as_ref().map(|name| name.name.as_str()), Some("new name"));
        assert!(lid_pairs.is_empty(), "a PN-only add reveals no LID mapping");
    }

    #[test]
    fn group_change_reads_a_description_set_and_cleared() {
        let set = Node::new("notification").attr("from", group_jid()).with_content(vec![
            Node::new("description")
                .attr("id", "T1")
                .with_content(vec![Node::new("body").with_content(NodeContent::Bytes(b"hello".to_vec()))]),
        ]);
        let (evt, _) = parse_group_change(&set).expect("a description change");
        let topic = evt.topic.expect("a topic");
        assert_eq!(topic.topic, "hello");
        assert_eq!(topic.topic_id, "T1");
        assert!(!topic.topic_deleted);

        let deleted = Node::new("notification")
            .attr("from", group_jid())
            .with_content(vec![Node::new("description").attr("id", "T2").with_content(vec![Node::new("delete")])]);
        let (evt, _) = parse_group_change(&deleted).expect("a deleted description");
        let topic = evt.topic.expect("a topic");
        assert!(topic.topic_deleted);
        assert_eq!(topic.topic, "");
    }

    #[test]
    fn group_change_description_without_a_body_is_rejected() {
        // Go returns an error here instead of silently keeping a partial change.
        let node = Node::new("notification")
            .attr("from", group_jid())
            .with_content(vec![Node::new("description").attr("id", "T1")]);
        assert!(matches!(parse_group_change(&node), Err(ClientError::MissingElement { tag: "body", .. })));
    }

    #[test]
    fn group_change_records_unknown_children_instead_of_dropping_them() {
        let node = Node::new("notification").attr("from", group_jid()).with_content(vec![Node::new("future_thing")]);
        let (evt, _) = parse_group_change(&node).expect("a change");
        assert_eq!(evt.unknown_changes, vec!["future_thing".to_owned()]);
    }

    #[test]
    fn group_change_without_a_from_attribute_is_rejected() {
        let node = Node::new("notification").with_content(vec![Node::new("locked")]);
        assert!(matches!(parse_group_change(&node), Err(ClientError::MissingElement { tag: "from", .. })));
    }
}
