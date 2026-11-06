// Ported from the Go client `types/group.go` (`GroupName`) and `group.go`
// (`ParticipantChange`, `ParticipantRequestChange`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Port of the group types of `Go types/group.go` and the participant
//! change constants of `Go group.go`.
//!
//! The group server helpers (are we a group JID, group JID construction) stay in
//! [`crate::types::jid`] — this module only carries the data types.
//!
//! Divergence from Go: `time.Time` fields are represented as Unix seconds
//! (`i64`), like in [`crate::types::message`]. Go's zero `time.Time` maps to `0`.

use super::jid::Jid;
use serde::{Deserialize, Serialize};

/// Port of the Go struct `GroupName`: the name of a group plus who set it and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupName {
    pub name: String,
    /// `NameSetAt` — Unix seconds, `0` when unset.
    pub name_set_at: i64,
    /// `NameSetBy` — the JID of the user who set the name.
    pub name_set_by: Jid,
    /// `NameSetByPN` — the phone-number JID variant of `name_set_by`.
    pub name_set_by_pn: Jid,
}

/// Port of the Go type `ParticipantChange` (`Go group.go`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParticipantChange {
    /// `ParticipantChangeAdd = "add"`
    #[serde(rename = "add")]
    Add,
    /// `ParticipantChangeRemove = "remove"`
    #[serde(rename = "remove")]
    Remove,
    /// `ParticipantChangePromote = "promote"`
    #[serde(rename = "promote")]
    Promote,
    /// `ParticipantChangeDemote = "demote"`
    #[serde(rename = "demote")]
    Demote,
}

impl ParticipantChange {
    /// The wire value used by WhatsApp (identical to the Go constant value).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Promote => "promote",
            Self::Demote => "demote",
        }
    }
}

/// Port of the Go type `ParticipantRequestChange` (`Go group.go`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParticipantRequestChange {
    /// `ParticipantChangeApprove = "approve"`
    #[serde(rename = "approve")]
    Approve,
    /// `ParticipantChangeReject = "reject"`
    #[serde(rename = "reject")]
    Reject,
}

impl ParticipantRequestChange {
    /// The wire value used by WhatsApp (identical to the Go constant value).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `ParticipantChange` constant of `Go group.go`, as
    /// `(Go constant name, Go value)`. Update this list together with the enum.
    const GO_PARTICIPANT_CHANGES: &[(&str, &str)] = &[
        ("ParticipantChangeAdd", "add"),
        ("ParticipantChangeRemove", "remove"),
        ("ParticipantChangePromote", "promote"),
        ("ParticipantChangeDemote", "demote"),
    ];

    /// Every `ParticipantRequestChange` constant of `Go group.go`.
    const GO_PARTICIPANT_REQUEST_CHANGES: &[(&str, &str)] =
        &[("ParticipantChangeApprove", "approve"), ("ParticipantChangeReject", "reject")];

    const PARTICIPANT_CHANGES: [ParticipantChange; 4] =
        [ParticipantChange::Add, ParticipantChange::Remove, ParticipantChange::Promote, ParticipantChange::Demote];

    const PARTICIPANT_REQUEST_CHANGES: [ParticipantRequestChange; 2] =
        [ParticipantRequestChange::Approve, ParticipantRequestChange::Reject];

    #[test]
    fn participant_change_covers_every_go_constant() {
        assert_eq!(
            PARTICIPANT_CHANGES.len(),
            GO_PARTICIPANT_CHANGES.len(),
            "the Rust enum and the Go constants have drifted apart"
        );
        for (go_name, go_value) in GO_PARTICIPANT_CHANGES {
            let hit = PARTICIPANT_CHANGES
                .iter()
                .find(|v| v.as_str() == *go_value)
                .unwrap_or_else(|| panic!("missing Rust variant for {go_name} = {go_value:?}"));
            assert_eq!(serde_json::to_string(hit).unwrap(), format!("\"{go_value}\""));
        }
    }

    #[test]
    fn participant_request_change_covers_every_go_constant() {
        assert_eq!(
            PARTICIPANT_REQUEST_CHANGES.len(),
            GO_PARTICIPANT_REQUEST_CHANGES.len(),
            "the Rust enum and the Go constants have drifted apart"
        );
        for (go_name, go_value) in GO_PARTICIPANT_REQUEST_CHANGES {
            let hit = PARTICIPANT_REQUEST_CHANGES
                .iter()
                .find(|v| v.as_str() == *go_value)
                .unwrap_or_else(|| panic!("missing Rust variant for {go_name} = {go_value:?}"));
            assert_eq!(serde_json::to_string(hit).unwrap(), format!("\"{go_value}\""));
        }
    }

    #[test]
    fn participant_change_serde_roundtrip() {
        for value in PARTICIPANT_CHANGES {
            let json = serde_json::to_string(&value).unwrap();
            let back: ParticipantChange = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back);
        }
    }

    #[test]
    fn participant_request_change_serde_roundtrip() {
        for value in PARTICIPANT_REQUEST_CHANGES {
            let json = serde_json::to_string(&value).unwrap();
            let back: ParticipantRequestChange = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back);
        }
    }

    #[test]
    fn group_name_serde_roundtrip() {
        let value = GroupName {
            name: "Rust port".to_owned(),
            name_set_at: 1_700_000_000,
            name_set_by: Jid::new("491234567890", "s.whatsapp.net"),
            name_set_by_pn: Jid::new("1234567890", "lid"),
        };
        let json = serde_json::to_string(&value).unwrap();
        let back: GroupName = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
        // Field names follow the Go struct (camelCase on the wire).
        assert!(json.contains("\"nameSetAt\":1700000000"));
        assert!(json.contains("\"nameSetByPn\""));
    }
}
