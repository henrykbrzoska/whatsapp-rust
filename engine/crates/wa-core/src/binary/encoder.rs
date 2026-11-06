// Ported from the Go client `binary/encoder.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The binary XML encoder (`binaryEncoder`).

use crate::types::jid::{DEFAULT_USER_SERVER, Jid, MESSENGER_SERVER};
use crate::types::jid::{HIDDEN_USER_SERVER, HOSTED_LID_SERVER, HOSTED_SERVER, INTEROP_SERVER};

use super::errors::BinaryError;
use super::node::{Attrs, Node, NodeContent};
use super::token;

/// Number of bytes `writeNode` adds for the tag itself.
const TAG_SIZE: usize = 1;

/// Go's `math.MaxInt32` — the largest length `BINARY_32` can carry.
const MAX_INT32: usize = i32::MAX as usize;

/// `binaryEncoder` — appends to a byte buffer starting with the `0` data type.
pub struct Encoder {
    data: Vec<u8>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    /// `newEncoder` — a buffer pre-filled with the "not compressed" data type.
    pub fn new() -> Self {
        Self { data: vec![0] }
    }

    /// `getData`.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    fn push_byte(&mut self, byte: u8) {
        self.data.push(byte);
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }

    fn push_int_n(&mut self, value: usize, n: usize, little_endian: bool) {
        for i in 0..n {
            let shift = if little_endian { i } else { n - i - 1 };
            self.push_byte(((value >> (shift * 8)) & 0xFF) as u8);
        }
    }

    fn push_int20(&mut self, value: usize) {
        self.push_bytes(&[((value >> 16) & 0x0F) as u8, ((value >> 8) & 0xFF) as u8, (value & 0xFF) as u8]);
    }

    fn push_int8(&mut self, value: usize) {
        self.push_int_n(value, 1, false);
    }

    fn push_int16(&mut self, value: usize) {
        self.push_int_n(value, 2, false);
    }

    fn push_int32(&mut self, value: usize) {
        self.push_int_n(value, 4, false);
    }

    fn push_string(&mut self, value: &str) {
        self.push_bytes(value.as_bytes());
    }

    fn write_byte_length(&mut self, length: usize) {
        if length < 256 {
            self.push_byte(token::BINARY_8);
            self.push_int8(length);
        } else if length < (1 << 20) {
            self.push_byte(token::BINARY_20);
            self.push_int20(length);
        } else if length < MAX_INT32 {
            self.push_byte(token::BINARY_32);
            self.push_int32(length);
        } else {
            // Go panics here; a length that large can't happen in practice.
            self.push_byte(token::BINARY_32);
            self.push_int32(MAX_INT32);
        }
    }

    /// `writeNode`.
    pub fn write_node(&mut self, node: &Node) {
        if node.tag == "0" {
            self.push_byte(token::LIST_8);
            self.push_byte(token::LIST_EMPTY);
            return;
        }

        let has_content = usize::from(!node.content.is_empty());
        self.write_list_start(2 * self.count_attributes(&node.attrs) + TAG_SIZE + has_content);
        self.write_string(&node.tag);
        self.write_attributes(&node.attrs);
        if !node.content.is_empty() {
            self.write_content(&node.content);
        }
    }

    /// `write` — dispatches on the content type.
    fn write_content(&mut self, content: &NodeContent) {
        match content {
            NodeContent::Empty => self.push_byte(token::LIST_EMPTY),
            NodeContent::Text(text) => self.write_string(text),
            NodeContent::Jid(jid) => self.write_jid(jid),
            NodeContent::Bytes(bytes) => self.write_bytes(bytes),
            NodeContent::Nodes(nodes) => {
                self.write_list_start(nodes.len());
                for node in nodes {
                    self.write_node(node);
                }
            }
        }
    }

    fn write_string(&mut self, data: &str) {
        if let Some(index) = token::index_of_single_token(data) {
            self.push_byte(index);
        } else if let Some((dictionary, index)) = token::index_of_double_byte_token(data) {
            self.push_byte(token::DICTIONARY_0 + dictionary);
            self.push_byte(index);
        } else if validate_nibble(data) {
            self.write_packed_bytes(data, token::NIBBLE_8).expect("nibble was validated");
        } else if validate_hex(data) {
            self.write_packed_bytes(data, token::HEX_8).expect("hex was validated");
        } else {
            self.write_string_raw(data);
        }
    }

    fn write_bytes(&mut self, value: &[u8]) {
        self.write_byte_length(value.len());
        self.push_bytes(value);
    }

    fn write_string_raw(&mut self, value: &str) {
        self.write_byte_length(value.len());
        self.push_string(value);
    }

    /// `writeJID`.
    fn write_jid(&mut self, jid: &Jid) {
        if ((jid.server == DEFAULT_USER_SERVER || jid.server == HIDDEN_USER_SERVER) && jid.device > 0)
            || jid.server == HOSTED_SERVER
            || jid.server == HOSTED_LID_SERVER
        {
            self.push_byte(token::AD_JID);
            self.push_byte(jid.actual_agent());
            self.push_byte(jid.device as u8);
            self.write_string(&jid.user);
        } else if jid.server == MESSENGER_SERVER {
            self.push_byte(token::FB_JID);
            self.write_string(&jid.user);
            self.push_int16(jid.device as usize);
            self.write_string(&jid.server);
        } else if jid.server == INTEROP_SERVER {
            self.push_byte(token::INTEROP_JID);
            self.write_string(&jid.user);
            self.push_int16(jid.device as usize);
            self.push_int16(jid.integrator as usize);
            self.write_string(&jid.server);
        } else {
            self.push_byte(token::JID_PAIR);
            if jid.user.is_empty() {
                self.push_byte(token::LIST_EMPTY);
            } else {
                self.write_string(&jid.user);
            }
            self.write_string(&jid.server);
        }
    }

    /// `writeAttributes` — empty values are skipped (like Go).
    fn write_attributes(&mut self, attrs: &Attrs) {
        for (key, value) in attrs {
            if value.as_str() == Some("") {
                continue;
            }
            self.write_string(key);
            self.write_attr_value(value);
        }
    }

    fn write_attr_value(&mut self, value: &super::node::AttrValue) {
        use super::node::AttrValue;
        match value {
            AttrValue::Text(text) => self.write_string(text),
            AttrValue::Jid(jid) => self.write_jid(jid),
            AttrValue::Int(number) => self.write_string(&number.to_string()),
            AttrValue::Uint(number) => self.write_string(&number.to_string()),
            AttrValue::Bool(flag) => self.write_string(if *flag { "true" } else { "false" }),
        }
    }

    /// `countAttributes` — how many attributes are actually written.
    fn count_attributes(&self, attrs: &Attrs) -> usize {
        attrs.values().filter(|value| value.as_str() != Some("")).count()
    }

    fn write_list_start(&mut self, list_size: usize) {
        if list_size == 0 {
            self.push_byte(token::LIST_EMPTY);
        } else if list_size < 256 {
            self.push_byte(token::LIST_8);
            self.push_int8(list_size);
        } else {
            self.push_byte(token::LIST_16);
            self.push_int16(list_size);
        }
    }

    /// `writePackedBytes` — nibble or hex packing.
    fn write_packed_bytes(&mut self, value: &str, data_type: u8) -> Result<(), BinaryError> {
        let bytes = value.as_bytes();
        if bytes.len() > token::PACKED_MAX {
            return Err(BinaryError::PackedTooLong(bytes.len()));
        }

        self.push_byte(data_type);

        let mut rounded_length = bytes.len().div_ceil(2) as u8;
        if !bytes.len().is_multiple_of(2) {
            rounded_length |= 128;
        }
        self.push_byte(rounded_length);

        let packer: fn(u8) -> Result<u8, BinaryError> = match data_type {
            token::NIBBLE_8 => pack_nibble,
            token::HEX_8 => pack_hex,
            other => return Err(BinaryError::InvalidPackedType(other)),
        };

        for pair in bytes.chunks(2) {
            let first = packer(pair[0])?;
            let second = if pair.len() == 2 { packer(pair[1])? } else { packer(0)? };
            self.push_byte((first << 4) | second);
        }
        Ok(())
    }
}

/// `validateNibble` — digits, `-` and `.`, up to [`token::PACKED_MAX`] chars.
pub fn validate_nibble(value: &str) -> bool {
    value.len() <= token::PACKED_MAX && value.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// `packNibble`.
fn pack_nibble(value: u8) -> Result<u8, BinaryError> {
    match value {
        b'-' => Ok(10),
        b'.' => Ok(11),
        0 => Ok(15),
        b'0'..=b'9' => Ok(value - b'0'),
        other => Err(BinaryError::InvalidPackedByte { tag: token::NIBBLE_8, value: other }),
    }
}

/// `validateHex` — uppercase hex digits, up to [`token::PACKED_MAX`] chars.
pub fn validate_hex(value: &str) -> bool {
    value.len() <= token::PACKED_MAX && value.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

/// `packHex`.
fn pack_hex(value: u8) -> Result<u8, BinaryError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'A'..=b'F' => Ok(10 + value - b'A'),
        0 => Ok(15),
        other => Err(BinaryError::InvalidPackedByte { tag: token::HEX_8, value: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_nibble_and_hex() {
        assert!(validate_nibble("48123456789"));
        assert!(validate_nibble("48-1.2"));
        assert!(!validate_nibble("48abc"));
        assert!(!validate_nibble("abcdef")); // lowercase letters are not nibbles
        assert!(!validate_nibble(&"1".repeat(token::PACKED_MAX + 1)));

        assert!(validate_hex("3EB0C767D097"));
        assert!(validate_hex("0123"));
        assert!(!validate_hex("3eb0c767")); // lowercase is rejected
        assert!(!validate_hex("3EB0C767D09Z"));
        assert!(!validate_hex(&"A".repeat(token::PACKED_MAX + 1)));
    }

    #[test]
    fn packs_nibbles_with_a_trailing_half_byte() {
        // 3 digits -> 2 bytes, last nibble 0, high bit of the length set.
        let mut encoder = Encoder::new();
        encoder.write_string("123");
        assert_eq!(encoder.as_slice(), &[0, token::NIBBLE_8, 0x80 | 2, 0x12, 0x3F]);

        let mut encoder = Encoder::new();
        encoder.write_string("1234");
        assert_eq!(encoder.as_slice(), &[0, token::NIBBLE_8, 2, 0x12, 0x34]);
    }

    #[test]
    fn packs_hex() {
        let mut encoder = Encoder::new();
        encoder.write_string("3EB0");
        assert_eq!(encoder.as_slice(), &[0, token::HEX_8, 2, 0x3E, 0xB0]);
    }

    #[test]
    fn single_and_double_byte_tokens() {
        let mut encoder = Encoder::new();
        encoder.write_string("message");
        assert_eq!(encoder.as_slice(), &[0, token::index_of_single_token("message").unwrap()]);

        let mut encoder = Encoder::new();
        encoder.write_string("read-self");
        assert_eq!(encoder.as_slice(), &[0, token::DICTIONARY_0, 0]);
    }

    #[test]
    fn binary_lengths() {
        let mut encoder = Encoder::new();
        encoder.write_string_raw(&"a".repeat(3));
        assert_eq!(encoder.as_slice()[..3], [0, token::BINARY_8, 3]);

        let mut encoder = Encoder::new();
        encoder.write_string_raw(&"a".repeat(300));
        assert_eq!(encoder.as_slice()[..4], [0, token::BINARY_20, 0x00, 0x01]);
        assert_eq!(encoder.as_slice()[4], 0x2C);

        let mut encoder = Encoder::new();
        encoder.write_string_raw(&"a".repeat(1 << 20));
        assert_eq!(encoder.as_slice()[..5], [0, token::BINARY_32, 0x00, 0x10, 0x00], "BINARY_32 is big-endian");
    }

    #[test]
    fn empty_string_is_not_a_token() {
        let mut encoder = Encoder::new();
        encoder.write_string("");
        // validateNibble("") is true, so an empty string is packed as zero nibbles.
        assert_eq!(encoder.as_slice(), &[0, token::NIBBLE_8, 0]);
    }

    #[test]
    fn jid_variants() {
        let mut encoder = Encoder::new();
        encoder.write_jid(&Jid::new("48123456789", DEFAULT_USER_SERVER));
        assert_eq!(encoder.as_slice()[1], token::JID_PAIR);

        // A device-qualified user JID is written as an AD-JID.
        let mut encoder = Encoder::new();
        encoder.write_jid(&Jid::parse("5511999:3@s.whatsapp.net").unwrap());
        assert_eq!(encoder.as_slice()[1], token::AD_JID);
        assert_eq!(encoder.as_slice()[2], 0, "agent of a default-server JID is 0");
        assert_eq!(encoder.as_slice()[3], 3);

        // A server-only JID has an empty user.
        let mut encoder = Encoder::new();
        encoder.write_jid(&Jid::new("", "g.us"));
        assert_eq!(
            encoder.as_slice(),
            &[0, token::JID_PAIR, token::LIST_EMPTY, token::index_of_single_token("g.us").unwrap()]
        );

        let mut encoder = Encoder::new();
        encoder.write_jid(&Jid::new("user", MESSENGER_SERVER));
        assert_eq!(encoder.as_slice()[1], token::FB_JID);

        let mut encoder = Encoder::new();
        encoder.write_jid(&Jid::new("user", INTEROP_SERVER));
        assert_eq!(encoder.as_slice()[1], token::INTEROP_JID);
    }

    #[test]
    fn empty_attributes_are_skipped() {
        let mut node = Node::new("a").attr("x", "1");
        node.attrs.insert("skip".into(), "".into());
        let data = super::super::node::marshal(&node);
        // [0, LIST_8, 3 (= 2*1 attrs + tag + no content), tag, key, value]
        assert_eq!(data[..3], [0, token::LIST_8, 3]);
        assert_eq!(
            super::super::node::unmarshal(&data[1..]).unwrap(),
            Node::new("a").attr("x", "1"),
            "the empty attribute must not come back"
        );
    }
}
