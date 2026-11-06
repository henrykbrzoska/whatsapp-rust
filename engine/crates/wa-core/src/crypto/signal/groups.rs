// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Sender Keys (group messaging): per-sender chains distributed with a
//! [`SenderKeyDistributionMessage`] over pairwise sessions, then used for
//! [`SenderKeyMessage`]s (`<enc type="skmsg">`).
//!
//! ```text
//! SenderKeyDistributionMessage = version(0x33) || proto
//! SenderKeyMessage             = version(0x33) || proto || XEdDSA(signing_key, version || proto)(64)
//! ```

use std::collections::VecDeque;

use prost::Message as _;

use super::SignalError;
use super::address::SenderKeyName;
use super::ecc::{decode_point, decode_private, serialize_point, sign, verify_djb};
use super::kdf::{SenderChainKey, SenderMessageKey};
use super::proto::{
    SenderChainKeyStructure, SenderKeyDistributionMessageProto, SenderKeyMessageProto, SenderKeyRecordStructure,
    SenderKeyStateStructure, SenderMessageKeyStructure, SenderSigningKeyStructure,
};
use super::ratchet::{CURRENT_VERSION, MAX_FUTURE_MESSAGES, MAX_MESSAGE_KEYS};
use super::store::SignalStore;
use crate::crypto::cbc;
use crate::crypto::keys::KeyPair;

/// Sender-key states kept per record (libsignal `maxStates`).
pub const MAX_SENDER_KEY_STATES: usize = 5;
/// XEdDSA signature length appended to a [`SenderKeyMessage`].
pub const SIGNATURE_LENGTH: usize = 64;

fn version_byte() -> u8 {
    ((CURRENT_VERSION << 4) | CURRENT_VERSION) as u8
}

fn check_version(byte: u8) -> Result<(), SignalError> {
    match u32::from(byte >> 4) {
        CURRENT_VERSION => Ok(()),
        v => Err(SignalError::invalid(format!("unsupported sender-key message version {v}"))),
    }
}

/// One sender chain (`SenderKeyStateStructure`).
#[derive(Clone, PartialEq, Eq)]
pub struct SenderKeyState {
    pub key_id: u32,
    pub chain_key: SenderChainKey,
    pub signing_public: [u8; 32],
    /// Only present for our own sending state.
    pub signing_private: Option<[u8; 32]>,
    /// Skipped message keys, oldest first, capped at [`MAX_MESSAGE_KEYS`].
    pub message_keys: VecDeque<SenderMessageKey>,
}

impl std::fmt::Debug for SenderKeyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderKeyState")
            .field("key_id", &self.key_id)
            .field("iteration", &self.chain_key.iteration)
            .field("skipped", &self.message_keys.len())
            .finish_non_exhaustive()
    }
}

impl SenderKeyState {
    pub fn new(
        key_id: u32,
        chain_key: SenderChainKey,
        signing_public: [u8; 32],
        signing_private: Option<[u8; 32]>,
    ) -> Self {
        Self { key_id, chain_key, signing_public, signing_private, message_keys: VecDeque::new() }
    }

    fn add_message_key(&mut self, key: SenderMessageKey) {
        self.message_keys.push_back(key);
        while self.message_keys.len() > MAX_MESSAGE_KEYS {
            self.message_keys.pop_front();
        }
    }

    /// The key for `iteration`, advancing the chain and caching skipped
    /// keys (Go: `GroupCipher.getSenderKey`).
    pub fn sender_message_key(&mut self, iteration: u32) -> Result<SenderMessageKey, SignalError> {
        let current = self.chain_key.iteration;
        if current > iteration {
            return match self.message_keys.iter().position(|k| k.iteration == iteration) {
                Some(pos) => {
                    self.message_keys.remove(pos).ok_or(SignalError::OldCounter { current, received: iteration })
                }
                None => Err(SignalError::OldCounter { current, received: iteration }),
            };
        }
        if iteration - current > MAX_FUTURE_MESSAGES {
            return Err(SignalError::invalid(format!(
                "sender-key iteration {iteration} is too far ahead of {current}"
            )));
        }
        while self.chain_key.iteration < iteration {
            let skipped = self.chain_key.message_key()?;
            self.add_message_key(skipped);
            self.chain_key = self.chain_key.next();
        }
        let key = self.chain_key.message_key()?;
        self.chain_key = self.chain_key.next();
        Ok(key)
    }

    pub fn to_structure(&self) -> SenderKeyStateStructure {
        SenderKeyStateStructure {
            sender_key_id: Some(self.key_id),
            sender_chain_key: Some(SenderChainKeyStructure {
                iteration: Some(self.chain_key.iteration),
                seed: Some(self.chain_key.seed.to_vec()),
            }),
            sender_signing_key: Some(SenderSigningKeyStructure {
                public: Some(serialize_point(&self.signing_public).to_vec()),
                private: self.signing_private.map(|p| p.to_vec()),
            }),
            sender_message_keys: self
                .message_keys
                .iter()
                .map(|k| SenderMessageKeyStructure { iteration: Some(k.iteration), seed: Some(k.seed.to_vec()) })
                .collect(),
        }
    }

    pub fn from_structure(s: &SenderKeyStateStructure) -> Result<Self, SignalError> {
        let chain = s.sender_chain_key.as_ref().ok_or_else(|| SignalError::invalid("sender key without chain"))?;
        let seed = <[u8; 32]>::try_from(chain.seed.as_deref().unwrap_or_default())
            .map_err(|_| SignalError::invalid("sender chain seed is not 32 bytes"))?;
        let signing =
            s.sender_signing_key.as_ref().ok_or_else(|| SignalError::invalid("sender key without signing key"))?;
        let signing_public = super::ecc::decode_point_lenient(signing.public.as_deref().unwrap_or_default())?;
        let signing_private = match signing.private.as_deref() {
            Some(p) if !p.is_empty() => Some(decode_private(p)?),
            _ => None,
        };
        let mut message_keys = VecDeque::with_capacity(s.sender_message_keys.len());
        for k in &s.sender_message_keys {
            let seed = <[u8; 32]>::try_from(k.seed.as_deref().unwrap_or_default())
                .map_err(|_| SignalError::invalid("sender message key seed is not 32 bytes"))?;
            message_keys.push_back(SenderMessageKey::from_seed(k.iteration.unwrap_or_default(), seed)?);
        }
        Ok(Self {
            key_id: s.sender_key_id.unwrap_or_default(),
            chain_key: SenderChainKey::new(chain.iteration.unwrap_or_default(), seed),
            signing_public,
            signing_private,
            message_keys,
        })
    }
}

/// All sender-key states of one sender in one group, newest first
/// (`SenderKeyRecordStructure`, what `wa_sender_keys.sender_key` stores).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderKeyRecord {
    pub states: VecDeque<SenderKeyState>,
}

impl SenderKeyRecord {
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// The newest state (used for sending).
    pub fn state(&self) -> Option<&SenderKeyState> {
        self.states.front()
    }

    pub fn state_by_id(&self, key_id: u32) -> Option<&SenderKeyState> {
        self.states.iter().find(|s| s.key_id == key_id)
    }

    /// Adds a state in front, dropping the oldest beyond [`MAX_SENDER_KEY_STATES`].
    pub fn add_state(&mut self, state: SenderKeyState) {
        self.states.push_front(state);
        self.states.truncate(MAX_SENDER_KEY_STATES);
    }

    /// Replaces every state with `state` (our own fresh sending chain).
    pub fn set_state(&mut self, state: SenderKeyState) {
        self.states.clear();
        self.states.push_front(state);
    }

    pub fn serialize(&self) -> Vec<u8> {
        SenderKeyRecordStructure { sender_key_states: self.states.iter().map(SenderKeyState::to_structure).collect() }
            .encode_to_vec()
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, SignalError> {
        let record = SenderKeyRecordStructure::decode(bytes)
            .map_err(|e| SignalError::invalid(format!("sender key record decode failed: {e}")))?;
        let states =
            record.sender_key_states.iter().map(SenderKeyState::from_structure).collect::<Result<VecDeque<_>, _>>()?;
        Ok(Self { states })
    }
}

/// A group message (`<enc type="skmsg">`). Go: `protocol.SenderKeyMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderKeyMessage {
    pub key_id: u32,
    pub iteration: u32,
    pub ciphertext: Vec<u8>,
    /// `version || proto || signature`.
    pub serialized: Vec<u8>,
}

impl SenderKeyMessage {
    /// Builds and signs a message with the sender's signing key.
    pub fn new(key_id: u32, iteration: u32, ciphertext: Vec<u8>, signing_private: &[u8; 32]) -> Self {
        let proto = SenderKeyMessageProto {
            id: Some(key_id),
            iteration: Some(iteration),
            ciphertext: Some(ciphertext.clone()),
        };
        let mut serialized = Vec::with_capacity(1 + proto.encoded_len() + SIGNATURE_LENGTH);
        serialized.push(version_byte());
        proto.encode_raw(&mut serialized);
        let signature = sign(signing_private, &serialized);
        serialized.extend_from_slice(&signature);
        Self { key_id, iteration, ciphertext, serialized }
    }

    /// Parses network bytes (the signature is checked separately).
    pub fn deserialize(bytes: &[u8]) -> Result<Self, SignalError> {
        if bytes.len() <= 1 + SIGNATURE_LENGTH {
            return Err(SignalError::invalid("sender-key message too short"));
        }
        check_version(bytes[0])?;
        let body = &bytes[1..bytes.len() - SIGNATURE_LENGTH];
        let proto = SenderKeyMessageProto::decode(body)
            .map_err(|e| SignalError::invalid(format!("sender-key message decode failed: {e}")))?;
        let (Some(key_id), Some(iteration), Some(ciphertext)) = (proto.id, proto.iteration, proto.ciphertext) else {
            return Err(SignalError::invalid("incomplete sender-key message"));
        };
        Ok(Self { key_id, iteration, ciphertext, serialized: bytes.to_vec() })
    }

    /// Go: `SenderKeyMessage.VerifySignature`.
    pub fn verify_signature(&self, signing_public: &[u8; 32]) -> bool {
        let Some(split) = self.serialized.len().checked_sub(SIGNATURE_LENGTH) else {
            return false;
        };
        let (body, signature) = self.serialized.split_at(split);
        verify_djb(signing_public, body, signature)
    }
}

/// Hands a sender chain to group members (sent inside a pairwise message as
/// `Message.senderKeyDistributionMessage.axolotlSenderKeyDistributionMessage`).
/// Go: `protocol.SenderKeyDistributionMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderKeyDistributionMessage {
    pub key_id: u32,
    pub iteration: u32,
    pub chain_key: [u8; 32],
    pub signing_key: [u8; 32],
    pub serialized: Vec<u8>,
}

impl SenderKeyDistributionMessage {
    pub fn new(key_id: u32, iteration: u32, chain_key: [u8; 32], signing_key: [u8; 32]) -> Self {
        let proto = SenderKeyDistributionMessageProto {
            id: Some(key_id),
            iteration: Some(iteration),
            chain_key: Some(chain_key.to_vec()),
            signing_key: Some(serialize_point(&signing_key).to_vec()),
        };
        let mut serialized = Vec::with_capacity(1 + proto.encoded_len());
        serialized.push(version_byte());
        proto.encode_raw(&mut serialized);
        Self { key_id, iteration, chain_key, signing_key, serialized }
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, SignalError> {
        let (&first, body) = bytes.split_first().ok_or_else(|| SignalError::invalid("empty distribution message"))?;
        check_version(first)?;
        let proto = SenderKeyDistributionMessageProto::decode(body)
            .map_err(|e| SignalError::invalid(format!("distribution message decode failed: {e}")))?;
        let (Some(key_id), Some(iteration), Some(chain_key), Some(signing_key)) =
            (proto.id, proto.iteration, proto.chain_key, proto.signing_key)
        else {
            return Err(SignalError::invalid("incomplete distribution message"));
        };
        let chain_key = <[u8; 32]>::try_from(chain_key.as_slice())
            .map_err(|_| SignalError::invalid("distribution chain key is not 32 bytes"))?;
        Ok(Self { key_id, iteration, chain_key, signing_key: decode_point(&signing_key)?, serialized: bytes.to_vec() })
    }
}

async fn load_record<S: SignalStore + ?Sized>(store: &S, name: &SenderKeyName) -> Result<SenderKeyRecord, SignalError> {
    match store.load_sender_key(name).await? {
        Some(bytes) if !bytes.is_empty() => SenderKeyRecord::deserialize(&bytes),
        _ => Ok(SenderKeyRecord::default()),
    }
}

/// Creates/processes sender-key distribution messages.
/// Go: `groups.SessionBuilder`.
pub struct GroupSessionBuilder<'a, S: SignalStore + ?Sized> {
    store: &'a S,
}

impl<'a, S: SignalStore + ?Sized> GroupSessionBuilder<'a, S> {
    pub fn new(store: &'a S) -> Self {
        Self { store }
    }

    /// Stores a received distribution message as a new state.
    /// Go: `SessionBuilder.Process`.
    pub async fn process(
        &self,
        name: &SenderKeyName,
        message: &SenderKeyDistributionMessage,
    ) -> Result<(), SignalError> {
        let mut record = load_record(self.store, name).await?;
        record.add_state(SenderKeyState::new(
            message.key_id,
            SenderChainKey::new(message.iteration, message.chain_key),
            message.signing_key,
            None,
        ));
        self.store.store_sender_key(name, &record.serialize()).await?;
        Ok(())
    }

    /// Our own distribution message for `name` (creating the sending chain on
    /// first use). Go: `SessionBuilder.Create`.
    pub async fn create(&self, name: &SenderKeyName) -> Result<SenderKeyDistributionMessage, SignalError> {
        let mut record = load_record(self.store, name).await?;
        if record.is_empty() {
            let mut rng = rand::rngs::OsRng;
            let key_id = rand::RngCore::next_u32(&mut rng) & 0x7fff_ffff;
            let mut seed = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rng, &mut seed);
            let signing = KeyPair::generate();
            record.set_state(SenderKeyState::new(
                key_id,
                SenderChainKey::new(0, seed),
                signing.public,
                Some(signing.private),
            ));
            self.store.store_sender_key(name, &record.serialize()).await?;
        }
        let state = record.state().ok_or_else(|| SignalError::NoSenderKeyForUser(name.to_string()))?;
        Ok(SenderKeyDistributionMessage::new(
            state.key_id,
            state.chain_key.iteration,
            state.chain_key.seed,
            state.signing_public,
        ))
    }
}

/// Encrypts/decrypts group messages for one [`SenderKeyName`].
/// Go: `groups.GroupCipher`.
pub struct GroupCipher<'a, S: SignalStore + ?Sized> {
    store: &'a S,
    name: SenderKeyName,
}

impl<'a, S: SignalStore + ?Sized> GroupCipher<'a, S> {
    pub fn new(store: &'a S, name: SenderKeyName) -> Self {
        Self { store, name }
    }

    /// Encrypts with our sending chain. Go: `GroupCipher.Encrypt`.
    pub async fn encrypt(&self, plaintext: &[u8]) -> Result<SenderKeyMessage, SignalError> {
        let mut record = load_record(self.store, &self.name).await?;
        let state = record.states.front_mut().ok_or_else(|| SignalError::NoSenderKeyForUser(self.name.to_string()))?;
        let signing_private = state
            .signing_private
            .ok_or_else(|| SignalError::InvalidKey("sender key has no signing private key".into()))?;
        let key = state.chain_key.message_key()?;
        let ciphertext = cbc::encrypt_with_iv(&key.cipher_key, &key.iv, plaintext)?;
        let message = SenderKeyMessage::new(state.key_id, key.iteration, ciphertext, &signing_private);
        state.chain_key = state.chain_key.next();
        self.store.store_sender_key(&self.name, &record.serialize()).await?;
        Ok(message)
    }

    /// Decrypts `<enc type="skmsg">` bytes. Missing state →
    /// [`SignalError::NoSenderKeyForUser`]; bad signature → [`SignalError::BadMac`].
    /// Go: `GroupCipher.Decrypt`.
    pub async fn decrypt(&self, bytes: &[u8]) -> Result<Vec<u8>, SignalError> {
        let message = SenderKeyMessage::deserialize(bytes)?;
        self.decrypt_message(&message).await
    }

    /// Like [`Self::decrypt`] for an already parsed message.
    pub async fn decrypt_message(&self, message: &SenderKeyMessage) -> Result<Vec<u8>, SignalError> {
        let no_key = || SignalError::NoSenderKeyForUser(self.name.to_string());
        let mut record = load_record(self.store, &self.name).await?;
        let state = record.states.iter_mut().find(|s| s.key_id == message.key_id).ok_or_else(no_key)?;
        if !message.verify_signature(&state.signing_public) {
            return Err(SignalError::BadMac);
        }
        let key = state.sender_message_key(message.iteration)?;
        let plaintext = cbc::decrypt(&key.cipher_key, &key.iv, &message.ciphertext)
            .map_err(|e| SignalError::invalid(format!("group message body decryption failed: {e}")))?;
        self.store.store_sender_key(&self.name, &record.serialize()).await?;
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::signal::address::SignalAddress;
    use crate::crypto::signal::store::memory::MemoryStore;

    fn name() -> SenderKeyName {
        SenderKeyName::new("120363000000000000@g.us", SignalAddress::new("4911", 0))
    }

    // Generated with libsignal (Go) v0.2.2 as a test oracle only:
    // `GroupSessionBuilder.Create` for ("120363@g.us", "4911:0"), then
    // `GroupCipher.Encrypt` of "group one" / "group two" (`SignedSerialize`).
    const GO_SKDM: &str = "3308a699abd00d10001a20b0674d94783f12dbe0d0dc19a58a3ad4f2d2dcdf8173b948657fd7dfc6a4e9042221\
                           0557aaf7bc44eb64ab43cdd371f86fdfd78b624f6eae1de14e09031814f121e31d";
    const GO_SKMSG1: &str = "3308a699abd00d10001a105256e8d84b8306f41aafe9874b75d366c0428d950a2cade0d203199ed207105c31fd\
                             7ea2cb636c769e29680d3da25e1c36c3aba863cd5a8789d435ba26e9f81716979df4decf8127a7440a26a5feba8b";
    const GO_SKMSG2: &str = "3308a699abd00d10011a102fceea118228136c68b889160904a86371ee11b80d446ed5be1aa7df0735d23d034a\
                             ef8a2575a1f5bb91db9db69b608d5582c345eb7e97611752a605c6cbd1b551b8519d9d44d26c0714a10400169c83";

    fn unhex(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        hex::decode(clean).unwrap()
    }

    #[tokio::test]
    async fn decrypts_go_libsignal_group_messages() {
        let bob = MemoryStore::new(2);
        let go_name = SenderKeyName::new("120363@g.us", SignalAddress::new("4911", 0));
        let skdm = SenderKeyDistributionMessage::deserialize(&unhex(GO_SKDM)).unwrap();
        assert_eq!(skdm.iteration, 0);
        // Our re-encoding is byte-identical to Go's.
        assert_eq!(
            SenderKeyDistributionMessage::new(skdm.key_id, skdm.iteration, skdm.chain_key, skdm.signing_key).serialized,
            unhex(GO_SKDM)
        );
        GroupSessionBuilder::new(&bob).process(&go_name, &skdm).await.unwrap();
        let cipher = GroupCipher::new(&bob, go_name.clone());
        // Out of order on purpose.
        assert_eq!(cipher.decrypt(&unhex(GO_SKMSG2)).await.unwrap(), b"group two");
        assert_eq!(cipher.decrypt(&unhex(GO_SKMSG1)).await.unwrap(), b"group one");
        let mut tampered = unhex(GO_SKMSG1);
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(matches!(cipher.decrypt(&tampered).await, Err(SignalError::BadMac)));
    }

    #[tokio::test]
    async fn group_encrypt_decrypt_round_trip() {
        let alice = MemoryStore::new(1);
        let bob = MemoryStore::new(2);
        let skdm = GroupSessionBuilder::new(&alice).create(&name()).await.unwrap();
        // Creating again returns the same chain.
        assert_eq!(GroupSessionBuilder::new(&alice).create(&name()).await.unwrap().key_id, skdm.key_id);

        let received = SenderKeyDistributionMessage::deserialize(&skdm.serialized).unwrap();
        assert_eq!(received, skdm);
        GroupSessionBuilder::new(&bob).process(&name(), &received).await.unwrap();

        let alice_cipher = GroupCipher::new(&alice, name());
        let bob_cipher = GroupCipher::new(&bob, name());
        for i in 0..5 {
            let text = format!("group message {i}");
            let msg = alice_cipher.encrypt(text.as_bytes()).await.unwrap();
            assert_eq!(msg.iteration, i);
            assert_eq!(bob_cipher.decrypt(&msg.serialized).await.unwrap(), text.as_bytes());
        }
        // Bob cannot send on Alice's chain.
        assert!(matches!(bob_cipher.encrypt(b"x").await, Err(SignalError::InvalidKey(_))));
    }

    #[tokio::test]
    async fn group_out_of_order_and_replay() {
        let alice = MemoryStore::new(1);
        let bob = MemoryStore::new(2);
        let skdm = GroupSessionBuilder::new(&alice).create(&name()).await.unwrap();
        GroupSessionBuilder::new(&bob).process(&name(), &skdm).await.unwrap();
        let alice_cipher = GroupCipher::new(&alice, name());
        let bob_cipher = GroupCipher::new(&bob, name());
        let msgs: Vec<_> = {
            let mut v = Vec::new();
            for i in 0..4 {
                v.push(alice_cipher.encrypt(format!("m{i}").as_bytes()).await.unwrap());
            }
            v
        };
        assert_eq!(bob_cipher.decrypt(&msgs[3].serialized).await.unwrap(), b"m3");
        assert_eq!(bob_cipher.decrypt(&msgs[1].serialized).await.unwrap(), b"m1");
        assert_eq!(bob_cipher.decrypt(&msgs[0].serialized).await.unwrap(), b"m0");
        assert!(bob_cipher.decrypt(&msgs[1].serialized).await.unwrap_err().is_old_counter());
        assert_eq!(bob_cipher.decrypt(&msgs[2].serialized).await.unwrap(), b"m2");
    }

    #[tokio::test]
    async fn group_failures() {
        let alice = MemoryStore::new(1);
        let bob = MemoryStore::new(2);
        let skdm = GroupSessionBuilder::new(&alice).create(&name()).await.unwrap();
        let msg = GroupCipher::new(&alice, name()).encrypt(b"secret").await.unwrap();

        // Unknown sender key.
        let err = GroupCipher::new(&bob, name()).decrypt(&msg.serialized).await.unwrap_err();
        assert!(err.is_no_sender_key_for_user(), "{err:?}");

        GroupSessionBuilder::new(&bob).process(&name(), &skdm).await.unwrap();
        // Tampered signature.
        let mut tampered = msg.serialized.clone();
        let last = tampered.len() - 1;
        tampered[last - 10] ^= 0x01;
        assert!(matches!(GroupCipher::new(&bob, name()).decrypt(&tampered).await, Err(SignalError::BadMac)));
        // Tampered body (signature covers it).
        let mut body = msg.serialized.clone();
        body[5] ^= 0x01;
        assert!(GroupCipher::new(&bob, name()).decrypt(&body).await.is_err());
        // Garbage never panics.
        for junk in [&[][..], &[0x33][..], &[0x13; 80][..], &[0x33; 70][..]] {
            assert!(GroupCipher::new(&bob, name()).decrypt(junk).await.is_err());
            assert!(SenderKeyDistributionMessage::deserialize(junk).is_err());
        }
        // The failed attempts did not consume the key.
        assert_eq!(GroupCipher::new(&bob, name()).decrypt(&msg.serialized).await.unwrap(), b"secret");
    }

    #[test]
    fn record_round_trip_and_cap() {
        let mut record = SenderKeyRecord::default();
        for id in 0..7 {
            let kp = KeyPair::generate();
            record.add_state(SenderKeyState::new(id, SenderChainKey::new(id, [id as u8; 32]), kp.public, None));
        }
        assert_eq!(record.states.len(), MAX_SENDER_KEY_STATES);
        assert_eq!(record.state().unwrap().key_id, 6);
        assert!(record.state_by_id(0).is_none());
        record.states[0].sender_message_key(9).unwrap();
        assert_eq!(record.states[0].message_keys.len(), 3);
        let back = SenderKeyRecord::deserialize(&record.serialize()).unwrap();
        assert_eq!(back, record);
    }
}
