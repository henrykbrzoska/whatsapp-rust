// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Curve25519 helpers for the Signal layer: X25519 agreement, XEdDSA
//! signatures (over [`crate::crypto::keys`]) and the `0x05` (`DjbType`)
//! public-key encoding.

use curve25519_dalek::montgomery::MontgomeryPoint;
use rand::RngCore;

use super::SignalError;
use crate::crypto::keys::{DJB_TYPE, KeyPair, calculate_signature, djb_public, verify_signature};

/// X25519 Diffie-Hellman: `their_public · clamp(our_private)`.
///
/// Go: `ecc.CalculateKeyAgreement` / `kdf.CalculateSharedSecret`.
pub fn calculate_agreement(their_public: &[u8; 32], our_private: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint(*their_public).mul_clamped(*our_private).to_bytes()
}

/// Parses a serialized public key: `0x05 || key` (33 bytes).
///
/// Go: `ecc.DecodePoint` — any other type byte or length is rejected.
pub fn decode_point(bytes: &[u8]) -> Result<[u8; 32], SignalError> {
    match bytes {
        [DJB_TYPE, rest @ ..] if rest.len() == 32 => {
            let mut key = [0u8; 32];
            key.copy_from_slice(rest);
            Ok(key)
        }
        [] => Err(SignalError::InvalidKey("empty public key".into())),
        [kind, ..] if bytes.len() == 33 => Err(SignalError::InvalidKey(format!("bad key type {kind:#04x}"))),
        _ => Err(SignalError::InvalidKey(format!("public key has {} bytes, expected 33", bytes.len()))),
    }
}

/// Like [`decode_point`] but also accepts the raw 32-byte form (what the
/// WhatsApp `<identity>`/`<value>` nodes carry).
pub fn decode_point_lenient(bytes: &[u8]) -> Result<[u8; 32], SignalError> {
    if bytes.len() == 32 {
        let mut key = [0u8; 32];
        key.copy_from_slice(bytes);
        return Ok(key);
    }
    decode_point(bytes)
}

/// `0x05 || public` (Go: `DjbECPublicKey.Serialize`).
pub fn serialize_point(public: &[u8; 32]) -> [u8; 33] {
    djb_public(public)
}

/// Parses a 32-byte private key.
pub fn decode_private(bytes: &[u8]) -> Result<[u8; 32], SignalError> {
    <[u8; 32]>::try_from(bytes)
        .map_err(|_| SignalError::InvalidKey(format!("private key has {} bytes, expected 32", bytes.len())))
}

/// XEdDSA signature of `message` with a Curve25519 private key.
///
/// Go: `ecc.CalculateSignature` (random 64-byte nonce input).
pub fn sign(private: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let mut random = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut random);
    calculate_signature(private, message, &random)
}

/// Verifies an XEdDSA signature made by [`sign`] / `KeyPair::sign`.
///
/// Go: `ecc.VerifySignature`. `public` may be raw (32) or serialized (33);
/// anything else (or a signature that is not 64 bytes) is simply invalid.
pub fn verify_djb(public: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = decode_point_lenient(public) else {
        return false;
    };
    let Ok(signature) = <&[u8; 64]>::try_from(signature) else {
        return false;
    };
    verify_signature(&key, message, signature)
}

/// A fresh random key pair (Go: `ecc.GenerateKeyPair`).
pub fn generate_key_pair() -> KeyPair {
    KeyPair::generate()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex<const N: usize>(s: &str) -> [u8; N] {
        let v = hex::decode(s).unwrap();
        v.try_into().unwrap()
    }

    #[test]
    fn rfc7748_agreement_vector() {
        // RFC 7748 §6.1.
        let alice_private: [u8; 32] = unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob_public: [u8; 32] = unhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let bob_private: [u8; 32] = unhex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let alice_public: [u8; 32] = unhex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        let shared = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";
        assert_eq!(hex::encode(calculate_agreement(&bob_public, &alice_private)), shared);
        assert_eq!(hex::encode(calculate_agreement(&alice_public, &bob_private)), shared);
    }

    #[test]
    fn decode_point_requires_type_byte() {
        let kp = KeyPair::generate();
        assert_eq!(decode_point(&serialize_point(&kp.public)).unwrap(), kp.public);
        assert!(decode_point(&kp.public).is_err());
        assert!(decode_point(&[]).is_err());
        let mut bad = serialize_point(&kp.public);
        bad[0] = 0x04;
        assert!(matches!(decode_point(&bad), Err(SignalError::InvalidKey(_))));
        assert_eq!(decode_point_lenient(&kp.public).unwrap(), kp.public);
        assert!(decode_private(&[1; 31]).is_err());
    }

    #[test]
    fn sign_and_verify() {
        let kp = KeyPair::generate();
        let sig = sign(&kp.private, b"hello");
        assert!(verify_djb(&kp.public, b"hello", &sig));
        assert!(verify_djb(&serialize_point(&kp.public), b"hello", &sig));
        assert!(!verify_djb(&kp.public, b"hellO", &sig));
        assert!(!verify_djb(&kp.public, b"hello", &sig[..63]));
        assert!(!verify_djb(&[1, 2, 3], b"hello", &sig));
    }
}
