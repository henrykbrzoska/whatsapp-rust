// Ported from the Go client (`util/keys`, `hkdfutil`, `gcmutil`, `cbcutil`),
// © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Cryptographic primitives (port of the Go client `util/keys`, `util/hkdfutil`,
//! `util/gcmutil`, `util/cbcutil`).
//!
//! * [`keys`] — Curve25519 key pairs, pre-keys, identity keys, Signal padding.
//! * [`hkdf`] — HKDF-SHA256 (`hkdfutil`).
//! * [`gcm`] — AES-256-GCM (`gcmutil`).
//! * [`cbc`] — AES-256-CBC + PKCS#7 + HMAC stream MAC (`cbcutil`).
//!
//! * [`signal`] — the Signal protocol layer (X3DH, Double Ratchet, Sender Keys).

pub mod cbc;
pub mod gcm;
pub mod hkdf;
pub mod keys;
pub mod msgsecret;
pub mod signal;

pub use cbc::{BLOCK_LEN, MAC_LEN, StreamEncrypted, decrypt_in_place, encrypt_stream, encrypt_stream_with_iv};
pub use gcm::{NONCE_LEN, TAG_LEN, decrypt as gcm_decrypt, encrypt as gcm_encrypt, prepare};
pub use hkdf::{MAX_OUTPUT_LEN, PRK_LEN, extract as hkdf_extract, hkdf_expand, sha256 as hkdf_sha256};
pub use keys::{IdentityKey, KeyPair, PreKey, pad_message, unpad_message};

/// Errors raised by the crypto primitives.
///
/// Go returns `error` values from `gcmutil`/`cbcutil` and panics inside
/// `hkdfutil`/`cbcutil.Encrypt`; every failure here is a value — no primitive
/// panics outside `#[cfg(test)]`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    /// AEAD authentication failed (tampered ciphertext, AAD or wrong key).
    #[error("AEAD failure: {0}")]
    Aead(&'static str),
    /// A key had the wrong length.
    #[error("invalid key length: expected {expected} bytes, got {actual}")]
    InvalidKeyLength { expected: usize, actual: usize },
    /// An IV/nonce had the wrong length.
    #[error("invalid IV length: expected {expected} bytes, got {actual}")]
    InvalidIvLength { expected: usize, actual: usize },
    /// CBC input was empty or not a multiple of the block size.
    #[error("invalid CBC block length: {actual} bytes is not a positive multiple of {block}")]
    InvalidBlockLength { block: usize, actual: usize },
    /// PKCS#7 unpadding failed (`unpad_message` and `cbcutil.unpad`).
    #[error("bad padding")]
    BadPadding,
    /// HKDF expand failed (only reachable with an invalid input key).
    #[error("HKDF expand failed")]
    HkdfExpand,
    /// A serialized key had an unexpected type byte or length.
    #[error("invalid key encoding: expected {expected} bytes, got {actual}")]
    InvalidKeyEncoding { expected: usize, actual: usize },
    /// A `PreKeyRecordStructure` could not be decoded or was missing fields.
    #[error("invalid pre-key record: {0}")]
    InvalidPreKeyRecord(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_descriptive() {
        assert_eq!(
            CryptoError::InvalidKeyLength { expected: 32, actual: 16 }.to_string(),
            "invalid key length: expected 32 bytes, got 16"
        );
        assert_eq!(CryptoError::BadPadding.to_string(), "bad padding");
        assert_eq!(
            CryptoError::InvalidBlockLength { block: 16, actual: 15 }.to_string(),
            "invalid CBC block length: 15 bytes is not a positive multiple of 16"
        );
        assert_eq!(CryptoError::Aead("tag mismatch").to_string(), "AEAD failure: tag mismatch");
    }
}
