// Ported from the Go client (`appstate/keys.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! App state sync keys — lookup, expansion, caching and self-generation.
//!
//! Go counterparts: `Processor.getAppStateKey`, `Processor.GetMissingKeyIDs`
//! (`Go appstate/keys.go`) plus the storage half of
//! `handleAppStateSyncKeyShare` (`Go message.go`).
//!
//! Every mutation carries the ID of the sync key it was encrypted with; the key
//! material itself never goes over the wire. A key is read from
//! [`AppStateSyncKeyStore`], expanded into the five [`ExpandedAppStateKeys`] and
//! reused for every mutation that names it — hence [`AppStateKeyCache`].
//!
//! Errors are [`anyhow::Error`]s: a store failure and
//! [`AppStateError::KeyNotFound`] both surface unchanged, so a caller that cares
//! (the patch fetch, to request the missing keys) can `downcast_ref` them.
//!
//! Key *shares* (`AppStateSyncKeyShare`, sent by the primary device in a
//! protocol message) are persisted by [`persist_app_state_sync_key_share`];
//! the client then re-fetches the collections
//! (`Client::handle_app_state_sync_key_share`).

use std::collections::HashMap;

use anyhow::Result;
use prost::Message;
use rand::RngCore;
use tokio::sync::Mutex;
use wa_proto::wa_web_protobufs_e2e::{AppStateSyncKeyFingerprint, AppStateSyncKeyShare};

use crate::store::traits::{AppStateSyncKey, AppStateSyncKeyStore};

use super::{AppStateError, ExpandedAppStateKeys, PatchList};

/// Length of a key ID on the wire (WhatsApp uses 6 random bytes).
pub const KEY_ID_LEN: usize = 6;
/// Length of the raw key material that is expanded into [`ExpandedAppStateKeys`].
pub const SYNC_KEY_LEN: usize = 32;

/// Reads and expands one sync key (`Processor.getAppStateKey` without the cache).
///
/// Fails with [`AppStateError::KeyNotFound`] when the store has no key with this
/// ID, and with the store's own error when the lookup itself fails.
pub async fn get_app_state_sync_key(
    store: &dyn AppStateSyncKeyStore,
    key_id: &[u8],
) -> Result<ExpandedAppStateKeys, AppStateError> {
    let key = store.get_app_state_sync_key(key_id).await.map_err(|err| AppStateError::Store(err.to_string()))?;
    match key {
        Some(key) => Ok(ExpandedAppStateKeys::expand(&key.data)?),
        None => Err(AppStateError::KeyNotFound),
    }
}

/// `Processor.keyCache` — expanded keys of this sync session, by key ID.
///
/// Go keeps one cache per `Processor` (i.e. per client); here it is an explicit
/// value the caller owns, so `Client` needs no extra field.
#[derive(Debug, Default)]
pub struct AppStateKeyCache {
    keys: Mutex<HashMap<Vec<u8>, ExpandedAppStateKeys>>,
}

impl AppStateKeyCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self { keys: Mutex::new(HashMap::new()) }
    }

    /// `Processor.getAppStateKey` — from the cache, else from the store.
    pub async fn get(
        &self,
        store: &dyn AppStateSyncKeyStore,
        key_id: &[u8],
    ) -> Result<ExpandedAppStateKeys, AppStateError> {
        if let Some(keys) = self.keys.lock().await.get(key_id) {
            return Ok(keys.clone());
        }
        let keys = get_app_state_sync_key(store, key_id).await?;
        self.keys.lock().await.insert(key_id.to_vec(), keys.clone());
        Ok(keys)
    }

    /// Puts already-expanded keys into the cache (after a key share).
    pub async fn insert(&self, key_id: &[u8], keys: ExpandedAppStateKeys) {
        self.keys.lock().await.insert(key_id.to_vec(), keys);
    }

    /// Drops one key from the cache.
    pub async fn invalidate(&self, key_id: &[u8]) {
        self.keys.lock().await.remove(key_id);
    }

    /// How many keys are cached.
    pub async fn len(&self) -> usize {
        self.keys.lock().await.len()
    }

    /// `true` when nothing is cached.
    pub async fn is_empty(&self) -> bool {
        self.keys.lock().await.is_empty()
    }
}

/// A self-generated sync key plus the ID it is stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedAppStateSyncKey {
    /// The random 6-byte key ID.
    pub key_id: Vec<u8>,
    /// The key material and its fingerprint.
    pub key: AppStateSyncKey,
}

/// Generates a sync key from `rng` (deterministic in tests).
///
/// The fingerprint is the marshalled `AppStateSyncKeyFingerprint` a key share
/// would carry (`rawID`, `currentIndex`, `deviceIndexes`).
pub fn generate_app_state_sync_key<R: RngCore + ?Sized>(rng: &mut R) -> GeneratedAppStateSyncKey {
    let mut key_id = vec![0u8; KEY_ID_LEN];
    rng.fill_bytes(&mut key_id);
    let mut data = vec![0u8; SYNC_KEY_LEN];
    rng.fill_bytes(&mut data);
    let fingerprint =
        AppStateSyncKeyFingerprint { raw_id: Some(rng.next_u32()), current_index: Some(0), device_indexes: vec![0] };
    GeneratedAppStateSyncKey {
        key_id,
        key: AppStateSyncKey { data, fingerprint: fingerprint.encode_to_vec(), timestamp: now_millis() },
    }
}

/// Generates a sync key and stores it (`PutAppStateSyncKey`).
pub async fn store_generated_app_state_sync_key<R: RngCore + ?Sized>(
    store: &dyn AppStateSyncKeyStore,
    rng: &mut R,
) -> Result<GeneratedAppStateSyncKey> {
    let generated = generate_app_state_sync_key(rng);
    store
        .put_app_state_sync_key(&generated.key_id, generated.key.clone())
        .await
        .map_err(|err| AppStateError::Store(err.to_string()))?;
    Ok(generated)
}

/// Returns the newest stored key ID, generating and storing one when the store
/// is empty.
///
/// Go refuses to send a patch without a key ("creating app state keys is not
/// yet supported"); the engine creates its own key instead, which the primary
/// device's key share supersedes later (see [`persist_app_state_sync_key_share`]).
pub async fn ensure_app_state_sync_key<R: RngCore + ?Sized>(
    store: &dyn AppStateSyncKeyStore,
    rng: &mut R,
) -> Result<Vec<u8>> {
    match store.get_latest_app_state_sync_key_id().await {
        Ok(Some(key_id)) => Ok(key_id),
        Ok(None) => Ok(store_generated_app_state_sync_key(store, rng).await?.key_id),
        Err(err) => Err(AppStateError::Store(err.to_string()).into()),
    }
}

/// `Processor.GetMissingKeyIDs` — the key IDs a patch list names but the store
/// does not have, deduplicated and in wire order.
///
/// The client turns this into an `APP_STATE_SYNC_KEY_REQUEST` peer message; the
/// share that answers it goes through [`persist_app_state_sync_key_share`].
pub async fn missing_key_ids(store: &dyn AppStateSyncKeyStore, list: &PatchList) -> Result<Vec<Vec<u8>>> {
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    if let Some(snapshot) = &list.snapshot {
        if let Some(key_id) = snapshot.key_id.as_ref().and_then(|key_id| key_id.id.clone()) {
            candidates.push(key_id);
        }
        for record in &snapshot.records {
            if let Some(key_id) = record.key_id.as_ref().and_then(|key_id| key_id.id.clone()) {
                candidates.push(key_id);
            }
        }
    }
    for patch in &list.patches {
        if let Some(key_id) = patch.key_id.as_ref().and_then(|key_id| key_id.id.clone()) {
            candidates.push(key_id);
        }
    }

    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut missing: Vec<Vec<u8>> = Vec::new();
    for key_id in candidates {
        if seen.contains(&key_id) {
            continue;
        }
        seen.push(key_id.clone());
        match store.get_app_state_sync_key(&key_id).await {
            Ok(Some(_)) => {}
            Ok(None) => missing.push(key_id),
            Err(err) => return Err(AppStateError::Store(err.to_string()).into()),
        }
    }
    Ok(missing)
}

/// The storage half of Go `handleAppStateSyncKeyShare` — writes every key of
/// a share into the store and returns the IDs that were stored, in share order.
///
/// Like Go, the fingerprint is stored marshalled and the store keeps the newer
/// row when an ID already exists (`PutAppStateSyncKey` upserts on timestamp).
/// A store failure skips that key (Go logs and continues); a key without an ID
/// or without key material cannot be used for anything and is skipped too.
pub async fn persist_app_state_sync_key_share(
    store: &dyn AppStateSyncKeyStore,
    share: &AppStateSyncKeyShare,
) -> Vec<Vec<u8>> {
    let mut stored = Vec::with_capacity(share.keys.len());
    for key in &share.keys {
        let Some(key_id) = key.key_id.as_ref().and_then(|id| id.key_id.clone()).filter(|id| !id.is_empty()) else {
            tracing::warn!("ignoring an app state sync key share entry without a key ID");
            continue;
        };
        let Some(data) = key.key_data.as_ref() else {
            tracing::warn!("ignoring app state sync key {} without key data", hex::encode(&key_id));
            continue;
        };
        let Some(material) = data.key_data.clone().filter(|material| !material.is_empty()) else {
            tracing::warn!("ignoring app state sync key {} without key material", hex::encode(&key_id));
            continue;
        };
        let fingerprint = data.fingerprint.clone().unwrap_or_default().encode_to_vec();
        let timestamp = data.timestamp.unwrap_or_default();
        let row = AppStateSyncKey { data: material, fingerprint, timestamp };
        match store.put_app_state_sync_key(&key_id, row).await {
            Ok(()) => {
                tracing::debug!("received app state sync key {} (ts: {timestamp})", hex::encode(&key_id));
                stored.push(key_id);
            }
            Err(err) => tracing::error!("failed to store app state sync key {}: {err}", hex::encode(&key_id)),
        }
    }
    stored
}

/// Milliseconds since the Unix epoch, `0` before it.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::sync::Arc;
    use wa_proto::wa_web_protobufs_server_sync::{
        KeyId, SyncdIndex, SyncdMutation, SyncdPatch, SyncdRecord, SyncdValue,
    };

    use super::*;
    use crate::appstate::{SyncdOperation, WAPatchName, hash::HASH_LEN};
    use crate::store::sqlite::{Container, SqlStore};
    use crate::types::jid::Jid;

    /// A `SqlStore` for a device that is actually persisted: the
    /// `wa_app_state_sync_keys` rows have a foreign key onto `wa_device`.
    async fn store() -> Arc<SqlStore> {
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
        Arc::new(SqlStore::new(container.pool().clone(), &Jid::new_ad("1", 0, 1)))
    }

    #[tokio::test]
    async fn generated_key_roundtrips_through_the_store() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(7);
        let generated = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("store key");
        assert_eq!(generated.key_id.len(), KEY_ID_LEN);
        assert_eq!(generated.key.data.len(), SYNC_KEY_LEN);

        let stored = store.get_app_state_sync_key(&generated.key_id).await.expect("read key").expect("key exists");
        assert_eq!(stored, generated.key);
        let fingerprint =
            AppStateSyncKeyFingerprint::decode(stored.fingerprint.as_slice()).expect("decode the fingerprint");
        assert_eq!(fingerprint.current_index, Some(0));
        assert_eq!(fingerprint.device_indexes, vec![0]);
        assert!(fingerprint.raw_id.is_some());
    }

    #[test]
    fn generated_keys_are_deterministic_for_one_seed() {
        let mut first = StdRng::seed_from_u64(99);
        let mut second = StdRng::seed_from_u64(99);
        let a = generate_app_state_sync_key(&mut first);
        let b = generate_app_state_sync_key(&mut second);
        assert_eq!(a.key_id, b.key_id);
        assert_eq!(a.key.data, b.key.data);
        assert_eq!(a.key.fingerprint, b.key.fingerprint);
        assert_ne!(generate_app_state_sync_key(&mut first).key_id, a.key_id);
    }

    #[tokio::test]
    async fn lookup_expands_the_key_and_rejects_unknown_ids() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(3);
        let generated = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("store key");

        let keys = get_app_state_sync_key(store.as_ref(), &generated.key_id).await.expect("expand");
        assert_eq!(keys, ExpandedAppStateKeys::expand(&generated.key.data).expect("expand"));

        assert!(matches!(get_app_state_sync_key(store.as_ref(), b"nope!!").await, Err(AppStateError::KeyNotFound)));
    }

    #[tokio::test]
    async fn the_cache_reads_once_and_can_be_invalidated() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(11);
        let generated = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("store key");

        let cache = AppStateKeyCache::new();
        assert!(cache.is_empty().await);
        let first = cache.get(store.as_ref(), &generated.key_id).await.expect("cache miss");
        assert_eq!(cache.len().await, 1);
        let second = cache.get(store.as_ref(), &generated.key_id).await.expect("cache hit");
        assert_eq!(first, second);

        cache.invalidate(&generated.key_id).await;
        assert!(cache.is_empty().await);
        assert!(matches!(cache.get(store.as_ref(), b"missing").await, Err(AppStateError::KeyNotFound)));
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn ensure_generates_only_when_the_store_is_empty() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(21);
        let created = ensure_app_state_sync_key(store.as_ref(), &mut rng).await.expect("ensure");
        assert_eq!(created.len(), KEY_ID_LEN);
        let again = ensure_app_state_sync_key(store.as_ref(), &mut rng).await.expect("ensure");
        assert_eq!(again, created, "the existing key must be reused");
        assert_eq!(store.get_all_app_state_sync_keys().await.expect("list").len(), 1);
    }

    fn patch_with_key(key_id: &[u8]) -> SyncdPatch {
        let record = SyncdRecord {
            index: Some(SyncdIndex { blob: Some(vec![0u8; HASH_LEN / 4]) }),
            value: Some(SyncdValue { blob: Some(vec![0u8; 48]) }),
            key_id: Some(KeyId { id: Some(key_id.to_vec()) }),
        };
        SyncdPatch {
            key_id: Some(KeyId { id: Some(key_id.to_vec()) }),
            mutations: vec![SyncdMutation { operation: Some(SyncdOperation::Set as i32), record: Some(record) }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn missing_key_ids_lists_only_keys_we_do_not_have() {
        let store = store().await;
        let mut rng = StdRng::seed_from_u64(5);
        let known = store_generated_app_state_sync_key(store.as_ref(), &mut rng).await.expect("store key");

        let mut list = PatchList::new(WAPatchName::REGULAR_LOW);
        list.patches = vec![patch_with_key(&known.key_id), patch_with_key(b"other!"), patch_with_key(b"other!")];
        let missing = missing_key_ids(store.as_ref(), &list).await.expect("missing");
        assert_eq!(missing, vec![b"other!".to_vec()], "deduplicated, and the known key is not reported");

        let empty = missing_key_ids(store.as_ref(), &PatchList::new(WAPatchName::REGULAR_HIGH)).await.expect("missing");
        assert!(empty.is_empty(), "an empty patch list needs no keys");
    }

    fn share_key(
        key_id: &[u8],
        data: &[u8],
        timestamp: i64,
        raw_id: u32,
    ) -> wa_proto::wa_web_protobufs_e2e::AppStateSyncKey {
        use wa_proto::wa_web_protobufs_e2e::{AppStateSyncKey as ShareKey, AppStateSyncKeyData, AppStateSyncKeyId};
        ShareKey {
            key_id: Some(AppStateSyncKeyId { key_id: Some(key_id.to_vec()) }),
            key_data: Some(AppStateSyncKeyData {
                key_data: Some(data.to_vec()),
                fingerprint: Some(AppStateSyncKeyFingerprint {
                    raw_id: Some(raw_id),
                    current_index: Some(1),
                    device_indexes: vec![0, 1],
                }),
                timestamp: Some(timestamp),
            }),
        }
    }

    #[tokio::test]
    async fn a_key_share_is_persisted_with_its_marshalled_fingerprint() {
        let store = store().await;
        let share = AppStateSyncKeyShare {
            keys: vec![share_key(b"phone1", &[7u8; 32], 100, 42), share_key(b"phone2", &[8u8; 32], 200, 43)],
        };
        let stored = persist_app_state_sync_key_share(store.as_ref(), &share).await;
        assert_eq!(stored, vec![b"phone1".to_vec(), b"phone2".to_vec()]);

        let row = store.get_app_state_sync_key(b"phone1").await.expect("get").expect("stored");
        assert_eq!(row.data, vec![7u8; 32]);
        assert_eq!(row.timestamp, 100);
        let fingerprint = AppStateSyncKeyFingerprint::decode(row.fingerprint.as_slice()).expect("fingerprint");
        assert_eq!(fingerprint.raw_id, Some(42));
        assert_eq!(fingerprint.device_indexes, vec![0, 1]);
        assert_eq!(store.get_latest_app_state_sync_key_id().await.expect("latest").as_deref(), Some(&b"phone2"[..]));

        // The stored key is usable for decryption right away.
        let expanded = get_app_state_sync_key(store.as_ref(), b"phone1").await.expect("expand");
        assert_eq!(expanded, ExpandedAppStateKeys::expand(&[7u8; 32]).expect("expand"));
    }

    #[tokio::test]
    async fn a_key_share_skips_unusable_entries_and_keeps_the_newer_key() {
        use wa_proto::wa_web_protobufs_e2e::{AppStateSyncKey as ShareKey, AppStateSyncKeyId};
        let store = store().await;
        persist_app_state_sync_key_share(
            store.as_ref(),
            &AppStateSyncKeyShare { keys: vec![share_key(b"phone1", &[1u8; 32], 500, 1)] },
        )
        .await;

        let share = AppStateSyncKeyShare {
            keys: vec![
                // No ID at all.
                ShareKey { key_id: None, ..share_key(b"x", &[2u8; 32], 1, 1) },
                // An ID but no key data.
                ShareKey { key_id: Some(AppStateSyncKeyId { key_id: Some(b"nodata".to_vec()) }), key_data: None },
                // Empty key material.
                share_key(b"empty!", &[], 1, 1),
                // An older copy of an existing key: accepted by the store, but ignored.
                share_key(b"phone1", &[9u8; 32], 100, 1),
            ],
        };
        let stored = persist_app_state_sync_key_share(store.as_ref(), &share).await;
        assert_eq!(stored, vec![b"phone1".to_vec()]);
        assert_eq!(store.get_app_state_sync_key(b"nodata").await.expect("get"), None);
        assert_eq!(store.get_app_state_sync_key(b"empty!").await.expect("get"), None);
        let row = store.get_app_state_sync_key(b"phone1").await.expect("get").expect("stored");
        assert_eq!((row.data, row.timestamp), (vec![1u8; 32], 500), "the newer key wins");
    }
}
