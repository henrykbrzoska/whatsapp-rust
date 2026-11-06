// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The Double Ratchet: symmetric chains with a skipped-message-key store
//! ([`Chain`]), the DH ratchet step ([`SessionState::ratchet_receiver_chain`])
//! and the X3DH session initialisation for both roles
//! ([`calculate_sender_session`] = Alice, [`calculate_receiver_session`] = Bob).
//!
//! X3DH master secret (Signal X3DH spec §3.3, with the libsignal "discontinuity"
//! prefix of 32 `0xFF` bytes):
//!
//! ```text
//! F || DH(IK_A, SPK_B) || DH(EK_A, IK_B) || DH(EK_A, SPK_B) [|| DH(EK_A, OPK_B)]
//! ```

use std::collections::VecDeque;

use super::SignalError;
use super::ecc::{calculate_agreement, decode_point_lenient, decode_private, serialize_point};
use super::kdf::{ChainKey, MessageKeys, RootKey, root_and_chain_keys};
use super::proto::{ChainKeyStructure, ChainStructure, MessageKeyStructure};
use super::session::SessionState;
use crate::crypto::keys::KeyPair;

/// The only session/message version WhatsApp speaks (libsignal `CurrentVersion`).
pub const CURRENT_VERSION: u32 = 3;
/// Skipped message keys kept per chain (libsignal `maxMessageKeys`).
pub const MAX_MESSAGE_KEYS: usize = 2000;
/// How far ahead of the chain a counter may jump (libsignal: 2000).
pub const MAX_FUTURE_MESSAGES: u32 = 2000;
/// Receiver chains kept per session (libsignal `maxReceiverChains`).
pub const MAX_RECEIVER_CHAINS: usize = 5;

/// `32 × 0xFF` — prepended to the X3DH DH outputs (Go: `ratchet.genDiscontinuity`).
pub fn gen_discontinuity() -> [u8; 32] {
    [0xFF; 32]
}

/// Whether we are "Alice" in a symmetric (key-exchange) initialisation:
/// the side with the lexicographically smaller base key sends first.
pub fn is_sender(our_key: &[u8; 32], their_key: &[u8; 32]) -> bool {
    serialize_point(our_key) < serialize_point(their_key)
}

/// One sending or receiving chain of the Double Ratchet
/// (`SessionStructure.Chain`).
#[derive(Clone, PartialEq, Eq)]
pub struct Chain {
    /// The ratchet public key of this chain (ours for the sender chain,
    /// theirs for receiver chains).
    pub ratchet_key: [u8; 32],
    /// Our ratchet private key — sender chain only.
    pub ratchet_private: Option<[u8; 32]>,
    pub chain_key: ChainKey,
    /// Skipped message keys (oldest first), capped at [`MAX_MESSAGE_KEYS`].
    pub message_keys: VecDeque<MessageKeys>,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("ratchet_key", &hex::encode(self.ratchet_key))
            .field("index", &self.chain_key.index)
            .field("skipped", &self.message_keys.len())
            .finish()
    }
}

impl Chain {
    /// A sender chain owned by `ratchet`.
    pub fn sender(ratchet: &KeyPair, chain_key: ChainKey) -> Self {
        Self {
            ratchet_key: ratchet.public,
            ratchet_private: Some(ratchet.private),
            chain_key,
            message_keys: VecDeque::new(),
        }
    }

    /// A receiver chain for the remote ratchet key `their_ratchet`.
    pub fn receiver(their_ratchet: [u8; 32], chain_key: ChainKey) -> Self {
        Self { ratchet_key: their_ratchet, ratchet_private: None, chain_key, message_keys: VecDeque::new() }
    }

    /// Our ratchet key pair (sender chain only).
    pub fn ratchet_key_pair(&self) -> Option<KeyPair> {
        self.ratchet_private.map(|private| KeyPair { public: self.ratchet_key, private })
    }

    /// Symmetric ratchet step: advance the chain key by one.
    pub fn next_chain_key(&mut self) {
        self.chain_key = self.chain_key.next();
    }

    /// Sending side: the keys for the current index, then advance.
    pub fn next_message_keys(&mut self) -> Result<MessageKeys, SignalError> {
        let keys = self.chain_key.message_keys()?;
        self.next_chain_key();
        Ok(keys)
    }

    fn store_skipped(&mut self, keys: MessageKeys) {
        self.message_keys.push_back(keys);
        while self.message_keys.len() > MAX_MESSAGE_KEYS {
            self.message_keys.pop_front();
        }
    }

    /// Receiving side: the keys for `counter`, caching every skipped key on
    /// the way (out-of-order delivery) and consuming a cached key when the
    /// counter is behind the chain.
    ///
    /// Behind the chain with no cached key → [`SignalError::DuplicateMessage`];
    /// more than [`MAX_FUTURE_MESSAGES`] ahead → [`SignalError::InvalidMessage`].
    pub fn message_keys_for(&mut self, counter: u32) -> Result<MessageKeys, SignalError> {
        let index = self.chain_key.index;
        if index > counter {
            return match self.message_keys.iter().position(|k| k.index == counter) {
                Some(pos) => self.message_keys.remove(pos).ok_or(SignalError::DuplicateMessage(counter)),
                None => Err(SignalError::DuplicateMessage(counter)),
            };
        }
        if counter - index > MAX_FUTURE_MESSAGES {
            return Err(SignalError::invalid(format!("message counter {counter} is too far ahead of {index}")));
        }
        while self.chain_key.index < counter {
            let skipped = self.chain_key.message_keys()?;
            self.store_skipped(skipped);
            self.next_chain_key();
        }
        self.next_message_keys()
    }

    pub(crate) fn to_structure(&self) -> ChainStructure {
        ChainStructure {
            sender_ratchet_key: Some(serialize_point(&self.ratchet_key).to_vec()),
            sender_ratchet_key_private: self.ratchet_private.map(|p| p.to_vec()),
            chain_key: Some(ChainKeyStructure {
                index: Some(self.chain_key.index),
                key: Some(self.chain_key.key.to_vec()),
            }),
            message_keys: self
                .message_keys
                .iter()
                .map(|k| MessageKeyStructure {
                    index: Some(k.index),
                    cipher_key: Some(k.cipher_key.to_vec()),
                    mac_key: Some(k.mac_key.to_vec()),
                    iv: Some(k.iv.to_vec()),
                })
                .collect(),
        }
    }

    pub(crate) fn from_structure(s: &ChainStructure) -> Result<Self, SignalError> {
        let ratchet_key = decode_point_lenient(s.sender_ratchet_key.as_deref().unwrap_or_default())?;
        let ratchet_private = match s.sender_ratchet_key_private.as_deref() {
            Some(p) if !p.is_empty() => Some(decode_private(p)?),
            _ => None,
        };
        let ck = s.chain_key.as_ref().ok_or_else(|| SignalError::invalid("chain without chain key"))?;
        let key = <[u8; 32]>::try_from(ck.key.as_deref().unwrap_or_default())
            .map_err(|_| SignalError::invalid("chain key is not 32 bytes"))?;
        let mut message_keys = VecDeque::with_capacity(s.message_keys.len());
        for mk in &s.message_keys {
            let cipher_key = <[u8; 32]>::try_from(mk.cipher_key.as_deref().unwrap_or_default());
            let mac_key = <[u8; 32]>::try_from(mk.mac_key.as_deref().unwrap_or_default());
            let iv = <[u8; 16]>::try_from(mk.iv.as_deref().unwrap_or_default());
            match (cipher_key, mac_key, iv) {
                (Ok(cipher_key), Ok(mac_key), Ok(iv)) => {
                    message_keys.push_back(MessageKeys { cipher_key, mac_key, iv, index: mk.index.unwrap_or_default() })
                }
                _ => return Err(SignalError::invalid("malformed stored message key")),
            }
        }
        Ok(Self {
            ratchet_key,
            ratchet_private,
            chain_key: ChainKey::new(key, ck.index.unwrap_or_default()),
            message_keys,
        })
    }
}

impl SessionState {
    /// The receiver chain for `their_ratchet`, if we have one.
    pub fn receiver_chain_mut(&mut self, their_ratchet: &[u8; 32]) -> Option<&mut Chain> {
        self.receiver_chains.iter_mut().find(|c| &c.ratchet_key == their_ratchet)
    }

    /// Appends a receiver chain, dropping the oldest beyond [`MAX_RECEIVER_CHAINS`].
    pub fn add_receiver_chain(&mut self, chain: Chain) {
        self.receiver_chains.push(chain);
        while self.receiver_chains.len() > MAX_RECEIVER_CHAINS {
            self.receiver_chains.remove(0);
        }
    }

    /// The DH ratchet step on receipt of a new remote ratchet key: derive the
    /// receiver chain from `DH(their, our_current)`, then a fresh sending
    /// chain from `DH(their, our_new)`. No-op when the chain already exists.
    pub fn ratchet_receiver_chain(&mut self, their_ratchet: &[u8; 32]) -> Result<(), SignalError> {
        if self.receiver_chains.iter().any(|c| &c.ratchet_key == their_ratchet) {
            return Ok(());
        }
        let sender = self.sender_chain.as_ref().ok_or_else(|| SignalError::NoSession("no sender chain".into()))?;
        let our_ratchet = sender
            .ratchet_key_pair()
            .ok_or_else(|| SignalError::InvalidKey("sender chain without private key".into()))?;
        let sender_index = sender.chain_key.index;

        let (root, receiver_chain_key) = self.root_key.create_chain(their_ratchet, &our_ratchet)?;
        let new_ratchet = KeyPair::generate();
        let (root, sender_chain_key) = root.create_chain(their_ratchet, &new_ratchet)?;

        self.root_key = root;
        self.add_receiver_chain(Chain::receiver(*their_ratchet, receiver_chain_key));
        self.previous_counter = sender_index.saturating_sub(1);
        self.sender_chain = Some(Chain::sender(&new_ratchet, sender_chain_key));
        Ok(())
    }
}

/// Alice's X3DH inputs (Go: `ratchet.SenderParameters`).
#[derive(Debug, Clone)]
pub struct AliceParameters {
    pub our_identity: KeyPair,
    pub our_base_key: KeyPair,
    pub their_identity: [u8; 32],
    pub their_signed_pre_key: [u8; 32],
    pub their_one_time_pre_key: Option<[u8; 32]>,
    /// Bob's first ratchet key — his signed pre-key.
    pub their_ratchet_key: [u8; 32],
}

/// Bob's X3DH inputs (Go: `ratchet.ReceiverParameters`).
#[derive(Debug, Clone)]
pub struct BobParameters {
    pub our_identity: KeyPair,
    pub our_signed_pre_key: KeyPair,
    pub our_one_time_pre_key: Option<KeyPair>,
    /// Our first ratchet key — the signed pre-key.
    pub our_ratchet_key: KeyPair,
    pub their_identity: [u8; 32],
    pub their_base_key: [u8; 32],
}

fn new_state(local_identity: [u8; 32], remote_identity: [u8; 32], root_key: RootKey) -> SessionState {
    SessionState {
        version: CURRENT_VERSION,
        local_identity,
        remote_identity,
        root_key,
        previous_counter: 0,
        sender_chain: None,
        receiver_chains: Vec::new(),
        pending_pre_key: None,
        remote_registration_id: 0,
        local_registration_id: 0,
        alice_base_key: None,
    }
}

/// X3DH as the initiator (Alice) → a session that can send immediately.
///
/// Go: `ratchet.CalculateSenderSession`.
pub fn calculate_sender_session(p: &AliceParameters) -> Result<SessionState, SignalError> {
    let mut secrets = Vec::with_capacity(32 * 5);
    secrets.extend_from_slice(&gen_discontinuity());
    secrets.extend_from_slice(&calculate_agreement(&p.their_signed_pre_key, &p.our_identity.private));
    secrets.extend_from_slice(&calculate_agreement(&p.their_identity, &p.our_base_key.private));
    secrets.extend_from_slice(&calculate_agreement(&p.their_signed_pre_key, &p.our_base_key.private));
    if let Some(otpk) = &p.their_one_time_pre_key {
        secrets.extend_from_slice(&calculate_agreement(otpk, &p.our_base_key.private));
    }
    let (root, chain) = root_and_chain_keys(&secrets)?;

    let sending_ratchet = KeyPair::generate();
    let (root, sending_chain) = root.create_chain(&p.their_ratchet_key, &sending_ratchet)?;

    let mut state = new_state(p.our_identity.public, p.their_identity, root);
    state.add_receiver_chain(Chain::receiver(p.their_ratchet_key, chain));
    state.sender_chain = Some(Chain::sender(&sending_ratchet, sending_chain));
    Ok(state)
}

/// X3DH as the responder (Bob), on receipt of Alice's first message.
///
/// Go: `ratchet.CalculateReceiverSession`.
pub fn calculate_receiver_session(p: &BobParameters) -> Result<SessionState, SignalError> {
    let mut secrets = Vec::with_capacity(32 * 5);
    secrets.extend_from_slice(&gen_discontinuity());
    secrets.extend_from_slice(&calculate_agreement(&p.their_identity, &p.our_signed_pre_key.private));
    secrets.extend_from_slice(&calculate_agreement(&p.their_base_key, &p.our_identity.private));
    secrets.extend_from_slice(&calculate_agreement(&p.their_base_key, &p.our_signed_pre_key.private));
    if let Some(otpk) = &p.our_one_time_pre_key {
        secrets.extend_from_slice(&calculate_agreement(&p.their_base_key, &otpk.private));
    }
    let (root, chain) = root_and_chain_keys(&secrets)?;

    let mut state = new_state(p.our_identity.public, p.their_identity, root);
    state.sender_chain = Some(Chain::sender(&p.our_ratchet_key, chain));
    Ok(state)
}

/// Symmetric initialisation (legacy key exchange): the side whose base key
/// sorts first plays Alice. Go: `ratchet.CalculateSymmetricSession`.
#[allow(clippy::too_many_arguments)]
pub fn calculate_symmetric_session(
    our_identity: &KeyPair,
    our_base_key: &KeyPair,
    our_ratchet_key: &KeyPair,
    their_identity: [u8; 32],
    their_base_key: [u8; 32],
    their_ratchet_key: [u8; 32],
) -> Result<SessionState, SignalError> {
    if is_sender(&our_base_key.public, &their_base_key) {
        calculate_sender_session(&AliceParameters {
            our_identity: our_identity.clone(),
            our_base_key: our_base_key.clone(),
            their_identity,
            their_signed_pre_key: their_base_key,
            their_one_time_pre_key: None,
            their_ratchet_key,
        })
    } else {
        calculate_receiver_session(&BobParameters {
            our_identity: our_identity.clone(),
            our_signed_pre_key: our_base_key.clone(),
            our_one_time_pre_key: None,
            our_ratchet_key: our_ratchet_key.clone(),
            their_identity,
            their_base_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> Chain {
        Chain::receiver([1u8; 32], ChainKey::new([2u8; 32], 0))
    }

    #[test]
    fn out_of_order_keys_are_cached_and_consumed_once() {
        let mut sender = chain();
        let expected: Vec<MessageKeys> = (0..6).map(|_| sender.next_message_keys().unwrap()).collect();

        let mut receiver = chain();
        assert_eq!(receiver.message_keys_for(5).unwrap(), expected[5]);
        assert_eq!(receiver.message_keys.len(), 5);
        assert_eq!(receiver.message_keys_for(3).unwrap(), expected[3]);
        assert_eq!(receiver.message_keys_for(0).unwrap(), expected[0]);
        assert_eq!(receiver.message_keys.len(), 3);
        assert!(matches!(receiver.message_keys_for(3), Err(SignalError::DuplicateMessage(3))));
        assert!(matches!(receiver.message_keys_for(5), Err(SignalError::DuplicateMessage(5))));
    }

    #[test]
    fn skipped_key_store_is_capped() {
        let mut receiver = chain();
        receiver.message_keys_for(MAX_FUTURE_MESSAGES).unwrap();
        assert_eq!(receiver.message_keys.len(), MAX_MESSAGE_KEYS);
        let mut far = receiver.clone();
        let next = far.chain_key.index;
        assert!(matches!(far.message_keys_for(next + MAX_FUTURE_MESSAGES + 1), Err(SignalError::InvalidMessage(_))));
        receiver.message_keys_for(next + 10).unwrap();
        assert_eq!(receiver.message_keys.len(), MAX_MESSAGE_KEYS, "oldest keys are evicted");
        assert!(receiver.message_keys_for(0).is_err(), "evicted key is gone");
    }

    #[test]
    fn chain_structure_round_trip() {
        let kp = KeyPair::generate();
        let mut c = Chain::sender(&kp, ChainKey::new([4u8; 32], 7));
        c.store_skipped(c.chain_key.message_keys().unwrap());
        let back = Chain::from_structure(&c.to_structure()).unwrap();
        assert_eq!(back, c);
        assert!(Chain::from_structure(&ChainStructure::default()).is_err());
    }

    #[test]
    fn x3dh_both_roles_agree() {
        let alice_identity = KeyPair::generate();
        let alice_base = KeyPair::generate();
        let bob_identity = KeyPair::generate();
        let bob_spk = KeyPair::generate();
        let bob_otpk = KeyPair::generate();

        let alice = calculate_sender_session(&AliceParameters {
            our_identity: alice_identity.clone(),
            our_base_key: alice_base.clone(),
            their_identity: bob_identity.public,
            their_signed_pre_key: bob_spk.public,
            their_one_time_pre_key: Some(bob_otpk.public),
            their_ratchet_key: bob_spk.public,
        })
        .unwrap();
        let bob = calculate_receiver_session(&BobParameters {
            our_identity: bob_identity.clone(),
            our_signed_pre_key: bob_spk.clone(),
            our_one_time_pre_key: Some(bob_otpk),
            our_ratchet_key: bob_spk.clone(),
            their_identity: alice_identity.public,
            their_base_key: alice_base.public,
        })
        .unwrap();
        // Alice's receiver chain for Bob's ratchet key == Bob's sender chain.
        let bob_sender = bob.sender_chain.as_ref().unwrap();
        assert_eq!(alice.receiver_chains[0].chain_key, bob_sender.chain_key);
        assert_eq!(alice.version, CURRENT_VERSION);
        assert!(gen_discontinuity().iter().all(|b| *b == 0xFF));
    }

    #[test]
    fn symmetric_session_roles() {
        let a = KeyPair::generate();
        let b = KeyPair::generate();
        assert_ne!(is_sender(&a.public, &b.public), is_sender(&b.public, &a.public));
        let ida = KeyPair::generate();
        let idb = KeyPair::generate();
        let sa = calculate_symmetric_session(&ida, &a, &a, idb.public, b.public, b.public).unwrap();
        let sb = calculate_symmetric_session(&idb, &b, &b, ida.public, a.public, a.public).unwrap();
        // Exactly one side has a receiver chain from initialisation (Alice).
        assert_ne!(sa.receiver_chains.is_empty(), sb.receiver_chains.is_empty());
    }
}
