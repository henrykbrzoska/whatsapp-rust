// Ported from the Go client (`store/sqlstore/container.go`), © Tulir Asokan,
// MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `Container` — one SQLite database holding any number of devices.
//!
//! Foreign keys are mandatory (`?_foreign_keys=on` in Go): the store relies on
//! `ON DELETE CASCADE` to wipe a device, and `upgrade` refuses to run without
//! them.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use rand::{Rng, RngCore};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use wa_proto::wa_adv::AdvSignedDeviceIdentity;

use super::lid_map::CachedLidMap;
use super::store::SqlStore;
use crate::crypto::keys::{KeyPair, PreKey};
use crate::store::device::Device;
use crate::store::traits::{DeviceContainer, Result, StoreError};
use crate::types::jid::Jid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Schema version written to `wa_version` (matches Go upgrades v16).
pub const SCHEMA_VERSION: i64 = 16;

const DEVICE_COLUMNS: &str = "
    jid, lid, registration_id, noise_key, identity_key,
    signed_pre_key, signed_pre_key_id, signed_pre_key_sig,
    adv_key, adv_details, adv_account_sig, adv_account_sig_key, adv_device_sig,
    platform, business_name, push_name, facebook_uuid, lid_migration_ts, companion_meta_nonce";

const INSERT_DEVICE: &str = "
    INSERT INTO wa_device (jid, lid, registration_id, noise_key, identity_key,
                           signed_pre_key, signed_pre_key_id, signed_pre_key_sig,
                           adv_key, adv_details, adv_account_sig, adv_account_sig_key, adv_device_sig,
                           platform, business_name, push_name, facebook_uuid, lid_migration_ts, companion_meta_nonce)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
    ON CONFLICT (jid) DO UPDATE
        SET lid=excluded.lid,
            platform=excluded.platform,
            business_name=excluded.business_name,
            push_name=excluded.push_name,
            lid_migration_ts=excluded.lid_migration_ts,
            companion_meta_nonce=excluded.companion_meta_nonce";

/// Cheap to clone: the pool and the global LID map are shared.
#[derive(Clone)]
pub struct Container {
    db: SqlitePool,
    lid_map: Arc<CachedLidMap>,
}

fn key32(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes.try_into().map_err(|_| StoreError::InvalidLength.into())
}

impl Container {
    /// `sqlstore.New` — opens a database from a sqlx SQLite URL
    /// (`sqlite://path/to/wa.db`, `sqlite::memory:`) with foreign keys forced on,
    /// then runs [`upgrade`](Self::upgrade).
    pub async fn new(url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?.create_if_missing(true).foreign_keys(true);
        Self::with_options(opts).await
    }

    /// Opens (creating if needed) a SQLite database file and runs migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal);
        Self::with_options(opts).await
    }

    /// In-memory database (tests).
    pub async fn open_memory() -> Result<Self> {
        Self::new("sqlite::memory:").await
    }

    async fn with_options(opts: SqliteConnectOptions) -> Result<Self> {
        // One connection for :memory: so every query sees the same database.
        let db = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
        let c = Self::with_pool(db);
        c.upgrade().await?;
        Ok(c)
    }

    /// `NewWithDB` — wraps an existing pool. Does **not** upgrade; call
    /// [`upgrade`](Self::upgrade) yourself.
    pub fn with_pool(db: SqlitePool) -> Self {
        Self { lid_map: Arc::new(CachedLidMap::new(db.clone())), db }
    }

    /// `Container.Upgrade()` — checks `PRAGMA foreign_keys` and applies
    /// pending migrations (idempotent).
    pub async fn upgrade(&self) -> Result<()> {
        let fk: bool = sqlx::query_scalar("PRAGMA foreign_keys").fetch_one(&self.db).await?;
        if !fk {
            return Err(StoreError::ForeignKeysDisabled.into());
        }
        MIGRATOR.run(&self.db).await?;
        Ok(())
    }

    pub async fn version(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT version FROM wa_version").fetch_one(&self.db).await?)
    }

    /// JIDs of all stored devices.
    pub async fn device_jids(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT jid FROM wa_device").fetch_all(&self.db).await?)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.db
    }

    /// The global LID map shared by every device of this container.
    pub fn lid_map(&self) -> &Arc<CachedLidMap> {
        &self.lid_map
    }

    fn scan_device(&self, row: &SqliteRow) -> Result<Device> {
        let jid: Jid = row.try_get::<String, _>("jid")?.parse()?;
        let lid: Option<String> = row.try_get("lid")?;
        let noise = key32(row.try_get("noise_key")?)?;
        let identity = key32(row.try_get("identity_key")?)?;
        let spk_priv = key32(row.try_get("signed_pre_key")?)?;
        let spk_sig: [u8; 64] =
            row.try_get::<Vec<u8>, _>("signed_pre_key_sig")?.try_into().map_err(|_| StoreError::InvalidLength)?;
        let spk_id: i64 = row.try_get("signed_pre_key_id")?;
        let registration_id: i64 = row.try_get("registration_id")?;

        let mut signed_pre_key = PreKey::from_private(spk_id as u32, spk_priv);
        signed_pre_key.signature = Some(spk_sig);

        let mut device = Device::with_keys(
            KeyPair::from_private(noise),
            KeyPair::from_private(identity),
            signed_pre_key,
            registration_id as u32,
            row.try_get("adv_key")?,
            Arc::new(self.clone()),
        );
        device.id = Some(jid);
        device.lid = lid.filter(|l| !l.is_empty()).map(|l| l.parse()).transpose()?.unwrap_or_default();
        device.account = Some(AdvSignedDeviceIdentity {
            details: Some(row.try_get("adv_details")?),
            account_signature: Some(row.try_get("adv_account_sig")?),
            account_signature_key: Some(row.try_get("adv_account_sig_key")?),
            device_signature: Some(row.try_get("adv_device_sig")?),
        });
        device.platform = row.try_get("platform")?;
        device.business_name = row.try_get("business_name")?;
        device.push_name = row.try_get("push_name")?;
        device.facebook_uuid = row.try_get("facebook_uuid")?;
        device.lid_migration_timestamp = row.try_get("lid_migration_ts")?;
        device.companion_meta_nonce = row.try_get("companion_meta_nonce")?;
        self.initialize_device(&mut device);
        Ok(device)
    }

    /// `GetAllDevices`.
    pub async fn get_all_devices(&self) -> Result<Vec<Device>> {
        let rows = sqlx::query(&format!("SELECT {DEVICE_COLUMNS} FROM wa_device")).fetch_all(&self.db).await?;
        rows.iter().map(|r| self.scan_device(r)).collect()
    }

    /// `GetFirstDevice` — the first stored device, or a fresh unsaved one.
    pub async fn get_first_device(&self) -> Result<Device> {
        Ok(match self.get_all_devices().await?.into_iter().next() {
            Some(d) => d,
            None => self.new_device(),
        })
    }

    /// `GetDevice` — usually needs the AD-JID. `None` when not found.
    pub async fn get_device(&self, jid: &Jid) -> Result<Option<Device>> {
        let row = sqlx::query(&format!("SELECT {DEVICE_COLUMNS} FROM wa_device WHERE jid=?1"))
            .bind(jid.to_string())
            .fetch_optional(&self.db)
            .await?;
        row.map(|r| self.scan_device(&r)).transpose()
    }

    /// `NewDevice` — fresh NoiseKey, IdentityKey, signed pre-key #1,
    /// registration ID and 32-byte AdvSecretKey. Nothing is stored until
    /// [`Device::save`] (pairing does that).
    pub fn new_device(&self) -> Device {
        let identity_key = KeyPair::generate();
        let signed_pre_key = identity_key.create_signed_pre_key(1);
        let mut adv_secret_key = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut adv_secret_key);
        Device::with_keys(
            KeyPair::generate(),
            identity_key,
            signed_pre_key,
            rand::thread_rng().r#gen::<u32>(),
            adv_secret_key,
            Arc::new(self.clone()),
        )
    }

    /// `initializeDevice` — per-device `SqlStore` + the **shared** LID map.
    fn initialize_device(&self, device: &mut Device) {
        let jid = device.id.clone().unwrap_or_default();
        device.set_all_stores(Arc::new(SqlStore::new(self.db.clone(), &jid)));
        device.lids = self.lid_map.clone();
        device.container = Arc::new(self.clone());
        device.initialized = true;
    }

    /// `PutDevice` — prefer [`Device::save`].
    pub async fn put_device(&self, device: &mut Device) -> Result<()> {
        let Some(id) = device.id.clone() else {
            return Err(StoreError::DeviceIdMustBeSet.into());
        };
        let Some(account) = device.account.clone() else {
            return Err(StoreError::AccountMissing.into());
        };
        let lid = (!device.lid.is_empty()).then(|| device.lid.to_string());
        let signature = device.signed_pre_key.signature.ok_or(StoreError::InvalidLength)?;
        let result = sqlx::query(INSERT_DEVICE)
            .bind(id.to_string())
            .bind(lid)
            .bind(i64::from(device.registration_id))
            .bind(&device.noise_key.private[..])
            .bind(&device.identity_key.private[..])
            .bind(&device.signed_pre_key.key_pair.private[..])
            .bind(i64::from(device.signed_pre_key.key_id))
            .bind(&signature[..])
            .bind(&device.adv_secret_key)
            .bind(account.details.unwrap_or_default())
            .bind(account.account_signature.unwrap_or_default())
            .bind(account.account_signature_key.unwrap_or_default())
            .bind(account.device_signature.unwrap_or_default())
            .bind(&device.platform)
            .bind(&device.business_name)
            .bind(&device.push_name)
            .bind(device.facebook_uuid.as_deref().filter(|u| !u.is_empty()))
            .bind(device.lid_migration_timestamp)
            .bind(&device.companion_meta_nonce)
            .execute(&self.db)
            .await;
        // Like Go: wire the stores even if the write failed.
        if !device.initialized {
            self.initialize_device(device);
        }
        result?;
        Ok(())
    }

    /// `DeleteDevice` — prefer [`Device::delete`]. Cascades to every `wa_*`
    /// row owned by the device (sessions, keys, contacts, …).
    pub async fn delete_device(&self, device: &Device) -> Result<()> {
        let Some(id) = &device.id else {
            return Err(StoreError::DeviceIdMustBeSet.into());
        };
        sqlx::query("DELETE FROM wa_device WHERE jid=?1").bind(id.to_string()).execute(&self.db).await?;
        Ok(())
    }
}

#[async_trait]
impl DeviceContainer for Container {
    async fn put_device(&self, device: &mut Device) -> Result<()> {
        Container::put_device(self, device).await
    }

    async fn delete_device(&self, device: &Device) -> Result<()> {
        Container::delete_device(self, device).await
    }
}
