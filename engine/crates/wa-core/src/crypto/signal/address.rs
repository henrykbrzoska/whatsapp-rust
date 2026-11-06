// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Signal addresses (`name:device`) and sender-key names (`group` + sender).
//!
//! The string forms are database keys: `wa_sessions.their_id`,
//! `wa_identity_keys.their_id` and `wa_sender_keys.sender_id` all use
//! `SignalAddress::to_string()` (Go: `JID.SignalAddress().String()`).

use std::fmt;

use crate::types::jid::Jid;

/// A Signal protocol address — one device of one user.
///
/// Go: `types.JID.SignalAddress()` (`name` = `JID.SignalAddressUser()`,
/// i.e. `user` or `user_agent`; `device_id` = `JID.Device`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SignalAddress {
    pub name: String,
    pub device_id: u32,
}

impl SignalAddress {
    /// Go: `protocol.NewSignalAddress`.
    pub fn new(name: impl Into<String>, device_id: u32) -> Self {
        Self { name: name.into(), device_id }
    }

    /// Go: `JID.SignalAddress()`.
    pub fn from_jid(jid: &Jid) -> Self {
        Self::new(jid.signal_address_user(), u32::from(jid.device))
    }

    /// The same user's primary device (device `0`).
    pub fn primary(&self) -> Self {
        Self::new(self.name.clone(), 0)
    }

    /// `true` for device `0` (the phone / primary device).
    pub fn is_primary_device(&self) -> bool {
        self.device_id == 0
    }
}

impl fmt::Display for SignalAddress {
    /// Go: `SignalAddress.String()` — `name:device`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.device_id)
    }
}

impl From<&Jid> for SignalAddress {
    fn from(jid: &Jid) -> Self {
        Self::from_jid(jid)
    }
}

/// Identifies one sender's sender-key chain inside one group.
///
/// Go: `protocol.SenderKeyName` (`groupID` = chat JID string, `sender` = the
/// sender's [`SignalAddress`]). Stored as
/// `wa_sender_keys (chat_id = group_id, sender_id = sender.to_string())`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SenderKeyName {
    pub group_id: String,
    pub sender: SignalAddress,
}

impl SenderKeyName {
    /// Go: `protocol.NewSenderKeyName`.
    pub fn new(group_id: impl Into<String>, sender: SignalAddress) -> Self {
        Self { group_id: group_id.into(), sender }
    }

    /// The `sender_id` column value.
    pub fn sender_id(&self) -> String {
        self.sender.to_string()
    }
}

impl fmt::Display for SenderKeyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.group_id, self.sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_go_signal_address() {
        let addr = SignalAddress::new("12345", 3);
        assert_eq!(addr.to_string(), "12345:3");
        assert!(!addr.is_primary_device());
        assert_eq!(addr.primary(), SignalAddress::new("12345", 0));
        assert!(addr.primary().is_primary_device());
    }

    #[test]
    fn from_jid_uses_signal_address_user_and_device() {
        let jid: Jid = "12345:7@s.whatsapp.net".parse().unwrap();
        assert_eq!(SignalAddress::from(&jid).to_string(), "12345:7");
        let plain: Jid = "999@s.whatsapp.net".parse().unwrap();
        assert_eq!(SignalAddress::from_jid(&plain).to_string(), "999:0");
    }

    #[test]
    fn sender_key_name_keys() {
        let name = SenderKeyName::new("120363@g.us", SignalAddress::new("111", 2));
        assert_eq!(name.sender_id(), "111:2");
        assert_eq!(name.to_string(), "120363@g.us::111:2");
    }
}
