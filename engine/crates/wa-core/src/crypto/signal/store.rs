// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The async store the Signal layer runs on ([`SignalStore`]), its adapter
//! over a wa-core [`Device`] ([`DeviceStore`]), the no-write variant used for
//! `state="false"` messages ([`SessionlessSignalStore`]) and [`PreKeyBundle`].
//!
//! Addresses are formatted with [`SignalAddress`]'s `Display` (`name:device`),
//! exactly the keys the Go client writes to `wa_sessions` / `wa_identity_keys` /
//! `wa_sender_keys`. Trust is *not* re-implemented here: `SqlStore` already
//! returns `true` for unknown identities and byte equality otherwise.

use async_trait::async_trait;

use super::address::{SenderKeyName, SignalAddress};
use crate::crypto::keys::{KeyPair, PreKey};
use crate::store::device::Device;
use crate::store::traits::Result;

/// A remote device's public pre-key bundle (from `<iq xmlns="encrypt">`).
///
/// Go: `libsignal/keys/prekey.Bundle` built by `nodeToPreKeyBundle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreKeyBundle {
    pub registration_id: u32,
    pub device_id: u32,
    pub pre_key_id: Option<u32>,
    pub pre_key_public: Option<[u8; 32]>,
    pub signed_pre_key_id: u32,
    pub signed_pre_key_public: [u8; 32],
    pub signed_pre_key_signature: [u8; 64],
    pub identity_key: [u8; 32],
}

/// Everything the Signal layer persists (D2: async end to end).
///
/// Go: libsignal's `store.SignalProtocol` (identity + pre-key + signed
/// pre-key + session + sender-key stores), implemented by the Go client's
/// `store.Device`.
#[async_trait]
pub trait SignalStore: Send + Sync {
    /// Our identity key pair (Go: `Device.IdentityKey`).
    fn identity_key_pair(&self) -> &KeyPair;
    /// Our registration ID (Go: `Device.RegistrationID`).
    fn registration_id(&self) -> u32;
    /// Our (single) signed pre-key (Go: `Device.SignedPreKey`).
    fn signed_pre_key(&self) -> &PreKey;

    async fn load_session(&self, addr: &SignalAddress) -> Result<Option<Vec<u8>>>;
    async fn store_session(&self, addr: &SignalAddress, record: &[u8]) -> Result<()>;
    async fn delete_session(&self, addr: &SignalAddress) -> Result<()>;
    async fn load_pre_key(&self, id: u32) -> Result<Option<KeyPair>>;
    async fn remove_pre_key(&self, id: u32) -> Result<()>;
    async fn load_sender_key(&self, name: &SenderKeyName) -> Result<Option<Vec<u8>>>;
    async fn store_sender_key(&self, name: &SenderKeyName, record: &[u8]) -> Result<()>;
    async fn is_trusted_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool>;
    /// Stores the identity; `true` when it replaced a different key.
    async fn save_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool>;
    async fn delete_identity(&self, addr: &SignalAddress) -> Result<()>;
}

/// [`SignalStore`] over a wa-core [`Device`] (its `Arc<dyn …>` store fields).
///
/// Go: `store.Device` implementing the libsignal store interfaces.
#[derive(Debug, Clone)]
pub struct DeviceStore {
    pub device: Device,
}

impl DeviceStore {
    pub fn new(device: Device) -> Self {
        Self { device }
    }
}

#[async_trait]
impl SignalStore for DeviceStore {
    fn identity_key_pair(&self) -> &KeyPair {
        &self.device.identity_key
    }

    fn registration_id(&self) -> u32 {
        self.device.registration_id
    }

    fn signed_pre_key(&self) -> &PreKey {
        &self.device.signed_pre_key
    }

    async fn load_session(&self, addr: &SignalAddress) -> Result<Option<Vec<u8>>> {
        self.device.sessions.get_session(&addr.to_string()).await
    }

    async fn store_session(&self, addr: &SignalAddress, record: &[u8]) -> Result<()> {
        self.device.sessions.put_session(&addr.to_string(), record).await
    }

    async fn delete_session(&self, addr: &SignalAddress) -> Result<()> {
        self.device.sessions.delete_session(&addr.to_string()).await
    }

    async fn load_pre_key(&self, id: u32) -> Result<Option<KeyPair>> {
        Ok(self.device.pre_keys.get_pre_key(id).await?.map(|k| k.key_pair))
    }

    async fn remove_pre_key(&self, id: u32) -> Result<()> {
        self.device.pre_keys.remove_pre_key(id).await
    }

    async fn load_sender_key(&self, name: &SenderKeyName) -> Result<Option<Vec<u8>>> {
        self.device.sender_keys.get_sender_key(&name.group_id, &name.sender_id()).await
    }

    async fn store_sender_key(&self, name: &SenderKeyName, record: &[u8]) -> Result<()> {
        self.device.sender_keys.put_sender_key(&name.group_id, &name.sender_id(), record).await
    }

    async fn is_trusted_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool> {
        self.device.identities.is_trusted_identity(&addr.to_string(), *key).await
    }

    async fn save_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool> {
        let address = addr.to_string();
        // `is_trusted_identity` is `false` only when a *different* key is stored.
        let replaced = !self.device.identities.is_trusted_identity(&address, *key).await?;
        self.device.identities.put_identity(&address, *key).await?;
        Ok(replaced)
    }

    async fn delete_identity(&self, addr: &SignalAddress) -> Result<()> {
        self.device.identities.delete_identity(&addr.to_string()).await
    }
}

/// A store that never writes sessions — for messages carrying
/// `state="false"`. Go: `sessionlessSignalStore` (`message.go`).
#[derive(Debug, Clone)]
pub struct SessionlessSignalStore<S>(pub S);

#[async_trait]
impl<S: SignalStore> SignalStore for SessionlessSignalStore<S> {
    fn identity_key_pair(&self) -> &KeyPair {
        self.0.identity_key_pair()
    }

    fn registration_id(&self) -> u32 {
        self.0.registration_id()
    }

    fn signed_pre_key(&self) -> &PreKey {
        self.0.signed_pre_key()
    }

    async fn load_session(&self, addr: &SignalAddress) -> Result<Option<Vec<u8>>> {
        self.0.load_session(addr).await
    }

    async fn store_session(&self, _addr: &SignalAddress, _record: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn delete_session(&self, addr: &SignalAddress) -> Result<()> {
        self.0.delete_session(addr).await
    }

    async fn load_pre_key(&self, id: u32) -> Result<Option<KeyPair>> {
        self.0.load_pre_key(id).await
    }

    async fn remove_pre_key(&self, id: u32) -> Result<()> {
        self.0.remove_pre_key(id).await
    }

    async fn load_sender_key(&self, name: &SenderKeyName) -> Result<Option<Vec<u8>>> {
        self.0.load_sender_key(name).await
    }

    async fn store_sender_key(&self, name: &SenderKeyName, record: &[u8]) -> Result<()> {
        self.0.store_sender_key(name, record).await
    }

    async fn is_trusted_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool> {
        self.0.is_trusted_identity(addr, key).await
    }

    async fn save_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool> {
        self.0.save_identity(addr, key).await
    }

    async fn delete_identity(&self, addr: &SignalAddress) -> Result<()> {
        self.0.delete_identity(addr).await
    }
}

/// An in-memory [`SignalStore`] for unit tests.
#[cfg(test)]
pub(crate) mod memory {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    pub(crate) struct MemoryStore {
        pub identity: KeyPair,
        pub registration_id: u32,
        pub signed_pre_key: PreKey,
        pub pre_keys: Mutex<HashMap<u32, PreKey>>,
        pub sessions: Mutex<HashMap<String, Vec<u8>>>,
        pub identities: Mutex<HashMap<String, [u8; 32]>>,
        pub sender_keys: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl MemoryStore {
        pub fn new(registration_id: u32) -> Self {
            let identity = KeyPair::generate();
            let signed_pre_key = identity.create_signed_pre_key(1);
            Self::with_keys(identity, signed_pre_key, registration_id)
        }

        pub fn with_keys(identity: KeyPair, signed_pre_key: PreKey, registration_id: u32) -> Self {
            Self {
                identity,
                registration_id,
                signed_pre_key,
                pre_keys: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                identities: Mutex::new(HashMap::new()),
                sender_keys: Mutex::new(HashMap::new()),
            }
        }

        /// Generates a one-time pre-key and returns this device's bundle.
        pub fn bundle(&self, pre_key_id: Option<u32>) -> PreKeyBundle {
            let otpk = pre_key_id.map(PreKey::generate);
            if let Some(k) = &otpk {
                self.pre_keys.lock().unwrap().insert(k.key_id, k.clone());
            }
            PreKeyBundle {
                registration_id: self.registration_id,
                device_id: 0,
                pre_key_id,
                pre_key_public: otpk.map(|k| k.key_pair.public),
                signed_pre_key_id: self.signed_pre_key.key_id,
                signed_pre_key_public: self.signed_pre_key.key_pair.public,
                signed_pre_key_signature: self.signed_pre_key.signature.unwrap(),
                identity_key: self.identity.public,
            }
        }
    }

    #[async_trait]
    impl SignalStore for MemoryStore {
        fn identity_key_pair(&self) -> &KeyPair {
            &self.identity
        }
        fn registration_id(&self) -> u32 {
            self.registration_id
        }
        fn signed_pre_key(&self) -> &PreKey {
            &self.signed_pre_key
        }
        async fn load_session(&self, addr: &SignalAddress) -> Result<Option<Vec<u8>>> {
            Ok(self.sessions.lock().unwrap().get(&addr.to_string()).cloned())
        }
        async fn store_session(&self, addr: &SignalAddress, record: &[u8]) -> Result<()> {
            self.sessions.lock().unwrap().insert(addr.to_string(), record.to_vec());
            Ok(())
        }
        async fn delete_session(&self, addr: &SignalAddress) -> Result<()> {
            self.sessions.lock().unwrap().remove(&addr.to_string());
            Ok(())
        }
        async fn load_pre_key(&self, id: u32) -> Result<Option<KeyPair>> {
            Ok(self.pre_keys.lock().unwrap().get(&id).map(|k| k.key_pair.clone()))
        }
        async fn remove_pre_key(&self, id: u32) -> Result<()> {
            self.pre_keys.lock().unwrap().remove(&id);
            Ok(())
        }
        async fn load_sender_key(&self, name: &SenderKeyName) -> Result<Option<Vec<u8>>> {
            Ok(self.sender_keys.lock().unwrap().get(&name.to_string()).cloned())
        }
        async fn store_sender_key(&self, name: &SenderKeyName, record: &[u8]) -> Result<()> {
            self.sender_keys.lock().unwrap().insert(name.to_string(), record.to_vec());
            Ok(())
        }
        async fn is_trusted_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool> {
            Ok(self.identities.lock().unwrap().get(&addr.to_string()).is_none_or(|k| k == key))
        }
        async fn save_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool> {
            let old = self.identities.lock().unwrap().insert(addr.to_string(), *key);
            Ok(old.is_some_and(|k| &k != key))
        }
        async fn delete_identity(&self, addr: &SignalAddress) -> Result<()> {
            self.identities.lock().unwrap().remove(&addr.to_string());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::Container;
    use crate::types::jid::Jid;

    async fn saved_device(c: &Container) -> Device {
        let mut d = c.new_device();
        d.id = Some(Jid::new_ad("1555", 0, 2));
        d.account = Some(wa_proto::wa_adv::AdvSignedDeviceIdentity {
            details: Some(b"details".to_vec()),
            account_signature_key: Some(vec![1; 32]),
            account_signature: Some(vec![2; 64]),
            device_signature: Some(vec![3; 64]),
        });
        d.save().await.unwrap();
        d
    }

    #[tokio::test]
    async fn device_store_round_trips() {
        let c = Container::open_memory().await.unwrap();
        let device = saved_device(&c).await;
        let store = DeviceStore::new(device.clone());
        let addr = SignalAddress::new("4911", 3);

        assert_eq!(store.registration_id(), device.registration_id);
        assert_eq!(store.identity_key_pair(), &device.identity_key);
        assert_eq!(store.signed_pre_key().key_id, device.signed_pre_key.key_id);

        // Identity: unknown → trusted; saved → trusted; different → untrusted.
        let key = [7u8; 32];
        assert!(store.is_trusted_identity(&addr, &key).await.unwrap());
        assert!(!store.save_identity(&addr, &key).await.unwrap());
        assert!(store.is_trusted_identity(&addr, &key).await.unwrap());
        assert!(!store.is_trusted_identity(&addr, &[8u8; 32]).await.unwrap());
        assert!(store.save_identity(&addr, &[8u8; 32]).await.unwrap(), "replacing reports true");
        store.delete_identity(&addr).await.unwrap();
        assert!(store.is_trusted_identity(&addr, &key).await.unwrap());

        // Sessions are keyed on `name:device`.
        assert_eq!(store.load_session(&addr).await.unwrap(), None);
        store.store_session(&addr, b"record").await.unwrap();
        assert_eq!(store.load_session(&addr).await.unwrap().as_deref(), Some(&b"record"[..]));
        assert!(device.sessions.has_session("4911:3").await.unwrap());
        store.delete_session(&addr).await.unwrap();
        assert_eq!(store.load_session(&addr).await.unwrap(), None);

        // Pre-keys: remove actually deletes the row.
        let keys = device.pre_keys.get_or_gen_pre_keys(2).await.unwrap();
        let id = keys[0].key_id;
        assert_eq!(store.load_pre_key(id).await.unwrap(), Some(keys[0].key_pair.clone()));
        store.remove_pre_key(id).await.unwrap();
        assert_eq!(store.load_pre_key(id).await.unwrap(), None);
        assert!(device.pre_keys.get_pre_key(id).await.unwrap().is_none());

        // Sender keys use (group, `name:device`).
        let name = SenderKeyName::new("123@g.us", addr.clone());
        store.store_sender_key(&name, b"sk").await.unwrap();
        assert_eq!(store.load_sender_key(&name).await.unwrap().as_deref(), Some(&b"sk"[..]));
        assert_eq!(device.sender_keys.get_sender_key("123@g.us", "4911:3").await.unwrap().as_deref(), Some(&b"sk"[..]));
    }

    #[tokio::test]
    async fn sessionless_store_writes_no_session() {
        let c = Container::open_memory().await.unwrap();
        let device = saved_device(&c).await;
        let store = SessionlessSignalStore(DeviceStore::new(device.clone()));
        let addr = SignalAddress::new("4911", 0);
        store.store_session(&addr, b"record").await.unwrap();
        assert_eq!(store.load_session(&addr).await.unwrap(), None);
        assert!(!device.sessions.has_session("4911:0").await.unwrap());
        // Everything else still goes through.
        store.save_identity(&addr, &[1; 32]).await.unwrap();
        assert!(!store.is_trusted_identity(&addr, &[2; 32]).await.unwrap());
    }
}
