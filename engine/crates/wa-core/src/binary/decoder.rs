// Ported from the Go client `binary/decoder.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The binary XML decoder (`binaryDecoder`).

use crate::types::jid::{BROADCAST_SERVER, GROUP_SERVER, Jid, MESSENGER_SERVER};
use crate::types::jid::{DEFAULT_USER_SERVER, INTEROP_SERVER, NEWSLETTER_SERVER};

use super::errors::BinaryError;
use super::node::{AttrValue, Attrs, Node, NodeContent};
use super::token;

/// A decoded value — the Rust equivalent of Go's `read` returning `any`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `LIST_EMPTY` — no value.
    Nil,
    /// A list of child nodes.
    Nodes(Vec<Node>),
    /// A textual value.
    Text(String),
    /// Raw bytes (content read with `as_string == false`).
    Bytes(Vec<u8>),
    /// A JID.
    Jid(Jid),
}

impl Value {
    fn into_content(self) -> NodeContent {
        match self {
            Value::Nil => NodeContent::Empty,
            Value::Nodes(nodes) => NodeContent::Nodes(nodes),
            Value::Text(text) => NodeContent::Text(text),
            Value::Bytes(bytes) => NodeContent::Bytes(bytes),
            Value::Jid(jid) => NodeContent::Jid(jid),
        }
    }

    fn into_attr(self) -> Option<AttrValue> {
        match self {
            Value::Nil => None,
            Value::Text(text) => Some(AttrValue::Text(text)),
            Value::Bytes(bytes) => Some(AttrValue::Text(String::from_utf8_lossy(&bytes).into_owned())),
            Value::Jid(jid) => Some(AttrValue::Jid(jid)),
            Value::Nodes(_) => None,
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }
}

/// `binaryDecoder` — a cursor over a binary XML document.
pub struct Decoder<'a> {
    data: &'a [u8],
    index: usize,
}

impl<'a> Decoder<'a> {
    /// `newDecoder`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, index: 0 }
    }

    /// Current cursor position (used by `Unmarshal` for the leftover check).
    pub fn position(&self) -> usize {
        self.index
    }

    fn check_eos(&self, length: usize) -> Result<(), BinaryError> {
        if self.index + length > self.data.len() {
            return Err(BinaryError::UnexpectedEof);
        }
        Ok(())
    }

    fn read_byte(&mut self) -> Result<u8, BinaryError> {
        self.check_eos(1)?;
        let byte = self.data[self.index];
        self.index += 1;
        Ok(byte)
    }

    fn read_int_n(&mut self, n: usize, little_endian: bool) -> Result<usize, BinaryError> {
        self.check_eos(n)?;
        let mut value = 0usize;
        for i in 0..n {
            let shift = if little_endian { i } else { n - i - 1 };
            value |= (self.data[self.index + i] as usize) << (shift * 8);
        }
        self.index += n;
        Ok(value)
    }

    fn read_int8(&mut self, little_endian: bool) -> Result<usize, BinaryError> {
        self.read_int_n(1, little_endian)
    }

    fn read_int16(&mut self, little_endian: bool) -> Result<usize, BinaryError> {
        self.read_int_n(2, little_endian)
    }

    fn read_int32(&mut self, little_endian: bool) -> Result<usize, BinaryError> {
        self.read_int_n(4, little_endian)
    }

    fn read_int20(&mut self) -> Result<usize, BinaryError> {
        self.check_eos(3)?;
        let value = (((self.data[self.index] & 15) as usize) << 16)
            + ((self.data[self.index + 1] as usize) << 8)
            + self.data[self.index + 2] as usize;
        self.index += 3;
        Ok(value)
    }

    /// `readPacked8` — nibble or hex packed strings.
    fn read_packed8(&mut self, tag: u8) -> Result<String, BinaryError> {
        let start_byte = self.read_byte()?;
        let mut out = String::new();
        for _ in 0..(start_byte & 127) {
            let current = self.read_byte()?;
            let lower = unpack_byte(tag, (current & 0xF0) >> 4)?;
            let upper = unpack_byte(tag, current & 0x0F)?;
            out.push(lower as char);
            out.push(upper as char);
        }
        if start_byte >> 7 != 0 {
            out.pop();
        }
        Ok(out)
    }

    fn read_list_size(&mut self, tag: u8) -> Result<usize, BinaryError> {
        match tag {
            token::LIST_EMPTY => Ok(0),
            token::LIST_8 => self.read_int8(false),
            token::LIST_16 => self.read_int16(false),
            other => Err(BinaryError::InvalidToken { token: other, position: self.index }),
        }
    }

    /// `read` — one value; `as_string` decides whether byte content becomes a string.
    pub fn read_value(&mut self, as_string: bool) -> Result<Value, BinaryError> {
        let tag = self.read_byte()?;
        match tag {
            token::LIST_EMPTY => Ok(Value::Nil),
            token::LIST_8 | token::LIST_16 => Ok(Value::Nodes(self.read_list(tag)?)),
            token::BINARY_8 => {
                let size = self.read_int8(false)?;
                self.read_bytes_or_string(size, as_string)
            }
            token::BINARY_20 => {
                let size = self.read_int20()?;
                self.read_bytes_or_string(size, as_string)
            }
            token::BINARY_32 => {
                let size = self.read_int32(false)?;
                self.read_bytes_or_string(size, as_string)
            }
            token::DICTIONARY_0 | token::DICTIONARY_1 | token::DICTIONARY_2 | token::DICTIONARY_3 => {
                let index = self.read_int8(false)? as u8;
                token::get_double_token(tag - token::DICTIONARY_0, index)
                    .map(|token| Value::Text(token.to_owned()))
                    .ok_or(BinaryError::InvalidToken { token: tag, position: self.index })
            }
            token::FB_JID => self.read_fb_jid(),
            token::INTEROP_JID => self.read_interop_jid(),
            token::JID_PAIR => self.read_jid_pair(),
            token::AD_JID => self.read_ad_jid(),
            token::NIBBLE_8 | token::HEX_8 => Ok(Value::Text(self.read_packed8(tag)?)),
            other => match token::SINGLE_BYTE_TOKENS.get(other as usize) {
                Some(token) if other >= 1 => Ok(Value::Text((*token).to_owned())),
                _ => Err(BinaryError::InvalidToken { token: other, position: self.index }),
            },
        }
    }

    fn read_jid_pair(&mut self) -> Result<Value, BinaryError> {
        let user = self.read_value(true)?;
        let server = self.read_value(true)?;
        let Some(server) = server.as_text() else {
            return Err(BinaryError::InvalidJidType { expected: "a server string", got: format!("{server:?}") });
        };
        match user {
            Value::Nil => Ok(Value::Jid(Jid::new("", server))),
            Value::Text(user) => Ok(Value::Jid(Jid::new(user, server))),
            other => Err(BinaryError::InvalidJidType { expected: "a user string", got: format!("{other:?}") }),
        }
    }

    fn read_interop_jid(&mut self) -> Result<Value, BinaryError> {
        let user = self.read_text()?;
        let device = self.read_int16(false)? as u16;
        let integrator = self.read_int16(false)? as u16;
        let server = self.read_text()?;
        if server != INTEROP_SERVER {
            return Err(BinaryError::InvalidJidType { expected: INTEROP_SERVER, got: server });
        }
        Ok(Value::Jid(Jid { user, device, integrator, server, ..Default::default() }))
    }

    fn read_fb_jid(&mut self) -> Result<Value, BinaryError> {
        let user = self.read_text()?;
        let device = self.read_int16(false)? as u16;
        let server = self.read_text()?;
        if server != MESSENGER_SERVER {
            return Err(BinaryError::InvalidJidType { expected: MESSENGER_SERVER, got: server });
        }
        Ok(Value::Jid(Jid { user, device, server, ..Default::default() }))
    }

    fn read_ad_jid(&mut self) -> Result<Value, BinaryError> {
        let agent = self.read_byte()?;
        let device = self.read_byte()?;
        let user = self.read_text()?;
        Ok(Value::Jid(Jid::new_ad(user, agent, device)))
    }

    fn read_text(&mut self) -> Result<String, BinaryError> {
        match self.read_value(true)? {
            Value::Text(text) => Ok(text),
            other => Err(BinaryError::InvalidJidType { expected: "a string", got: format!("{other:?}") }),
        }
    }

    fn read_attributes(&mut self, count: usize) -> Result<Attrs, BinaryError> {
        if count == 0 {
            return Ok(Attrs::new());
        }
        let mut attrs = Attrs::new();
        for _ in 0..count {
            let key = match self.read_value(true)? {
                Value::Text(key) => key,
                _ => return Err(BinaryError::NonStringKey { position: self.index }),
            };
            let value = self.read_value(true)?;
            if let Some(value) = value.into_attr() {
                attrs.insert(key, value);
            }
        }
        Ok(attrs)
    }

    fn read_list(&mut self, tag: u8) -> Result<Vec<Node>, BinaryError> {
        let size = self.read_list_size(tag)?;
        let mut nodes = Vec::with_capacity(size);
        for _ in 0..size {
            nodes.push(self.read_node()?);
        }
        Ok(nodes)
    }

    /// `readNode`.
    pub fn read_node(&mut self) -> Result<Node, BinaryError> {
        let size = self.read_int8(false)? as u8;
        let list_size = self.read_list_size(size)?;

        let tag = self.read_value(true)?.as_text().map(str::to_owned).unwrap_or_default();
        if list_size == 0 || tag.is_empty() {
            return Err(BinaryError::InvalidNode);
        }

        let attrs = self.read_attributes((list_size - 1) >> 1)?;
        if list_size % 2 == 1 {
            return Ok(Node { tag, attrs, content: NodeContent::Empty });
        }

        let content = self.read_value(false)?.into_content();
        Ok(Node { tag, attrs, content })
    }

    fn read_bytes_or_string(&mut self, length: usize, as_string: bool) -> Result<Value, BinaryError> {
        let data = self.read_raw(length)?;
        if as_string {
            Ok(Value::Text(String::from_utf8_lossy(data).into_owned()))
        } else {
            Ok(Value::Bytes(data.to_vec()))
        }
    }

    fn read_raw(&mut self, length: usize) -> Result<&'a [u8], BinaryError> {
        self.check_eos(length)?;
        let data = &self.data[self.index..self.index + length];
        self.index += length;
        Ok(data)
    }
}

/// `unpackByte`.
fn unpack_byte(tag: u8, value: u8) -> Result<u8, BinaryError> {
    match tag {
        token::NIBBLE_8 => unpack_nibble(value),
        token::HEX_8 => unpack_hex(value),
        other => Err(BinaryError::UnknownPackedTag(other)),
    }
}

/// `unpackNibble`.
fn unpack_nibble(value: u8) -> Result<u8, BinaryError> {
    match value {
        0..=9 => Ok(b'0' + value),
        10 => Ok(b'-'),
        11 => Ok(b'.'),
        15 => Ok(0),
        other => Err(BinaryError::InvalidPackedByte { tag: token::NIBBLE_8, value: other }),
    }
}

/// `unpackHex`.
fn unpack_hex(value: u8) -> Result<u8, BinaryError> {
    match value {
        0..=9 => Ok(b'0' + value),
        10..=15 => Ok(b'A' + value - 10),
        other => Err(BinaryError::InvalidPackedByte { tag: token::HEX_8, value: other }),
    }
}

/// Servers that `AttrUtility::get_jid` accepts when auto-detecting a JID (see `Node.UnmarshalJSON`).
pub const JID_ATTR_SERVERS: [&str; 4] = [DEFAULT_USER_SERVER, NEWSLETTER_SERVER, GROUP_SERVER, BROADCAST_SERVER];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::{marshal, node::Node, unmarshal};

    #[test]
    fn reads_single_byte_token() {
        let data = [token::index_of_single_token("message").unwrap()];
        let mut decoder = Decoder::new(&data);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Text("message".into()));
        assert_eq!(decoder.position(), 1);
    }

    #[test]
    fn reads_double_byte_token() {
        let mut decoder = Decoder::new(&[token::DICTIONARY_0, 0]);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Text("read-self".into()));
    }

    #[test]
    fn reads_nibble_and_hex() {
        let mut decoder = Decoder::new(&[token::NIBBLE_8, 2, 0x12, 0x34]);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Text("1234".into()));

        let mut decoder = Decoder::new(&[token::NIBBLE_8, 0x80 | 2, 0x48, 0x1F]);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Text("481".into()));

        let mut decoder = Decoder::new(&[token::HEX_8, 2, 0x3E, 0xB0]);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Text("3EB0".into()));
    }

    #[test]
    fn nibble_packing_keeps_dash_dot_and_the_high_bit_trim() {
        // 0xAB -> '-', '.'; 0x9F -> '9', 0xF (a NUL pad byte); 0x82 sets the
        // "odd length" bit, so the padding byte is dropped again.
        let mut decoder = Decoder::new(&[token::NIBBLE_8, 0x82, 0xAB, 0x9F]);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Text("-.9".into()));
    }

    #[test]
    fn reads_jid_pairs() {
        // JID_PAIR, user "1234" packed as nibbles, server = single token "s.whatsapp.net"
        let data = [
            token::JID_PAIR,
            token::NIBBLE_8,
            2,
            0x12,
            0x34,
            token::index_of_single_token(DEFAULT_USER_SERVER).unwrap(),
        ];
        let mut decoder = Decoder::new(&data);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Jid(Jid::new("1234", DEFAULT_USER_SERVER)));

        // An empty user is LIST_EMPTY.
        let data = [token::JID_PAIR, token::LIST_EMPTY, token::index_of_single_token(GROUP_SERVER).unwrap()];
        let mut decoder = Decoder::new(&data);
        assert_eq!(decoder.read_value(true).unwrap(), Value::Jid(Jid::new("", GROUP_SERVER)));
    }

    #[test]
    fn rejects_bad_jid_pairs() {
        // The server must be a string.
        let data = [
            token::JID_PAIR,
            token::index_of_single_token("type").expect("type is a single-byte token"),
            token::LIST_EMPTY,
        ];
        let mut decoder = Decoder::new(&data);
        assert!(matches!(decoder.read_value(true), Err(BinaryError::InvalidJidType { .. })));
    }

    #[test]
    fn errors_on_truncated_and_unknown_data() {
        let mut decoder = Decoder::new(&[token::BINARY_8, 5, b'a']);
        assert_eq!(decoder.read_value(true), Err(BinaryError::UnexpectedEof));

        // A truncated double-byte token: the dictionary tag without its index.
        let mut decoder = Decoder::new(&[token::DICTIONARY_0]);
        assert_eq!(decoder.read_value(true), Err(BinaryError::UnexpectedEof));

        // 240 is past the single-byte token table (236 entries).
        let mut decoder = Decoder::new(&[240]);
        assert_eq!(decoder.read_value(true), Err(BinaryError::InvalidToken { token: 240, position: 1 }));
    }

    #[test]
    fn rejects_empty_documents() {
        assert_eq!(unmarshal(&[]), Err(BinaryError::UnexpectedEof));
        // LIST_EMPTY as the size tag means "list of zero" and then the tag read fails.
        assert_eq!(unmarshal(&[token::LIST_EMPTY]), Err(BinaryError::UnexpectedEof));
        // A zero-size LIST_8 has no tag either.
        assert_eq!(unmarshal(&[token::LIST_8, 0]), Err(BinaryError::UnexpectedEof));
        // A tag that is past the end of the single-byte token table.
        assert_eq!(unmarshal(&[token::LIST_8, 1, 240]), Err(BinaryError::InvalidToken { token: 240, position: 3 }));
    }

    #[test]
    fn reports_leftover_bytes() {
        // `marshal` writes the data type byte, `unmarshal` expects the payload.
        let mut data = marshal(&Node::new("a"));
        data.push(0xFF);
        assert_eq!(unmarshal(&data[1..]), Err(BinaryError::LeftoverBytes(1)));
    }
}
