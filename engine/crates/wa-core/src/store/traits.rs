// Ported from the Go client (`store/store.go`, `store/noop.go`), © Tulir Asokan,
// MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Store interfaces for WhatsApp multi-device (port of `store/store.go`).
//!
//! Every Go interface is an `#[async_trait]` here; `context.Context` is
//! dropped (cancellation = dropping the future). Go's `time.Time` maps to
//! [`SystemTime`], with the zero time mapped to `None` where Go checks
//! `IsZero()`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::crypto::keys::PreKey;
use crate::store::device::Device;
use crate::types::jid::Jid;

pub type Result<T> = anyhow::Result<T>;

/// Sentinel errors (`store.ErrDeviceDeleted`, `sqlstore.ErrInvalidLength`, …).
/// They travel inside `anyhow::Error`; match them with `err.downcast_ref::<StoreError>()`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("invalid use of deleted device")]
    DeviceDeleted,
    #[error("device store is not initialized (save the device first)")]
    NotInitialized,
    #[error("device JID must be known before accessing database")]
    DeviceIdMustBeSet,
    #[error("device account (ADV identity) must be set before saving")]
    AccountMissing,
    #[error("database returned byte array with illegal length")]
    InvalidLength,
    #[error("foreign keys are not enabled")]
    ForeignKeysDisabled,
}

/// `store.MutedForever` — 9999-12-31T23:59:59.999999999Z (stored as `-1`).
pub fn muted_forever() -> SystemTime {
    UNIX_EPOCH + Duration::new(253_402_300_799, 999_999_999)
}

#[async_trait]
pub trait IdentityStore: Send + Sync {
    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()>;
    async fn delete_all_identities(&self, phone: &str) -> Result<()>;
    async fn delete_identity(&self, address: &str) -> Result<()>;
    async fn is_trusted_identity(&self, address: &str, key: [u8; 32]) -> Result<bool>;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get_session(&self, address: &str) -> Result<Option<Vec<u8>>>;
    async fn has_session(&self, address: &str) -> Result<bool>;
    /// Every requested address is present in the result; `None` = no session.
    async fn get_many_sessions(&self, addresses: &[String]) -> Result<HashMap<String, Option<Vec<u8>>>>;
    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()>;
    async fn put_many_sessions(&self, sessions: HashMap<String, Vec<u8>>) -> Result<()>;
    async fn delete_all_sessions(&self, phone: &str) -> Result<()>;
    async fn delete_session(&self, address: &str) -> Result<()>;
    async fn migrate_pn_to_lid(&self, pn: &Jid, lid: &Jid) -> Result<()>;
}

#[async_trait]
pub trait PreKeyStore: Send + Sync {
    async fn get_or_gen_pre_keys(&self, count: u32) -> Result<Vec<PreKey>>;
    async fn gen_one_pre_key(&self) -> Result<PreKey>;
    async fn get_pre_key(&self, id: u32) -> Result<Option<PreKey>>;
    async fn remove_pre_key(&self, id: u32) -> Result<()>;
    async fn mark_pre_keys_as_uploaded(&self, up_to_id: u32) -> Result<()>;
    async fn uploaded_pre_key_count(&self) -> Result<usize>;
}

#[async_trait]
pub trait SenderKeyStore: Send + Sync {
    async fn put_sender_key(&self, group: &str, user: &str, session: &[u8]) -> Result<()>;
    async fn get_sender_key(&self, group: &str, user: &str) -> Result<Option<Vec<u8>>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStateSyncKey {
    pub data: Vec<u8>,
    pub fingerprint: Vec<u8>,
    pub timestamp: i64,
}

#[async_trait]
pub trait AppStateSyncKeyStore: Send + Sync {
    async fn put_app_state_sync_key(&self, id: &[u8], key: AppStateSyncKey) -> Result<()>;
    async fn get_app_state_sync_key(&self, id: &[u8]) -> Result<Option<AppStateSyncKey>>;
    async fn get_latest_app_state_sync_key_id(&self) -> Result<Option<Vec<u8>>>;
    async fn get_all_app_state_sync_keys(&self) -> Result<Vec<AppStateSyncKey>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStateMutationMac {
    pub index_mac: Vec<u8>,
    pub value_mac: Vec<u8>,
}

#[async_trait]
pub trait AppStateStore: Send + Sync {
    async fn put_app_state_version(&self, name: &str, version: u64, hash: [u8; 128]) -> Result<()>;
    /// `(0, [0; 128])` when nothing is stored — the correct initial state.
    async fn get_app_state_version(&self, name: &str) -> Result<(u64, [u8; 128])>;
    async fn delete_app_state_version(&self, name: &str) -> Result<()>;

    async fn put_app_state_mutation_macs(
        &self,
        name: &str,
        version: u64,
        mutations: &[AppStateMutationMac],
    ) -> Result<()>;
    async fn delete_app_state_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> Result<()>;
    async fn get_app_state_mutation_mac(&self, name: &str, index_mac: &[u8]) -> Result<Option<Vec<u8>>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactEntry {
    pub jid: Jid,
    pub first_name: String,
    pub full_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedPhoneEntry {
    pub jid: Jid,
    pub redacted_phone: String,
}

/// `types.ContactInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactInfo {
    pub found: bool,
    pub first_name: String,
    pub full_name: String,
    pub push_name: String,
    pub business_name: String,
    pub redacted_phone: String,
}

#[async_trait]
pub trait ContactStore: Send + Sync {
    /// Returns `(changed, previous_name)`.
    async fn put_push_name(&self, user: &Jid, push_name: &str) -> Result<(bool, String)>;
    /// Returns `(changed, previous_name)`.
    async fn put_business_name(&self, user: &Jid, business_name: &str) -> Result<(bool, String)>;
    async fn put_contact_name(&self, user: &Jid, first_name: &str, full_name: &str) -> Result<()>;
    async fn put_all_contact_names(&self, contacts: Vec<ContactEntry>) -> Result<()>;
    async fn put_many_redacted_phones(&self, entries: Vec<RedactedPhoneEntry>) -> Result<()>;
    async fn get_contact(&self, user: &Jid) -> Result<ContactInfo>;
    async fn get_all_contacts(&self) -> Result<HashMap<Jid, ContactInfo>>;
}

/// `types.LocalChatSettings`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalChatSettings {
    pub found: bool,
    /// `None` = not muted; [`muted_forever()`] = muted forever.
    pub muted_until: Option<SystemTime>,
    pub pinned: bool,
    pub archived: bool,
}

#[async_trait]
pub trait ChatSettingsStore: Send + Sync {
    async fn put_muted_until(&self, chat: &Jid, muted_until: Option<SystemTime>) -> Result<()>;
    async fn put_pinned(&self, chat: &Jid, pinned: bool) -> Result<()>;
    async fn put_archived(&self, chat: &Jid, archived: bool) -> Result<()>;
    async fn put_wasa_root_secret_id(&self, chat: &Jid, id: &str) -> Result<()>;
    async fn get_wasa_root_secret_id(&self, chat: &Jid) -> Result<Option<String>>;
    async fn get_chat_settings(&self, chat: &Jid) -> Result<LocalChatSettings>;
}

/// `store.DeviceContainer` — what a [`Device`] needs to save/delete itself.
#[async_trait]
pub trait DeviceContainer: Send + Sync {
    /// Stores the device and, on first save, wires its stores (`initializeDevice`).
    async fn put_device(&self, device: &mut Device) -> Result<()>;
    async fn delete_device(&self, device: &Device) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSecretInsert {
    pub chat: Jid,
    pub sender: Jid,
    pub id: String,
    pub secret: Vec<u8>,
}

#[async_trait]
pub trait MsgSecretStore: Send + Sync {
    async fn put_message_secrets(&self, inserts: &[MessageSecretInsert]) -> Result<()>;
    async fn put_message_secret(&self, chat: &Jid, sender: &Jid, id: &str, secret: &[u8]) -> Result<()>;
    /// `(secret, real_sender)` — the sender may be the LID/PN alternative of `sender`.
    async fn get_message_secret(&self, chat: &Jid, sender: &Jid, id: &str) -> Result<Option<(Vec<u8>, Jid)>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyToken {
    pub user: Jid,
    pub token: Vec<u8>,
    pub timestamp: SystemTime,
    pub sender_timestamp: Option<SystemTime>,
}

#[async_trait]
pub trait PrivacyTokenStore: Send + Sync {
    async fn put_privacy_tokens(&self, tokens: &[PrivacyToken]) -> Result<()>;
    async fn get_privacy_token(&self, user: &Jid) -> Result<Option<PrivacyToken>>;
    async fn delete_expired_privacy_tokens(&self, cutoff: SystemTime) -> Result<u64>;
}

#[async_trait]
pub trait NctSaltStore: Send + Sync {
    async fn put_nct_salt(&self, salt: &[u8]) -> Result<()>;
    async fn get_nct_salt(&self) -> Result<Option<Vec<u8>>>;
    async fn delete_nct_salt(&self) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferedEvent {
    pub plaintext: Option<Vec<u8>>,
    pub insert_time: SystemTime,
    pub server_time: SystemTime,
}

/// Boxed unit of work for [`EventBuffer::do_decryption_txn`].
pub type TxnFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

#[async_trait]
pub trait EventBuffer: Send + Sync {
    async fn get_buffered_event(&self, ciphertext_hash: [u8; 32]) -> Result<Option<BufferedEvent>>;
    async fn put_buffered_event(
        &self,
        ciphertext_hash: [u8; 32],
        plaintext: &[u8],
        server_timestamp: SystemTime,
    ) -> Result<()>;
    /// Runs one decryption (Signal state update + buffered plaintext) exclusively.
    async fn do_decryption_txn<'a>(&'a self, work: TxnFuture<'a>) -> Result<()>;
    async fn clear_buffered_event_plaintext(&self, ciphertext_hash: [u8; 32]) -> Result<()>;
    async fn delete_old_buffered_hashes(&self) -> Result<()>;

    /// `(format, plaintext)` of a recently sent message (retry receipts).
    async fn get_outgoing_event(&self, chat: &Jid, alt_chat: &Jid, id: &str) -> Result<Option<(String, Vec<u8>)>>;
    async fn add_outgoing_event(&self, chat: &Jid, id: &str, format: &str, plaintext: &[u8]) -> Result<()>;
    async fn delete_old_outgoing_events(&self) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LidMapping {
    pub lid: Jid,
    pub pn: Jid,
}

/// Global (not per-device) LID ↔ PN map — see `AGENTS.md` §8.
#[async_trait]
pub trait LidStore: Send + Sync {
    async fn put_many_lid_mappings(&self, mappings: &[LidMapping]) -> Result<()>;
    async fn put_lid_mapping(&self, lid: &Jid, pn: &Jid) -> Result<()>;
    /// `None` when unknown. The device part of `lid` is carried over.
    async fn get_pn_for_lid(&self, lid: &Jid) -> Result<Option<Jid>>;
    /// `None` when unknown. The device part of `pn` is carried over.
    async fn get_lid_for_pn(&self, pn: &Jid) -> Result<Option<Jid>>;
    async fn get_many_lids_for_pns(&self, pns: &[Jid]) -> Result<HashMap<Jid, Jid>>;
}

/// `store.AllSessionSpecificStores` — everything scoped to one device.
pub trait AllSessionSpecificStores:
    IdentityStore
    + SessionStore
    + PreKeyStore
    + SenderKeyStore
    + AppStateSyncKeyStore
    + AppStateStore
    + ContactStore
    + ChatSettingsStore
    + MsgSecretStore
    + PrivacyTokenStore
    + NctSaltStore
    + EventBuffer
{
}

impl<T> AllSessionSpecificStores for T where
    T: IdentityStore
        + SessionStore
        + PreKeyStore
        + SenderKeyStore
        + AppStateSyncKeyStore
        + AppStateStore
        + ContactStore
        + ChatSettingsStore
        + MsgSecretStore
        + PrivacyTokenStore
        + NctSaltStore
        + EventBuffer
{
}

/// `store.AllStores` — session-specific stores plus the global ones.
pub trait AllStores: AllSessionSpecificStores + LidStore {}
impl<T: AllSessionSpecificStores + LidStore> AllStores for T {}

/// `store.NoopStore` — every call fails with the configured error. Used for
/// devices that are not saved yet ([`StoreError::NotInitialized`]) or were
/// deleted ([`StoreError::DeviceDeleted`]).
#[derive(Debug, Clone)]
pub struct NoopStore {
    pub error: StoreError,
}

impl NoopStore {
    pub fn new(error: StoreError) -> Self {
        Self { error }
    }

    fn fail<T>(&self) -> Result<T> {
        Err(self.error.clone().into())
    }
}

#[async_trait]
impl IdentityStore for NoopStore {
    async fn put_identity(&self, _: &str, _: [u8; 32]) -> Result<()> {
        self.fail()
    }
    async fn delete_all_identities(&self, _: &str) -> Result<()> {
        self.fail()
    }
    async fn delete_identity(&self, _: &str) -> Result<()> {
        self.fail()
    }
    async fn is_trusted_identity(&self, _: &str, _: [u8; 32]) -> Result<bool> {
        self.fail()
    }
}

#[async_trait]
impl SessionStore for NoopStore {
    async fn get_session(&self, _: &str) -> Result<Option<Vec<u8>>> {
        self.fail()
    }
    async fn has_session(&self, _: &str) -> Result<bool> {
        self.fail()
    }
    async fn get_many_sessions(&self, _: &[String]) -> Result<HashMap<String, Option<Vec<u8>>>> {
        self.fail()
    }
    async fn put_session(&self, _: &str, _: &[u8]) -> Result<()> {
        self.fail()
    }
    async fn put_many_sessions(&self, _: HashMap<String, Vec<u8>>) -> Result<()> {
        self.fail()
    }
    async fn delete_all_sessions(&self, _: &str) -> Result<()> {
        self.fail()
    }
    async fn delete_session(&self, _: &str) -> Result<()> {
        self.fail()
    }
    async fn migrate_pn_to_lid(&self, _: &Jid, _: &Jid) -> Result<()> {
        self.fail()
    }
}

#[async_trait]
impl PreKeyStore for NoopStore {
    async fn get_or_gen_pre_keys(&self, _: u32) -> Result<Vec<PreKey>> {
        self.fail()
    }
    async fn gen_one_pre_key(&self) -> Result<PreKey> {
        self.fail()
    }
    async fn get_pre_key(&self, _: u32) -> Result<Option<PreKey>> {
        self.fail()
    }
    async fn remove_pre_key(&self, _: u32) -> Result<()> {
        self.fail()
    }
    async fn mark_pre_keys_as_uploaded(&self, _: u32) -> Result<()> {
        self.fail()
    }
    async fn uploaded_pre_key_count(&self) -> Result<usize> {
        self.fail()
    }
}

#[async_trait]
impl SenderKeyStore for NoopStore {
    async fn put_sender_key(&self, _: &str, _: &str, _: &[u8]) -> Result<()> {
        self.fail()
    }
    async fn get_sender_key(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>> {
        self.fail()
    }
}

#[async_trait]
impl AppStateSyncKeyStore for NoopStore {
    async fn put_app_state_sync_key(&self, _: &[u8], _: AppStateSyncKey) -> Result<()> {
        self.fail()
    }
    async fn get_app_state_sync_key(&self, _: &[u8]) -> Result<Option<AppStateSyncKey>> {
        self.fail()
    }
    async fn get_latest_app_state_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        self.fail()
    }
    async fn get_all_app_state_sync_keys(&self) -> Result<Vec<AppStateSyncKey>> {
        self.fail()
    }
}

#[async_trait]
impl AppStateStore for NoopStore {
    async fn put_app_state_version(&self, _: &str, _: u64, _: [u8; 128]) -> Result<()> {
        self.fail()
    }
    async fn get_app_state_version(&self, _: &str) -> Result<(u64, [u8; 128])> {
        self.fail()
    }
    async fn delete_app_state_version(&self, _: &str) -> Result<()> {
        self.fail()
    }
    async fn put_app_state_mutation_macs(&self, _: &str, _: u64, _: &[AppStateMutationMac]) -> Result<()> {
        self.fail()
    }
    async fn delete_app_state_mutation_macs(&self, _: &str, _: &[Vec<u8>]) -> Result<()> {
        self.fail()
    }
    async fn get_app_state_mutation_mac(&self, _: &str, _: &[u8]) -> Result<Option<Vec<u8>>> {
        self.fail()
    }
}

#[async_trait]
impl ContactStore for NoopStore {
    async fn put_push_name(&self, _: &Jid, _: &str) -> Result<(bool, String)> {
        self.fail()
    }
    async fn put_business_name(&self, _: &Jid, _: &str) -> Result<(bool, String)> {
        self.fail()
    }
    async fn put_contact_name(&self, _: &Jid, _: &str, _: &str) -> Result<()> {
        self.fail()
    }
    async fn put_all_contact_names(&self, _: Vec<ContactEntry>) -> Result<()> {
        self.fail()
    }
    async fn put_many_redacted_phones(&self, _: Vec<RedactedPhoneEntry>) -> Result<()> {
        self.fail()
    }
    async fn get_contact(&self, _: &Jid) -> Result<ContactInfo> {
        self.fail()
    }
    async fn get_all_contacts(&self) -> Result<HashMap<Jid, ContactInfo>> {
        self.fail()
    }
}

#[async_trait]
impl ChatSettingsStore for NoopStore {
    async fn put_muted_until(&self, _: &Jid, _: Option<SystemTime>) -> Result<()> {
        self.fail()
    }
    async fn put_pinned(&self, _: &Jid, _: bool) -> Result<()> {
        self.fail()
    }
    async fn put_archived(&self, _: &Jid, _: bool) -> Result<()> {
        self.fail()
    }
    async fn put_wasa_root_secret_id(&self, _: &Jid, _: &str) -> Result<()> {
        self.fail()
    }
    async fn get_wasa_root_secret_id(&self, _: &Jid) -> Result<Option<String>> {
        self.fail()
    }
    async fn get_chat_settings(&self, _: &Jid) -> Result<LocalChatSettings> {
        self.fail()
    }
}

#[async_trait]
impl DeviceContainer for NoopStore {
    async fn put_device(&self, _: &mut Device) -> Result<()> {
        self.fail()
    }
    async fn delete_device(&self, _: &Device) -> Result<()> {
        self.fail()
    }
}

#[async_trait]
impl MsgSecretStore for NoopStore {
    async fn put_message_secrets(&self, _: &[MessageSecretInsert]) -> Result<()> {
        self.fail()
    }
    async fn put_message_secret(&self, _: &Jid, _: &Jid, _: &str, _: &[u8]) -> Result<()> {
        self.fail()
    }
    async fn get_message_secret(&self, _: &Jid, _: &Jid, _: &str) -> Result<Option<(Vec<u8>, Jid)>> {
        self.fail()
    }
}

#[async_trait]
impl PrivacyTokenStore for NoopStore {
    async fn put_privacy_tokens(&self, _: &[PrivacyToken]) -> Result<()> {
        self.fail()
    }
    async fn get_privacy_token(&self, _: &Jid) -> Result<Option<PrivacyToken>> {
        self.fail()
    }
    async fn delete_expired_privacy_tokens(&self, _: SystemTime) -> Result<u64> {
        self.fail()
    }
}

#[async_trait]
impl NctSaltStore for NoopStore {
    async fn put_nct_salt(&self, _: &[u8]) -> Result<()> {
        self.fail()
    }
    async fn get_nct_salt(&self) -> Result<Option<Vec<u8>>> {
        self.fail()
    }
    async fn delete_nct_salt(&self) -> Result<()> {
        self.fail()
    }
}

#[async_trait]
impl EventBuffer for NoopStore {
    async fn get_buffered_event(&self, _: [u8; 32]) -> Result<Option<BufferedEvent>> {
        self.fail()
    }
    async fn put_buffered_event(&self, _: [u8; 32], _: &[u8], _: SystemTime) -> Result<()> {
        self.fail()
    }
    async fn do_decryption_txn<'a>(&'a self, _: TxnFuture<'a>) -> Result<()> {
        self.fail()
    }
    async fn clear_buffered_event_plaintext(&self, _: [u8; 32]) -> Result<()> {
        self.fail()
    }
    async fn delete_old_buffered_hashes(&self) -> Result<()> {
        self.fail()
    }
    async fn get_outgoing_event(&self, _: &Jid, _: &Jid, _: &str) -> Result<Option<(String, Vec<u8>)>> {
        self.fail()
    }
    async fn add_outgoing_event(&self, _: &Jid, _: &str, _: &str, _: &[u8]) -> Result<()> {
        self.fail()
    }
    async fn delete_old_outgoing_events(&self) -> Result<()> {
        self.fail()
    }
}

#[async_trait]
impl LidStore for NoopStore {
    async fn put_many_lid_mappings(&self, _: &[LidMapping]) -> Result<()> {
        self.fail()
    }
    async fn put_lid_mapping(&self, _: &Jid, _: &Jid) -> Result<()> {
        self.fail()
    }
    async fn get_pn_for_lid(&self, _: &Jid) -> Result<Option<Jid>> {
        self.fail()
    }
    async fn get_lid_for_pn(&self, _: &Jid) -> Result<Option<Jid>> {
        self.fail()
    }
    async fn get_many_lids_for_pns(&self, _: &[Jid]) -> Result<HashMap<Jid, Jid>> {
        self.fail()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_store_fails_with_its_error() {
        let s = NoopStore::new(StoreError::DeviceDeleted);
        let err = s.get_session("x").await.unwrap_err();
        assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::DeviceDeleted));
        assert!(s.get_lid_for_pn(&Jid::new("1", "s.whatsapp.net")).await.is_err());
    }

    #[test]
    fn muted_forever_is_year_9999() {
        let secs = muted_forever().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 253_402_300_799);
    }
}
