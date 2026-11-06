// Ported from the Go client `privacysettings.go` and the privacy types of
// `types/user.go`, © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The user's privacy settings (`xmlns="privacy"` IQs and the
//! `<privacy>` change notification).
//!
//! Go counterparts: `Go privacysettings.go` (`TryFetchPrivacySettings`,
//! `GetPrivacySettings`, `SetPrivacySetting`, `SetDefaultDisappearingTimer`,
//! `parsePrivacySettings`, `handlePrivacySettingsNotification`) and the
//! `PrivacySetting*` types of `Go types/user.go`.
//!
//! The Rust client has no `privacySettingsCache` field yet, so every call
//! fetches from the server (Go's `ignoreCache = true` path).
//! `handle_privacy_settings_notification` returns the event payload; the
//! caller dispatches `WaEvent::PrivacySettings`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::binary::node::Node;
use crate::types::jid::Jid;

use super::ack::attr_text;
use super::{Client, ClientError, InfoQuery, IqKind};

/// Go `types.PrivacySettingType` — which setting a `<category>` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub enum PrivacySettingType {
    #[serde(rename = "groupadd")]
    GroupAdd,
    #[serde(rename = "last")]
    LastSeen,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "profile")]
    Profile,
    #[serde(rename = "readreceipts")]
    ReadReceipts,
    #[serde(rename = "online")]
    Online,
    #[serde(rename = "calladd")]
    CallAdd,
    #[serde(rename = "messages")]
    Messages,
    #[serde(rename = "defense")]
    Defense,
    #[serde(rename = "stickers")]
    Stickers,
}

impl PrivacySettingType {
    /// Every Go constant, in declaration order.
    pub const ALL: [Self; 10] = [
        Self::GroupAdd,
        Self::LastSeen,
        Self::Status,
        Self::Profile,
        Self::ReadReceipts,
        Self::Online,
        Self::CallAdd,
        Self::Messages,
        Self::Defense,
        Self::Stickers,
    ];

    /// The wire value (identical to the Go constant value).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::GroupAdd => "groupadd",
            Self::LastSeen => "last",
            Self::Status => "status",
            Self::Profile => "profile",
            Self::ReadReceipts => "readreceipts",
            Self::Online => "online",
            Self::CallAdd => "calladd",
            Self::Messages => "messages",
            Self::Defense => "defense",
            Self::Stickers => "stickers",
        }
    }

    /// Parse a wire value; `None` for names this port does not know.
    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == value)
    }
}

/// Go `types.PrivacySetting` — the value of one setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub enum PrivacySetting {
    /// `PrivacySettingUndefined = ""` (unset or unknown value).
    #[default]
    #[serde(rename = "")]
    Undefined,
    #[serde(rename = "all")]
    All,
    #[serde(rename = "contacts")]
    Contacts,
    #[serde(rename = "contact_allowlist")]
    ContactAllowlist,
    #[serde(rename = "contact_blacklist")]
    ContactBlacklist,
    #[serde(rename = "match_last_seen")]
    MatchLastSeen,
    #[serde(rename = "known")]
    Known,
    #[serde(rename = "none")]
    None,
    #[serde(rename = "on_standard")]
    OnStandard,
    #[serde(rename = "off")]
    Off,
}

impl PrivacySetting {
    /// Every Go constant, in declaration order.
    pub const ALL: [Self; 10] = [
        Self::Undefined,
        Self::All,
        Self::Contacts,
        Self::ContactAllowlist,
        Self::ContactBlacklist,
        Self::MatchLastSeen,
        Self::Known,
        Self::None,
        Self::OnStandard,
        Self::Off,
    ];

    /// The wire value (identical to the Go constant value).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Undefined => "",
            Self::All => "all",
            Self::Contacts => "contacts",
            Self::ContactAllowlist => "contact_allowlist",
            Self::ContactBlacklist => "contact_blacklist",
            Self::MatchLastSeen => "match_last_seen",
            Self::Known => "known",
            Self::None => "none",
            Self::OnStandard => "on_standard",
            Self::Off => "off",
        }
    }

    /// Parse a wire value; unknown values become [`Self::Undefined`] (logged).
    pub fn from_wire(value: &str) -> Self {
        Self::ALL.into_iter().find(|v| v.as_str() == value).unwrap_or_else(|| {
            tracing::debug!("unknown privacy setting value '{value}'");
            Self::Undefined
        })
    }
}

/// Go `types.PrivacySettings`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
#[specta(rename_all = "camelCase")]
pub struct PrivacySettings {
    pub group_add: PrivacySetting,
    pub last_seen: PrivacySetting,
    pub status: PrivacySetting,
    pub profile: PrivacySetting,
    pub read_receipts: PrivacySetting,
    pub call_add: PrivacySetting,
    pub online: PrivacySetting,
    pub messages: PrivacySetting,
    pub defense: PrivacySetting,
    pub stickers: PrivacySetting,
}

impl PrivacySettings {
    /// Store `value` in the field `name` refers to.
    pub fn set(&mut self, name: PrivacySettingType, value: PrivacySetting) {
        match name {
            PrivacySettingType::GroupAdd => self.group_add = value,
            PrivacySettingType::LastSeen => self.last_seen = value,
            PrivacySettingType::Status => self.status = value,
            PrivacySettingType::Profile => self.profile = value,
            PrivacySettingType::ReadReceipts => self.read_receipts = value,
            PrivacySettingType::Online => self.online = value,
            PrivacySettingType::CallAdd => self.call_add = value,
            PrivacySettingType::Messages => self.messages = value,
            PrivacySettingType::Defense => self.defense = value,
            PrivacySettingType::Stickers => self.stickers = value,
        }
    }

    /// Read the field `name` refers to.
    pub fn get(&self, name: PrivacySettingType) -> PrivacySetting {
        match name {
            PrivacySettingType::GroupAdd => self.group_add,
            PrivacySettingType::LastSeen => self.last_seen,
            PrivacySettingType::Status => self.status,
            PrivacySettingType::Profile => self.profile,
            PrivacySettingType::ReadReceipts => self.read_receipts,
            PrivacySettingType::Online => self.online,
            PrivacySettingType::CallAdd => self.call_add,
            PrivacySettingType::Messages => self.messages,
            PrivacySettingType::Defense => self.defense,
            PrivacySettingType::Stickers => self.stickers,
        }
    }
}

/// Payload of the Go `events.PrivacySettings` event: the full new settings
/// plus which of them the notification changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[specta(rename_all = "camelCase")]
pub struct PrivacySettingsData {
    pub new_settings: PrivacySettings,
    /// The settings named in the notification (Go's `*Changed` booleans).
    pub changed: Vec<PrivacySettingType>,
}

impl PrivacySettingsData {
    /// Go `evt.<Name>Changed`.
    pub fn is_changed(&self, name: PrivacySettingType) -> bool {
        self.changed.contains(&name)
    }
}

/// Go `parsePrivacySettings` — apply every `<category name value>` child of
/// `privacy_node` to `settings` and report which ones changed.
pub fn parse_privacy_settings(privacy_node: &Node, settings: &mut PrivacySettings) -> PrivacySettingsData {
    let mut changed = Vec::new();
    for child in privacy_node.children_by_tag("category") {
        let name = attr_text(child, "name").unwrap_or_default();
        let Some(name) = PrivacySettingType::from_wire(&name) else {
            tracing::debug!("ignoring unknown privacy setting category '{name}'");
            continue;
        };
        let value = PrivacySetting::from_wire(&attr_text(child, "value").unwrap_or_default());
        settings.set(name, value);
        if !changed.contains(&name) {
            changed.push(name);
        }
    }
    PrivacySettingsData { new_settings: *settings, changed }
}

/// Go `TryFetchPrivacySettings`'s query.
pub fn build_get_privacy_settings_query() -> InfoQuery {
    InfoQuery {
        namespace: "privacy".into(),
        kind: IqKind::Get,
        to: Some(Jid::server_jid()),
        content: vec![Node::new("privacy")],
        ..Default::default()
    }
}

/// Go `SetPrivacySetting`'s query.
pub fn build_set_privacy_setting_query(name: PrivacySettingType, value: PrivacySetting) -> InfoQuery {
    InfoQuery {
        namespace: "privacy".into(),
        kind: IqKind::Set,
        to: Some(Jid::server_jid()),
        content: vec![
            Node::new("privacy")
                .with_content(vec![Node::new("category").attr("name", name.as_str()).attr("value", value.as_str())]),
        ],
        ..Default::default()
    }
}

/// Go `SetDefaultDisappearingTimer`'s query.
pub fn build_default_disappearing_timer_query(timer: Duration) -> InfoQuery {
    InfoQuery {
        namespace: "disappearing_mode".into(),
        kind: IqKind::Set,
        to: Some(Jid::server_jid()),
        content: vec![Node::new("disappearing_mode").attr("duration", timer.as_secs().to_string())],
        ..Default::default()
    }
}

/// Parse the `<privacy>` child of a `TryFetchPrivacySettings` response.
pub fn parse_privacy_settings_response(response: &Node) -> Result<PrivacySettings, ClientError> {
    let privacy = response
        .optional_child_by_tag(&["privacy"])
        .ok_or(ClientError::MissingElement { tag: "privacy", parent: "response to privacy settings query" })?;
    let mut settings = PrivacySettings::default();
    parse_privacy_settings(privacy, &mut settings);
    Ok(settings)
}

impl Client {
    /// Go `TryFetchPrivacySettings(ctx, true)` — fetch from the server.
    // NOTE: Go's `privacySettingsCache` needs a `Client` field; every call fetches from the server.
    pub async fn try_fetch_privacy_settings(&self) -> Result<PrivacySettings, ClientError> {
        let response = self.send_iq(build_get_privacy_settings_query()).await?;
        parse_privacy_settings_response(&response)
    }

    /// Go `GetPrivacySettings` — like [`Self::try_fetch_privacy_settings`],
    /// but logs failures and returns the empty settings instead.
    pub async fn get_privacy_settings(&self) -> PrivacySettings {
        match self.try_fetch_privacy_settings().await {
            Ok(settings) => settings,
            Err(err) => {
                tracing::error!("failed to fetch privacy settings: {err}");
                PrivacySettings::default()
            }
        }
    }

    /// Go `SetPrivacySetting` — change one setting and return the settings
    /// as they are after the change.
    pub async fn set_privacy_setting(
        &self,
        name: PrivacySettingType,
        value: PrivacySetting,
    ) -> Result<PrivacySettings, ClientError> {
        let mut settings = self.try_fetch_privacy_settings().await?;
        self.send_iq(build_set_privacy_setting_query(name, value)).await?;
        settings.set(name, value);
        Ok(settings)
    }

    /// Go `SetDefaultDisappearingTimer` — whole seconds, `0` turns it off.
    pub async fn set_default_disappearing_timer(&self, timer: Duration) -> Result<(), ClientError> {
        self.send_iq(build_default_disappearing_timer_query(timer)).await.map(|_| ())
    }

    /// Go `handlePrivacySettingsNotification` — refetch the current settings,
    /// apply the notification's `<category>` children and return the event
    /// payload (`None`, logged, when the refetch fails). The caller
    /// dispatches `WaEvent::PrivacySettings`.
    pub async fn handle_privacy_settings_notification(&self, privacy_node: &Node) -> Option<PrivacySettingsData> {
        tracing::debug!("parsing privacy settings change notification");
        let mut settings = match self.try_fetch_privacy_settings().await {
            Ok(settings) => settings,
            Err(err) => {
                tracing::error!("failed to fetch privacy settings when handling change: {err}");
                return None;
            }
        };
        Some(parse_privacy_settings(privacy_node, &mut settings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::node::AttrValue;

    /// Every `PrivacySettingType` constant of `Go types/user.go`.
    const GO_TYPES: &[&str] = &[
        "groupadd",
        "last",
        "status",
        "profile",
        "readreceipts",
        "online",
        "calladd",
        "messages",
        "defense",
        "stickers",
    ];

    /// Every `PrivacySetting` constant of `Go types/user.go`.
    const GO_VALUES: &[&str] = &[
        "",
        "all",
        "contacts",
        "contact_allowlist",
        "contact_blacklist",
        "match_last_seen",
        "known",
        "none",
        "on_standard",
        "off",
    ];

    fn category(name: &str, value: &str) -> Node {
        Node::new("category").attr("name", name.to_owned()).attr("value", value.to_owned())
    }

    #[test]
    fn privacysetting_enums_cover_every_go_constant() {
        assert_eq!(PrivacySettingType::ALL.len(), GO_TYPES.len());
        for go in GO_TYPES {
            let t = PrivacySettingType::from_wire(go).unwrap();
            assert_eq!(serde_json::to_string(&t).unwrap(), format!("\"{go}\""));
        }
        assert_eq!(PrivacySetting::ALL.len(), GO_VALUES.len());
        for go in GO_VALUES {
            let v = PrivacySetting::from_wire(go);
            assert_eq!(v.as_str(), *go);
            assert_eq!(serde_json::to_string(&v).unwrap(), format!("\"{go}\""));
        }
        assert_eq!(PrivacySetting::from_wire("bogus"), PrivacySetting::Undefined);
    }

    #[test]
    fn privacysettings_get_iq_shape() {
        let query = build_get_privacy_settings_query();
        let node = query.to_node("abc");
        assert_eq!(node.tag, "iq");
        assert_eq!(node.attrs.get("xmlns").and_then(AttrValue::as_str), Some("privacy"));
        assert_eq!(node.attrs.get("type").and_then(AttrValue::as_str), Some("get"));
        assert_eq!(node.attrs.get("to").and_then(AttrValue::as_jid), Some(&Jid::server_jid()));
        let children = node.children();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].tag, "privacy");
        assert!(children[0].content.is_empty());
    }

    #[test]
    fn privacysettings_set_iq_shape() {
        let node = build_set_privacy_setting_query(PrivacySettingType::LastSeen, PrivacySetting::Contacts).to_node("x");
        assert_eq!(node.attrs.get("xmlns").and_then(AttrValue::as_str), Some("privacy"));
        assert_eq!(node.attrs.get("type").and_then(AttrValue::as_str), Some("set"));
        let privacy = &node.children()[0];
        assert_eq!(privacy.tag, "privacy");
        let cat = &privacy.children()[0];
        assert_eq!(cat.tag, "category");
        assert_eq!(cat.attrs.get("name").and_then(AttrValue::as_str), Some("last"));
        assert_eq!(cat.attrs.get("value").and_then(AttrValue::as_str), Some("contacts"));
    }

    #[test]
    fn privacysettings_disappearing_timer_iq_shape() {
        let node = build_default_disappearing_timer_query(Duration::from_secs(86_400)).to_node("x");
        assert_eq!(node.attrs.get("xmlns").and_then(AttrValue::as_str), Some("disappearing_mode"));
        let child = &node.children()[0];
        assert_eq!(child.tag, "disappearing_mode");
        assert_eq!(child.attrs.get("duration").and_then(AttrValue::as_str), Some("86400"));
    }

    #[test]
    fn privacysettings_response_parsing() {
        let response =
            Node::new("iq").attr("type", "result").with_content(vec![Node::new("privacy").with_content(vec![
                category("groupadd", "contacts"),
                category("last", "none"),
                category("online", "match_last_seen"),
                category("readreceipts", "all"),
                category("defense", "on_standard"),
                category("unknownsetting", "all"),
                Node::new("other").attr("name", "status").attr("value", "all"),
            ])]);
        let settings = parse_privacy_settings_response(&response).unwrap();
        assert_eq!(settings.group_add, PrivacySetting::Contacts);
        assert_eq!(settings.last_seen, PrivacySetting::None);
        assert_eq!(settings.online, PrivacySetting::MatchLastSeen);
        assert_eq!(settings.read_receipts, PrivacySetting::All);
        assert_eq!(settings.defense, PrivacySetting::OnStandard);
        // Non-`category` children are ignored.
        assert_eq!(settings.status, PrivacySetting::Undefined);

        let missing = Node::new("iq").attr("type", "result");
        assert!(matches!(
            parse_privacy_settings_response(&missing),
            Err(ClientError::MissingElement { tag: "privacy", .. })
        ));
    }

    #[test]
    fn privacysettings_event_mapping() {
        let mut current =
            PrivacySettings { last_seen: PrivacySetting::All, profile: PrivacySetting::All, ..Default::default() };
        let notification = Node::new("privacy")
            .with_content(vec![category("last", "contacts"), category("stickers", "contact_allowlist")]);
        let evt = parse_privacy_settings(&notification, &mut current);
        assert_eq!(evt.changed, vec![PrivacySettingType::LastSeen, PrivacySettingType::Stickers]);
        assert!(evt.is_changed(PrivacySettingType::LastSeen));
        assert!(!evt.is_changed(PrivacySettingType::Profile));
        assert_eq!(evt.new_settings.last_seen, PrivacySetting::Contacts);
        assert_eq!(evt.new_settings.stickers, PrivacySetting::ContactAllowlist);
        // Untouched settings keep the fetched value.
        assert_eq!(evt.new_settings.profile, PrivacySetting::All);
        assert_eq!(evt.new_settings, current);

        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["newSettings"]["lastSeen"], "contacts");
        assert_eq!(json["changed"], serde_json::json!(["last", "stickers"]));
    }

    #[test]
    fn privacysettings_set_and_get_roundtrip() {
        let mut settings = PrivacySettings::default();
        for (i, name) in PrivacySettingType::ALL.into_iter().enumerate() {
            let value = PrivacySetting::ALL[(i + 1) % PrivacySetting::ALL.len()];
            settings.set(name, value);
            assert_eq!(settings.get(name), value);
        }
    }
}
