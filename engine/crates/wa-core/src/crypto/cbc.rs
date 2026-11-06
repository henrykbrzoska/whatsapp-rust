// Ported from the Go client (`util/cbcutil/cbc.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! AES-256-CBC with PKCS#7 padding — the "media" encryption used by
//! `EncryptStream` (upload) and `DecryptFile` (download).
//!
//! Go counterpart: `Go util/cbcutil` (`Encrypt`, `Decrypt`,
//! `DecryptFile`, `EncryptStream`).
//!
//! Padding matches Go exactly: the plaintext is always padded, so an
//! already block-aligned input grows by one full 16-byte block, and the pad
//! byte is the pad length.
//!
//! The Go functions accept any AES key size; WhatsApp only uses 32-byte
//! AES-256 keys, so 16/24-byte keys are rejected with
//! [`CryptoError::InvalidKeyLength`] (documented deviation — adding AES-128/192
//! arms is a two-line match when a call site needs it).
//!
//! This module is pure bytes-in/bytes-out: no `std::fs` (Go's `DecryptFile`
//! takes an `os.File`; here the caller owns the buffer, see
//! [`decrypt_in_place`]).

use aes::Aes256;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use cbc::{Decryptor, Encryptor};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::CryptoError;

/// AES block size in bytes (`aes.BlockSize`).
pub const BLOCK_LEN: usize = 16;
/// AES-256 key length in bytes.
pub const KEY_LEN: usize = 32;
/// Truncated HMAC-SHA256 length used by the media stream MAC.
pub const MAC_LEN: usize = 10;

type HmacSha256 = Hmac<Sha256>;

fn check_key(key: &[u8]) -> Result<(), CryptoError> {
    if key.len() != KEY_LEN {
        return Err(CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: key.len() });
    }
    Ok(())
}

fn check_iv(iv: &[u8]) -> Result<(), CryptoError> {
    if iv.len() != BLOCK_LEN {
        return Err(CryptoError::InvalidIvLength { expected: BLOCK_LEN, actual: iv.len() });
    }
    Ok(())
}

/// `cbcutil.Encrypt` — AES-256-CBC with PKCS#7 padding.
///
/// With `iv = None` a random 16-byte IV is generated and **prepended** to the
/// returned ciphertext (Go's behaviour); with `Some(iv)` the ciphertext only
/// contains the encrypted blocks. The IV is never included in the MAC or in
/// any hash — that is the caller's job in [`encrypt_stream`].
pub fn encrypt(key: &[u8], iv: Option<&[u8]>, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    check_key(key)?;
    match iv {
        None => {
            let mut random_iv = [0u8; BLOCK_LEN];
            rand::thread_rng().fill_bytes(&mut random_iv);
            let mut out = Vec::with_capacity(BLOCK_LEN + padded_len(plaintext.len()));
            out.extend_from_slice(&random_iv);
            out.extend_from_slice(&encrypt_with_iv(key, &random_iv, plaintext)?);
            Ok(out)
        }
        Some(iv) => encrypt_with_iv(key, iv, plaintext),
    }
}

/// CBC-encrypts with an explicit IV (no IV prefix in the output).
pub fn encrypt_with_iv(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    check_key(key)?;
    check_iv(iv)?;
    let encryptor = Encryptor::<Aes256>::new_from_slices(key, iv)
        .map_err(|_| CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: key.len() })?;
    Ok(encryptor.encrypt_padded_vec_mut::<Pkcs7>(plaintext))
}

/// `cbcutil.Decrypt` — AES-256-CBC decryption with PKCS#7 unpadding.
///
/// Rejects an empty or non-block-multiple ciphertext with
/// [`CryptoError::InvalidBlockLength`] and a bad pad with
/// [`CryptoError::BadPadding`] (Go errors on both, never panics).
pub fn decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    check_key(key)?;
    check_iv(iv)?;
    check_ciphertext_len(ciphertext.len())?;
    let decryptor = Decryptor::<Aes256>::new_from_slices(key, iv)
        .map_err(|_| CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: key.len() })?;
    decryptor.decrypt_padded_vec_mut::<Pkcs7>(ciphertext).map_err(|_| CryptoError::BadPadding)
}

/// `cbcutil.DecryptFile` — decrypts a whole buffer in place and truncates the
/// padding away (used by media downloads; the caller reads/writes the file).
pub fn decrypt_in_place(key: &[u8], iv: &[u8], buf: &mut Vec<u8>) -> Result<(), CryptoError> {
    check_key(key)?;
    check_iv(iv)?;
    check_ciphertext_len(buf.len())?;
    let decryptor = Decryptor::<Aes256>::new_from_slices(key, iv)
        .map_err(|_| CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: key.len() })?;
    let plain_len =
        decryptor.decrypt_padded_mut::<Pkcs7>(buf.as_mut_slice()).map_err(|_| CryptoError::BadPadding)?.len();
    buf.truncate(plain_len);
    Ok(())
}

/// Result of [`encrypt_stream`] — the media upload payload plus its hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEncrypted {
    /// The CBC ciphertext (padded), **without** the MAC.
    pub ciphertext: Vec<u8>,
    /// The IV the ciphertext was encrypted with.
    pub iv: Vec<u8>,
    /// The truncated (10-byte) HMAC-SHA256 of `iv || ciphertext`.
    pub mac: Vec<u8>,
    /// Length of the plaintext.
    pub plaintext_len: u64,
    /// Length of the uploaded object: `ciphertext.len() + mac.len()`
    /// (Go's `uploadSize`, i.e. `FileLength` + padding + MAC).
    pub ciphertext_len: u64,
    /// SHA-256 of the plaintext — `file_sha256` in `UploadResponse`.
    pub plaintext_hash: [u8; 32],
    /// SHA-256 of `ciphertext || mac` — `file_enc_sha256` in `UploadResponse`
    /// (the IV is derived from the media key and never uploaded, so it is not
    /// part of this hash, exactly like `cbcutil.EncryptStream`).
    pub ciphertext_hash: [u8; 32],
}

/// `cbcutil.EncryptStream` with a random IV.
///
/// Go's `EncryptStream(key, iv, macKey, reader, writer)` takes the IV and an
/// `io.Reader`/`io.Writer`; the media upload path generates a fresh IV, so this
/// wrapper does that and calls [`encrypt_stream_with_iv`].
pub fn encrypt_stream(key: &[u8], mac_key: &[u8], plaintext: &[u8]) -> Result<StreamEncrypted, CryptoError> {
    let mut iv = [0u8; BLOCK_LEN];
    rand::thread_rng().fill_bytes(&mut iv);
    encrypt_stream_with_iv(key, &iv, mac_key, plaintext)
}

/// `cbcutil.EncryptStream` — CBC + PKCS#7 + `HMAC-SHA256(macKey, iv || ciphertext)`
/// truncated to 10 bytes, plus the plaintext/ciphertext SHA-256 hashes.
///
/// Go returns `(plainHash, cipherHash, plainSize, cipherSize)`; sizes here are
/// carried in [`StreamEncrypted`] and the ciphertext/MAC are returned as bytes
/// instead of being streamed into an `io.Writer`.
pub fn encrypt_stream_with_iv(
    key: &[u8],
    iv: &[u8],
    mac_key: &[u8],
    plaintext: &[u8],
) -> Result<StreamEncrypted, CryptoError> {
    let ciphertext = encrypt_with_iv(key, iv, plaintext)?;

    let mut mac = <HmacSha256 as Mac>::new_from_slice(mac_key)
        .map_err(|_| CryptoError::InvalidKeyLength { expected: 0, actual: mac_key.len() })?;
    mac.update(iv);
    mac.update(&ciphertext);
    let mac = mac.finalize().into_bytes()[..MAC_LEN].to_vec();

    let mut cipher_hasher = Sha256::new();
    cipher_hasher.update(&ciphertext);
    cipher_hasher.update(&mac);

    Ok(StreamEncrypted {
        plaintext_len: plaintext.len() as u64,
        ciphertext_len: (ciphertext.len() + mac.len()) as u64,
        plaintext_hash: Sha256::digest(plaintext).into(),
        ciphertext_hash: cipher_hasher.finalize().into(),
        ciphertext,
        iv: iv.to_vec(),
        mac,
    })
}

fn check_ciphertext_len(len: usize) -> Result<(), CryptoError> {
    if len == 0 || !len.is_multiple_of(BLOCK_LEN) {
        return Err(CryptoError::InvalidBlockLength { block: BLOCK_LEN, actual: len });
    }
    Ok(())
}

fn padded_len(len: usize) -> usize {
    len + (BLOCK_LEN - len % BLOCK_LEN)
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
        hex("000102030405060708090a0b0c0d0e0f")
    }

    fn plaintext() -> Vec<u8> {
        hex("4d6564696120656e6372797074696f6e2c2066756c6c20626c6f636b")
    }

    #[test]
    fn go_encrypt_vector_with_fixed_iv() {
        // Go: cbcutil.Encrypt(key, iv, plaintext) — 28-byte plaintext → 32-byte ciphertext.
        let ciphertext = encrypt(&key(), Some(&iv()), &plaintext()).expect("encrypt");
        assert_eq!(hex::encode(&ciphertext), "94f6c29ccff6f3b3a219061996d6e955f4165570e861f53a0477c940741c2196");
        assert_eq!(decrypt(&key(), &iv(), &ciphertext).expect("decrypt"), plaintext());
    }

    #[test]
    fn random_iv_is_prepended_and_roundtrips() {
        let ciphertext = encrypt(&key(), None, &plaintext()).expect("encrypt");
        assert_eq!(ciphertext.len(), BLOCK_LEN + padded_len(plaintext().len()));
        let (random_iv, body) = ciphertext.split_at(BLOCK_LEN);
        assert_ne!(random_iv, iv().as_slice());
        assert_eq!(decrypt(&key(), random_iv, body).expect("decrypt"), plaintext());
    }

    #[test]
    fn aligned_plaintext_gets_a_full_padding_block() {
        let aligned = [7u8; BLOCK_LEN * 2];
        let ciphertext = encrypt(&key(), Some(&iv()), &aligned).expect("encrypt");
        assert_eq!(ciphertext.len(), BLOCK_LEN * 3);
        assert_eq!(decrypt(&key(), &iv(), &ciphertext).expect("decrypt"), aligned.to_vec());
    }

    #[test]
    fn decrypt_in_place_truncates_padding() {
        let mut buf = encrypt(&key(), Some(&iv()), &plaintext()).expect("encrypt");
        decrypt_in_place(&key(), &iv(), &mut buf).expect("decrypt");
        assert_eq!(buf, plaintext());
    }

    #[test]
    fn decrypt_rejects_bad_padding_and_bad_lengths() {
        let mut ciphertext = encrypt(&key(), Some(&iv()), &plaintext()).expect("encrypt");
        let last = ciphertext.len() - 1;
        ciphertext[last] = 0x20; // pad larger than the block size
        assert!(matches!(
            decrypt(&key(), &iv(), &ciphertext),
            Err(CryptoError::BadPadding) | Err(CryptoError::InvalidBlockLength { .. })
        ));

        assert!(matches!(
            decrypt(&key(), &iv(), &[0u8; BLOCK_LEN - 1]),
            Err(CryptoError::InvalidBlockLength { block: BLOCK_LEN, actual: 15 })
        ));
        assert!(matches!(decrypt(&key(), &iv(), &[]), Err(CryptoError::InvalidBlockLength { .. })));
        assert!(matches!(
            decrypt(&[0u8; 16], &iv(), &[0u8; BLOCK_LEN]),
            Err(CryptoError::InvalidKeyLength { expected: KEY_LEN, actual: 16 })
        ));
        assert!(matches!(
            decrypt(&key(), &[0u8; 8], &[0u8; BLOCK_LEN]),
            Err(CryptoError::InvalidIvLength { expected: BLOCK_LEN, actual: 8 })
        ));
        let mut short = vec![0u8; BLOCK_LEN - 1];
        assert!(decrypt_in_place(&key(), &iv(), &mut short).is_err());
    }

    #[test]
    fn encrypt_stream_matches_go_vector() {
        // Go: cbcutil.EncryptStream(key, iv, macKey, plaintext)
        let plaintext = hex("74686520717569636b2062726f776e20666f78206a756d7073206f766572");
        let mac_key = hex("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20");
        let out = encrypt_stream_with_iv(&key(), &iv(), &mac_key, &plaintext).expect("encrypt");

        let mut expected = hex("dc5cf5b0c2750f7ffa6274c87f70682adfc8846739d58734643e67fbf2fae8d6f2d91e614cb3dc3b0d2b");
        let mac = expected.split_off(expected.len() - MAC_LEN);
        assert_eq!(out.ciphertext, expected);
        assert_eq!(out.mac, mac);
        assert_eq!(out.iv, iv());
        assert_eq!(out.plaintext_len, 30);
        assert_eq!(out.ciphertext_len, 42);
        assert_eq!(hex::encode(out.plaintext_hash), "fa85c34fa89174a6eb3e0952ab27c375a0cff3a79db24ad48cb83b8ca77daf5a");
        assert_eq!(
            hex::encode(out.ciphertext_hash),
            "61e3b4787d5e11dec74993564c5edf24d07f6b62b252458697fc49e61b4a80c6"
        );

        // The MAC covers iv || ciphertext and can be re-derived by the receiver.
        let mut mac_check = <HmacSha256 as Mac>::new_from_slice(&mac_key).expect("hmac key");
        mac_check.update(&out.iv);
        mac_check.update(&out.ciphertext);
        assert_eq!(mac_check.finalize().into_bytes()[..MAC_LEN].to_vec(), out.mac);
    }

    #[test]
    fn encrypt_stream_uses_a_random_iv() {
        let a = encrypt_stream(&key(), b"mac key", b"hello media").expect("encrypt");
        let b = encrypt_stream(&key(), b"mac key", b"hello media").expect("encrypt");
        assert_ne!(a.iv, b.iv);
        assert_eq!(a.ciphertext.len(), BLOCK_LEN);
        assert_eq!(decrypt(&key(), &a.iv, &a.ciphertext).expect("decrypt"), b"hello media");
    }
}
