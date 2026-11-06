// Ported from the Go client `binary/attrs.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `AttrUtility` — reads several attributes and collects the errors.

use std::time::Duration;

use crate::types::jid::Jid;

use super::node::{Attrs, Node};

/// One failed attribute read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrError {
    /// A required attribute is missing.
    Missing(String),
    /// The attribute has the wrong type.
    WrongType { key: String, expected: &'static str, got: &'static str },
    /// The value isn't a number/bool/JID.
    Invalid { key: String, value: String },
}

impl std::fmt::Display for AttrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttrError::Missing(key) => write!(f, "didn't find required attribute '{key}'"),
            AttrError::WrongType { key, expected, got } => {
                write!(f, "expected attribute '{key}' to be {expected}, but was {got}")
            }
            AttrError::Invalid { key, value } => {
                write!(f, "failed to parse '{value}' in attribute '{key}'")
            }
        }
    }
}

/// `binary.AttrUtility` — reads attributes and remembers every failure, so the
/// caller can check once at the end instead of after each attribute.
#[derive(Debug, Default)]
pub struct AttrGetter<'a> {
    attrs: Option<&'a Attrs>,
    errors: Vec<AttrError>,
}

impl<'a> AttrGetter<'a> {
    pub fn new(attrs: &'a Attrs) -> Self {
        Self { attrs: Some(attrs), errors: Vec::new() }
    }

    /// The errors collected so far.
    pub fn errors(&self) -> &[AttrError] {
        &self.errors
    }

    /// `OK` — true when no error was collected.
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn error(&self) -> Option<String> {
        (!self.ok()).then(|| self.errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
    }

    fn record(&mut self, error: AttrError) {
        self.errors.push(error);
    }

    /// The raw attribute value.
    pub fn raw(&self, key: &str) -> Option<&super::node::AttrValue> {
        self.attrs.and_then(|attrs| attrs.get(key))
    }

    /// `GetJID`.
    pub fn get_jid(&mut self, key: &str, require: bool) -> Option<Jid> {
        match self.raw(key) {
            None => {
                if require {
                    self.record(AttrError::Missing(key.to_owned()));
                }
                None
            }
            Some(super::node::AttrValue::Jid(jid)) => Some(jid.clone()),
            Some(other) => {
                self.record(AttrError::WrongType { key: key.to_owned(), expected: "JID", got: type_name(other) });
                None
            }
        }
    }

    /// `JID` — required.
    pub fn jid(&mut self, key: &str) -> Jid {
        self.get_jid(key, true).unwrap_or_default()
    }

    /// `OptionalJID`.
    pub fn optional_jid(&mut self, key: &str) -> Option<Jid> {
        self.get_jid(key, false)
    }

    /// `GetString`.
    pub fn get_string(&mut self, key: &str, require: bool) -> Option<String> {
        match self.raw(key) {
            None => {
                if require {
                    self.record(AttrError::Missing(key.to_owned()));
                }
                None
            }
            Some(super::node::AttrValue::Text(text)) => Some(text.clone()),
            Some(super::node::AttrValue::Jid(jid)) => Some(jid.to_string()),
            Some(super::node::AttrValue::Int(value)) => Some(value.to_string()),
            Some(super::node::AttrValue::Uint(value)) => Some(value.to_string()),
            Some(super::node::AttrValue::Bool(value)) => Some(value.to_string()),
        }
    }

    /// `String` — required.
    pub fn string(&mut self, key: &str) -> String {
        self.get_string(key, true).unwrap_or_default()
    }

    /// `OptionalString`.
    pub fn optional_string(&mut self, key: &str) -> String {
        self.get_string(key, false).unwrap_or_default()
    }

    /// `GetInt64`.
    pub fn get_int64(&mut self, key: &str, require: bool) -> Option<i64> {
        match self.raw(key) {
            Some(super::node::AttrValue::Int(value)) => Some(*value),
            Some(super::node::AttrValue::Uint(value)) => Some(*value as i64),
            _ => {
                let text = self.get_string(key, require)?;
                match text.parse::<i64>() {
                    Ok(value) => Some(value),
                    Err(_) => {
                        self.record(AttrError::Invalid { key: key.to_owned(), value: text });
                        None
                    }
                }
            }
        }
    }

    /// `Int64` — required.
    pub fn int64(&mut self, key: &str) -> i64 {
        self.get_int64(key, true).unwrap_or_default()
    }

    /// `OptionalInt`.
    pub fn optional_int(&mut self, key: &str) -> i32 {
        self.get_int64(key, false).unwrap_or_default() as i32
    }

    /// `Int` — required.
    pub fn int(&mut self, key: &str) -> i32 {
        self.get_int64(key, true).unwrap_or_default() as i32
    }

    /// `GetUint64`.
    pub fn get_uint64(&mut self, key: &str, require: bool) -> Option<u64> {
        match self.raw(key) {
            Some(super::node::AttrValue::Uint(value)) => Some(*value),
            Some(super::node::AttrValue::Int(value)) if *value >= 0 => Some(*value as u64),
            _ => {
                let text = self.get_string(key, require)?;
                match text.parse::<u64>() {
                    Ok(value) => Some(value),
                    Err(_) => {
                        self.record(AttrError::Invalid { key: key.to_owned(), value: text });
                        None
                    }
                }
            }
        }
    }

    /// `Uint64` — required.
    pub fn uint64(&mut self, key: &str) -> u64 {
        self.get_uint64(key, true).unwrap_or_default()
    }

    /// `GetBool` — Go's `strconv.ParseBool` accepts 1/t/T/TRUE/true/True/0/f/F/…
    pub fn get_bool(&mut self, key: &str, require: bool) -> Option<bool> {
        let text = self.get_string(key, require)?;
        match parse_bool(&text) {
            Some(value) => Some(value),
            None => {
                self.record(AttrError::Invalid { key: key.to_owned(), value: text });
                None
            }
        }
    }

    /// `Bool` — required.
    pub fn bool(&mut self, key: &str) -> bool {
        self.get_bool(key, true).unwrap_or_default()
    }

    /// `OptionalBool`.
    pub fn optional_bool(&mut self, key: &str) -> bool {
        self.get_bool(key, false).unwrap_or_default()
    }

    /// `GetUnixTime` — seconds since the epoch; `None` for a missing/zero value.
    pub fn get_unix_time(&mut self, key: &str, require: bool) -> Option<Duration> {
        let value = self.get_int64(key, require)?;
        (value != 0).then(|| Duration::from_secs(value as u64))
    }

    /// `GetUnixMilli` — milliseconds since the epoch.
    pub fn get_unix_milli(&mut self, key: &str, require: bool) -> Option<Duration> {
        let value = self.get_int64(key, require)?;
        (value != 0).then(|| Duration::from_millis(value as u64))
    }

    /// `OptionalUnixTime`.
    pub fn optional_unix_time(&mut self, key: &str) -> Option<Duration> {
        self.get_unix_time(key, false)
    }

    /// `UnixTime` — required.
    pub fn unix_time(&mut self, key: &str) -> Option<Duration> {
        self.get_unix_time(key, true)
    }

    /// `OptionalUnixMilli`.
    pub fn optional_unix_milli(&mut self, key: &str) -> Option<Duration> {
        self.get_unix_milli(key, false)
    }
}

/// `AttrGetter` for a node (Go's `Node.AttrGetter`).
impl Node {
    pub fn attr_getter(&self) -> AttrGetter<'_> {
        AttrGetter::new(&self.attrs)
    }
}

fn type_name(value: &super::node::AttrValue) -> &'static str {
    use super::node::AttrValue;
    match value {
        AttrValue::Text(_) => "string",
        AttrValue::Jid(_) => "JID",
        AttrValue::Int(_) => "integer",
        AttrValue::Uint(_) => "unsigned integer",
        AttrValue::Bool(_) => "boolean",
    }
}

/// `strconv.ParseBool` — which accepts more than just `true`/`false`.
fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::jid::Jid;

    fn node() -> Node {
        Node::new("iq")
            .attr("id", "1234")
            .attr("type", "get")
            .attr("to", Jid::new("48123456789", "s.whatsapp.net"))
            .attr("count", "42")
            .attr("ratio", "-7")
            .attr("flag", "true")
            .attr("ts", "1700000000")
    }

    #[test]
    fn reads_typed_values() {
        let node = node();
        let mut getter = node.attr_getter();
        assert_eq!(getter.string("id"), "1234");
        assert_eq!(getter.optional_string("missing"), "");
        assert_eq!(getter.jid("to").user, "48123456789");
        assert_eq!(getter.int("count"), 42);
        assert_eq!(getter.int64("ratio"), -7);
        assert_eq!(getter.uint64("count"), 42);
        assert!(getter.bool("flag"));
        assert_eq!(getter.unix_time("ts"), Some(Duration::from_secs(1_700_000_000)));
        assert!(getter.ok(), "{:?}", getter.errors());
    }

    #[test]
    fn collects_errors() {
        let node = node();
        let mut getter = node.attr_getter();
        getter.jid("missing");
        getter.int("type");
        getter.bool("id");
        let errors = getter.errors().to_vec();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert_eq!(errors[0], AttrError::Missing("missing".into()));
        assert!(matches!(errors[1], AttrError::Invalid { .. }));
        assert!(getter.error().unwrap().contains("didn't find required attribute 'missing'"));
    }

    #[test]
    fn missing_optional_values_are_not_errors() {
        let node = node();
        let mut getter = node.attr_getter();
        assert_eq!(getter.optional_jid("missing"), None);
        assert_eq!(getter.optional_int("missing"), 0);
        assert!(!getter.optional_bool("missing"));
        assert_eq!(getter.optional_unix_time("missing"), None);
        assert_eq!(getter.optional_unix_milli("ts"), Some(Duration::from_millis(1_700_000_000)));
        assert!(getter.ok());
    }

    #[test]
    fn zero_time_is_none_and_bool_parsing_is_go_like() {
        let node = Node::new("x").attr("ts", "0").attr("a", "1").attr("b", "False");
        let mut getter = node.attr_getter();
        assert_eq!(getter.unix_time("ts"), None);
        assert!(getter.bool("a"));
        assert!(!getter.bool("b"));
        assert!(getter.ok());
    }
}
