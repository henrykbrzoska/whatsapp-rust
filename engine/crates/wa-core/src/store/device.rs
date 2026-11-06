// Ported from the Go client (`store/store.go`: `Device`), © Tulir Asokan,
// MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `store.Device` — keys and identity of one linked device plus handles to
//! its stores.

use std::sync::Arc;

use wa_proto::wa_adv::AdvSignedDeviceIdentity;

use crate::crypto::keys::{KeyPair, PreKey};
use crate::store::traits::*;
use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid};

#[derive(Clone)]
pub struct Device {
    pub noise_key: KeyPair,
    pub identity_key: KeyPair,
    pub signed_pre_key: PreKey,
    pub registration_id: u32,
    pub adv_secret_key: Vec<u8>,

    /// AD-JID of this device; `None` until paired.
    pub id: Option<Jid>,
    pub lid: Jid,

    pub account: Option<AdvSignedDeviceIdentity>,
    pub platform: String,
    pub business_name: String,
    pub push_name: String,

    pub lid_migration_timestamp: i64,
    pub companion_meta_nonce: String,

    /// Hyphenated UUID text (stored as-is, like Go's `uuid.NullUUID`).
    pub facebook_uuid: Option<String>,

    pub initialized: bool,
    pub deleted: bool,

    pub identities: Arc<dyn IdentityStore>,
    pub sessions: Arc<dyn SessionStore>,
    pub pre_keys: Arc<dyn PreKeyStore>,
    pub sender_keys: Arc<dyn SenderKeyStore>,
    pub app_state_keys: Arc<dyn AppStateSyncKeyStore>,
    pub app_state: Arc<dyn AppStateStore>,
    pub contacts: Arc<dyn ContactStore>,
    pub chat_settings: Arc<dyn ChatSettingsStore>,
    pub msg_secrets: Arc<dyn MsgSecretStore>,
    pub privacy_tokens: Arc<dyn PrivacyTokenStore>,
    pub nct_salt: Arc<dyn NctSaltStore>,
    pub event_buffer: Arc<dyn EventBuffer>,
    /// Global LID map shared by every device of the container.
    pub lids: Arc<dyn LidStore>,
    pub container: Arc<dyn DeviceContainer>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("id", &self.id)
            .field("lid", &self.lid)
            .field("registration_id", &self.registration_id)
            .field("platform", &self.platform)
            .field("push_name", &self.push_name)
            .field("initialized", &self.initialized)
            .field("deleted", &self.deleted)
            .finish_non_exhaustive()
    }
}

impl Device {
    /// A device with the given keys whose stores all fail with
    /// [`StoreError::NotInitialized`] until it is saved/loaded by a container.
    pub fn with_keys(
        noise_key: KeyPair,
        identity_key: KeyPair,
        signed_pre_key: PreKey,
        registration_id: u32,
        adv_secret_key: Vec<u8>,
        container: Arc<dyn DeviceContainer>,
    ) -> Self {
        let noop = Arc::new(NoopStore::new(StoreError::NotInitialized));
        Self {
            noise_key,
            identity_key,
            signed_pre_key,
            registration_id,
            adv_secret_key,
            id: None,
            lid: Jid::default(),
            account: None,
            platform: String::new(),
            business_name: String::new(),
            push_name: String::new(),
            lid_migration_timestamp: 0,
            companion_meta_nonce: String::new(),
            facebook_uuid: None,
            initialized: false,
            deleted: false,
            identities: noop.clone(),
            sessions: noop.clone(),
            pre_keys: noop.clone(),
            sender_keys: noop.clone(),
            app_state_keys: noop.clone(),
            app_state: noop.clone(),
            contacts: noop.clone(),
            chat_settings: noop.clone(),
            msg_secrets: noop.clone(),
            privacy_tokens: noop.clone(),
            nct_salt: noop.clone(),
            event_buffer: noop.clone(),
            lids: noop,
            container,
        }
    }

    /// `GetJID` — the AD-JID, or the empty JID before pairing.
    pub fn get_jid(&self) -> Jid {
        self.id.clone().unwrap_or_default()
    }

    /// `GetLID`.
    pub fn get_lid(&self) -> Jid {
        self.lid.clone()
    }

    /// `SetAllStores` — points every session-specific store at `store`.
    pub fn set_all_stores(&mut self, store: Arc<dyn AllSessionSpecificStores>) {
        self.identities = store.clone();
        self.sessions = store.clone();
        self.pre_keys = store.clone();
        self.sender_keys = store.clone();
        self.app_state_keys = store.clone();
        self.app_state = store.clone();
        self.contacts = store.clone();
        self.chat_settings = store.clone();
        self.msg_secrets = store.clone();
        self.privacy_tokens = store.clone();
        self.nct_salt = store.clone();
        self.event_buffer = store;
    }

    /// `Save` — persists the device through its container.
    pub async fn save(&mut self) -> Result<()> {
        if self.deleted {
            return Err(StoreError::DeviceDeleted.into());
        }
        let container = self.container.clone();
        container.put_device(self).await
    }

    /// `Delete` — removes the device (cascading to all its rows) and turns
    /// every store into a [`StoreError::DeviceDeleted`] no-op. Idempotent.
    pub async fn delete(&mut self) -> Result<()> {
        if self.deleted {
            return Ok(());
        }
        self.container.delete_device(self).await?;
        self.id = None;
        self.lid = Jid::default();
        self.deleted = true;
        self.set_all_stores(Arc::new(NoopStore::new(StoreError::DeviceDeleted)));
        Ok(())
    }

    /// `GetAltJID` — PN → LID or LID → PN through the global LID map.
    pub async fn get_alt_jid(&self, jid: &Jid) -> Result<Option<Jid>> {
        match jid.server.as_str() {
            DEFAULT_USER_SERVER => self.lids.get_lid_for_pn(jid).await,
            HIDDEN_USER_SERVER => self.lids.get_pn_for_lid(jid).await,
            _ => Ok(None),
        }
    }
}
