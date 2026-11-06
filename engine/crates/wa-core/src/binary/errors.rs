// Ported from the Go client `binary/errors.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Errors returned by the binary XML encoder/decoder.

/// Everything that can go wrong while decoding WhatsApp's binary XML.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BinaryError {
    /// `ErrInvalidType` — the value can't be written to the stream.
    #[error("unsupported payload type: {0}")]
    InvalidType(&'static str),
    /// `ErrInvalidJIDType` — a JID had an unexpected server.
    #[error("invalid JID type: expected {expected:?}, got {got:?}")]
    InvalidJidType { expected: &'static str, got: String },
    /// `ErrInvalidNode` — the node has no tag or a zero-size list.
    #[error("invalid node")]
    InvalidNode,
    /// `ErrInvalidToken` — the byte isn't a known token.
    #[error("invalid token {token} at position {position}")]
    InvalidToken { token: u8, position: usize },
    /// `ErrNonStringKey` — an attribute key decoded to a non-string value.
    #[error("non-string key at position {position}")]
    NonStringKey { position: usize },
    /// `io.EOF` — the buffer ended before the document did.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// `Unmarshal` — trailing bytes after a complete node.
    #[error("{0} leftover bytes after decoding")]
    LeftoverBytes(usize),
    /// `writePackedBytes`/`validateNibble` — too long to pack.
    #[error("too many bytes to pack: {0}")]
    PackedTooLong(usize),
    /// `unpackNibble`/`unpackHex` — a nibble value with no character.
    #[error("invalid packed byte {value} for tag {tag}")]
    InvalidPackedByte { tag: u8, value: u8 },
    /// `unpackByte` with a tag that isn't nibble/hex.
    #[error("unpackByte with unknown tag {0}")]
    UnknownPackedTag(u8),
    /// `Unpack` — zlib failed.
    #[error("failed to decompress zlib data: {0}")]
    Zlib(String),
    /// A write of a value the encoder can't represent (e.g. an attribute in a list).
    #[error("invalid packed byte data type {0}")]
    InvalidPackedType(u8),
}
