// Ported from the Go client (`util/keys/keypair.go`) and libsignal (Go)
// (`ecc/SignCurve25519.go`), © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Curve25519 key pairs, pre-keys and the libsignal "curve25519 sign"
//! signature scheme (Ed25519 over the birationally-equivalent Montgomery key).
//!
//! Go counterparts: `Go util/keys` (`KeyPair`, `PreKey`),
//! `libsignal (Go)/ecc` (`IdentityKey`, `DjbECPublicKey`) and
//! `Go message.go` (`padMessage`, `unpadMessage`).

use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::montgomery::MontgomeryPoint;
use curve25519_dalek::scalar::Scalar;
use prost::Message as _;
use rand::RngCore;
use sha2::{Digest, Sha512};

use super::CryptoError;

/// `ecc.DjbType` — prefix byte of a serialized Curve25519 public key.
pub const DJB_TYPE: u8 = 0x05;

/// A Curve25519 key pair (`keys.KeyPair`).
#[derive(Clone, PartialEq, Eq)]
pub struct KeyPair {
    pub public: [u8; 32],
    pub private: [u8; 32],
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print private key material.
        f.debug_struct("KeyPair").field("public", &hex(&self.public)).finish_non_exhaustive()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Clamps a scalar the X25519 way (`priv[0] &= 248; priv[31] &= 127; priv[31] |= 64`).
pub fn clamp(mut key: [u8; 32]) -> [u8; 32] {
    key[0] &= 248;
    key[31] &= 127;
    key[31] |= 64;
    key
}

impl KeyPair {
    /// `NewKeyPairFromPrivateKey` — derives the public key (`curve25519.ScalarBaseMult`).
    pub fn from_private(private: [u8; 32]) -> Self {
        let public = MontgomeryPoint::mul_base_clamped(private).to_bytes();
        Self { public, private }
    }

    /// `NewKeyPair` — a fresh random, clamped key pair.
    pub fn generate() -> Self {
        let mut private = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut private);
        Self::from_private(clamp(private))
    }

    /// `CreateSignedPreKey` — a new pre-key signed by this (identity) key.
    pub fn create_signed_pre_key(&self, key_id: u32) -> PreKey {
        let mut key = PreKey::generate(key_id);
        key.signature = Some(self.sign(&key.key_pair));
        key
    }

    /// `Sign` — signs `0x05 || key_to_sign.public` with this private key.
    pub fn sign(&self, key_to_sign: &KeyPair) -> [u8; 64] {
        let mut random = [0u8; 64];
        rand::thread_rng().fill_bytes(&mut random);
        calculate_signature(&self.private, &djb_public(&key_to_sign.public), &random)
    }

    /// The serialized public key (`0x05 || public`), the form used everywhere
    /// in the Signal protocol and in the WhatsApp wire format.
    ///
    /// Go counterpart: `libsignal/ecc.DjbECPublicKey.Serialize()` — the shortcut
    /// the Go client builds inline before every signature.
    pub fn public_serialized(&self) -> [u8; 33] {
        djb_public(&self.public)
    }
}

/// A Signal identity key — a Curve25519 public key that never changes for a
/// device (`libsignal/ecc.IdentityKey`).
///
/// Go counterpart: `libsignal (Go)/ecc.IdentityKey` (`Serialize` derives
/// the 33-byte `0x05 || key` form, `Verify` checks a signed pre-key signature).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdentityKey(pub [u8; 32]);

impl IdentityKey {
    /// `IdentityKey.Serialize()` — `0x05 || public_key`.
    pub fn serialize(&self) -> [u8; 33] {
        djb_public(&self.0)
    }

    /// Parses a raw 32-byte key or a serialized `0x05 || key` blob.
    ///
    /// Go counterpart: `IdentityKey.Decode`, which rejects any other type byte.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        match bytes {
            [DJB_TYPE, rest @ ..] if rest.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(rest);
                Ok(Self(key))
            }
            raw if raw.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(raw);
                Ok(Self(key))
            }
            _ => Err(CryptoError::InvalidKeyEncoding { expected: 33, actual: bytes.len() }),
        }
    }

    /// `IdentityKey.FromKeyPair` — the identity key of a key pair.
    pub fn from_pair(key_pair: &KeyPair) -> Self {
        Self(key_pair.public)
    }

    /// `IdentityKey.Verify` — verifies a libsignal signature (signed pre-keys).
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> bool {
        verify_signature(&self.0, message, signature)
    }

    /// The raw 32-byte key material.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("IdentityKey").field(&hex(&self.0)).finish()
    }
}

impl From<KeyPair> for IdentityKey {
    fn from(key_pair: KeyPair) -> Self {
        Self(key_pair.public)
    }
}

/// `0x05 || public` — the serialized form signed in signed pre-keys.
pub fn djb_public(public: &[u8; 32]) -> [u8; 33] {
    let mut out = [0u8; 33];
    out[0] = DJB_TYPE;
    out[1..].copy_from_slice(public);
    out
}

/// A one-time or signed pre-key (`keys.PreKey`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreKey {
    pub key_pair: KeyPair,
    pub key_id: u32,
    pub signature: Option<[u8; 64]>,
}

impl PreKey {
    /// `NewPreKey`.
    pub fn generate(key_id: u32) -> Self {
        Self { key_pair: KeyPair::generate(), key_id, signature: None }
    }

    pub fn from_private(key_id: u32, private: [u8; 32]) -> Self {
        Self { key_pair: KeyPair::from_private(private), key_id, signature: None }
    }

    /// Serializes the pre-key as a Signal `PreKeyRecordStructure`.
    ///
    /// Go counterpart: `libsignal/protocol.PreKeyRecord.Serialize()` over the
    /// `PreKeyRecordStructure` protobuf (`id = 1`, `publicKey = 2`,
    /// `privateKey = 3`; the public key carries the `0x05` type byte).
    ///
    /// The signature of a signed pre-key is not part of this structure — that
    /// uses `SignedPreKeyRecordStructure` (Phase 4 Task B).
    pub fn to_record(&self) -> Vec<u8> {
        PreKeyRecordStructure {
            id: self.key_id,
            public_key: self.public_serialized().to_vec(),
            private_key: self.key_pair.private.to_vec(),
        }
        .encode_to_vec()
    }

    /// Parses a Signal `PreKeyRecordStructure` (see [`Self::to_record`]).
    ///
    /// Go counterpart: `libsignal/protocol.NewPreKeyRecordFromStruct`.
    pub fn from_record(bytes: &[u8]) -> Result<Self, CryptoError> {
        let record = PreKeyRecordStructure::decode(bytes)
            .map_err(|_| CryptoError::InvalidPreKeyRecord("protobuf decode failed"))?;
        if record.public_key.len() != 33 || record.public_key[0] != DJB_TYPE {
            return Err(CryptoError::InvalidPreKeyRecord("public key is not 0x05 || 32 bytes"));
        }
        if record.private_key.len() != 32 {
            return Err(CryptoError::InvalidPreKeyRecord("private key is not 32 bytes"));
        }
        let mut public = [0u8; 32];
        public.copy_from_slice(&record.public_key[1..]);
        let mut private = [0u8; 32];
        private.copy_from_slice(&record.private_key);
        Ok(Self { key_pair: KeyPair { public, private }, key_id: record.id, signature: None })
    }

    /// The serialized public key of this pre-key (`0x05 || public`).
    pub fn public_serialized(&self) -> [u8; 33] {
        self.key_pair.public_serialized()
    }
}

/// Signal's `PreKeyRecordStructure` protobuf (`LocalStorageProtocol.proto`).
#[derive(Clone, PartialEq, prost::Message)]
struct PreKeyRecordStructure {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(bytes = "vec", tag = "2")]
    public_key: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    private_key: Vec<u8>,
}

/// `padMessage` — appends the Signal message padding (D4).
///
/// Go counterpart: `Go message.go:padMessage` — a random pad length in
/// `1..=15` taken from a random byte masked with `0x0f` (`0` becomes `0x0f`),
/// then that many copies of the length itself. `unpad_message` with
/// `version == 3` skips it; older messages rely on it.
pub fn pad_message(plaintext: &[u8]) -> Vec<u8> {
    let mut random = [0u8; 1];
    rand::thread_rng().fill_bytes(&mut random);
    let mut pad_len = random[0] & 0x0f;
    if pad_len == 0 {
        pad_len = 0x0f;
    }
    let mut out = Vec::with_capacity(plaintext.len() + usize::from(pad_len));
    out.extend_from_slice(plaintext);
    out.resize(plaintext.len() + usize::from(pad_len), pad_len);
    out
}

/// `unpadMessage` — strips the Signal message padding (D4).
///
/// Go counterpart: `Go message.go:unpadMessage` — `version == 3` returns
/// the plaintext untouched, empty input and a pad byte that is not repeated at
/// the end of the message are errors. Go's `isValidPadding` does not check that
/// the pad byte is `<= 16`, so neither do we; the pad is always `<= len` here,
/// which is what Go's slice would rely on.
pub fn unpad_message(plaintext: &[u8], version: i32) -> Result<Vec<u8>, CryptoError> {
    if version == 3 {
        return Ok(plaintext.to_vec());
    }
    let Some((&pad, rest)) = plaintext.split_last() else {
        return Err(CryptoError::BadPadding);
    };
    let pad_len = usize::from(pad);
    if pad_len > rest.len() + 1 {
        return Err(CryptoError::BadPadding);
    }
    if !plaintext.ends_with(&vec![pad; pad_len]) {
        return Err(CryptoError::BadPadding);
    }
    Ok(plaintext[..plaintext.len() - pad_len].to_vec())
}

fn reduce(wide: [u8; 64]) -> Scalar {
    Scalar::from_bytes_mod_order_wide(&wide)
}

/// libsignal `CalculateSignature`/`sign`: an Ed25519-style signature made with
/// a Curve25519 private key. The Edwards public key's sign bit is stored in the
/// top bit of the signature so verifiers can recover it from the Montgomery key.
pub fn calculate_signature(private: &[u8; 32], message: &[u8], random: &[u8; 64]) -> [u8; 64] {
    let a = Scalar::from_bytes_mod_order(*private);
    let public_ed = (ED25519_BASEPOINT_POINT * a).compress().to_bytes();

    let mut diversifier = [0xFFu8; 32];
    diversifier[0] = 0xFE;
    let r = reduce(
        Sha512::new()
            .chain_update(diversifier)
            .chain_update(private)
            .chain_update(message)
            .chain_update(random)
            .finalize()
            .into(),
    );
    let encoded_r = (ED25519_BASEPOINT_POINT * r).compress().to_bytes();

    let h =
        reduce(Sha512::new().chain_update(encoded_r).chain_update(public_ed).chain_update(message).finalize().into());
    let s = h * a + r;

    let mut signature = [0u8; 64];
    signature[..32].copy_from_slice(&encoded_r);
    signature[32..].copy_from_slice(s.as_bytes());
    signature[63] |= public_ed[31] & 0x80;
    signature
}

/// libsignal `VerifySignature`/`verify` for [`calculate_signature`].
pub fn verify_signature(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let mut mont = *public;
    mont[31] &= 0x7F;
    let sign_bit = (signature[63] & 0x80) >> 7;
    let Some(a_point) = MontgomeryPoint(mont).to_edwards(sign_bit) else {
        return false;
    };
    let a_ed = a_point.compress().to_bytes();

    let mut sig = *signature;
    sig[63] &= 0x7F;
    let r_bytes: [u8; 32] = sig[..32].try_into().expect("32 bytes");
    let s_bytes: [u8; 32] = sig[32..].try_into().expect("32 bytes");
    let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(s_bytes)) else {
        return false;
    };
    if CompressedEdwardsY(r_bytes).decompress().is_none() {
        return false;
    }
    let h = reduce(Sha512::new().chain_update(r_bytes).chain_update(a_ed).chain_update(message).finalize().into());
    // R == s·B − h·A
    let check = EdwardsPoint::vartime_double_scalar_mul_basepoint(&h, &(-a_point), &s);
    check.compress().to_bytes() == r_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_clamped_and_match_public() {
        let kp = KeyPair::generate();
        assert_eq!(kp.private[0] & 7, 0);
        assert_eq!(kp.private[31] & 0x80, 0);
        assert_eq!(kp.private[31] & 0x40, 0x40);
        assert_eq!(KeyPair::from_private(kp.private), kp);
        assert!(!format!("{kp:?}").contains(&hex(&kp.private)), "Debug must not leak the private key");
    }

    #[test]
    fn x25519_rfc7748_vector() {
        // RFC 7748 §6.1 — Alice's key pair.
        let private: [u8; 32] = decode("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let kp = KeyPair::from_private(private);
        assert_eq!(hex(&kp.public), "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
    }

    #[test]
    fn signed_pre_key_verifies() {
        let identity = KeyPair::generate();
        let spk = identity.create_signed_pre_key(1);
        let sig = spk.signature.unwrap();
        let msg = djb_public(&spk.key_pair.public);
        assert!(verify_signature(&identity.public, &msg, &sig));

        let mut tampered = msg;
        tampered[5] ^= 1;
        assert!(!verify_signature(&identity.public, &tampered, &sig));
        assert!(!verify_signature(&KeyPair::generate().public, &msg, &sig));
    }

    #[test]
    fn libsignal_signature_vector() {
        // libsignal-protocol-java `Curve25519Test.testSignature`.
        let alice_identity_private: [u8; 32] =
            decode("c0972484 12e58bf0 5df48796 82051327 94178e36 7637f581 8f81e0e6 ce73e865");
        let alice_identity_public: [u8; 32] =
            decode("ab7e717d 4a163b7d 9a1d8071 dfe9dcf8 cdcd1cea 3339b635 6be84d88 7e322c64");
        let alice_ephemeral_public: [u8; 33] =
            decode("05 edce9d9c 415ca78c b7252e72 c2c4a554 d3eb2948 5a0e1d50 3118d1a8 2d99fb4a");
        let alice_signature: [u8; 64] = decode(
            "5de88ca9 a89b4a11 5da79109 c67c9c74 64a3e418 0274f1cb 8c63c298 4e286dfb \
             ede82deb 9dcd9fae 0bfbb821 569b3d90 01bd8130 cd11d486 cef047bd 60b86e88",
        );
        assert_eq!(KeyPair::from_private(alice_identity_private).public, alice_identity_public);
        assert!(verify_signature(&alice_identity_public, &alice_ephemeral_public, &alice_signature));
        let mut bad = alice_signature;
        bad[0] ^= 1;
        assert!(!verify_signature(&alice_identity_public, &alice_ephemeral_public, &bad));
    }

    fn decode<const N: usize>(s: &str) -> [u8; N] {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let v: Vec<u8> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect();
        v.try_into().unwrap()
    }

    #[test]
    fn identity_key_serializes_with_type_byte() {
        let kp = KeyPair::generate();
        let identity = IdentityKey::from_pair(&kp);
        assert_eq!(identity.serialize(), kp.public_serialized());
        assert_eq!(identity.serialize()[0], DJB_TYPE);
        assert_eq!(&identity.serialize()[1..], &kp.public);

        assert_eq!(IdentityKey::from_bytes(&identity.serialize()), Ok(identity));
        assert_eq!(IdentityKey::from_bytes(&kp.public), Ok(identity));
        assert_eq!(
            IdentityKey::from_bytes(&[0x04; 33]),
            Err(CryptoError::InvalidKeyEncoding { expected: 33, actual: 33 })
        );
        assert_eq!(
            IdentityKey::from_bytes(&[0u8; 31]),
            Err(CryptoError::InvalidKeyEncoding { expected: 33, actual: 31 })
        );
        assert_eq!(IdentityKey::from(kp.clone()), identity);
        assert!(format!("{identity:?}").contains(&hex(&kp.public)));
    }

    #[test]
    fn identity_key_verifies_signed_pre_key() {
        let pair = KeyPair::generate();
        let identity = IdentityKey::from_pair(&pair);
        let spk = pair.create_signed_pre_key(7);
        let signature = spk.signature.expect("signed pre-key has a signature");
        assert!(identity.verify(&spk.public_serialized(), &signature));
        assert!(!identity.verify(b"other message", &signature));
    }

    #[test]
    fn pre_key_record_roundtrip() {
        let pre_key = PreKey::from_private(42, [0x11; 32]);
        let record = pre_key.to_record();
        let parsed = PreKey::from_record(&record).expect("roundtrip");
        assert_eq!(parsed, pre_key);

        // `PreKeyRecordStructure { id: 42, publicKey, privateKey }` — field 1 is a varint.
        assert_eq!(record[0], 0x08);
        assert_eq!(record[1], 42);
        assert!(record.windows(33).any(|w| w == pre_key.public_serialized()));

        assert!(matches!(PreKey::from_record(&[0x08, 0x01]), Err(CryptoError::InvalidPreKeyRecord(_))));
        assert!(matches!(PreKey::from_record(b"not a record"), Err(CryptoError::InvalidPreKeyRecord(_))));
    }

    #[test]
    fn pad_message_roundtrips() {
        for len in [0usize, 1, 15, 16, 17, 255] {
            let plaintext = vec![0xABu8; len];
            let padded = pad_message(&plaintext);
            let pad_len = usize::from(*padded.last().expect("padded is never empty"));
            assert!((1..=15).contains(&pad_len), "pad length {pad_len} out of range");
            assert_eq!(padded.len(), plaintext.len() + pad_len);
            assert!(padded[len..].iter().all(|b| usize::from(*b) == pad_len));
            assert_eq!(unpad_message(&padded, 2).expect("unpad"), plaintext);
        }
    }

    #[test]
    fn unpad_message_validation() {
        // version 3 messages are not padded at all.
        assert_eq!(unpad_message(b"unpadded", 3).expect("v3"), b"unpadded");
        // empty input is an error for older versions.
        assert_eq!(unpad_message(&[], 2), Err(CryptoError::BadPadding));
        // pad byte larger than the message.
        assert_eq!(unpad_message(&[0x01, 0x02, 0x09], 2), Err(CryptoError::BadPadding));
        // pad byte not repeated at the end.
        assert_eq!(unpad_message(&[0x01, 0x02, 0x02, 0x03], 2), Err(CryptoError::BadPadding));
        // a zero pad byte is valid and strips nothing (Go's `isValidPadding`).
        assert_eq!(unpad_message(&[0xAA, 0xBB, 0x00], 2).expect("pad 0"), [0xAA, 0xBB, 0x00]);
        // a well-formed pad is stripped.
        assert_eq!(unpad_message(&[0xAA, 0xBB, 0x03, 0x03, 0x03], 2).expect("pad 3"), [0xAA, 0xBB]);
    }
}
