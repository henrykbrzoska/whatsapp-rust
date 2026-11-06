//! Port of `Go types/events/events.go` — the `WaEvent` union sent to
//! the UI (`API-WHATSAPP.md` §2). Phase 0 carries connection-level and local
//! store events; message/group/presence variants arrive with their phases.

use serde::{Deserialize, Serialize};
use specta::Type;

use super::jid::Jid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
#[specta(rename_all = "snake_case")]
pub enum WaEvent {
    #[specta(rename_all = "camelCase")]
    Qr {
        codes: Vec<String>,
    },
    #[specta(rename_all = "camelCase")]
    PairSuccess {
        id: String,
        lid: String,
        business_name: String,
        platform: String,
    },
    #[specta(rename_all = "camelCase")]
    PairError {
        id: String,
        lid: String,
        business_name: String,
        platform: String,
        error: String,
    },
    Connected,
    Disconnected,
    #[specta(rename_all = "camelCase")]
    LoggedOut {
        on_connect: bool,
        reason: i32,
    },
    StreamReplaced,
    #[specta(rename_all = "camelCase")]
    StreamError {
        code: String,
    },
    ManualLoginReconnect,
    QrScannedWithoutMultidevice,
    #[specta(rename_all = "camelCase")]
    ConnectFailure {
        reason: i32,
        message: String,
    },
    ClientOutdated,
    #[specta(rename_all = "camelCase")]
    TemporaryBan {
        code: i32,
        expire_ms: i64,
    },
    #[specta(rename_all = "camelCase")]
    KeepAliveTimeout {
        error_count: u32,
    },
    KeepAliveRestored,
    /// History-sync notification (Go `events.HistorySyncNotification`): the
    /// phone announces a blob of history. The engine acks it, emits this, and
    /// then downloads and imports the blob — see `HistoryImported`.
    #[specta(rename_all = "camelCase")]
    HistorySync {
        message_id: String,
        chat: String,
        sender: String,
        conversation_timestamp: i64,
        sync_type: String,
    },
    /// A history-sync blob was downloaded and written into the local database
    /// (`bebok-server::history_import`). The UI refreshes the chat list and
    /// the open thread when this arrives.
    #[specta(rename_all = "camelCase")]
    HistoryImported {
        /// Messages the blob contained.
        messages: u64,
        /// Messages that were new to the database.
        stored: u64,
        /// Chats the blob described.
        conversations: u64,
        /// Chats that got a display name out of it.
        named: u64,
        /// `true` when the phone said this was its oldest chunk.
        end_of_history: bool,
    },
    #[specta(rename_all = "camelCase")]
    HistoryExportProgress {
        chat: String,
        exported: u64,
        total: u64,
    },
    #[specta(rename_all = "camelCase")]
    SearchIndexProgress {
        indexed: u64,
        total: u64,
    },
    /// Go `events.Message` — one decrypted incoming message (`API-WHATSAPP.md`
    /// §2). `message` is the unwrapped `waE2E::Message` and `raw` the message as
    /// it arrived, both hex-encoded protobufs (the UI decodes them; a JSON
    /// mapping is TODO(Phase 5)).
    #[specta(rename_all = "camelCase")]
    Message {
        info: crate::types::message::MessageInfo,
        message: Option<String>,
        raw: Option<String>,
        /// The plain-text body, when the message has one.
        text: Option<String>,
        is_ephemeral: bool,
        is_view_once: bool,
        is_edit: bool,
        retry_count: i32,
    },
    /// Go `events.FBMessage` — a decoded Facebook/Instagram (armadillo) message;
    /// the payload stays an opaque hex blob until the armadillo mapper lands.
    #[specta(rename_all = "camelCase")]
    FbMessage {
        info: crate::types::message::MessageInfo,
        /// Hex-encoded `waMsgTransport::MessageTransport`.
        transport: Option<String>,
        /// Hex-encoded `waConsumerApplication::ConsumerApplication`, when the
        /// transport carried one.
        application: Option<String>,
        retry_count: i32,
    },
    /// Go `events.UndecryptableMessage` — a message we could not decrypt (or
    /// that arrived without an `<enc>` child at all).
    #[specta(rename_all = "camelCase")]
    UndecryptableMessage {
        info: crate::types::message::MessageInfo,
        /// True when the node had no `<enc>` child (`UnavailableMessage`).
        is_unavailable: bool,
        /// The `<unavailable type="…">` reason, when `is_unavailable`.
        unavailable_type: String,
        /// Why decryption failed: `identity`, `sender_key`, `decrypt` or
        /// `no_session`; empty when the message was simply unavailable.
        decrypt_fail_mode: String,
    },
    /// Go `events.IdentityChange` — a contact's identity key changed (or a
    /// session was reset implicitly).
    #[specta(rename_all = "camelCase")]
    IdentityChange {
        jid: String,
        implicit: bool,
    },
    /// Go `events.Receipt` — one receipt (`delivered`/`read`/`played`/…); the
    /// UI derives the ticks from it.
    #[specta(rename_all = "camelCase")]
    Receipt {
        receipt: crate::types::receipt::Receipt,
    },
    /// A retry receipt was answered: the cached message was re-encrypted and
    /// sent again (`client/retry.rs`).
    #[specta(rename_all = "camelCase")]
    RetryReceiptResent {
        chat: String,
        sender: String,
        message_id: String,
        retry_count: i32,
    },
    /// A retry receipt could not be answered (no cached message, no session,
    /// send failure).
    #[specta(rename_all = "camelCase")]
    RetryReceiptFailed {
        chat: String,
        sender: String,
        message_id: String,
        retry_count: i32,
        error: String,
    },
    /// The Signal session was dropped so the next send starts from a fresh
    /// pre-key bundle (`shouldRecreateSession`).
    #[specta(rename_all = "camelCase")]
    SessionRecreated {
        jid: String,
        reason: String,
    },
    /// Go `events.MediaRetry` — the server asked us to re-upload a media file
    /// (`client/mediaretry.rs`). Task E extends this variant with the upload
    /// result; the fields below are the notification itself.
    #[specta(rename_all = "camelCase")]
    MediaRetry {
        /// The `ID`/`chat`/`sender` of the media message being retried.
        message_id: String,
        chat: String,
        sender: String,
        is_from_me: bool,
        timestamp: i64,
        /// Hex-encoded AES-GCM ciphertext of the notification (`<encrypt>`).
        ciphertext: Option<String>,
        /// Hex-encoded IV of the notification.
        iv: Option<String>,
        /// The numeric `error` attribute, when the server sent one.
        error_code: Option<i32>,
    },
    /// Go `events.GroupInfo` — a group metadata / participant change
    /// notification (`client/notification.rs`, port of
    /// `Go group.go:parseGroupChange`).
    #[specta(rename_all = "camelCase")]
    GroupInfo {
        info: GroupInfoEventData,
    },
    /// Go `events.JoinedGroup` — we were added to (or created) a group.
    #[specta(rename_all = "camelCase")]
    JoinedGroup {
        info: JoinedGroupData,
    },
    /// Go `events.Picture` — a contact/group picture was added, changed or
    /// removed (`client/notification.rs`, port of
    /// `Go notification.go:handlePictureNotification`).
    #[specta(rename_all = "camelCase")]
    Picture {
        jid: Jid,
        /// Who changed the picture; empty when the server omitted it.
        author: Jid,
        timestamp: Option<i64>,
        remove: bool,
        picture_id: String,
    },
    /// Go `events.Presence` — a user went online/offline. The payload is the
    /// one `client/presence.rs:parse_presence` produces.
    #[specta(rename_all = "camelCase")]
    Presence {
        presence: crate::client::presence::PresenceData,
    },
    /// Go `events.ChatPresence` — a user started/stopped typing.
    #[specta(rename_all = "camelCase")]
    ChatPresence {
        chat_presence: crate::client::presence::ChatPresenceData,
    },
    /// Go `events.PrivacySettings` — the user's privacy settings changed.
    #[specta(rename_all = "camelCase")]
    PrivacySettings {
        settings: crate::client::privacysettings::PrivacySettingsData,
    },
    /// Go `events.NewsletterLiveUpdate` — reaction/view counts for channel
    /// messages (`client/notification.rs`).
    #[specta(rename_all = "camelCase")]
    NewsletterLiveUpdate {
        id: Jid,
        time: Option<i64>,
        messages: Vec<NewsletterMessageData>,
    },
    /// Progress of an app-state sync run (`client/appstate.rs`).
    #[specta(rename_all = "camelCase")]
    AppStateSyncProgress {
        /// The patch name being synced (`regular`, `critical_unblock_low`, …).
        name: String,
        applied: u64,
        total: u64,
    },
}

/// Go `types.GroupParticipant` — one member of a group.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupParticipantData {
    /// The participant as the server listed it (LID or PN address).
    pub jid: Jid,
    /// The LID address of the participant (`lid` server), when known.
    pub lid: Jid,
    /// The phone-number address of the participant, when known.
    pub phone_number: Jid,
    /// The push name the server included (`display_name`), when any.
    pub display_name: String,
    pub is_admin: bool,
    pub is_super_admin: bool,
    /// The numeric `error` attribute of a participant update (`403`, …).
    pub error: i32,
    /// Present on an `add` that failed with an `add_request` (invite needed).
    pub add_request: Option<GroupParticipantAddRequestData>,
}

/// Go `types.GroupParticipantAddRequest` — the invite code a failed `add` needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupParticipantAddRequestData {
    pub code: String,
    pub expiration: Option<i64>,
}

/// Go `types.GroupInfo` — the full metadata of one group
/// (`Go group.go:parseGroupNode`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupInfoData {
    pub jid: Jid,
    pub owner_jid: Jid,
    /// The phone-number address of the owner, when the server sent one.
    pub owner_pn: Jid,
    pub name: String,
    pub name_set_at: Option<i64>,
    pub name_set_by: Jid,
    pub name_set_by_pn: Jid,
    pub topic: String,
    pub topic_id: String,
    pub topic_set_at: Option<i64>,
    pub topic_set_by: Jid,
    pub topic_set_by_pn: Jid,
    pub topic_deleted: bool,
    pub is_locked: bool,
    pub is_announce: bool,
    pub announce_version_id: String,
    pub is_ephemeral: bool,
    pub disappearing_timer: u32,
    pub is_incognito: bool,
    pub is_parent: bool,
    /// `request_required` for a community's announcement group.
    pub default_membership_approval_mode: String,
    pub linked_parent_jid: Jid,
    pub is_default_sub_group: bool,
    pub is_join_approval_required: bool,
    /// `pn` or `lid` — which address the group is keyed on.
    pub addressing_mode: String,
    pub group_created: Option<i64>,
    pub creator_country_code: String,
    pub participant_version_id: String,
    pub participants: Vec<GroupParticipantData>,
    pub participant_count: i32,
    /// `admin_add` or `all_member_add`.
    pub member_add_mode: String,
    /// The group is suspended by WhatsApp.
    pub suspended: bool,
}

/// Go `types.GroupName` with the `Option`al timestamp of this port.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupNameData {
    pub name: String,
    pub name_set_at: Option<i64>,
    pub name_set_by: Jid,
    pub name_set_by_pn: Jid,
}

/// Go `types.GroupTopic`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupTopicData {
    pub topic: String,
    pub topic_id: String,
    pub topic_set_at: Option<i64>,
    pub topic_set_by: Jid,
    pub topic_set_by_pn: Jid,
    pub topic_deleted: bool,
}

/// Go `types.GroupAnnounce`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupAnnounceData {
    pub is_announce: bool,
    pub announce_version_id: String,
}

/// Go `types.GroupEphemeral`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupEphemeralData {
    pub is_ephemeral: bool,
    pub disappearing_timer: u32,
}

/// Go `types.GroupLocked`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupLockedData {
    pub is_locked: bool,
}

/// Go `types.GroupDelete`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupDeleteData {
    pub deleted: bool,
    pub delete_reason: String,
}

/// Go `types.GroupLinkTarget`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupLinkTargetData {
    pub jid: Jid,
    pub name: String,
    pub name_set_at: Option<i64>,
    pub is_default_sub_group: bool,
}

/// Go `types.GroupLinkChange` — a sub-group link/unlink in a community.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupLinkChangeData {
    /// `link_type`/`unlink_type`: `sub` or `parent`.
    pub change_type: String,
    /// `unlink_reason`, empty for a link.
    pub unlink_reason: String,
    pub group: GroupLinkTargetData,
}

/// Go `events.GroupInfo` — one group change notification. Every change field
/// is `None` unless this notification carried that change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct GroupInfoEventData {
    /// The group the notification is about.
    pub jid: Jid,
    /// The `notify` attribute (`` when the server omitted it).
    pub notify: String,
    pub sender: Jid,
    pub sender_pn: Jid,
    pub timestamp: Option<i64>,
    /// `reason` of the `add` that made us join (when we joined).
    pub join_reason: String,
    pub name: Option<GroupNameData>,
    pub topic: Option<GroupTopicData>,
    pub locked: Option<GroupLockedData>,
    pub announce: Option<GroupAnnounceData>,
    pub ephemeral: Option<GroupEphemeralData>,
    pub delete: Option<GroupDeleteData>,
    pub link: Option<GroupLinkChangeData>,
    pub unlink: Option<GroupLinkChangeData>,
    /// Set when the server rotated the invite link.
    pub new_invite_link: Option<String>,
    pub prev_participant_version_id: String,
    pub participant_version_id: String,
    pub join: Vec<Jid>,
    pub leave: Vec<Jid>,
    pub promote: Vec<Jid>,
    pub demote: Vec<Jid>,
    /// Tags of changes this port does not parse (Go's `UnknownChanges`).
    pub unknown_changes: Vec<String>,
    pub membership_approval_mode: bool,
    pub suspended: bool,
    pub unsuspended: bool,
}

/// Go `events.JoinedGroup` — sent when we created a group or were added to one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct JoinedGroupData {
    /// Why we joined (`invite`, `add`, …).
    pub reason: String,
    /// The notification type (`create`, …).
    #[serde(rename = "type")]
    pub join_type: String,
    /// The `key` of a `create` notification; empty otherwise.
    pub create_key: String,
    /// Who invited us; empty when the server omitted it.
    pub sender: Jid,
    pub sender_pn: Jid,
    pub notify: String,
    /// The group as the server described it.
    pub info: GroupInfoData,
}

/// One message of a `newsletter` live-update notification
/// (`Go notification.go:parseNewsletterMessages`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct NewsletterMessageData {
    /// `server_id` — the channel-side message ID.
    pub server_id: i64,
    /// `id` — the message ID proper.
    pub message_id: String,
    /// The `type` attribute (`text`, `media`, `reaction`, …).
    pub message_type: String,
    pub timestamp: Option<i64>,
    /// Hex-encoded `waE2E::Message` from the `<plaintext>` child, when any.
    pub message: Option<String>,
    /// Go `types.NewsletterMessage.ViewsCount` — view count of the channel message.
    pub views_count: i32,
    /// Go `types.NewsletterMessage.ForwardsCount`.
    pub forwards_count: i32,
    /// Go `types.NewsletterMessage.ReactionCounts` — emoji code → count.
    pub reaction_counts: std::collections::HashMap<String, i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_and_camel_case_fields() {
        let e = WaEvent::KeepAliveTimeout { error_count: 2 };
        let json = serde_json_string(&e);
        assert_eq!(json, r#"{"type":"keep_alive_timeout","errorCount":2}"#);
        assert_eq!(serde_json_string(&WaEvent::Connected), r#"{"type":"connected"}"#);
    }

    fn serde_json_string(e: &WaEvent) -> String {
        serde_json::to_string(e).unwrap()
    }
}
