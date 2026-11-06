// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Pairwise sessions: the persisted [`SessionRecord`] / [`SessionState`],
//! the wire messages ([`SignalMessage`], [`PreKeySignalMessage`]), session
//! setup ([`SessionBuilder`], X3DH) and [`SessionCipher`] (Double Ratchet
//! encryption/decryption).
//!
//! Wire framing (WhatsApp `<enc type="msg|pkmsg">` payloads):
//!
//! ```text
//! SignalMessage       = version(0x33) || proto || HMAC-SHA256(mac_key, IK_sender(33) || IK_receiver(33) || version || proto)[..8]
//! PreKeySignalMessage = version(0x33) || proto      (proto.message = a full SignalMessage)
//! ```

use std::collections::VecDeque;

use prost::Message as _;

use super::SignalError;
use super::address::SignalAddress;
use super::ecc::{decode_point, decode_point_lenient, serialize_point, verify_djb};
use super::kdf::{RootKey, hmac_sha256};
use super::proto::{
    PendingPreKeyStructure, PreKeySignalMessageProto, RecordStructure, SessionStructure, SignalMessageProto,
};
use super::ratchet::{
    AliceParameters, BobParameters, CURRENT_VERSION, Chain, calculate_receiver_session, calculate_sender_session,
};
use super::store::SignalStore;
use crate::crypto::cbc;
use crate::crypto::keys::KeyPair;

pub use super::store::PreKeyBundle;

/// Archived session states kept per record (libsignal `archivedStatesMaxLength`).
pub const MAX_PREVIOUS_SESSIONS: usize = 40;
/// Truncated MAC length of a [`SignalMessage`].
pub const MAC_LENGTH: usize = 8;

fn version_byte(version: u32) -> u8 {
    // Versions are 4-bit; `CURRENT_VERSION` is 3 → 0x33.
    (((version & 0x0f) << 4) | CURRENT_VERSION) as u8
}

fn check_version(byte: u8) -> Result<u32, SignalError> {
    let version = u32::from(byte >> 4);
    if version < CURRENT_VERSION {
        return Err(SignalError::invalid(format!("legacy message version {version}")));
    }
    if version > CURRENT_VERSION {
        return Err(SignalError::invalid(format!("unknown message version {version}")));
    }
    Ok(version)
}

fn key32(bytes: Option<&[u8]>, what: &str) -> Result<[u8; 32], SignalError> {
    <[u8; 32]>::try_from(bytes.unwrap_or_default()).map_err(|_| SignalError::invalid(format!("{what} is not 32 bytes")))
}

// ---------------------------------------------------------------------------
// Session state / record
// ---------------------------------------------------------------------------

/// `PendingPreKey` — what Alice keeps until Bob answers, so every message she
/// sends meanwhile is a [`PreKeySignalMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPreKey {
    pub pre_key_id: Option<u32>,
    pub signed_pre_key_id: u32,
    pub base_key: [u8; 32],
}

/// One Double Ratchet session (`SessionStructure`).
#[derive(Clone, PartialEq, Eq)]
pub struct SessionState {
    pub version: u32,
    pub local_identity: [u8; 32],
    pub remote_identity: [u8; 32],
    pub root_key: RootKey,
    pub previous_counter: u32,
    pub sender_chain: Option<Chain>,
    pub receiver_chains: Vec<Chain>,
    pub pending_pre_key: Option<PendingPreKey>,
    pub remote_registration_id: u32,
    pub local_registration_id: u32,
    pub alice_base_key: Option<[u8; 32]>,
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("version", &self.version)
            .field("remote_identity", &hex::encode(self.remote_identity))
            .field("previous_counter", &self.previous_counter)
            .field("sender_chain", &self.sender_chain)
            .field("receiver_chains", &self.receiver_chains)
            .field("pending_pre_key", &self.pending_pre_key)
            .finish_non_exhaustive()
    }
}

impl SessionState {
    /// Go: `SessionState.Serialize` (one `SessionStructure`).
    pub fn to_structure(&self) -> SessionStructure {
        SessionStructure {
            session_version: Some(self.version),
            local_identity_public: Some(serialize_point(&self.local_identity).to_vec()),
            remote_identity_public: Some(serialize_point(&self.remote_identity).to_vec()),
            root_key: Some(self.root_key.0.to_vec()),
            previous_counter: Some(self.previous_counter),
            sender_chain: self.sender_chain.as_ref().map(Chain::to_structure),
            receiver_chains: self.receiver_chains.iter().map(Chain::to_structure).collect(),
            pending_key_exchange: None,
            pending_pre_key: self.pending_pre_key.as_ref().map(|p| PendingPreKeyStructure {
                pre_key_id: p.pre_key_id,
                signed_pre_key_id: Some(p.signed_pre_key_id as i32),
                base_key: Some(serialize_point(&p.base_key).to_vec()),
            }),
            remote_registration_id: Some(self.remote_registration_id),
            local_registration_id: Some(self.local_registration_id),
            needs_refresh: None,
            alice_base_key: self.alice_base_key.map(|k| serialize_point(&k).to_vec()),
        }
    }

    /// Parses a `SessionStructure`; `Ok(None)` for an empty (fresh) structure.
    pub fn from_structure(s: &SessionStructure) -> Result<Option<Self>, SignalError> {
        let Some(root) = s.root_key.as_deref().filter(|r| !r.is_empty()) else {
            return Ok(None);
        };
        let root_key = RootKey(key32(Some(root), "root key")?);
        let sender_chain = s.sender_chain.as_ref().map(Chain::from_structure).transpose()?;
        let receiver_chains = s.receiver_chains.iter().map(Chain::from_structure).collect::<Result<Vec<_>, _>>()?;
        let pending_pre_key = match &s.pending_pre_key {
            Some(p) => Some(PendingPreKey {
                pre_key_id: p.pre_key_id,
                signed_pre_key_id: p.signed_pre_key_id.unwrap_or_default() as u32,
                base_key: decode_point_lenient(p.base_key.as_deref().unwrap_or_default())?,
            }),
            None => None,
        };
        let alice_base_key = match s.alice_base_key.as_deref() {
            Some(k) if !k.is_empty() => Some(decode_point_lenient(k)?),
            _ => None,
        };
        Ok(Some(Self {
            version: s.session_version.unwrap_or(CURRENT_VERSION),
            local_identity: decode_point_lenient(s.local_identity_public.as_deref().unwrap_or_default())?,
            remote_identity: decode_point_lenient(s.remote_identity_public.as_deref().unwrap_or_default())?,
            root_key,
            previous_counter: s.previous_counter.unwrap_or_default(),
            sender_chain,
            receiver_chains,
            pending_pre_key,
            remote_registration_id: s.remote_registration_id.unwrap_or_default(),
            local_registration_id: s.local_registration_id.unwrap_or_default(),
            alice_base_key,
        }))
    }

    /// The counter the next outgoing message will carry.
    pub fn sender_counter(&self) -> Option<u32> {
        self.sender_chain.as_ref().map(|c| c.chain_key.index)
    }
}

/// All sessions with one remote address: the current one plus archived ones
/// (`RecordStructure`, what `wa_sessions.session` stores).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionRecord {
    pub current: Option<SessionState>,
    /// Most recently archived first.
    pub previous: VecDeque<SessionState>,
}

impl SessionRecord {
    /// A record with no session (Go: `record.NewSession`).
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when there is no current session.
    pub fn is_fresh(&self) -> bool {
        self.current.is_none()
    }

    /// Go: `SessionRecord.HasSessionState(version, aliceBaseKey)`.
    pub fn has_session_state(&self, version: u32, alice_base_key: &[u8; 32]) -> bool {
        self.current
            .iter()
            .chain(self.previous.iter())
            .any(|s| s.version == version && s.alice_base_key.as_ref() == Some(alice_base_key))
    }

    /// Moves the current session to the archive.
    pub fn archive_current_state(&mut self) {
        if let Some(current) = self.current.take() {
            self.previous.push_front(current);
            self.previous.truncate(MAX_PREVIOUS_SESSIONS);
        }
    }

    /// Replaces the current session, archiving the old one.
    pub fn promote_state(&mut self, state: SessionState) {
        self.archive_current_state();
        self.current = Some(state);
    }

    /// Serializes as `RecordStructure` bytes.
    pub fn serialize(&self) -> Vec<u8> {
        RecordStructure {
            current_session: self.current.as_ref().map(SessionState::to_structure),
            previous_sessions: self.previous.iter().map(SessionState::to_structure).collect(),
        }
        .encode_to_vec()
    }

    /// Parses `RecordStructure` bytes (from the store, never from the network).
    pub fn deserialize(bytes: &[u8]) -> Result<Self, SignalError> {
        let record = RecordStructure::decode(bytes)
            .map_err(|e| SignalError::invalid(format!("session record decode failed: {e}")))?;
        let current = match &record.current_session {
            Some(s) => SessionState::from_structure(s)?,
            None => None,
        };
        let mut previous = VecDeque::new();
        for s in &record.previous_sessions {
            if let Some(state) = SessionState::from_structure(s)? {
                previous.push_back(state);
            }
        }
        Ok(Self { current, previous })
    }
}

// ---------------------------------------------------------------------------
// Wire messages
// ---------------------------------------------------------------------------

/// A Double Ratchet message (`<enc type="msg">`). Go: `protocol.SignalMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalMessage {
    pub version: u32,
    pub ratchet_key: [u8; 32],
    pub counter: u32,
    pub previous_counter: u32,
    pub ciphertext: Vec<u8>,
    /// The full serialized form including the MAC.
    pub serialized: Vec<u8>,
}

impl SignalMessage {
    /// Builds and MACs a message. `sender_identity` / `receiver_identity` are
    /// the raw 32-byte identity keys.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: u32,
        mac_key: &[u8; 32],
        ratchet_key: [u8; 32],
        counter: u32,
        previous_counter: u32,
        ciphertext: Vec<u8>,
        sender_identity: &[u8; 32],
        receiver_identity: &[u8; 32],
    ) -> Self {
        let proto = SignalMessageProto {
            ratchet_key: Some(serialize_point(&ratchet_key).to_vec()),
            counter: Some(counter),
            previous_counter: Some(previous_counter),
            ciphertext: Some(ciphertext.clone()),
        };
        let mut serialized = Vec::with_capacity(1 + proto.encoded_len() + MAC_LENGTH);
        serialized.push(version_byte(version));
        proto.encode_raw(&mut serialized);
        let mac = Self::mac(sender_identity, receiver_identity, mac_key, &serialized);
        serialized.extend_from_slice(&mac);
        Self { version, ratchet_key, counter, previous_counter, ciphertext, serialized }
    }

    fn mac(sender: &[u8; 32], receiver: &[u8; 32], mac_key: &[u8; 32], body: &[u8]) -> [u8; MAC_LENGTH] {
        let full = hmac_sha256(mac_key, &[&serialize_point(sender), &serialize_point(receiver), body]);
        let mut out = [0u8; MAC_LENGTH];
        out.copy_from_slice(&full[..MAC_LENGTH]);
        out
    }

    /// Parses network bytes. Go: `protocol.NewSignalMessageFromBytes`.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, SignalError> {
        if bytes.len() <= 1 + MAC_LENGTH {
            return Err(SignalError::invalid("signal message too short"));
        }
        let version = check_version(bytes[0])?;
        let body = &bytes[1..bytes.len() - MAC_LENGTH];
        let proto = SignalMessageProto::decode(body)
            .map_err(|e| SignalError::invalid(format!("signal message decode failed: {e}")))?;
        let (Some(ratchet_key), Some(counter), Some(ciphertext)) = (proto.ratchet_key, proto.counter, proto.ciphertext)
        else {
            return Err(SignalError::invalid("incomplete signal message"));
        };
        Ok(Self {
            version,
            ratchet_key: decode_point(&ratchet_key)?,
            counter,
            previous_counter: proto.previous_counter.unwrap_or_default(),
            ciphertext,
            serialized: bytes.to_vec(),
        })
    }

    /// Constant-time-ish MAC check. Go: `SignalMessage.VerifyMac`.
    pub fn verify_mac(&self, sender_identity: &[u8; 32], receiver_identity: &[u8; 32], mac_key: &[u8; 32]) -> bool {
        let Some(split) = self.serialized.len().checked_sub(MAC_LENGTH) else {
            return false;
        };
        let (body, their_mac) = self.serialized.split_at(split);
        let our_mac = Self::mac(sender_identity, receiver_identity, mac_key, body);
        our_mac.iter().zip(their_mac).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

/// The first message(s) of a session (`<enc type="pkmsg">`).
/// Go: `protocol.PreKeySignalMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreKeySignalMessage {
    pub version: u32,
    pub registration_id: u32,
    pub pre_key_id: Option<u32>,
    pub signed_pre_key_id: u32,
    pub base_key: [u8; 32],
    pub identity_key: [u8; 32],
    pub message: SignalMessage,
    pub serialized: Vec<u8>,
}

impl PreKeySignalMessage {
    /// Go: `protocol.NewPreKeySignalMessage`.
    pub fn new(
        registration_id: u32,
        pre_key_id: Option<u32>,
        signed_pre_key_id: u32,
        base_key: [u8; 32],
        identity_key: [u8; 32],
        message: SignalMessage,
    ) -> Self {
        let proto = PreKeySignalMessageProto {
            registration_id: Some(registration_id),
            pre_key_id,
            signed_pre_key_id: Some(signed_pre_key_id),
            base_key: Some(serialize_point(&base_key).to_vec()),
            identity_key: Some(serialize_point(&identity_key).to_vec()),
            message: Some(message.serialized.clone()),
        };
        let mut serialized = Vec::with_capacity(1 + proto.encoded_len());
        serialized.push(version_byte(message.version));
        proto.encode_raw(&mut serialized);
        Self {
            version: message.version,
            registration_id,
            pre_key_id,
            signed_pre_key_id,
            base_key,
            identity_key,
            message,
            serialized,
        }
    }

    /// Parses network bytes. Go: `protocol.NewPreKeySignalMessageFromBytes`.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, SignalError> {
        let (&first, body) = bytes.split_first().ok_or_else(|| SignalError::invalid("empty pre-key message"))?;
        let version = check_version(first)?;
        let proto = PreKeySignalMessageProto::decode(body)
            .map_err(|e| SignalError::invalid(format!("pre-key message decode failed: {e}")))?;
        let (Some(signed_pre_key_id), Some(base_key), Some(identity_key), Some(message)) =
            (proto.signed_pre_key_id, proto.base_key, proto.identity_key, proto.message)
        else {
            return Err(SignalError::invalid("incomplete pre-key message"));
        };
        Ok(Self {
            version,
            registration_id: proto.registration_id.unwrap_or_default(),
            pre_key_id: proto.pre_key_id,
            signed_pre_key_id,
            base_key: decode_point(&base_key)?,
            identity_key: decode_point(&identity_key)?,
            message: SignalMessage::deserialize(&message)?,
            serialized: bytes.to_vec(),
        })
    }
}

/// What [`SessionCipher::encrypt`] produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiphertextMessage {
    Signal(SignalMessage),
    PreKey(PreKeySignalMessage),
}

impl CiphertextMessage {
    /// The bytes that go into `<enc>`.
    pub fn serialize(&self) -> &[u8] {
        match self {
            Self::Signal(m) => &m.serialized,
            Self::PreKey(m) => &m.serialized,
        }
    }

    /// The `<enc type=…>` attribute: `msg` or `pkmsg`.
    pub fn enc_type(&self) -> &'static str {
        match self {
            Self::Signal(_) => "msg",
            Self::PreKey(_) => "pkmsg",
        }
    }
}

// ---------------------------------------------------------------------------
// Session builder (X3DH)
// ---------------------------------------------------------------------------

/// Builds sessions with one remote address. Go: `session.Builder`.
pub struct SessionBuilder<'a, S: SignalStore + ?Sized> {
    store: &'a S,
    remote: SignalAddress,
}

impl<'a, S: SignalStore + ?Sized> SessionBuilder<'a, S> {
    /// Go: `session.NewBuilderFromSignal`.
    pub fn new(store: &'a S, remote: SignalAddress) -> Self {
        Self { store, remote }
    }

    async fn load_record(&self) -> Result<SessionRecord, SignalError> {
        match self.store.load_session(&self.remote).await? {
            Some(bytes) if !bytes.is_empty() => SessionRecord::deserialize(&bytes),
            _ => Ok(SessionRecord::new()),
        }
    }

    /// X3DH as the initiator from a fetched [`PreKeyBundle`]; stores the new
    /// session and trusts the bundle's identity. Go: `Builder.ProcessBundle`.
    pub async fn process_bundle(&self, bundle: &PreKeyBundle) -> Result<(), SignalError> {
        let addr = &self.remote;
        if !self.store.is_trusted_identity(addr, &bundle.identity_key).await? {
            return Err(SignalError::UntrustedIdentity(addr.to_string()));
        }
        if !verify_djb(
            &bundle.identity_key,
            &serialize_point(&bundle.signed_pre_key_public),
            &bundle.signed_pre_key_signature,
        ) {
            return Err(SignalError::InvalidKey("invalid signature on signed pre-key".into()));
        }

        let mut record = self.load_record().await?;
        let our_base_key = KeyPair::generate();
        let mut state = calculate_sender_session(&AliceParameters {
            our_identity: self.store.identity_key_pair().clone(),
            our_base_key: our_base_key.clone(),
            their_identity: bundle.identity_key,
            their_signed_pre_key: bundle.signed_pre_key_public,
            their_one_time_pre_key: bundle.pre_key_public,
            their_ratchet_key: bundle.signed_pre_key_public,
        })?;
        state.pending_pre_key = Some(PendingPreKey {
            pre_key_id: bundle.pre_key_id.filter(|_| bundle.pre_key_public.is_some()),
            signed_pre_key_id: bundle.signed_pre_key_id,
            base_key: our_base_key.public,
        });
        state.local_registration_id = self.store.registration_id();
        state.remote_registration_id = bundle.registration_id;
        state.alice_base_key = Some(our_base_key.public);
        record.promote_state(state);

        self.store.store_session(addr, &record.serialize()).await?;
        self.store.save_identity(addr, &bundle.identity_key).await?;
        Ok(())
    }

    /// Alias of [`Self::process_bundle`] (Go: `ProcessBundle`).
    pub async fn build_session(&self, bundle: &PreKeyBundle) -> Result<(), SignalError> {
        self.process_bundle(bundle).await
    }

    /// X3DH as the responder on receipt of a [`PreKeySignalMessage`]: builds
    /// the session into `record` (not stored here). Returns the one-time
    /// pre-key ID to remove once decryption succeeded, `None` when the
    /// message belongs to a session we already have. Go: `Builder.Process`.
    pub async fn process(
        &self,
        record: &mut SessionRecord,
        message: &PreKeySignalMessage,
    ) -> Result<Option<u32>, SignalError> {
        let addr = &self.remote;
        if !self.store.is_trusted_identity(addr, &message.identity_key).await? {
            return Err(SignalError::UntrustedIdentity(addr.to_string()));
        }
        if record.has_session_state(message.version, &message.base_key) {
            tracing::debug!(%addr, "already have a session for this pre-key message");
            return Ok(None);
        }
        let signed = self.store.signed_pre_key();
        if signed.key_id != message.signed_pre_key_id {
            return Err(SignalError::NoSignedPreKey(message.signed_pre_key_id));
        }
        let one_time = match message.pre_key_id {
            Some(id) => Some(self.store.load_pre_key(id).await?.ok_or(SignalError::PreKeyNotFound(id))?),
            None => None,
        };
        let mut state = calculate_receiver_session(&BobParameters {
            our_identity: self.store.identity_key_pair().clone(),
            our_signed_pre_key: signed.key_pair.clone(),
            our_one_time_pre_key: one_time,
            our_ratchet_key: signed.key_pair.clone(),
            their_identity: message.identity_key,
            their_base_key: message.base_key,
        })?;
        state.local_registration_id = self.store.registration_id();
        state.remote_registration_id = message.registration_id;
        state.alice_base_key = Some(message.base_key);
        record.promote_state(state);

        self.store.save_identity(addr, &message.identity_key).await?;
        Ok(message.pre_key_id)
    }
}

// ---------------------------------------------------------------------------
// Session cipher (Double Ratchet)
// ---------------------------------------------------------------------------

/// Encrypts/decrypts for one remote address. Go: `session.Cipher`.
pub struct SessionCipher<'a, S: SignalStore + ?Sized> {
    store: &'a S,
    remote: SignalAddress,
}

impl<'a, S: SignalStore + ?Sized> SessionCipher<'a, S> {
    /// Go: `session.NewCipher`.
    pub fn new(store: &'a S, remote: SignalAddress) -> Self {
        Self { store, remote }
    }

    async fn load_record(&self) -> Result<Option<SessionRecord>, SignalError> {
        match self.store.load_session(&self.remote).await? {
            Some(bytes) if !bytes.is_empty() => SessionRecord::deserialize(&bytes).map(Some),
            _ => Ok(None),
        }
    }

    /// Encrypts with the current session; a [`PreKeySignalMessage`] while the
    /// peer has not answered yet. Go: `Cipher.Encrypt`.
    pub async fn encrypt(&self, plaintext: &[u8]) -> Result<CiphertextMessage, SignalError> {
        let no_session = || SignalError::NoSession(self.remote.to_string());
        let mut record = self.load_record().await?.ok_or_else(no_session)?;
        let state = record.current.as_mut().ok_or_else(no_session)?;
        let previous_counter = state.previous_counter;
        let (local_identity, remote_identity, version) = (state.local_identity, state.remote_identity, state.version);
        let chain = state.sender_chain.as_mut().ok_or_else(no_session)?;
        let ratchet_key = chain.ratchet_key;
        let keys = chain.next_message_keys()?;

        let ciphertext = cbc::encrypt_with_iv(&keys.cipher_key, &keys.iv, plaintext)?;
        let message = SignalMessage::new(
            version,
            &keys.mac_key,
            ratchet_key,
            keys.index,
            previous_counter,
            ciphertext,
            &local_identity,
            &remote_identity,
        );
        let out = match &state.pending_pre_key {
            Some(pending) => CiphertextMessage::PreKey(PreKeySignalMessage::new(
                state.local_registration_id,
                pending.pre_key_id,
                pending.signed_pre_key_id,
                pending.base_key,
                local_identity,
                message,
            )),
            None => CiphertextMessage::Signal(message),
        };
        self.store.store_session(&self.remote, &record.serialize()).await?;
        Ok(out)
    }

    /// Decrypts a normal [`SignalMessage`] (`<enc type="msg">`).
    /// Go: `Cipher.Decrypt`.
    pub async fn decrypt(&self, message: &SignalMessage) -> Result<Vec<u8>, SignalError> {
        let mut record = self.load_record().await?.ok_or_else(|| SignalError::NoSession(self.remote.to_string()))?;
        let plaintext = Self::decrypt_with_record(&mut record, message)?;
        let remote_identity = record.current.as_ref().map(|s| s.remote_identity).unwrap_or_default();
        if !self.store.is_trusted_identity(&self.remote, &remote_identity).await? {
            return Err(SignalError::UntrustedIdentity(self.remote.to_string()));
        }
        self.store.save_identity(&self.remote, &remote_identity).await?;
        self.store.store_session(&self.remote, &record.serialize()).await?;
        Ok(plaintext)
    }

    /// Parses and decrypts `<enc type="msg">` bytes.
    pub async fn decrypt_bytes(&self, bytes: &[u8]) -> Result<Vec<u8>, SignalError> {
        self.decrypt(&SignalMessage::deserialize(bytes)?).await
    }

    /// Decrypts a [`PreKeySignalMessage`] (`<enc type="pkmsg">`): builds the
    /// session if needed, decrypts, stores, then removes the used one-time
    /// pre-key. Go: `Cipher.DecryptMessage` / `DecryptMessageReturnKey`.
    pub async fn decrypt_message(&self, message: &PreKeySignalMessage) -> Result<Vec<u8>, SignalError> {
        let mut record = self.load_record().await?.unwrap_or_default();
        let builder = SessionBuilder::new(self.store, self.remote.clone());
        let used_pre_key = builder.process(&mut record, message).await?;
        let plaintext = Self::decrypt_with_record(&mut record, &message.message)?;
        self.store.store_session(&self.remote, &record.serialize()).await?;
        if let Some(id) = used_pre_key {
            self.store.remove_pre_key(id).await?;
        }
        Ok(plaintext)
    }

    /// Parses and decrypts `<enc type="pkmsg">` bytes.
    pub async fn decrypt_pre_key_bytes(&self, bytes: &[u8]) -> Result<Vec<u8>, SignalError> {
        self.decrypt_message(&PreKeySignalMessage::deserialize(bytes)?).await
    }

    /// Tries the current session, then every archived one (promoting the one
    /// that works). The record is only modified on success.
    /// Go: `Cipher.decryptWithRecord`.
    pub fn decrypt_with_record(record: &mut SessionRecord, message: &SignalMessage) -> Result<Vec<u8>, SignalError> {
        let mut first_error = None;
        if let Some(current) = &record.current {
            let mut state = current.clone();
            match Self::decrypt_with_state(&mut state, message) {
                Ok(plaintext) => {
                    record.current = Some(state);
                    return Ok(plaintext);
                }
                Err(err) => first_error = Some(err),
            }
        }
        for i in 0..record.previous.len() {
            let Some(mut state) = record.previous.get(i).cloned() else { break };
            if let Ok(plaintext) = Self::decrypt_with_state(&mut state, message) {
                record.previous.remove(i);
                record.promote_state(state);
                return Ok(plaintext);
            }
        }
        Err(first_error.unwrap_or_else(|| SignalError::NoSession("no valid session".into())))
    }

    /// One ratchet attempt on `state` (mutated in place).
    /// Go: `Cipher.decryptWithState`.
    pub fn decrypt_with_state(state: &mut SessionState, message: &SignalMessage) -> Result<Vec<u8>, SignalError> {
        if state.sender_chain.is_none() {
            return Err(SignalError::NoSession("uninitialized session".into()));
        }
        if message.version != state.version {
            return Err(SignalError::invalid(format!(
                "message version {} does not match session version {}",
                message.version, state.version
            )));
        }
        state.ratchet_receiver_chain(&message.ratchet_key)?;
        let chain = state
            .receiver_chain_mut(&message.ratchet_key)
            .ok_or_else(|| SignalError::invalid("receiver chain missing after ratchet"))?;
        let keys = chain.message_keys_for(message.counter)?;
        if !message.verify_mac(&state.remote_identity, &state.local_identity, &keys.mac_key) {
            return Err(SignalError::BadMac);
        }
        let plaintext = cbc::decrypt(&keys.cipher_key, &keys.iv, &message.ciphertext)
            .map_err(|e| SignalError::invalid(format!("message body decryption failed: {e}")))?;
        state.pending_pre_key = None;
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::signal::store::memory::MemoryStore;

    fn alice_addr() -> SignalAddress {
        SignalAddress::new("4911", 0)
    }

    fn bob_addr() -> SignalAddress {
        SignalAddress::new("4922", 3)
    }

    /// Alice builds a session from Bob's bundle and sends the first message;
    /// Bob decrypts it (X3DH as responder). Returns both stores.
    async fn established(one_time_pre_key: bool) -> (MemoryStore, MemoryStore) {
        let alice = MemoryStore::new(1111);
        let bob = MemoryStore::new(2222);
        let bundle = bob.bundle(one_time_pre_key.then_some(31));
        SessionBuilder::new(&alice, bob_addr()).build_session(&bundle).await.unwrap();

        let first = SessionCipher::new(&alice, bob_addr()).encrypt(b"hello bob").await.unwrap();
        assert_eq!(first.enc_type(), "pkmsg");
        let CiphertextMessage::PreKey(pkmsg) = &first else { panic!("expected pkmsg") };
        assert_eq!(pkmsg.registration_id, 1111);
        assert_eq!(pkmsg.pre_key_id, one_time_pre_key.then_some(31));
        assert_eq!(pkmsg.serialized[0], 0x33);

        let parsed = PreKeySignalMessage::deserialize(first.serialize()).unwrap();
        assert_eq!(&parsed, pkmsg);
        let plain = SessionCipher::new(&bob, alice_addr()).decrypt_message(&parsed).await.unwrap();
        assert_eq!(plain, b"hello bob");
        if one_time_pre_key {
            assert!(bob.pre_keys.lock().unwrap().is_empty(), "one-time pre-key removed after use");
        }
        (alice, bob)
    }

    // Cross-implementation fixture, generated once with libsignal (Go)
    // v0.2.2 used as a test oracle only (never copied): Alice (Go, identity
    // private = clamp(0x44 * 32), registration 1111) runs `Builder.ProcessBundle`
    // on Bob's bundle (registration 2222, signed pre-key #1, one-time pre-key
    // #31) and `Cipher.Encrypt`s "hello from go" and "second from go".
    const GO_BOB_ID_PRIV: &str = "1011111111111111111111111111111111111111111111111111111111111151";
    const GO_BOB_SPK_PRIV: &str = "2022222222222222222222222222222222222222222222222222222222222262";
    const GO_BOB_OTPK_PRIV: &str = "3033333333333333333333333333333333333333333333333333333333333373";
    const GO_ALICE_ID_PUB: &str = "ff2ee45601ec1b67310c7790404585ae697331eee1c1f8cf2419731c1fff3e6b";
    const GO_SPK_SIG: &str = "0d6a01fcaf0247be2b1bbfca91b523c9e7986d1552f0da2c176c3b8bfe15e80d\
                              846ed8af98946471d049c1a8c913cc68c5fa55838a87da6bf29c39e20969cd86";
    const GO_PKMSG1: &str = "33081f1221051b2f739bea0ec19b39387c4205b3d186cf768f6e36e1c810d83edcc76c19ea121a2105ff2ee456\
                             01ec1b67310c7790404585ae697331eee1c1f8cf2419731c1fff3e6b2242330a21052d3565cad7ac26f36f3f21\
                             7b57528a8d83a9ce27cbd95a54f665b75bb1e6bb08100018002210c3dc7f3f0adf48a1b808e8152b44358b5c7d\
                             47954ce0068228d7083001";
    const GO_PKMSG2: &str = "33081f1221051b2f739bea0ec19b39387c4205b3d186cf768f6e36e1c810d83edcc76c19ea121a2105ff2ee456\
                             01ec1b67310c7790404585ae697331eee1c1f8cf2419731c1fff3e6b2242330a21052d3565cad7ac26f36f3f21\
                             7b57528a8d83a9ce27cbd95a54f665b75bb1e6bb08100118002210a9a98a74caa2c1563577a189d9a28bd23e18\
                             5f25f564c62b28d7083001";

    fn unhex(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        hex::decode(clean).unwrap()
    }

    fn arr<const N: usize>(s: &str) -> [u8; N] {
        unhex(s).try_into().unwrap()
    }

    #[tokio::test]
    async fn decrypts_go_libsignal_pre_key_messages() {
        let bob = MemoryStore::with_keys(
            KeyPair::from_private(arr(GO_BOB_ID_PRIV)),
            crate::crypto::keys::PreKey {
                key_pair: KeyPair::from_private(arr(GO_BOB_SPK_PRIV)),
                key_id: 1,
                signature: Some(arr(GO_SPK_SIG)),
            },
            2222,
        );
        bob.pre_keys.lock().unwrap().insert(31, crate::crypto::keys::PreKey::from_private(31, arr(GO_BOB_OTPK_PRIV)));
        // The Go signature over Bob's signed pre-key verifies with our XEdDSA.
        assert!(verify_djb(&bob.identity.public, &bob.signed_pre_key.public_serialized(), &arr::<64>(GO_SPK_SIG)));

        let m1 = PreKeySignalMessage::deserialize(&unhex(GO_PKMSG1)).unwrap();
        assert_eq!(m1.registration_id, 1111);
        assert_eq!(m1.pre_key_id, Some(31));
        assert_eq!(m1.signed_pre_key_id, 1);
        assert_eq!(m1.identity_key, arr::<32>(GO_ALICE_ID_PUB));
        assert_eq!(m1.message.counter, 0);

        let cipher = SessionCipher::new(&bob, alice_addr());
        assert_eq!(cipher.decrypt_message(&m1).await.unwrap(), b"hello from go");
        // Second pkmsg of the same session: no new X3DH, next counter.
        assert_eq!(cipher.decrypt_pre_key_bytes(&unhex(GO_PKMSG2)).await.unwrap(), b"second from go");
        assert!(bob.pre_keys.lock().unwrap().is_empty());
        assert_eq!(bob.identities.lock().unwrap()["4911:0"], arr::<32>(GO_ALICE_ID_PUB));
        let record = SessionRecord::deserialize(&bob.sessions.lock().unwrap()["4911:0"]).unwrap();
        assert_eq!(record.current.as_ref().unwrap().remote_registration_id, 1111);
        assert!(record.previous.is_empty());
        // Bob can answer on the Go-initiated session.
        assert_eq!(cipher.encrypt(b"reply").await.unwrap().enc_type(), "msg");
    }

    #[tokio::test]
    async fn x3dh_round_trip_both_directions() {
        for one_time in [true, false] {
            let (alice, bob) = established(one_time).await;
            let to_bob = SessionCipher::new(&alice, bob_addr());
            let to_alice = SessionCipher::new(&bob, alice_addr());

            // Bob answers with a normal message; Alice's pending pre-key clears.
            let reply = to_alice.encrypt(b"hi alice").await.unwrap();
            assert_eq!(reply.enc_type(), "msg");
            let parsed = SignalMessage::deserialize(reply.serialize()).unwrap();
            assert_eq!(to_bob.decrypt(&parsed).await.unwrap(), b"hi alice");
            assert_eq!(to_bob.encrypt(b"now plain").await.unwrap().enc_type(), "msg");

            let record = SessionRecord::deserialize(&alice.sessions.lock().unwrap()["4922:3"]).unwrap();
            let state = record.current.as_ref().unwrap();
            assert_eq!(state.remote_registration_id, 2222);
            assert_eq!(state.local_registration_id, 1111);
            assert!(state.pending_pre_key.is_none());
            // Identities were recorded on both sides.
            assert_eq!(alice.identities.lock().unwrap()["4922:3"], bob.identity.public);
            assert_eq!(bob.identities.lock().unwrap()["4911:0"], alice.identity.public);
        }
    }

    #[tokio::test]
    async fn twenty_messages_each_way_alternating() {
        let (alice, bob) = established(true).await;
        let to_bob = SessionCipher::new(&alice, bob_addr());
        let to_alice = SessionCipher::new(&bob, alice_addr());
        for i in 0..20 {
            let a = format!("alice→bob #{i}");
            let m = to_bob.encrypt(a.as_bytes()).await.unwrap();
            let got = match &m {
                CiphertextMessage::PreKey(p) => to_alice.decrypt_message(p).await.unwrap(),
                CiphertextMessage::Signal(s) => to_alice.decrypt(s).await.unwrap(),
            };
            assert_eq!(got, a.as_bytes());

            let b = format!("bob→alice #{i}");
            let m = to_alice.encrypt(b.as_bytes()).await.unwrap();
            assert_eq!(to_bob.decrypt_bytes(m.serialize()).await.unwrap(), b.as_bytes());
        }
    }

    #[tokio::test]
    async fn out_of_order_and_duplicate() {
        let (alice, bob) = established(true).await;
        let to_bob = SessionCipher::new(&alice, bob_addr());
        let to_alice = SessionCipher::new(&bob, alice_addr());
        // Finish the handshake so Alice sends plain `msg`s.
        let r = to_alice.encrypt(b"ack").await.unwrap();
        to_bob.decrypt_bytes(r.serialize()).await.unwrap();

        let mut sent = Vec::new();
        for i in 1..=5 {
            sent.push(to_bob.encrypt(format!("m{i}").as_bytes()).await.unwrap());
        }
        // Receive #5, then #3, then the rest.
        assert_eq!(to_alice.decrypt_bytes(sent[4].serialize()).await.unwrap(), b"m5");
        assert_eq!(to_alice.decrypt_bytes(sent[2].serialize()).await.unwrap(), b"m3");
        assert_eq!(to_alice.decrypt_bytes(sent[0].serialize()).await.unwrap(), b"m1");
        // Replay → DuplicateMessage.
        let err = to_alice.decrypt_bytes(sent[2].serialize()).await.unwrap_err();
        assert!(matches!(err, SignalError::DuplicateMessage(_)), "{err:?}");
        assert!(err.is_old_counter());
        assert_eq!(to_alice.decrypt_bytes(sent[1].serialize()).await.unwrap(), b"m2");
        assert_eq!(to_alice.decrypt_bytes(sent[3].serialize()).await.unwrap(), b"m4");

        // Messages across a DH ratchet step, delivered late.
        let old_chain = to_bob.encrypt(b"before ratchet").await.unwrap();
        let r = to_alice.encrypt(b"ratchet please").await.unwrap();
        to_bob.decrypt_bytes(r.serialize()).await.unwrap();
        let new_chain = to_bob.encrypt(b"after ratchet").await.unwrap();
        assert_eq!(to_alice.decrypt_bytes(new_chain.serialize()).await.unwrap(), b"after ratchet");
        assert_eq!(to_alice.decrypt_bytes(old_chain.serialize()).await.unwrap(), b"before ratchet");
    }

    #[tokio::test]
    async fn tampering_and_garbage_are_errors_and_leave_state_intact() {
        let (alice, bob) = established(false).await;
        let to_bob = SessionCipher::new(&alice, bob_addr());
        let to_alice = SessionCipher::new(&bob, alice_addr());
        let r = to_alice.encrypt(b"ack").await.unwrap();
        to_bob.decrypt_bytes(r.serialize()).await.unwrap();

        let m = to_bob.encrypt(b"payload").await.unwrap();
        let mut bad = m.serialize().to_vec();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        assert!(matches!(to_alice.decrypt_bytes(&bad).await, Err(SignalError::BadMac)));
        assert_eq!(to_alice.decrypt_bytes(m.serialize()).await.unwrap(), b"payload");

        for junk in [&[][..], &[0x33][..], &[0x23; 20][..], &[0x33; 20][..], &[0x43; 12][..]] {
            assert!(to_alice.decrypt_bytes(junk).await.is_err());
            assert!(to_alice.decrypt_pre_key_bytes(junk).await.is_err());
        }
        // No session at all.
        let stranger = MemoryStore::new(9);
        assert!(matches!(
            SessionCipher::new(&stranger, alice_addr()).decrypt_bytes(m.serialize()).await,
            Err(SignalError::NoSession(_))
        ));
        assert!(matches!(
            SessionCipher::new(&stranger, alice_addr()).encrypt(b"x").await,
            Err(SignalError::NoSession(_))
        ));
    }

    #[tokio::test]
    async fn untrusted_identity_and_bad_bundles() {
        let alice = MemoryStore::new(1);
        let bob = MemoryStore::new(2);
        let bundle = bob.bundle(Some(5));

        // Bad signed pre-key signature.
        let mut forged = bundle.clone();
        forged.signed_pre_key_signature[0] ^= 1;
        assert!(matches!(
            SessionBuilder::new(&alice, bob_addr()).process_bundle(&forged).await,
            Err(SignalError::InvalidKey(_))
        ));

        // A different identity is already stored for Bob → untrusted.
        alice.identities.lock().unwrap().insert(bob_addr().to_string(), [9u8; 32]);
        let err = SessionBuilder::new(&alice, bob_addr()).process_bundle(&bundle).await.unwrap_err();
        assert!(err.is_untrusted_identity());

        // Clearing it (what `auto_trust_identity` does) makes the retry succeed.
        alice.delete_identity(&bob_addr()).await.unwrap();
        SessionBuilder::new(&alice, bob_addr()).process_bundle(&bundle).await.unwrap();
        let first = SessionCipher::new(&alice, bob_addr()).encrypt(b"x").await.unwrap();

        // Bob has a different identity stored for Alice → the pkmsg is untrusted.
        bob.identities.lock().unwrap().insert(alice_addr().to_string(), [1u8; 32]);
        let err = SessionCipher::new(&bob, alice_addr()).decrypt_pre_key_bytes(first.serialize()).await.unwrap_err();
        assert!(err.is_untrusted_identity());
        assert!(bob.sessions.lock().unwrap().is_empty(), "nothing stored on failure");
        bob.delete_identity(&alice_addr()).await.unwrap();

        // Unknown one-time pre-key / signed pre-key IDs.
        bob.pre_keys.lock().unwrap().clear();
        assert!(matches!(
            SessionCipher::new(&bob, alice_addr()).decrypt_pre_key_bytes(first.serialize()).await,
            Err(SignalError::PreKeyNotFound(5))
        ));
        let mut wrong_spk = PreKeySignalMessage::deserialize(first.serialize()).unwrap();
        wrong_spk.signed_pre_key_id = 77;
        assert!(matches!(
            SessionCipher::new(&bob, alice_addr()).decrypt_message(&wrong_spk).await,
            Err(SignalError::NoSignedPreKey(77))
        ));
    }

    #[tokio::test]
    async fn record_serialization_round_trips_and_resumes() {
        let (alice, bob) = established(true).await;
        let bytes = alice.sessions.lock().unwrap()["4922:3"].clone();
        let record = SessionRecord::deserialize(&bytes).unwrap();
        assert_eq!(record.serialize(), bytes);
        assert!(record.has_session_state(CURRENT_VERSION, &record.current.as_ref().unwrap().alice_base_key.unwrap()));

        // A second X3DH with Bob archives the first session; old in-flight
        // messages still decrypt via the archived state.
        let to_alice = SessionCipher::new(&bob, alice_addr());
        let late = to_alice.encrypt(b"late").await.unwrap();
        SessionBuilder::new(&alice, bob_addr()).process_bundle(&bob.bundle(Some(40))).await.unwrap();
        let record = SessionRecord::deserialize(&alice.sessions.lock().unwrap()["4922:3"]).unwrap();
        assert_eq!(record.previous.len(), 1);
        assert_eq!(SessionCipher::new(&alice, bob_addr()).decrypt_bytes(late.serialize()).await.unwrap(), b"late");

        // Empty / garbage records.
        assert!(SessionRecord::deserialize(&[]).unwrap().is_fresh());
        assert!(SessionRecord::deserialize(&[0xff, 0xff]).is_err());
    }
}
