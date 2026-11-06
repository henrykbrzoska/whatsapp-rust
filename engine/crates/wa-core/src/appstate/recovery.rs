// Ported from the Go client (`appstate/recovery.go`, `appstate/decode.go`), © Tulir
// Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Snapshot recovery and patch verification.
//!
//! Go counterparts: `ParseRecovery`, `Processor.ProcessRecovery`
//! (`Go appstate/recovery.go`) and `Processor.validatePatch`
//! (`Go appstate/decode.go`).
//!
//! There are two ways back to a consistent collection state:
//!
//! * **Patches** — [`verify_patch`] folds one `SyncdPatch` into the current
//!   [`HashState`], verifies the snapshot MAC and the patch MAC, and returns the
//!   new state. Nothing is written; the caller decrypts and applies mutations
//!   only after every MAC verified.
//! * **Snapshots** — the phone sends the whole collection (unencrypted but
//!   MAC'd per record) when the patch chain cannot be replayed any more.
//!   [`SnapshotRecovery::process`] verifies that the records add up to the
//!   collection LTHash, then replaces the stored version/LTHash and the
//!   mutation MACs.
//!
//! Both paths converge on the same 128-byte LTHash — that is exactly what the
//! tests assert: a collection rebuilt from a snapshot has the hash the phone
//! reported, and a patch on top of it lands on the hash the encoder computed.

use std::collections::HashMap;

use anyhow::Result;
use prost::Message;
use wa_proto::wa_web_protobuf_sync_action::SyncActionData;
use wa_proto::wa_web_protobufs_server_sync::{SyncdMutation, SyncdPatch, syncd_mutation::SyncdOperation};
use wa_proto::wa_web_protobufs_syncd_snapshot_recovery::SyncdSnapshotRecovery;

use crate::store::traits::{AppStateMutationMac, AppStateStore, AppStateSyncKeyStore};

use super::decode::decode_index_json;
use super::hash::{
    AppStateWarning, HASH_LEN, HashState, VALUE_MAC_LEN, generate_index_mac, verify_patch_mac, verify_snapshot_mac,
};
use super::keys::AppStateKeyCache;
use super::{AppStateError, ExpandedAppStateKeys, Mutation, WAPatchName};

/// `SyncdPlainTextRecord` of a snapshot, before its index MAC is known.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotRecoveryRecord {
    /// The plaintext `SyncActionData` the record carries.
    pub value: SyncActionData,
    /// The sync key ID the record was MAC'd with.
    pub key_id: Vec<u8>,
    /// The record's value MAC (32 bytes).
    pub mac: Vec<u8>,
}

/// A snapshot after its plaintext records were turned into mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRecord {
    /// The decoded index, e.g. `["mute", "123@s.whatsapp.net"]`.
    pub index: Vec<String>,
    /// The sync key ID of the record.
    pub key_id: Vec<u8>,
    /// `HMAC-SHA256(keys.index, index_json)`.
    pub index_mac: Vec<u8>,
    /// The record's value MAC.
    pub value_mac: Vec<u8>,
}

/// The outcome of [`SnapshotRecovery::process`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredSnapshot {
    /// The recovered collection.
    pub name: WAPatchName,
    /// The state the snapshot represents (version + LTHash).
    pub state: HashState,
    /// The mutations to apply to the local stores, in snapshot order.
    pub mutations: Vec<Mutation>,
    /// The MACs that were stored for later patches.
    pub records: Vec<RecoveredRecord>,
}

/// A `SyncdSnapshotRecovery` message, parsed and ready to be applied.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotRecovery {
    /// The collection being recovered.
    pub name: WAPatchName,
    /// The version the snapshot corresponds to.
    pub version: u64,
    /// The collection LTHash the records must add up to.
    pub collection_lthash: [u8; HASH_LEN],
    /// The plaintext records, in wire order.
    pub records: Vec<SnapshotRecoveryRecord>,
}

impl SnapshotRecovery {
    /// `ParseRecovery` — gunzips when `compressed`, then decodes the protobuf.
    pub fn parse(raw: &[u8], compressed: bool) -> Result<Self> {
        let data = if compressed { gunzip(raw)? } else { raw.to_vec() };
        let message = SyncdSnapshotRecovery::decode(data.as_slice())
            .map_err(|_| AppStateError::InvalidMutation("failed to unmarshal the snapshot recovery message"))
            .map_err(anyhow::Error::new)?;
        Self::from_message(&message)
    }

    /// Wraps a decoded `SyncdSnapshotRecovery`.
    ///
    /// A collection LTHash that is not exactly 128 bytes is rejected (Go returns
    /// `invalid lthash length: %d` there).
    pub fn from_message(message: &SyncdSnapshotRecovery) -> Result<Self> {
        let name = message
            .collection_name
            .clone()
            .ok_or(AppStateError::InvalidMutation("the snapshot recovery has no collection name"))?;
        let lthash = message
            .collection_lthash
            .as_deref()
            .ok_or(AppStateError::InvalidMutation("the snapshot recovery has no collection LTHash"))?;
        if lthash.len() != HASH_LEN {
            return Err(AppStateError::InvalidHashLength { expected: HASH_LEN, actual: lthash.len() }.into());
        }
        let mut collection_lthash = [0u8; HASH_LEN];
        collection_lthash.copy_from_slice(lthash);

        let mut records = Vec::with_capacity(message.mutation_records.len());
        for record in &message.mutation_records {
            let value = record.value.clone().ok_or(AppStateError::InvalidMutation("a snapshot record has no value"))?;
            let key_id =
                record.key_id.clone().ok_or(AppStateError::InvalidMutation("a snapshot record has no key ID"))?;
            let mac = record.mac.clone().ok_or(AppStateError::InvalidMutation("a snapshot record has no MAC"))?;
            records.push(SnapshotRecoveryRecord { value, key_id, mac });
        }

        Ok(Self {
            name: WAPatchName::new(name),
            version: message.version.and_then(|version| version.version).unwrap_or_default(),
            collection_lthash,
            records,
        })
    }

    /// `Processor.ProcessRecovery` — verifies the LTHash, replaces the stored
    /// version/LTHash and stores the mutation MACs, then returns the mutations.
    ///
    /// The whole snapshot is verified before the first write: a wrong LTHash or
    /// a missing sync key leaves the store untouched.
    pub async fn process(
        &self,
        keys: &AppStateKeyCache,
        key_store: &dyn AppStateSyncKeyStore,
        app_state: &dyn AppStateStore,
    ) -> Result<RecoveredSnapshot> {
        let mut records = Vec::with_capacity(self.records.len());
        let mut mutations = Vec::with_capacity(self.records.len());
        for record in &self.records {
            let expanded = keys.get(key_store, &record.key_id).await?;
            let index_json = record
                .value
                .index
                .as_deref()
                .ok_or(AppStateError::InvalidMutation("a snapshot record has no index JSON"))?;
            if record.mac.len() != VALUE_MAC_LEN {
                return Err(AppStateError::InvalidMutation("a snapshot record value MAC is not 32 bytes").into());
            }
            let index = decode_index_json(index_json)?;
            let index_mac = generate_index_mac(&expanded.index, index_json);
            records.push(RecoveredRecord {
                index: index.clone(),
                key_id: record.key_id.clone(),
                index_mac: index_mac.clone(),
                value_mac: record.mac.clone(),
            });
            mutations.push(Mutation {
                key_id: record.key_id.clone(),
                operation: SyncdOperation::Set,
                action: record.value.value.clone(),
                version: record.value.version.unwrap_or_default(),
                index,
                index_mac,
                value_mac: record.mac.clone(),
                patch_version: self.version,
            });
        }

        let computed = lthash_of(records.iter().map(|record| record.value_mac.as_slice()))?;
        if computed != self.collection_lthash {
            return Err(AppStateError::MismatchingLtHash.into());
        }
        let state = HashState::from_parts(self.version, computed);

        app_state
            .delete_app_state_version(self.name.as_str())
            .await
            .map_err(|err| AppStateError::Store(err.to_string()))?;
        app_state
            .put_app_state_version(self.name.as_str(), self.version, state.hash)
            .await
            .map_err(|err| AppStateError::Store(err.to_string()))?;
        let macs: Vec<AppStateMutationMac> = records
            .iter()
            .map(|record| AppStateMutationMac {
                index_mac: record.index_mac.clone(),
                value_mac: record.value_mac.clone(),
            })
            .collect();
        app_state
            .put_app_state_mutation_macs(self.name.as_str(), self.version, &macs)
            .await
            .map_err(|err| AppStateError::Store(err.to_string()))?;

        Ok(RecoveredSnapshot { name: self.name.clone(), state, mutations, records })
    }
}

/// The outcome of [`verify_patch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPatch {
    /// The collection state after this patch; `version` is the patch's version.
    pub state: HashState,
    /// Non-fatal hash problems (a REMOVE without a known previous `SET`).
    pub warnings: Vec<AppStateWarning>,
}

/// `Processor.validatePatch` — folds `patch` into `current` and verifies the
/// snapshot MAC and the patch MAC. Writes nothing.
pub async fn verify_patch(
    name: &WAPatchName,
    patch: &SyncdPatch,
    current: &HashState,
    keys: &ExpandedAppStateKeys,
    app_state: &dyn AppStateStore,
    validate_macs: bool,
) -> Result<VerifiedPatch> {
    verify_patch_chained(name, patch, current, keys, app_state, validate_macs, &HashMap::new()).await
}

/// [`verify_patch`] with an extra map of value MACs that are not in the store
/// yet — the MACs of the patches already verified in this batch.
///
/// Without it a chain of patches in one response would resolve its previous
/// values from a store that has not been written yet (the writes only happen
/// after every MAC of the batch verified).
pub async fn verify_patch_chained(
    name: &WAPatchName,
    patch: &SyncdPatch,
    current: &HashState,
    keys: &ExpandedAppStateKeys,
    app_state: &dyn AppStateStore,
    validate_macs: bool,
    known: &HashMap<Vec<u8>, Option<Vec<u8>>>,
) -> Result<VerifiedPatch> {
    // A patch without an explicit version is the next version of the collection
    // (the encoder leaves it unset and puts the incremented version into the
    // snapshot/patch MACs).
    let version =
        patch.version.as_ref().and_then(|version| version.version).unwrap_or_else(|| current.version.saturating_add(1));

    let mut previous: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    for (position, mutation) in patch.mutations.iter().enumerate() {
        let Some(index_mac) = index_mac_of_mutation(mutation)? else {
            continue;
        };
        if previous.contains_key(index_mac) {
            continue;
        }
        let mut in_patch: Option<Option<Vec<u8>>> = None;
        for earlier in patch.mutations.iter().take(position) {
            if index_mac_of_mutation(earlier)? == Some(index_mac) {
                in_patch = Some(match mutation_operation_of(earlier) {
                    SyncdOperation::Set => Some(value_mac_of_mutation(earlier)?.to_vec()),
                    // A REMOVE leaves no previous value behind.
                    SyncdOperation::Remove => None,
                });
            }
        }
        let resolved = match in_patch {
            Some(resolved) => resolved,
            None => match known.get(index_mac) {
                Some(resolved) => resolved.clone(),
                None => app_state
                    .get_app_state_mutation_mac(name.as_str(), index_mac)
                    .await
                    .map_err(|err| AppStateError::Store(err.to_string()))?,
            },
        };
        previous.insert(index_mac.to_vec(), resolved);
    }

    let mut state = current.clone();
    state.version = version;
    let warnings =
        state.update_hash(&patch.mutations, |index_mac, _max_index| Ok(previous.get(index_mac).cloned().flatten()))?;

    if validate_macs {
        let expected_snapshot_mac =
            patch.snapshot_mac.as_deref().ok_or(AppStateError::InvalidMutation("the patch has no snapshot MAC"))?;
        verify_snapshot_mac(&state, name, &keys.snapshot_mac, expected_snapshot_mac)?;
        if verify_patch_mac(patch, name, &keys.patch_mac, version).is_err() {
            return Err(AppStateError::MismatchingPatchMac.into());
        }
    }

    Ok(VerifiedPatch { state, warnings })
}

/// `Processor.decodeSnapshot`'s first half — the collection state a snapshot
/// represents, verified against the snapshot MAC.
///
/// A `SyncdSnapshot` carries *encrypted* records (the same `SyncdMutation` shape
/// as a patch), so the hash is folded over them as if they were `SET`s, with no
/// previous value.
pub fn verify_snapshot(
    name: &WAPatchName,
    snapshot: &wa_proto::wa_web_protobufs_server_sync::SyncdSnapshot,
    keys: &ExpandedAppStateKeys,
    validate_macs: bool,
) -> Result<VerifiedPatch> {
    let version = snapshot.version.as_ref().and_then(|version| version.version).unwrap_or_default();
    let mutations: Vec<SyncdMutation> = snapshot
        .records
        .iter()
        .map(|record| SyncdMutation { operation: Some(SyncdOperation::Set as i32), record: Some(record.clone()) })
        .collect();

    let mut state = HashState::new();
    state.version = version;
    let warnings = state.update_hash(&mutations, |_index_mac, _max_index| Ok(None))?;

    if validate_macs {
        let expected = snapshot.mac.as_deref().ok_or(AppStateError::InvalidMutation("the snapshot has no MAC"))?;
        verify_snapshot_mac(&state, name, &keys.snapshot_mac, expected)?;
    }
    Ok(VerifiedPatch { state, warnings })
}

/// The LTHash of a multiset of value MACs (their order does not matter).
///
/// This is the value a recovered snapshot must add up to; it is what makes the
/// snapshot, the patch chain and [`HashState`] converge.
pub fn lthash_of<'a>(value_macs: impl Iterator<Item = &'a [u8]>) -> Result<[u8; HASH_LEN], AppStateError> {
    let macs: Vec<&[u8]> = value_macs.collect();
    let mut hash = [0u8; HASH_LEN];
    super::lthash::WAPATCH_INTEGRITY.subtract_then_add_in_place(&mut hash, &[], &macs)?;
    Ok(hash)
}

/// The index MAC of a mutation, when its record has one.
pub(crate) fn index_mac_of_mutation(mutation: &SyncdMutation) -> Result<Option<&[u8]>, AppStateError> {
    let record = mutation.record.as_ref().ok_or(AppStateError::InvalidMutation("mutation has no record"))?;
    Ok(record.index.as_ref().and_then(|index| index.blob.as_deref()))
}

/// The trailing 32 bytes of a mutation's value blob (its value MAC).
pub(crate) fn value_mac_of_mutation(mutation: &SyncdMutation) -> Result<&[u8], AppStateError> {
    let record = mutation.record.as_ref().ok_or(AppStateError::InvalidMutation("mutation has no record"))?;
    let blob = record
        .value
        .as_ref()
        .and_then(|value| value.blob.as_deref())
        .ok_or(AppStateError::InvalidMutation("mutation has no value blob"))?;
    if blob.len() < VALUE_MAC_LEN {
        return Err(AppStateError::InvalidMutation("value blob is shorter than its value MAC"));
    }
    Ok(&blob[blob.len() - VALUE_MAC_LEN..])
}

/// The operation of a mutation, defaulting to `SET` like Go's getters.
pub(crate) fn mutation_operation_of(mutation: &SyncdMutation) -> SyncdOperation {
    mutation.operation.and_then(|raw| SyncdOperation::try_from(raw).ok()).unwrap_or(SyncdOperation::Set)
}

/// `gzip.NewReader` + `io.ReadAll`.
fn gunzip(raw: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;

    let mut decoder = flate2::read::GzDecoder::new(raw);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|err| AppStateError::Store(format!("failed to gunzip the snapshot recovery: {err}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use wa_proto::wa_web_protobuf_sync_action::{MuteAction, SyncActionData, SyncActionValue};
    use wa_proto::wa_web_protobufs_syncd_snapshot_recovery::{
        SyncdPlainTextRecord, SyncdSnapshotRecovery as SnapshotRecoveryProto, SyncdVersion as RecoveryVersion,
    };

    use super::*;
    use crate::appstate::keys::store_generated_app_state_sync_key;
    use crate::appstate::{MutationInfo, PatchInfo, encode_mutation, encode_patch};
    use crate::store::sqlite::{Container, SqlStore};
    use crate::types::jid::Jid;

    /// A `SqlStore` for a persisted device (the app state rows are foreign-keyed
    /// onto `wa_device`).
    async fn store() -> std::sync::Arc<SqlStore> {
        let container = Container::open_memory().await.expect("open memory db");
        let mut device = container.new_device();
        device.id = Some(Jid::new_ad("1", 0, 1));
        device.lid = Jid::new_ad("91", 1, 1);
        device.account = Some(wa_proto::wa_adv::AdvSignedDeviceIdentity {
            details: Some(b"details".to_vec()),
            account_signature_key: Some(vec![1; 32]),
            account_signature: Some(vec![2; 64]),
            device_signature: Some(vec![3; 64]),
        });
        device.save().await.expect("save the device");
        std::sync::Arc::new(SqlStore::new(container.pool().clone(), &Jid::new_ad("1", 0, 1)))
    }

    fn mute_value(muted: bool) -> SyncActionValue {
        SyncActionValue {
            mute_action: Some(MuteAction { muted: Some(muted), mute_end_timestamp: Some(0), ..Default::default() }),
            ..Default::default()
        }
    }

    fn mute_info(chat: &str, muted: bool) -> MutationInfo {
        MutationInfo { index: vec!["mute".into(), chat.into()], version: 2, value: mute_value(muted) }
    }

    #[tokio::test]
    async fn snapshot_recovery_converges_to_the_snapshot_hash() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(31);
        let key = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("key");
        let expanded = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");

        let mut encoded = Vec::new();
        for chat in ["111@s.whatsapp.net", "222@s.whatsapp.net", "333@s.whatsapp.net"] {
            encoded.push(
                encode_mutation(&mute_info(chat, true), &key.key_id, &expanded, 1_700_000_000_000).expect("encode"),
            );
        }
        let records: Vec<SnapshotRecoveryRecord> = encoded
            .iter()
            .map(|mutation| SnapshotRecoveryRecord {
                value: SyncActionData {
                    index: Some(mutation.index_json.clone()),
                    value: Some(mute_value(true)),
                    padding: Some(Vec::new()),
                    version: Some(2),
                },
                key_id: key.key_id.clone(),
                mac: mutation.value_mac.clone(),
            })
            .collect();
        let expected_hash = lthash_of(records.iter().map(|record| record.mac.as_slice())).expect("lthash");

        let recovery = SnapshotRecovery {
            name: WAPatchName::REGULAR_HIGH.into(),
            version: 7,
            collection_lthash: expected_hash,
            records,
        };
        let cache = AppStateKeyCache::new();
        let recovered = recovery.process(&cache, store.as_ref(), store.as_ref()).await.expect("recover");

        assert_eq!(recovered.state.hash, expected_hash, "the recovered hash is the snapshot hash");
        assert_eq!(recovered.state.version, 7);
        assert_eq!(recovered.mutations.len(), 3);
        assert_eq!(recovered.mutations[0].index, vec!["mute".to_string(), "111@s.whatsapp.net".to_string()]);
        assert_eq!(recovered.mutations[0].operation, SyncdOperation::Set);
        assert_eq!(recovered.mutations[0].patch_version, 7);

        let (version, hash) = store.get_app_state_version(WAPatchName::REGULAR_HIGH).await.expect("version");
        assert_eq!((version, hash), (7, expected_hash));
        let stored_mac = store
            .get_app_state_mutation_mac(WAPatchName::REGULAR_HIGH, &recovered.records[0].index_mac)
            .await
            .expect("mac")
            .expect("the MAC was stored");
        assert_eq!(stored_mac, recovered.records[0].value_mac);

        // A patch encoded on top of the empty state verifies against the encoder.
        let patch = PatchInfo {
            timestamp_millis: 1_700_000_000_000,
            name: WAPatchName::REGULAR_HIGH.into(),
            mutations: vec![mute_info("444@s.whatsapp.net", true)],
        };
        let encoded_patch =
            encode_patch(&key.key_id, &expanded, &HashState::new(), &patch, |_, _| Ok(None)).expect("encode patch");
        let verified = verify_patch(
            &WAPatchName::REGULAR_HIGH.into(),
            &encoded_patch.patch,
            &HashState::new(),
            &expanded,
            store.as_ref(),
            true,
        )
        .await
        .expect("the encoder and the verifier must agree");
        assert_eq!(verified.state.hash, encoded_patch.state.hash);
        assert_eq!(verified.state.version, 1);
        assert!(verified.warnings.is_empty());
    }

    #[tokio::test]
    async fn a_wrong_collection_lthash_is_rejected_before_any_write() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(41);
        let key = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("key");
        let expanded = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let mutation =
            encode_mutation(&mute_info("111@s.whatsapp.net", true), &key.key_id, &expanded, 1).expect("encode");

        let recovery = SnapshotRecovery {
            name: WAPatchName::REGULAR_HIGH.into(),
            version: 3,
            collection_lthash: [9u8; HASH_LEN],
            records: vec![SnapshotRecoveryRecord {
                value: SyncActionData {
                    index: Some(mutation.index_json.clone()),
                    value: Some(mute_value(true)),
                    padding: Some(Vec::new()),
                    version: Some(2),
                },
                key_id: key.key_id.clone(),
                mac: mutation.value_mac.clone(),
            }],
        };
        let cache = AppStateKeyCache::new();
        let err = recovery.process(&cache, store.as_ref(), store.as_ref()).await.expect_err("must be rejected");
        assert!(matches!(err.downcast_ref::<AppStateError>(), Some(AppStateError::MismatchingLtHash)));

        let (version, hash) = store.get_app_state_version(WAPatchName::REGULAR_HIGH).await.expect("version");
        assert_eq!((version, hash), (0, [0u8; HASH_LEN]), "nothing may be written after a failed verification");
    }

    #[tokio::test]
    async fn a_snapshot_with_an_unknown_sync_key_is_rejected() {
        let store = store().await;
        let recovery = SnapshotRecovery {
            name: WAPatchName::REGULAR_HIGH.into(),
            version: 1,
            collection_lthash: [0u8; HASH_LEN],
            records: vec![SnapshotRecoveryRecord {
                value: SyncActionData {
                    index: Some(b"[\"mute\",\"1@s.whatsapp.net\"]".to_vec()),
                    value: Some(mute_value(true)),
                    padding: Some(Vec::new()),
                    version: Some(2),
                },
                key_id: b"nokey!".to_vec(),
                mac: vec![0u8; VALUE_MAC_LEN],
            }],
        };
        let cache = AppStateKeyCache::new();
        let err = recovery.process(&cache, store.as_ref(), store.as_ref()).await.expect_err("must be rejected");
        assert!(matches!(err.downcast_ref::<AppStateError>(), Some(AppStateError::KeyNotFound)));
    }

    #[tokio::test]
    async fn a_tampered_patch_mac_is_rejected() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(51);
        let key = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("key");
        let expanded = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let patch = PatchInfo {
            timestamp_millis: 42,
            name: WAPatchName::REGULAR_HIGH.into(),
            mutations: vec![mute_info("111@s.whatsapp.net", true)],
        };
        let encoded =
            encode_patch(&key.key_id, &expanded, &HashState::new(), &patch, |_, _| Ok(None)).expect("encode patch");

        let mut tampered = encoded.patch.clone();
        tampered.patch_mac = Some(vec![0u8; 32]);
        let err = verify_patch(
            &WAPatchName::REGULAR_HIGH.into(),
            &tampered,
            &HashState::new(),
            &expanded,
            store.as_ref(),
            true,
        )
        .await
        .expect_err("a tampered patch MAC must be rejected");
        assert!(matches!(err.downcast_ref::<AppStateError>(), Some(AppStateError::MismatchingPatchMac)));
    }

    #[tokio::test]
    async fn a_patch_chained_on_stored_macs_converges_to_the_encoded_state() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(61);
        let key = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("key");
        let expanded = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let name: WAPatchName = WAPatchName::REGULAR_HIGH.into();

        // Patch 1 mutes a chat; its state and MAC are persisted like the client does.
        let first = PatchInfo {
            timestamp_millis: 7,
            name: name.clone(),
            mutations: vec![mute_info("111@s.whatsapp.net", true)],
        };
        let patch1 = encode_patch(&key.key_id, &expanded, &HashState::new(), &first, |_, _| Ok(None)).expect("patch 1");
        store.put_app_state_version(name.as_str(), patch1.state.version, patch1.state.hash).await.expect("version");
        let first_mac = value_mac_of_mutation(&patch1.patch.mutations[0]).expect("value mac").to_vec();
        let first_index_mac =
            index_mac_of_mutation(&patch1.patch.mutations[0]).expect("index mac").expect("present").to_vec();
        store
            .put_app_state_mutation_macs(
                name.as_str(),
                patch1.state.version,
                &[AppStateMutationMac { index_mac: first_index_mac.clone(), value_mac: first_mac.clone() }],
            )
            .await
            .expect("macs");

        // Patch 2 unmutes the same chat: the encoder resolves the previous value
        // MAC from the same place the verifier does (the store).
        let second = PatchInfo {
            timestamp_millis: 8,
            name: name.clone(),
            mutations: vec![mute_info("111@s.whatsapp.net", false)],
        };
        let patch2 = encode_patch(&key.key_id, &expanded, &patch1.state, &second, |index_mac, _| {
            Ok((index_mac == first_index_mac).then(|| first_mac.clone()))
        })
        .expect("patch 2");

        let verified = verify_patch(&name, &patch2.patch, &patch1.state, &expanded, store.as_ref(), true)
            .await
            .expect("patch 2 must verify");
        assert_eq!(verified.state.hash, patch2.state.hash, "the patch chain converges to the encoded hash");
        assert_eq!(verified.state.version, patch2.state.version);
        assert!(verified.warnings.is_empty());

        // The hash is the one of the multiset {first_mac removed, second added}.
        let second_mac = value_mac_of_mutation(&patch2.patch.mutations[0]).expect("value mac").to_vec();
        let expected = lthash_of([second_mac.as_slice()].into_iter()).expect("lthash");
        assert_eq!(verified.state.hash, expected);
    }

    #[test]
    fn the_recovery_message_roundtrips_through_zlib() {
        let payload = SnapshotRecoveryProto {
            version: Some(RecoveryVersion { version: Some(5) }),
            collection_name: Some(WAPatchName::REGULAR_LOW.to_string()),
            mutation_records: vec![SyncdPlainTextRecord {
                value: Some(SyncActionData {
                    index: Some(b"[\"pin_v1\",\"111@s.whatsapp.net\"]".to_vec()),
                    value: Some(SyncActionValue::default()),
                    padding: Some(Vec::new()),
                    version: Some(5),
                }),
                key_id: Some(vec![1, 2, 3, 4, 5, 6]),
                mac: Some(vec![7u8; VALUE_MAC_LEN]),
            }],
            collection_lthash: Some(vec![1u8; HASH_LEN]),
        };
        let raw = prost::Message::encode_to_vec(&payload);

        let plain = SnapshotRecovery::parse(&raw, false).expect("parse plain");
        assert_eq!(plain.name.as_str(), WAPatchName::REGULAR_LOW);
        assert_eq!(plain.version, 5);
        assert_eq!(plain.records.len(), 1);

        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&raw).expect("gzip");
        let compressed = encoder.finish().expect("gzip finish");
        let unpacked = SnapshotRecovery::parse(&compressed, true).expect("parse compressed");
        assert_eq!(unpacked, plain);

        let bad = SnapshotRecovery::parse(b"not a gzip stream", true);
        assert!(bad.is_err(), "a broken gzip stream is an error, not a panic");
    }
}
