// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Key derivation of the Double Ratchet and Sender Keys (Signal spec §5,
//! "KDF chains"), on top of [`crate::crypto::hkdf`].
//!
//! * root KDF: `HKDF(ikm = DH, salt = root_key, info = "WhisperRatchet", 64)`
//! * X3DH master KDF: `HKDF(ikm = master, salt = 0, info = "WhisperText", 64)`
//! * chain KDF: `HMAC(chain_key, 0x01)` = message key seed,
//!   `HMAC(chain_key, 0x02)` = next chain key
//! * message keys: `HKDF(seed, salt = 0, "WhisperMessageKeys", 80)` =
//!   `cipher(32) || mac(32) || iv(16)`
//! * sender message keys: `HKDF(seed, salt = 0, "WhisperGroup", 48)` =
//!   `iv(16) || cipher(32)`

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::SignalError;
use super::ecc::calculate_agreement;
use crate::crypto::hkdf;
use crate::crypto::keys::KeyPair;

type HmacSha256 = Hmac<Sha256>;

const ROOT_INFO: &[u8] = b"WhisperRatchet";
const TEXT_INFO: &[u8] = b"WhisperText";
const MESSAGE_KEYS_INFO: &[u8] = b"WhisperMessageKeys";
const GROUP_INFO: &[u8] = b"WhisperGroup";
const MESSAGE_KEY_SEED: u8 = 0x01;
const CHAIN_KEY_SEED: u8 = 0x02;

/// HMAC-SHA256 (32-byte output).
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    // `Hmac::new_from_slice` accepts keys of any length; the error arm is
    // unreachable but handled without a panic (an all-zero MAC never verifies).
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return [0u8; 32];
    };
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// HKDF-SHA256 (Go: `kdf.DeriveSecrets`). An empty salt is the RFC 5869 zero salt.
pub fn derive_secrets(ikm: &[u8], salt: &[u8], info: &[u8], len: u8) -> Result<Vec<u8>, SignalError> {
    let out = hkdf::sha256(ikm, salt, info, len);
    if out.len() != usize::from(len) {
        return Err(SignalError::Crypto(crate::crypto::CryptoError::HkdfExpand));
    }
    Ok(out)
}

fn split32(bytes: &[u8]) -> Result<([u8; 32], [u8; 32]), SignalError> {
    let first = bytes.get(..32).and_then(|b| <[u8; 32]>::try_from(b).ok());
    let second = bytes.get(32..64).and_then(|b| <[u8; 32]>::try_from(b).ok());
    match (first, second) {
        (Some(a), Some(b)) => Ok((a, b)),
        _ => Err(SignalError::Crypto(crate::crypto::CryptoError::HkdfExpand)),
    }
}

/// A root key of the Double Ratchet (Go: `keys/root.Key`).
#[derive(Clone, PartialEq, Eq)]
pub struct RootKey(pub [u8; 32]);

impl std::fmt::Debug for RootKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RootKey(..)")
    }
}

impl RootKey {
    /// The DH ratchet step: derive the next root key and a fresh chain key
    /// from `DH(their_ratchet, our_ratchet)` (Go: `root.Key.CreateChain`).
    pub fn create_chain(
        &self,
        their_ratchet: &[u8; 32],
        our_ratchet: &KeyPair,
    ) -> Result<(RootKey, ChainKey), SignalError> {
        let shared = calculate_agreement(their_ratchet, &our_ratchet.private);
        let derived = derive_secrets(&shared, &self.0, ROOT_INFO, 64)?;
        let (root, chain) = split32(&derived)?;
        Ok((RootKey(root), ChainKey::new(chain, 0)))
    }
}

/// X3DH master secret → initial `(root_key, chain_key)`
/// (Go: `ratchet.calculateDerivedKeys`).
pub fn root_and_chain_keys(master_secret: &[u8]) -> Result<(RootKey, ChainKey), SignalError> {
    let derived = derive_secrets(master_secret, &[], TEXT_INFO, 64)?;
    let (root, chain) = split32(&derived)?;
    Ok((RootKey(root), ChainKey::new(chain, 0)))
}

/// A sending/receiving chain key and its index (Go: `keys/chain.Key`).
#[derive(Clone, PartialEq, Eq)]
pub struct ChainKey {
    pub key: [u8; 32],
    pub index: u32,
}

impl std::fmt::Debug for ChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainKey").field("index", &self.index).finish_non_exhaustive()
    }
}

impl ChainKey {
    pub fn new(key: [u8; 32], index: u32) -> Self {
        Self { key, index }
    }

    /// The symmetric ratchet step (Go: `chain.Key.NextKey`).
    pub fn next(&self) -> ChainKey {
        ChainKey::new(hmac_sha256(&self.key, &[&[CHAIN_KEY_SEED]]), self.index.wrapping_add(1))
    }

    /// The message keys for this index (Go: `chain.Key.MessageKeys`).
    pub fn message_keys(&self) -> Result<MessageKeys, SignalError> {
        let seed = hmac_sha256(&self.key, &[&[MESSAGE_KEY_SEED]]);
        let derived = derive_secrets(&seed, &[], MESSAGE_KEYS_INFO, 80)?;
        let (cipher_key, mac_key) = split32(&derived)?;
        let iv = derived
            .get(64..80)
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .ok_or(SignalError::Crypto(crate::crypto::CryptoError::HkdfExpand))?;
        Ok(MessageKeys { cipher_key, mac_key, iv, index: self.index })
    }
}

/// Per-message AES-CBC key, HMAC key and IV (Go: `keys/message.Keys`).
#[derive(Clone, PartialEq, Eq)]
pub struct MessageKeys {
    pub cipher_key: [u8; 32],
    pub mac_key: [u8; 32],
    pub iv: [u8; 16],
    pub index: u32,
}

impl std::fmt::Debug for MessageKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageKeys").field("index", &self.index).finish_non_exhaustive()
    }
}

/// Convenience wrapper (Go: `chain.Key.MessageKeys`).
pub fn chain_key_message_keys(chain_key: &ChainKey) -> Result<MessageKeys, SignalError> {
    chain_key.message_keys()
}

/// A sender-key chain (Go: `groups/ratchet.SenderChainKey`).
#[derive(Clone, PartialEq, Eq)]
pub struct SenderChainKey {
    pub iteration: u32,
    pub seed: [u8; 32],
}

impl std::fmt::Debug for SenderChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderChainKey").field("iteration", &self.iteration).finish_non_exhaustive()
    }
}

impl SenderChainKey {
    pub fn new(iteration: u32, seed: [u8; 32]) -> Self {
        Self { iteration, seed }
    }

    /// The next chain key.
    pub fn next(&self) -> SenderChainKey {
        SenderChainKey::new(self.iteration.wrapping_add(1), hmac_sha256(&self.seed, &[&[CHAIN_KEY_SEED]]))
    }

    /// The message key of this iteration.
    pub fn message_key(&self) -> Result<SenderMessageKey, SignalError> {
        SenderMessageKey::from_seed(self.iteration, hmac_sha256(&self.seed, &[&[MESSAGE_KEY_SEED]]))
    }
}

/// A sender (group) message key (Go: `groups/ratchet.SenderMessageKey`).
/// Only `iteration` + `seed` are persisted; `iv`/`cipher_key` are derived.
#[derive(Clone, PartialEq, Eq)]
pub struct SenderMessageKey {
    pub iteration: u32,
    pub seed: [u8; 32],
    pub iv: [u8; 16],
    pub cipher_key: [u8; 32],
}

impl std::fmt::Debug for SenderMessageKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderMessageKey").field("iteration", &self.iteration).finish_non_exhaustive()
    }
}

impl SenderMessageKey {
    /// Derives `iv(16) || cipher_key(32)` from the seed.
    pub fn from_seed(iteration: u32, seed: [u8; 32]) -> Result<Self, SignalError> {
        let derived = derive_secrets(&seed, &[], GROUP_INFO, 48)?;
        let iv = derived.get(..16).and_then(|b| <[u8; 16]>::try_from(b).ok());
        let cipher_key = derived.get(16..48).and_then(|b| <[u8; 32]>::try_from(b).ok());
        match (iv, cipher_key) {
            (Some(iv), Some(cipher_key)) => Ok(Self { iteration, seed, iv, cipher_key }),
            _ => Err(SignalError::Crypto(crate::crypto::CryptoError::HkdfExpand)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_rfc4231_case2() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(hex::encode(mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn derive_secrets_rfc5869_case3() {
        // RFC 5869 test case 3 (zero-length salt and info).
        let ikm = [0x0bu8; 22];
        let okm = derive_secrets(&ikm, &[], &[], 42).unwrap();
        assert_eq!(
            hex::encode(okm),
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
        );
    }

    #[test]
    fn chain_key_ratchet_is_deterministic_and_distinct() {
        let ck = ChainKey::new([7u8; 32], 0);
        let next = ck.next();
        assert_eq!(next.index, 1);
        assert_ne!(next.key, ck.key);
        assert_eq!(ck.next(), next);
        let mk = chain_key_message_keys(&ck).unwrap();
        assert_eq!(mk.index, 0);
        assert_ne!(mk.cipher_key, mk.mac_key);
        // The message key seed and the next chain key are different HMACs.
        assert_ne!(hmac_sha256(&ck.key, &[&[1]]), next.key);
    }

    #[test]
    fn root_ratchet_is_symmetric() {
        let alice = KeyPair::generate();
        let bob = KeyPair::generate();
        let root = RootKey([3u8; 32]);
        let (r1, c1) = root.create_chain(&bob.public, &alice).unwrap();
        let (r2, c2) = root.create_chain(&alice.public, &bob).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(c1, c2);
        assert_eq!(c1.index, 0);
    }

    #[test]
    fn sender_chain_key_steps() {
        let ck = SenderChainKey::new(5, [9u8; 32]);
        let mk = ck.message_key().unwrap();
        assert_eq!(mk.iteration, 5);
        assert_eq!(ck.next().iteration, 6);
        let again = SenderMessageKey::from_seed(5, mk.seed).unwrap();
        assert_eq!(again, mk);
    }
}
