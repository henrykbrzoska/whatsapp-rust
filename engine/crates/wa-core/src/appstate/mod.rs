// Ported from the Go client (`appstate/keys.go`, `appstate/errors.go`, `appstate.go`),
// © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! App state (server-side synchronisation, "syncd") — the pure core.
//!
//! This is Task A of Phase 5: the pieces that need no store, no socket and no
//! key fetches, so they can be tested entirely offline and byte-for-byte
//! against Go.
//!
//! * [`lthash`] — the summation hash that keeps app state integrity across
//!   mutations (`Go appstate/lthash/lthash.go`).
//! * [`hash`] — [`hash::HashState`], snapshot/patch/content/index MACs.
//! * [`decode`] — `SyncActionData`/`SyncdPatch` decoding, `PatchList` parsing.
//! * [`encode`] — `SyncActionData`/`SyncdMutation`/`SyncdPatch` encoding.
//!
//! Key *storage* and key *sharing* (`keys.go`, Task B) and patch recovery
//! (`recovery.go`, Task B) live outside this module; they consume
//! [`ExpandedAppStateKeys`], [`hash::HashState`] and [`decode::Mutation`] from
//! here.

pub mod decode;
pub mod encode;
pub mod hash;
pub mod keys;
pub mod lthash;
pub mod recovery;

pub use decode::{Mutation, PatchList, decode_index_json, decode_mutation, decode_patch_list, decode_sync_action_data};
pub use encode::{
    EncodedMutation, EncodedPatch, MutationInfo, PatchInfo, encode_index_json, encode_mutation, encode_patch,
    encode_sync_action_data,
};
pub use hash::{
    AppStateWarning, HashState, generate_content_mac, generate_index_mac, generate_patch_mac, verify_patch_mac,
    verify_snapshot_mac,
};
pub use keys::{
    AppStateKeyCache, GeneratedAppStateSyncKey, KEY_ID_LEN, SYNC_KEY_LEN, ensure_app_state_sync_key,
    generate_app_state_sync_key, get_app_state_sync_key, missing_key_ids, persist_app_state_sync_key_share,
    store_generated_app_state_sync_key,
};
pub use lthash::{LTHash, LtHashOp, WAPATCH_INTEGRITY};
pub use recovery::{
    RecoveredRecord, RecoveredSnapshot, SnapshotRecovery, SnapshotRecoveryRecord, VerifiedPatch, lthash_of,
    verify_patch, verify_patch_chained, verify_snapshot,
};

use crate::crypto::CryptoError;

/// `appstate.WAPatchName` — the name of one app state collection.
///
/// Kept as a thin newtype over `String` (not an enum) because the server may
/// add collections at any time; unknown names must round-trip unchanged.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WAPatchName(String);

impl WAPatchName {
    /// `critical_block` — user settings: push name, locale, security settings.
    pub const CRITICAL_BLOCK: &'static str = "critical_block";
    /// `critical_unblock_low` — the user's contact list.
    pub const CRITICAL_UNBLOCK_LOW: &'static str = "critical_unblock_low";
    /// `regular_low` — local chat settings: pin, archive, mark-as-read.
    pub const REGULAR_LOW: &'static str = "regular_low";
    /// `regular_high` — mute state, starred messages, deletes.
    pub const REGULAR_HIGH: &'static str = "regular_high";
    /// `regular` — protocol info about app state itself (labels, key expiry).
    pub const REGULAR: &'static str = "regular";

    /// Builds a patch name from any string (unknown names are kept verbatim).
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The wire name of this collection.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `AllPatchNames` — every collection name this engine knows about.
    pub fn all() -> [Self; 5] {
        [
            Self::new(Self::CRITICAL_BLOCK),
            Self::new(Self::CRITICAL_UNBLOCK_LOW),
            Self::new(Self::REGULAR_HIGH),
            Self::new(Self::REGULAR),
            Self::new(Self::REGULAR_LOW),
        ]
    }
}

impl From<&str> for WAPatchName {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for WAPatchName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for WAPatchName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// Index names used by the patch builders (`appstate/keys.go`). Only the ones the
// engine currently writes are listed; every other index is only ever *read*,
// and reads do not need a constant.
/// `regular_high` — mute state of a chat.
pub const INDEX_MUTE: &str = "mute";
/// `regular_low` — pinned state of a chat.
pub const INDEX_PIN: &str = "pin_v1";
/// `regular_low` — archived state of a chat.
pub const INDEX_ARCHIVE: &str = "archive";
/// `regular_low` — read/unread state of a chat.
pub const INDEX_MARK_CHAT_AS_READ: &str = "markChatAsRead";
/// `regular_high` — starred message.
pub const INDEX_STAR: &str = "star";
/// `regular_high` — chat deletion.
pub const INDEX_DELETE_CHAT: &str = "deleteChat";
/// `regular_high` — per-message deletion.
pub const INDEX_DELETE_MESSAGE_FOR_ME: &str = "deleteMessageForMe";
/// `regular` — label definition.
pub const INDEX_LABEL_EDIT: &str = "label_edit";
/// `regular` — label applied to a chat.
pub const INDEX_LABEL_ASSOCIATION_CHAT: &str = "label_jid";
/// `regular` — label applied to a message.
pub const INDEX_LABEL_ASSOCIATION_MESSAGE: &str = "label_message";
/// `critical_block` — the user's push name.
pub const INDEX_SETTING_PUSH_NAME: &str = "setting_pushName";
/// `critical_block` — the locale.
pub const INDEX_SETTING_LOCALE: &str = "setting_locale";
/// `critical_block` — the "security notification" preference.
pub const INDEX_SETTING_SECURITY_NOTIFICATION: &str = "setting_securityNotification";
/// `critical_unblock_low` — a contact.
pub const INDEX_CONTACT: &str = "contact";
/// `critical_unblock_low` — a LID-mapped contact.
pub const INDEX_LID_CONTACT: &str = "lid_contact";

/// Length of an expanded app state key (`hkdfutil.SHA256(..., 160) / 5`).
pub const APP_STATE_KEY_LEN: usize = 32;
/// Length of the expanded key material: five 32-byte keys.
pub const EXPANDED_APP_STATE_KEYS_LEN: usize = APP_STATE_KEY_LEN * 5;

/// HKDF info string used to expand an app state sync key.
pub const APP_STATE_KEY_HKDF_INFO: &[u8] = b"WhatsApp Mutation Keys";

/// `appstate.ExpandedAppStateKeys` — the five keys derived from one sync key.
///
/// Go counterpart: the struct of the same name plus `expandAppStateKeys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedAppStateKeys {
    /// Key for the HMAC over the JSON index (the mutation's "address").
    pub index: [u8; APP_STATE_KEY_LEN],
    /// AES-256-CBC key for the encrypted `SyncActionData`.
    pub value_encryption: [u8; APP_STATE_KEY_LEN],
    /// Key for the HMAC over the encrypted value (the content MAC).
    pub value_mac: [u8; APP_STATE_KEY_LEN],
    /// Key for the collection's LTHash snapshot MAC.
    pub snapshot_mac: [u8; APP_STATE_KEY_LEN],
    /// Key for a patch's patch MAC.
    pub patch_mac: [u8; APP_STATE_KEY_LEN],
}

impl ExpandedAppStateKeys {
    /// `expandAppStateKeys` — `HKDF-SHA256(key_data, nil, "WhatsApp Mutation Keys", 160)`
    /// split into five 32-byte keys, in Go's order.
    pub fn expand(key_data: &[u8]) -> Result<Self, AppStateError> {
        let expanded =
            crate::crypto::hkdf::sha256(key_data, &[], APP_STATE_KEY_HKDF_INFO, EXPANDED_APP_STATE_KEYS_LEN as u8);
        let mut keys = Self {
            index: [0; APP_STATE_KEY_LEN],
            value_encryption: [0; APP_STATE_KEY_LEN],
            value_mac: [0; APP_STATE_KEY_LEN],
            snapshot_mac: [0; APP_STATE_KEY_LEN],
            patch_mac: [0; APP_STATE_KEY_LEN],
        };
        if expanded.len() != EXPANDED_APP_STATE_KEYS_LEN {
            return Err(AppStateError::InvalidKeyData("key expansion did not produce 160 bytes"));
        }
        for (slot, chunk) in [
            &mut keys.index,
            &mut keys.value_encryption,
            &mut keys.value_mac,
            &mut keys.snapshot_mac,
            &mut keys.patch_mac,
        ]
        .into_iter()
        .zip(expanded.chunks_exact(APP_STATE_KEY_LEN))
        {
            slot.copy_from_slice(chunk);
        }
        Ok(keys)
    }
}

/// `SyncdOperation` — what a mutation does to one index entry.
pub use wa_proto::wa_web_protobufs_server_sync::syncd_mutation::SyncdOperation;

/// Reads a mutation's operation, defaulting to `SET` like Go's generated getter.
pub(crate) fn mutation_operation(mutation: &wa_proto::wa_web_protobufs_server_sync::SyncdMutation) -> SyncdOperation {
    mutation.operation.and_then(|raw| SyncdOperation::try_from(raw).ok()).unwrap_or(SyncdOperation::Set)
}

/// Errors raised by the app state core.
///
/// Go counterpart: the sentinel values in `appstate/errors.go` plus the
/// `fmt.Errorf` wrappers; every failure is a value here, nothing panics
/// outside `#[cfg(test)]`.
#[derive(Debug, thiserror::Error)]
pub enum AppStateError {
    /// An LTHash buffer or item had the wrong length (Go would slice-panic).
    #[error("invalid LTHash length: expected {expected} bytes, got {actual}")]
    InvalidHashLength { expected: usize, actual: usize },
    /// `ErrMissingPreviousSetValueOperation` — a REMOVE without a known value MAC.
    #[error("missing value MAC of previous SET operation for {index_mac}")]
    MissingPreviousSetValueOperation { index_mac: String },
    /// `ErrMismatchingLTHash` — the snapshot MAC did not match the LTHash.
    #[error("mismatching LTHash")]
    MismatchingLtHash,
    /// `ErrMismatchingPatchMAC` — the patch MAC did not verify.
    #[error("mismatching patch MAC")]
    MismatchingPatchMac,
    /// The snapshot MAC did not verify.
    #[error("mismatching snapshot MAC")]
    MismatchingSnapshotMac,
    /// `ErrMismatchingContentMAC` — the encrypted value MAC did not verify.
    #[error("mismatching content MAC")]
    MismatchingContentMac,
    /// `ErrMismatchingIndexMAC` — the index MAC did not verify.
    #[error("mismatching index MAC")]
    MismatchingIndexMac,
    /// `ErrKeyNotFound` — the patch references an app state key we do not have.
    #[error("didn't find app state key")]
    KeyNotFound,
    /// The sync key material could not be expanded.
    #[error("invalid app state key: {0}")]
    InvalidKeyData(&'static str),
    /// A `SyncdMutation`/`SyncdRecord` was malformed.
    #[error("invalid app state mutation: {0}")]
    InvalidMutation(&'static str),
    /// The `<collection>`/`<patch>` response was malformed.
    #[error("invalid app state patch list: {0}")]
    InvalidPatchList(&'static str),
    /// The JSON index of a mutation was not an array of strings.
    #[error("invalid app state index JSON: {0}")]
    InvalidIndexJson(&'static str),
    /// A protobuf payload could not be decoded.
    #[error("failed to unmarshal protobuf: {0}")]
    ProtobufDecode(#[from] prost::DecodeError),
    /// A protobuf payload could not be encoded.
    #[error("failed to marshal protobuf: {0}")]
    ProtobufEncode(#[from] prost::EncodeError),
    /// A crypto primitive failed.
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    /// The app state store failed (a `PUT`/`GET` against
    /// [`crate::store::traits::AppStateStore`] or
    /// [`crate::store::traits::AppStateSyncKeyStore`]).
    #[error("app state store error: {0}")]
    Store(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("test fixture is valid hex")
    }

    #[test]
    fn patch_name_roundtrips_and_keeps_unknown_names() {
        assert_eq!(WAPatchName::new(WAPatchName::REGULAR_LOW).as_str(), "regular_low");
        assert_eq!(WAPatchName::from("something_new").to_string(), "something_new");
        let all: Vec<String> = WAPatchName::all().iter().map(|n| n.as_str().to_owned()).collect();
        assert_eq!(all, vec!["critical_block", "critical_unblock_low", "regular_high", "regular", "regular_low"]);
    }

    #[test]
    fn expand_app_state_keys_matches_go() {
        // Go: hkdfutil.SHA256(keyData, nil, []byte("WhatsApp Mutation Keys"), 160)
        let key_data = hex("b0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecf");
        let keys = ExpandedAppStateKeys::expand(&key_data).expect("expand");
        assert_eq!(hex::encode(keys.index), "afd50b8af2fd7406de936c7f3c66a0f62fe9347f6e4960a0c817e6244f209e98");
        assert_eq!(
            hex::encode(keys.value_encryption),
            "f6ae2dce8cfd87b09d90db4ec951d4fe0fe0a2d5cf2e76b41fbaab20e64d0072"
        );
        assert_eq!(hex::encode(keys.value_mac), "f5211e074e254f3691ba7ac5f58a28bdbdde4ce321928b39414a0c4a53fd56cc");
        assert_eq!(hex::encode(keys.snapshot_mac), "49e82f71db1aadca6d9d5249f2813c20db51d4ec8a6c9f160346df81746841c3");
        assert_eq!(hex::encode(keys.patch_mac), "cb42ed9bdd4d7d1f290021658d6d1acd0b6c43af1b94d18141a8e8afc5168b90");
    }

    #[test]
    fn mutation_operation_defaults_to_set() {
        use wa_proto::wa_web_protobufs_server_sync::SyncdMutation;
        let unset = SyncdMutation::default();
        assert_eq!(mutation_operation(&unset), SyncdOperation::Set);
        let remove = SyncdMutation { operation: Some(SyncdOperation::Remove as i32), record: None };
        assert_eq!(mutation_operation(&remove), SyncdOperation::Remove);
        let bogus = SyncdMutation { operation: Some(99), record: None };
        assert_eq!(mutation_operation(&bogus), SyncdOperation::Set);
    }

    #[test]
    fn error_messages_are_descriptive() {
        assert_eq!(
            AppStateError::InvalidHashLength { expected: 128, actual: 64 }.to_string(),
            "invalid LTHash length: expected 128 bytes, got 64"
        );
        assert_eq!(AppStateError::KeyNotFound.to_string(), "didn't find app state key");
        assert_eq!(AppStateError::MismatchingPatchMac.to_string(), "mismatching patch MAC");
    }
}
