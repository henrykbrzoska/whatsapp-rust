//! Port of `Go types/jid.go`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const DEFAULT_USER_SERVER: &str = "s.whatsapp.net";
pub const GROUP_SERVER: &str = "g.us";
pub const LEGACY_USER_SERVER: &str = "c.us";
pub const BROADCAST_SERVER: &str = "broadcast";
pub const HIDDEN_USER_SERVER: &str = "lid";
pub const MESSENGER_SERVER: &str = "msgr";
pub const INTEROP_SERVER: &str = "interop";
pub const NEWSLETTER_SERVER: &str = "newsletter";
pub const HOSTED_SERVER: &str = "hosted";
pub const HOSTED_LID_SERVER: &str = "hosted.lid";
pub const BOT_SERVER: &str = "bot";

pub const WHATSAPP_DOMAIN: u8 = 0;
pub const LID_DOMAIN: u8 = 1;
pub const HOSTED_DOMAIN: u8 = 128;
pub const HOSTED_LID_DOMAIN: u8 = 129;

/// A WhatsApp ID: either a regular `user@server` pair or an AD-JID
/// (`user.agent:device@server`) addressing a specific device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Jid {
    pub user: String,
    pub raw_agent: u8,
    pub device: u16,
    pub integrator: u16,
    pub server: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JidError {
    #[error("unexpected number of dots in JID")]
    Dots,
    #[error("unexpected number of colons in JID")]
    Colons,
    #[error("failed to parse device from JID: {0}")]
    Device(String),
}

impl Jid {
    pub fn new(user: impl Into<String>, server: impl Into<String>) -> Self {
        Self { user: user.into(), server: server.into(), ..Default::default() }
    }

    /// `NewADJID` — agent values that are really domain types pick the server.
    pub fn new_ad(user: impl Into<String>, agent: u8, device: u8) -> Self {
        let (server, agent) = match agent {
            LID_DOMAIN => (HIDDEN_USER_SERVER, 0),
            HOSTED_DOMAIN => (HOSTED_SERVER, 0),
            HOSTED_LID_DOMAIN => (HOSTED_LID_SERVER, 0),
            a => (DEFAULT_USER_SERVER, a),
        };
        Self { user: user.into(), raw_agent: agent, device: device as u16, server: server.into(), ..Default::default() }
    }

    pub fn parse(s: &str) -> Result<Self, JidError> {
        let mut parts = s.split('@');
        let first = parts.next().unwrap_or_default();
        let Some(server) = parts.next() else {
            return Ok(Self::new("", first));
        };
        let mut jid = Self::new(first, server);
        if jid.user.contains('.') {
            let p: Vec<&str> = first.split('.').collect();
            if p.len() != 2 {
                return Err(JidError::Dots);
            }
            jid.user = p[0].to_string();
            let ad: Vec<&str> = p[1].split(':').collect();
            if ad.len() > 2 {
                return Err(JidError::Colons);
            }
            jid.raw_agent = ad[0].parse().map_err(|e: std::num::ParseIntError| JidError::Device(e.to_string()))?;
            if ad.len() == 2 {
                jid.device = ad[1].parse().map_err(|e: std::num::ParseIntError| JidError::Device(e.to_string()))?;
            }
        } else if jid.user.contains(':') {
            let p: Vec<&str> = first.split(':').collect();
            if p.len() != 2 {
                return Err(JidError::Colons);
            }
            jid.user = p[0].to_string();
            jid.device = p[1].parse().map_err(|e: std::num::ParseIntError| JidError::Device(e.to_string()))?;
        }
        Ok(jid)
    }

    pub fn actual_agent(&self) -> u8 {
        match self.server.as_str() {
            DEFAULT_USER_SERVER => WHATSAPP_DOMAIN,
            HIDDEN_USER_SERVER => LID_DOMAIN,
            HOSTED_SERVER => HOSTED_DOMAIN,
            HOSTED_LID_SERVER => HOSTED_LID_DOMAIN,
            _ => self.raw_agent,
        }
    }

    /// `JID.WithServer` — returns a copy pointing at another server.
    pub fn with_server(&self, server: impl Into<String>) -> Self {
        Self { server: server.into(), ..self.clone() }
    }

    /// `JID.WithUser` — returns a copy with another user part.
    pub fn with_user(&self, user: impl Into<String>) -> Self {
        Self { user: user.into(), ..self.clone() }
    }

    /// Returns a copy addressing another device of the same user.
    pub fn with_device(&self, device: u16) -> Self {
        Self { device, ..self.clone() }
    }

    /// `JID.UserInt` — the user as a number (only meaningful for user JIDs).
    pub fn user_int(&self) -> u64 {
        self.user.parse().unwrap_or(0)
    }

    pub fn to_non_ad(&self) -> Self {
        Self { user: self.user.clone(), server: self.server.clone(), integrator: self.integrator, ..Default::default() }
    }

    pub fn signal_address_user(&self) -> String {
        match self.actual_agent() {
            0 => self.user.clone(),
            a => format!("{}_{}", self.user, a),
        }
    }

    /// Go `JID.SignalAddress` — the `<user>.<device>` address a Signal session
    /// is keyed by (`signalProtocol.SignalAddress.String()`).
    pub fn signal_address(&self) -> String {
        format!("{}.{}", self.signal_address_user(), self.device)
    }

    pub fn ad_string(&self) -> String {
        format!("{}.{}:{}@{}", self.user, self.raw_agent, self.device, self.server)
    }

    pub fn is_empty(&self) -> bool {
        self.server.is_empty()
    }

    /// Go `types.ServerJID` — the bare `s.whatsapp.net` server JID.
    pub fn server_jid() -> Self {
        Self::new("", DEFAULT_USER_SERVER)
    }

    pub fn is_group(&self) -> bool {
        self.server == GROUP_SERVER
    }

    pub fn is_broadcast_list(&self) -> bool {
        self.server == BROADCAST_SERVER && self.user != "status"
    }

    pub fn is_newsletter(&self) -> bool {
        self.server == NEWSLETTER_SERVER
    }

    pub fn is_lid(&self) -> bool {
        matches!(self.server.as_str(), HIDDEN_USER_SERVER | HOSTED_LID_SERVER)
    }

    /// `JID.IsBot` — the special bot users plus the `bot` server.
    pub fn is_bot(&self) -> bool {
        (self.server == DEFAULT_USER_SERVER && self.device == 0 && is_bot_user(&self.user)) || self.server == BOT_SERVER
    }

    pub fn is_zero(&self) -> bool {
        self.user.is_empty() && self.server.is_empty()
    }
}

/// `botUserRegex` — the fixed bot numbers (`^1313555\d{4}$|^131655500\d{2}$`).
fn is_bot_user(user: &str) -> bool {
    user.len() == 11
        && user.bytes().all(|b| b.is_ascii_digit())
        && (user.starts_with("1313555") || user.starts_with("131655500"))
}

impl fmt::Display for Jid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.server.is_empty() {
            Ok(())
        } else if self.raw_agent > 0 {
            write!(f, "{}.{}:{}@{}", self.user, self.raw_agent, self.device, self.server)
        } else if self.device > 0 {
            write!(f, "{}:{}@{}", self.user, self.device, self.server)
        } else if !self.user.is_empty() {
            write!(f, "{}@{}", self.user, self.server)
        } else {
            f.write_str(&self.server)
        }
    }
}

impl FromStr for Jid {
    type Err = JidError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Jid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Jid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

// On the wire a JID is a plain string (`API-WHATSAPP.md` §2), so the exported
// TypeScript type is `string` rather than an object of its parts.
impl specta::Type for Jid {
    fn inline(opts: specta::DefOpts, generics: &[specta::DataType]) -> Result<specta::DataType, specta::ExportError> {
        <String as specta::Type>::inline(opts, generics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_regular() {
        let j = Jid::parse("48123456789@s.whatsapp.net").unwrap();
        assert_eq!(j.user, "48123456789");
        assert_eq!(j.server, DEFAULT_USER_SERVER);
        assert_eq!(j.device, 0);
        assert_eq!(j.to_string(), "48123456789@s.whatsapp.net");
    }

    #[test]
    fn parse_device() {
        let j = Jid::parse("123:7@s.whatsapp.net").unwrap();
        assert_eq!((j.user.as_str(), j.device), ("123", 7));
        assert_eq!(j.to_string(), "123:7@s.whatsapp.net");
    }

    #[test]
    fn parse_ad_jid() {
        let j = Jid::parse("123.2:5@s.whatsapp.net").unwrap();
        assert_eq!((j.raw_agent, j.device), (2, 5));
        assert_eq!(j.to_string(), "123.2:5@s.whatsapp.net");
        assert_eq!(j.ad_string(), "123.2:5@s.whatsapp.net");
    }

    #[test]
    fn parse_server_only() {
        let j = Jid::parse("g.us").unwrap();
        assert!(j.user.is_empty());
        assert_eq!(j.to_string(), "g.us");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Jid::parse("a.b.c@s.whatsapp.net"), Err(JidError::Dots));
        assert_eq!(Jid::parse("a:1:2@s.whatsapp.net"), Err(JidError::Colons));
        assert!(matches!(Jid::parse("a:x@s.whatsapp.net"), Err(JidError::Device(_))));
    }

    #[test]
    fn lid_and_signal_address() {
        let j = Jid::new_ad("999", LID_DOMAIN, 3);
        assert_eq!(j.server, HIDDEN_USER_SERVER);
        assert_eq!(j.to_string(), "999:3@lid");
        assert_eq!(j.signal_address_user(), "999_1");
        assert_eq!(j.signal_address(), "999_1.3");
        assert_eq!(Jid::new("1", DEFAULT_USER_SERVER).signal_address_user(), "1");
        assert_eq!(Jid::new("1", DEFAULT_USER_SERVER).signal_address(), "1.0");
        assert_eq!(Jid::parse("123:7@s.whatsapp.net").unwrap().signal_address(), "123.7");
    }

    #[test]
    fn non_ad_and_kinds() {
        let j = Jid::parse("123.2:5@s.whatsapp.net").unwrap().to_non_ad();
        assert_eq!(j.to_string(), "123@s.whatsapp.net");
        assert!(Jid::new("1", GROUP_SERVER).is_group());
        assert!(Jid::new("x", BROADCAST_SERVER).is_broadcast_list());
        assert!(!Jid::new("status", BROADCAST_SERVER).is_broadcast_list());
        assert!(Jid::default().is_empty());
    }

    #[test]
    fn with_server_and_user_keep_the_rest() {
        let j = Jid::parse("123.2:5@s.whatsapp.net").unwrap();
        let lid = j.with_server(HIDDEN_USER_SERVER);
        assert_eq!(lid.to_string(), "123.2:5@lid");
        assert_eq!(lid.raw_agent, 2);
        assert_eq!(lid.device, 5);
        // The original is untouched.
        assert_eq!(j.server, DEFAULT_USER_SERVER);

        let renamed = j.with_user("999");
        assert_eq!(renamed.to_string(), "999.2:5@s.whatsapp.net");
        assert_eq!(renamed.device, 5);
    }

    #[test]
    fn user_int_and_zero() {
        assert_eq!(Jid::new("48123456789", DEFAULT_USER_SERVER).user_int(), 48_123_456_789);
        assert_eq!(Jid::new("group", GROUP_SERVER).user_int(), 0);
        assert!(Jid::default().is_zero());
        assert!(!Jid::new("1", DEFAULT_USER_SERVER).is_zero());
    }

    #[test]
    fn lid_and_newsletter_kinds() {
        assert!(Jid::new("1", HIDDEN_USER_SERVER).is_lid());
        assert!(Jid::new("1", HOSTED_LID_SERVER).is_lid());
        assert!(!Jid::new("1", DEFAULT_USER_SERVER).is_lid());
        assert!(Jid::new("1", NEWSLETTER_SERVER).is_newsletter());
        assert!(!Jid::new("1", DEFAULT_USER_SERVER).is_newsletter());
    }

    #[test]
    fn bot_users() {
        assert!(Jid::new("13135550002", DEFAULT_USER_SERVER).is_bot());
        assert!(Jid::new("13165550012", DEFAULT_USER_SERVER).is_bot());
        assert!(!Jid::new("13135550002", DEFAULT_USER_SERVER).with_device(1).is_bot());
        assert!(!Jid::new("1313555000", DEFAULT_USER_SERVER).is_bot());
        assert!(Jid::new("whatever", BOT_SERVER).is_bot());
        assert!(!Jid::new("48123456789", DEFAULT_USER_SERVER).is_bot());
    }
}
