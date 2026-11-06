// Ported from the Go client `types/presence.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Port of `Go types/presence.go` (presence and chat-presence states).
//!
//! Every variant serializes to exactly the string value of the matching Go
//! constant, so these types can go straight onto the wire.

use serde::{Deserialize, Serialize};

/// Port of the Go type `Presence` (`types.PresenceAvailable` / `PresenceUnavailable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PresenceState {
    /// `PresenceAvailable = "available"`
    #[serde(rename = "available")]
    Available,
    /// `PresenceUnavailable = "unavailable"`
    #[serde(rename = "unavailable")]
    Unavailable,
}

impl PresenceState {
    /// The wire value used by WhatsApp (identical to the Go constant value).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Port of the Go type `ChatPresence` (`ChatPresenceComposing` / `ChatPresencePaused`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChatPresenceState {
    /// `ChatPresenceComposing = "composing"`
    #[serde(rename = "composing")]
    Composing,
    /// `ChatPresencePaused = "paused"`
    #[serde(rename = "paused")]
    Paused,
}

impl ChatPresenceState {
    /// The wire value used by WhatsApp (identical to the Go constant value).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Composing => "composing",
            Self::Paused => "paused",
        }
    }
}

/// Port of the Go type `ChatPresenceMedia`
/// (`ChatPresenceMediaText` / `ChatPresenceMediaAudio`).
///
/// Note: `ChatPresenceMediaText` is the *empty string* in Go, not `"text"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChatPresenceMedia {
    /// `ChatPresenceMediaText = ""`
    #[serde(rename = "")]
    Text,
    /// `ChatPresenceMediaAudio = "audio"`
    #[serde(rename = "audio")]
    Audio,
}

impl ChatPresenceMedia {
    /// The wire value used by WhatsApp (identical to the Go constant value).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "",
            Self::Audio => "audio",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `Presence` constant of `Go types/presence.go`, as
    /// `(Go constant name, Go value)`. Update this list together with the enum.
    const GO_PRESENCE_STATES: &[(&str, &str)] =
        &[("PresenceAvailable", "available"), ("PresenceUnavailable", "unavailable")];

    /// Every `ChatPresence` constant of `Go types/presence.go`.
    const GO_CHAT_PRESENCE_STATES: &[(&str, &str)] =
        &[("ChatPresenceComposing", "composing"), ("ChatPresencePaused", "paused")];

    /// Every `ChatPresenceMedia` constant of `Go types/presence.go`.
    const GO_CHAT_PRESENCE_MEDIA: &[(&str, &str)] =
        &[("ChatPresenceMediaText", ""), ("ChatPresenceMediaAudio", "audio")];

    const PRESENCE_STATES: [PresenceState; 2] = [PresenceState::Available, PresenceState::Unavailable];

    const CHAT_PRESENCE_STATES: [ChatPresenceState; 2] = [ChatPresenceState::Composing, ChatPresenceState::Paused];

    const CHAT_PRESENCE_MEDIA: [ChatPresenceMedia; 2] = [ChatPresenceMedia::Text, ChatPresenceMedia::Audio];

    #[test]
    fn presence_state_covers_every_go_constant() {
        assert_eq!(
            PRESENCE_STATES.len(),
            GO_PRESENCE_STATES.len(),
            "the Rust enum and the Go constants have drifted apart"
        );
        for (go_name, go_value) in GO_PRESENCE_STATES {
            let hit = PRESENCE_STATES
                .iter()
                .find(|v| v.as_str() == *go_value)
                .unwrap_or_else(|| panic!("missing Rust variant for {go_name} = {go_value:?}"));
            assert_eq!(serde_json::to_string(hit).unwrap(), format!("\"{go_value}\""));
        }
    }

    #[test]
    fn chat_presence_state_covers_every_go_constant() {
        assert_eq!(
            CHAT_PRESENCE_STATES.len(),
            GO_CHAT_PRESENCE_STATES.len(),
            "the Rust enum and the Go constants have drifted apart"
        );
        for (go_name, go_value) in GO_CHAT_PRESENCE_STATES {
            let hit = CHAT_PRESENCE_STATES
                .iter()
                .find(|v| v.as_str() == *go_value)
                .unwrap_or_else(|| panic!("missing Rust variant for {go_name} = {go_value:?}"));
            assert_eq!(serde_json::to_string(hit).unwrap(), format!("\"{go_value}\""));
        }
    }

    #[test]
    fn chat_presence_media_covers_every_go_constant() {
        assert_eq!(
            CHAT_PRESENCE_MEDIA.len(),
            GO_CHAT_PRESENCE_MEDIA.len(),
            "the Rust enum and the Go constants have drifted apart"
        );
        for (go_name, go_value) in GO_CHAT_PRESENCE_MEDIA {
            let hit = CHAT_PRESENCE_MEDIA
                .iter()
                .find(|v| v.as_str() == *go_value)
                .unwrap_or_else(|| panic!("missing Rust variant for {go_name} = {go_value:?}"));
            assert_eq!(serde_json::to_string(hit).unwrap(), format!("\"{go_value}\""));
        }
    }

    #[test]
    fn serde_roundtrip() {
        for value in PRESENCE_STATES {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<PresenceState>(&json).unwrap(), value);
        }
        for value in CHAT_PRESENCE_STATES {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<ChatPresenceState>(&json).unwrap(), value);
        }
        for value in CHAT_PRESENCE_MEDIA {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<ChatPresenceMedia>(&json).unwrap(), value);
        }
    }

    #[test]
    fn text_media_is_the_empty_string() {
        assert_eq!(serde_json::to_string(&ChatPresenceMedia::Text).unwrap(), "\"\"");
    }
}
