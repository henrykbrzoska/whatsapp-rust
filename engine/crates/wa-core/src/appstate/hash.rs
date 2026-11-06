// Ported from the Go client (`appstate/hash.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! App state MACs: [`HashState`], snapshot MACs, patch MACs, content MACs and
//! index MACs.
//!
//! Go counterparts: `HashState` (plus `updateHash`), `generateSnapshotMAC`,
//! `generatePatchMAC`, `generateContentMAC` and `concatAndHMAC` in
//! `Go appstate/hash.go`.
//!
//! A snapshot MAC binds the collection's LTHash + version + name; a patch MAC
//! binds the snapshot MAC, every mutation's value MAC, the new version and the
//! name; a content MAC authenticates one encrypted mutation; the index MAC
//! authenticates the JSON index of one mutation. All three use HMAC-SHA256
//! except the content MAC, which is a truncated HMAC-SHA512.

use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha512};
use wa_proto::wa_web_protobufs_server_sync::{SyncdMutation, SyncdPatch};

use super::lthash::WAPATCH_INTEGRITY;
use super::{AppStateError, SyncdOperation, WAPatchName, mutation_operation};

/// Length of an LTHash digest (`HashState.hash`).
pub const HASH_LEN: usize = 128;
/// Length of a value/content MAC (the trailing bytes of an encrypted value blob).
pub const VALUE_MAC_LEN: usize = 32;
/// Length of a snapshot MAC.
pub const SNAPSHOT_MAC_LEN: usize = 32;
/// Length of a patch MAC.
pub const PATCH_MAC_LEN: usize = 32;
/// Length of the truncated index/content MAC.
pub const CONTENT_MAC_LEN: usize = 32;

/// A non-fatal problem found while updating the hash.
///
/// Go appends plain `error` values to a `[]error` and logs them; the variants
/// here can be turned into an [`AppStateError`] with
/// [`AppStateWarning::as_error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStateWarning {
    /// `ErrMissingPreviousSetValueOperation` — a REMOVE whose index has no known
    /// previous value MAC (e.g. removing contact access for your own JID).
    MissingPreviousSetValueOperation { index_mac: Vec<u8> },
}

impl AppStateWarning {
    /// The error this warning corresponds to.
    pub fn as_error(&self) -> AppStateError {
        match self {
            Self::MissingPreviousSetValueOperation { index_mac } => {
                AppStateError::MissingPreviousSetValueOperation { index_mac: hex::encode(index_mac) }
            }
        }
    }
}

impl std::fmt::Display for AppStateWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_error().to_string())
    }
}

/// `HashState` — the LTHash of one collection plus its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashState {
    /// The state version the hash belongs to.
    pub version: u64,
    /// The 128-byte LTHash of every value MAC currently in the collection.
    pub hash: [u8; HASH_LEN],
}

impl Default for HashState {
    fn default() -> Self {
        Self::new()
    }
}

impl HashState {
    /// An empty collection at version 0 — the correct initial state.
    pub fn new() -> Self {
        Self { version: 0, hash: [0; HASH_LEN] }
    }

    /// Builds a state from a stored `(version, hash)` pair.
    pub fn from_parts(version: u64, hash: [u8; HASH_LEN]) -> Self {
        Self { version, hash }
    }

    /// `updateHash` — folds the mutations into the LTHash.
    ///
    /// `get_prev_value_mac(index_mac, max_index)` returns the value MAC of the
    /// previous `SET` for the same index (from the current patch or the store),
    /// or `None` when the index has never been set.
    ///
    /// Fatal problems are [`AppStateError`]s; problems Go only warns about are
    /// returned as [`AppStateWarning`]s.
    pub fn update_hash<F>(
        &mut self,
        mutations: &[SyncdMutation],
        mut get_prev_value_mac: F,
    ) -> Result<Vec<AppStateWarning>, AppStateError>
    where
        F: FnMut(&[u8], usize) -> Result<Option<Vec<u8>>, AppStateError>,
    {
        let mut added: Vec<Vec<u8>> = Vec::new();
        let mut removed: Vec<Vec<u8>> = Vec::new();
        let mut warnings: Vec<AppStateWarning> = Vec::new();

        for (index, mutation) in mutations.iter().enumerate() {
            let operation = mutation_operation(mutation);
            let record = mutation.record.as_ref().ok_or(AppStateError::InvalidMutation("mutation has no record"))?;

            if operation == SyncdOperation::Set {
                let value = record
                    .value
                    .as_ref()
                    .and_then(|value| value.blob.as_deref())
                    .ok_or(AppStateError::InvalidMutation("SET mutation has no value blob"))?;
                added.push(value_mac(value)?.to_vec());
            }

            let index_mac = record.index.as_ref().and_then(|index| index.blob.as_deref()).unwrap_or_default();
            match get_prev_value_mac(index_mac, index)? {
                Some(removal) => removed.push(removal),
                None if operation == SyncdOperation::Remove => {
                    warnings.push(AppStateWarning::MissingPreviousSetValueOperation { index_mac: index_mac.to_vec() })
                }
                None => {}
            }
        }

        let removed_refs: Vec<&[u8]> = removed.iter().map(Vec::as_slice).collect();
        let added_refs: Vec<&[u8]> = added.iter().map(Vec::as_slice).collect();
        WAPATCH_INTEGRITY.subtract_then_add_in_place(&mut self.hash, &removed_refs, &added_refs)?;
        Ok(warnings)
    }

    /// `generateSnapshotMAC` — `HMAC-SHA256(key, hash || be64(version) || name)`.
    pub fn generate_snapshot_mac(&self, name: &WAPatchName, key: &[u8]) -> Vec<u8> {
        concat_and_hmac_sha256(key, &[&self.hash, &self.version.to_be_bytes(), name.as_str().as_bytes()])
    }

    /// Verifies a snapshot MAC (the `SyncdSnapshot.mac` / `SyncdPatch.snapshotMAC`).
    pub fn verify_snapshot_mac(&self, name: &WAPatchName, key: &[u8], expected: &[u8]) -> Result<(), AppStateError> {
        verify_snapshot_mac(self, name, key, expected)
    }
}

/// Verifies `HomeStateSnapshotMAC`-style MACs against [`AppStateError::MismatchingLtHash`].
///
/// Go calls this `validateSnapshotMAC`; the key is the collection's
/// `SnapshotMAC` key from [`super::ExpandedAppStateKeys`].
pub fn verify_snapshot_mac(
    state: &HashState,
    name: &WAPatchName,
    key: &[u8],
    expected: &[u8],
) -> Result<(), AppStateError> {
    let snapshot_mac = state.generate_snapshot_mac(name, key);
    if constant_time_eq(&snapshot_mac, expected) { Ok(()) } else { Err(AppStateError::MismatchingLtHash) }
}

/// `generatePatchMAC` — `HMAC-SHA256(key, snapshotMAC || valueMAC* || be64(version) || name)`.
pub fn generate_patch_mac(
    patch: &SyncdPatch,
    name: &WAPatchName,
    key: &[u8],
    version: u64,
) -> Result<Vec<u8>, AppStateError> {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(patch.mutations.len() + 3);
    parts.push(patch.snapshot_mac.as_deref().unwrap_or_default());
    let value_macs = patch
        .mutations
        .iter()
        .map(|mutation| {
            let value = mutation
                .record
                .as_ref()
                .and_then(|record| record.value.as_ref())
                .and_then(|value| value.blob.as_deref())
                .ok_or(AppStateError::InvalidMutation("patch mutation has no value blob"))?;
            value_mac(value)
        })
        .collect::<Result<Vec<&[u8]>, AppStateError>>()?;
    parts.extend(value_macs);
    let version_bytes = version.to_be_bytes();
    parts.push(&version_bytes);
    parts.push(name.as_str().as_bytes());
    Ok(concat_and_hmac_sha256(key, &parts))
}

/// Verifies a patch MAC, returning [`AppStateError::MismatchingPatchMac`] on failure.
///
/// Go calls this inside `validatePatch`; the key is the collection's `PatchMAC`
/// key from [`super::ExpandedAppStateKeys`].
pub fn verify_patch_mac(patch: &SyncdPatch, name: &WAPatchName, key: &[u8], version: u64) -> Result<(), AppStateError> {
    let expected = patch.patch_mac.as_deref().ok_or(AppStateError::MismatchingPatchMac)?;
    let actual = generate_patch_mac(patch, name, key, version)?;
    if constant_time_eq(&actual, expected) { Ok(()) } else { Err(AppStateError::MismatchingPatchMac) }
}

/// `generateContentMAC` — truncated (`32`-byte) `HMAC-SHA512` over
/// `[operation + 1] || keyID || data || be64(len(keyID) + 1)`.
pub fn generate_content_mac(operation: SyncdOperation, data: &[u8], key_id: &[u8], key: &[u8]) -> Vec<u8> {
    let operation_byte = [operation as u8 + 1];
    let key_data_length = ((key_id.len() as u64) + 1).to_be_bytes();
    let mut mac = match Hmac::<Sha512>::new_from_slice(key) {
        Ok(mac) => mac,
        Err(_) => {
            // Unreachable: HMAC accepts keys of any length, but never panic.
            tracing::warn!("appstate: could not build the content-MAC key");
            return Vec::new();
        }
    };
    mac.update(&operation_byte);
    mac.update(key_id);
    mac.update(data);
    mac.update(&key_data_length);
    mac.finalize().into_bytes()[..CONTENT_MAC_LEN].to_vec()
}

/// `concatAndHMAC(sha256.New, key, ...)` — the index MAC and all SHA-256 MACs.
pub fn concat_and_hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut mac = match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mac) => mac,
        Err(_) => {
            tracing::warn!("appstate: could not build an HMAC-SHA256 key");
            return Vec::new();
        }
    };
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().to_vec()
}

/// The index MAC of a mutation: `HMAC-SHA256(keys.index, index_json)`.
pub fn generate_index_mac(key: &[u8], index_json: &[u8]) -> Vec<u8> {
    concat_and_hmac_sha256(key, &[index_json])
}

/// The trailing [`VALUE_MAC_LEN`] bytes of an encrypted value blob.
fn value_mac(value: &[u8]) -> Result<&[u8], AppStateError> {
    if value.len() < VALUE_MAC_LEN {
        return Err(AppStateError::InvalidMutation("value blob is shorter than its value MAC"));
    }
    Ok(&value[value.len() - VALUE_MAC_LEN..])
}

/// Compares two byte strings without an early exit on the first differing byte.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_proto::wa_web_protobufs_server_sync::{
        KeyId, SyncdIndex, SyncdMutation, SyncdPatch, SyncdRecord, SyncdValue, syncd_mutation::SyncdOperation as Op,
    };

    fn hex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("test fixture is valid hex")
    }

    fn hash_state_with_version(version: u64) -> HashState {
        let mut hash = [0u8; HASH_LEN];
        for (i, byte) in hash.iter_mut().enumerate() {
            *byte = i as u8;
        }
        HashState::from_parts(version, hash)
    }

    fn snapshot_key() -> Vec<u8> {
        (0..32).map(|i| 0x10u8.wrapping_add(i)).collect()
    }

    fn patch_key() -> Vec<u8> {
        (0..32).map(|i| 0x40u8.wrapping_add(i)).collect()
    }

    fn value_mac_seed() -> Vec<u8> {
        (0..32).map(|i| 0x70u8.wrapping_add(i)).collect()
    }

    fn mutation(op: Op, index_mac: &[u8], value: &[u8]) -> SyncdMutation {
        SyncdMutation {
            operation: Some(op as i32),
            record: Some(SyncdRecord {
                index: Some(SyncdIndex { blob: Some(index_mac.to_vec()) }),
                value: Some(SyncdValue { blob: Some(value.to_vec()) }),
                key_id: Some(KeyId { id: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]) }),
            }),
        }
    }

    /// A value blob with a distinct 32-byte MAC trailer.
    fn blob_with_mac(tag: u8) -> Vec<u8> {
        let mut blob = vec![tag; 16];
        blob.extend((0..32).map(|i| tag.wrapping_add(i)));
        blob
    }

    #[test]
    fn go_vector_snapshot_mac() {
        // Go: generateSnapshotMAC(hash{0..127}, version 7, "regular_low", snapshotKey)
        let state = hash_state_with_version(7);
        let mac = state.generate_snapshot_mac(&WAPatchName::new(WAPatchName::REGULAR_LOW), &snapshot_key());
        assert_eq!(hex::encode(&mac), "3a33140ea8a72e7625de8310a681bda925a7693dfb561c5e1a14529d4e775e9f");
    }

    #[test]
    fn go_vector_patch_mac() {
        // Go: generatePatchMAC(snapshotMAC, []valueMAC{valueMACSeed, 0x01..0x20}, 8, "regular_low", patchKey)
        let snapshot_mac = hex("3a33140ea8a72e7625de8310a681bda925a7693dfb561c5e1a14529d4e775e9f");
        let first = mutation(Op::Set, b"index-1", &value_mac_seed());
        let second_value: Vec<u8> = (1..=32).map(|i| i as u8).collect();
        let second = mutation(Op::Set, b"index-2", &second_value);
        let patch = SyncdPatch {
            snapshot_mac: Some(snapshot_mac),
            mutations: vec![first, second],
            patch_mac: None,
            key_id: None,
            version: None,
            external_mutations: None,
            exit_code: None,
            device_index: None,
            client_debug_data: None,
        };
        let mac = generate_patch_mac(&patch, &WAPatchName::new(WAPatchName::REGULAR_LOW), &patch_key(), 8)
            .expect("patch mac");
        assert_eq!(hex::encode(&mac), "ca7d54b8ff91f1caab65bd01c12bde32eaf53ca320bcab562304580d5e9e61a5");
    }

    #[test]
    fn go_vector_content_mac_set_and_remove() {
        // Go: generateContentMAC(SET/REMOVE, "encrypted content", keyID, valueMACKey)
        let key_id = hex("deadbeef");
        let key = (0..32).map(|i| 0x20u8.wrapping_add(i)).collect::<Vec<u8>>();
        assert_eq!(
            hex::encode(generate_content_mac(SyncdOperation::Set, b"encrypted content", &key_id, &key)),
            "811f174388ff88c639b68ed3bbc22933403211203f73fb0cf050e9d4b6cec844"
        );
        assert_eq!(
            hex::encode(generate_content_mac(SyncdOperation::Remove, b"encrypted content", &key_id, &key)),
            "c2bc04c162571bd97cc170f6cf6df38ad447b408e61152a996fc499408ff1c3c"
        );
    }

    #[test]
    fn go_vector_index_mac() {
        // Go: concatAndHMAC(sha256.New, expanded[0:32], indexJSON)
        let key = hex("afd50b8af2fd7406de936c7f3c66a0f62fe9347f6e4960a0c817e6244f209e98");
        let index_json = hex("5b226d757465222c223132333440732e77686174736170702e6e6574225d");
        assert_eq!(
            hex::encode(generate_index_mac(&key, &index_json)),
            "298c3924effcf768b7e3f01794bef4773cec83c3fabd95a527ff0136cd4b447d"
        );
    }

    #[test]
    fn snapshot_mac_verify_accepts_and_rejects() {
        let state = hash_state_with_version(7);
        let name = WAPatchName::new(WAPatchName::REGULAR_LOW);
        let mac = state.generate_snapshot_mac(&name, &snapshot_key());
        assert!(state.verify_snapshot_mac(&name, &snapshot_key(), &mac).is_ok());
        assert!(matches!(
            state.verify_snapshot_mac(&name, &snapshot_key(), &mac[..31]),
            Err(AppStateError::MismatchingLtHash)
        ));

        let mut tampered = mac.clone();
        tampered[0] ^= 1;
        assert!(matches!(
            state.verify_snapshot_mac(&name, &snapshot_key(), &tampered),
            Err(AppStateError::MismatchingLtHash)
        ));

        // A different key, name or version must not verify either.
        assert!(matches!(state.verify_snapshot_mac(&name, &[0u8; 32], &mac), Err(AppStateError::MismatchingLtHash)));
        assert!(matches!(
            state.verify_snapshot_mac(&WAPatchName::new(WAPatchName::REGULAR_HIGH), &snapshot_key(), &mac),
            Err(AppStateError::MismatchingLtHash)
        ));
        let mut newer = state.clone();
        newer.version = 8;
        assert!(matches!(
            newer.verify_snapshot_mac(&name, &snapshot_key(), &mac),
            Err(AppStateError::MismatchingLtHash)
        ));
    }

    #[test]
    fn patch_mac_verify_accepts_and_rejects() {
        let name = WAPatchName::new(WAPatchName::REGULAR_LOW);
        let mut patch = SyncdPatch {
            snapshot_mac: Some(hex("3a33140ea8a72e7625de8310a681bda925a7693dfb561c5e1a14529d4e775e9f")),
            mutations: vec![mutation(Op::Set, b"index-1", &value_mac_seed())],
            ..Default::default()
        };
        let mac = generate_patch_mac(&patch, &name, &patch_key(), 8).expect("patch mac");
        patch.patch_mac = Some(mac.clone());
        assert!(verify_patch_mac(&patch, &name, &patch_key(), 8).is_ok());

        // Wrong version.
        assert!(matches!(verify_patch_mac(&patch, &name, &patch_key(), 9), Err(AppStateError::MismatchingPatchMac)));
        // Wrong key.
        assert!(matches!(verify_patch_mac(&patch, &name, &[0u8; 32], 8), Err(AppStateError::MismatchingPatchMac)));
        // Tampered mutation set (a mutation was dropped).
        let mut tampered = patch.clone();
        tampered.mutations.clear();
        assert!(matches!(verify_patch_mac(&tampered, &name, &patch_key(), 8), Err(AppStateError::MismatchingPatchMac)));
        // Missing MAC.
        let mut missing = patch.clone();
        missing.patch_mac = None;
        assert!(matches!(verify_patch_mac(&missing, &name, &patch_key(), 8), Err(AppStateError::MismatchingPatchMac)));
    }

    #[test]
    fn update_hash_adds_sets_and_subtracts_known_removals() {
        let set_one = mutation(Op::Set, b"index-1", &blob_with_mac(1));
        let mut state = HashState::new();
        let warnings = state.update_hash(std::slice::from_ref(&set_one), |_, _| Ok(None)).expect("update hash");
        assert!(warnings.is_empty());
        let after_add = state.hash;
        assert_ne!(after_add, [0u8; HASH_LEN]);

        // Removing the same index again goes back to the empty hash.
        let added_blob = blob_with_mac(1);
        let prev_mac = added_blob[added_blob.len() - VALUE_MAC_LEN..].to_vec();
        let remove = mutation(Op::Remove, b"index-1", &[]);
        let warnings = state
            .update_hash(std::slice::from_ref(&remove), move |index_mac, _| {
                assert_eq!(index_mac, b"index-1");
                Ok(Some(prev_mac.clone()))
            })
            .expect("update hash");
        assert!(warnings.is_empty());
        assert_eq!(state.hash, [0u8; HASH_LEN]);
    }

    #[test]
    fn update_hash_warns_when_a_removal_has_no_previous_value() {
        let mut state = HashState::new();
        let remove = mutation(Op::Remove, b"own-jid-index", &[]);
        let warnings = state.update_hash(std::slice::from_ref(&remove), |_, _| Ok(None)).expect("update hash");
        assert_eq!(
            warnings,
            vec![AppStateWarning::MissingPreviousSetValueOperation { index_mac: b"own-jid-index".to_vec() }]
        );
        assert_eq!(
            warnings[0].to_string(),
            "missing value MAC of previous SET operation for 6f776e2d6a69642d696e646578"
        );
        assert!(matches!(warnings[0].as_error(), AppStateError::MissingPreviousSetValueOperation { .. }));
    }

    #[test]
    fn update_hash_rejects_malformed_mutations() {
        let mut state = HashState::new();
        let no_record = SyncdMutation { operation: Some(Op::Set as i32), record: None };
        assert!(matches!(
            state.update_hash(&[no_record], |_, _| Ok(None)),
            Err(AppStateError::InvalidMutation("mutation has no record"))
        ));

        let short_value = mutation(Op::Set, b"index-1", &[0u8; 4]);
        assert!(matches!(
            state.update_hash(&[short_value], |_, _| Ok(None)),
            Err(AppStateError::InvalidMutation("value blob is shorter than its value MAC"))
        ));

        let no_value = SyncdMutation {
            operation: Some(Op::Set as i32),
            record: Some(SyncdRecord {
                index: Some(SyncdIndex { blob: Some(b"index-1".to_vec()) }),
                value: None,
                key_id: None,
            }),
        };
        assert!(matches!(
            state.update_hash(&[no_value], |_, _| Ok(None)),
            Err(AppStateError::InvalidMutation("SET mutation has no value blob"))
        ));
    }

    #[test]
    fn update_hash_propagates_store_errors_and_uses_the_patch_order() {
        let mut state = HashState::new();
        let set = mutation(Op::Set, b"index-1", &blob_with_mac(1));
        let err = state.update_hash(std::slice::from_ref(&set), |_, _| Err(AppStateError::KeyNotFound));
        assert!(matches!(err, Err(AppStateError::KeyNotFound)));

        // The closure sees one call per mutation, with the mutation's index.
        let mut seen = Vec::new();
        let mut fresh = HashState::new();
        fresh
            .update_hash(&[set.clone(), set], |index_mac, max_index| {
                seen.push((index_mac.to_vec(), max_index));
                Ok(None)
            })
            .expect("update hash");
        assert_eq!(seen, vec![(b"index-1".to_vec(), 0), (b"index-1".to_vec(), 1)]);
    }

    #[test]
    fn constant_time_compare_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
