// Ported from the Go client (`util/gcmutil/gcm.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! AES-256-GCM helpers used by the WhatsApp protocol (media keys, app-state
//! keys, encrypted message secrets).
//!
//! Go counterpart: `Go util/gcmutil` (`Prepare`, `Encrypt`, `Decrypt`).
//!
//! WhatsApp always uses 32-byte AES-256 keys and 12-byte nonces; the nonce is
//! never generated here — it comes from the protocol (`HKDF` output or the
//! media key expansion), exactly like in Go.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};

/// AES-256 key length in bytes.
pub const KEY_LEN: usize = 32;
/// GCM nonce (IV) length in bytes — WhatsApp never uses another size.
pub const NONCE_LEN: usize = 12;
/// GCM authentication tag length in bytes.
pub const TAG_LEN: usize = 16;

/// `gcmutil.Prepare` — builds an AES-256-GCM cipher from a 32-byte key.
///
/// 16/24-byte keys are rejected with [`CryptoError::InvalidKeyLength`]: WhatsApp
/// and libsignal only ever use AES-256, and the `aes-gcm` crate has no AES-192
/// alias. Go returns an error for unknown key sizes as well — it never panics.
pub fn prepare(key: &[u8]) -> Result<Aes256Gcm, super::CryptoError> {
    if key.len() != KEY_LEN {
        return Err(super::CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: key.len() });
    }
    Aes256Gcm::new_from_slice(key)
        .map_err(|_| super::CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: key.len() })
}

/// Parses a 12-byte nonce without panicking on a bad length.
fn nonce(iv: &[u8]) -> Result<Nonce<aes_gcm::aead::consts::U12>, super::CryptoError> {
    if iv.len() != NONCE_LEN {
        return Err(super::CryptoError::InvalidIvLength { expected: NONCE_LEN, actual: iv.len() });
    }
    // The length was just checked, so `from_slice` cannot panic.
    Ok(*Nonce::from_slice(iv))
}

/// `gcmutil.Encrypt` — `AES-256-GCM(key, iv)` over `plaintext` with `aad`.
///
/// The returned bytes are `ciphertext || tag` (16-byte tag appended), the same
/// layout Go's `cipher.AEAD.Seal` produces.
pub fn encrypt(key: &[u8], iv: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, super::CryptoError> {
    let cipher = prepare(key)?;
    let nonce = nonce(iv)?;
    cipher.encrypt(&nonce, Payload { msg: plaintext, aad }).map_err(|_| super::CryptoError::Aead("encryption failed"))
}

/// `gcmutil.Decrypt` — verifies the tag and returns the plaintext.
///
/// A tag mismatch (tampered ciphertext, AAD or key) becomes
/// [`CryptoError::Aead`], matching Go's `cipher.AEAD.Open` error.
pub fn decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, super::CryptoError> {
    let cipher = prepare(key)?;
    let nonce = nonce(iv)?;
    cipher
        .decrypt(&nonce, Payload { msg: ciphertext, aad })
        .map_err(|_| super::CryptoError::Aead("authentication tag mismatch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("test fixture is valid hex")
    }

    fn key() -> Vec<u8> {
        hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4")
    }

    fn iv() -> Vec<u8> {
        hex("000102030405060708090a0b")
    }

    fn plaintext() -> Vec<u8> {
        hex("576861747341707020476f6c616e6720656e6372797074696f6e2074657374")
    }

    fn aad() -> Vec<u8> {
        hex("aabbccddeeff")
    }

    #[test]
    fn go_vector_and_roundtrip() {
        // Go: gcmutil.Encrypt(aes256Key, iv, plaintext, aad)
        let ciphertext = encrypt(&key(), &iv(), &plaintext(), &aad()).expect("encrypt");
        assert_eq!(
            hex::encode(&ciphertext),
            "5f09d027f705eb0e86e72dc5ec50fecb2012e590db15edbab7d9dc940cd76139d671a08ef8141207c1525463ee1fe1"
        );
        assert_eq!(decrypt(&key(), &iv(), &ciphertext, &aad()).expect("decrypt"), plaintext());
        assert_eq!(ciphertext.len(), plaintext().len() + TAG_LEN);
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        let ciphertext = encrypt(&key(), &iv(), &[], b"").expect("encrypt");
        assert_eq!(ciphertext.len(), TAG_LEN);
        assert_eq!(decrypt(&key(), &iv(), &ciphertext, b"").expect("decrypt"), Vec::<u8>::new());
    }

    #[test]
    fn tampered_ciphertext_and_aad_are_rejected() {
        let mut ciphertext = encrypt(&key(), &iv(), &plaintext(), &aad()).expect("encrypt");
        ciphertext[0] ^= 1;
        assert!(matches!(decrypt(&key(), &iv(), &ciphertext, &aad()), Err(super::super::CryptoError::Aead(_))));

        let ciphertext = encrypt(&key(), &iv(), &plaintext(), &aad()).expect("encrypt");
        assert!(matches!(decrypt(&key(), &iv(), &ciphertext, b"other aad"), Err(super::super::CryptoError::Aead(_))));
    }

    #[test]
    fn rejects_bad_key_and_iv_lengths() {
        assert!(matches!(
            prepare(&[0u8; 16]),
            Err(super::super::CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: 16 })
        ));
        assert!(matches!(
            encrypt(&key(), &[0u8; 16], b"x", b""),
            Err(super::super::CryptoError::InvalidIvLength { expected: NONCE_LEN, actual: 16 })
        ));
    }
}
