// Ported from the Go client (`store/sqlstore/store.go`), © Tulir Asokan,
// MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `SQLStore` — SQLite implementation of every session-specific store, scoped
//! to one device JID (`our_jid`). The LID map is **not** here: it is global
//! and lives in [`CachedLidMap`](super::CachedLidMap).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use async_trait::async_trait;
use sqlx::{Row, SqliteConnection, SqlitePool};
use tokio::sync::Mutex;

use crate::crypto::keys::PreKey;
use crate::store::traits::*;
use crate::types::jid::Jid;

const MUTATION_BATCH_SIZE: usize = 400;
const CONTACT_BATCH_SIZE: usize = 300;
/// WhatsApp servers buffer events for 14 days.
const EVENT_BUFFER_TTL: Duration = Duration::from_secs(14 * 24 * 3600);
const RETRY_BUFFER_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

pub(crate) fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs_f64().ceil() as i64),
    }
}

pub(crate) fn unix_millis(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

pub(crate) fn from_unix_secs(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

pub(crate) fn from_unix_millis(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

fn array<const N: usize>(bytes: Vec<u8>) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| StoreError::InvalidLength.into())
}

fn placeholders(from: usize, count: usize) -> String {
    (from..from + count).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",")
}

/// Keeps the last entry per key, in first-seen order (`DeduplicateUnsortedOverwrite`).
fn dedupe_last<T, K: std::hash::Hash + Eq>(items: Vec<T>, key: impl Fn(&T) -> K) -> Vec<T> {
    let mut index: HashMap<K, usize> = HashMap::new();
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        match index.get(&key(&item)) {
            Some(&i) => out[i] = item,
            None => {
                index.insert(key(&item), out.len());
                out.push(item);
            }
        }
    }
    out
}

pub struct SqlStore {
    db: SqlitePool,
    /// `our_jid` — the device AD-JID as text.
    jid: String,
    pre_key_lock: Mutex<()>,
    contact_cache: Mutex<HashMap<Jid, ContactInfo>>,
    migrated_pn_sessions: std::sync::Mutex<HashSet<String>>,
    decryption_lock: Mutex<()>,
}

impl SqlStore {
    /// `NewSQLStore`. Prefer `Container::get_device` / `new_device`.
    pub fn new(db: SqlitePool, jid: &Jid) -> Self {
        Self {
            db,
            jid: jid.to_string(),
            pre_key_lock: Mutex::new(()),
            contact_cache: Mutex::new(HashMap::new()),
            migrated_pn_sessions: std::sync::Mutex::new(HashSet::new()),
            decryption_lock: Mutex::new(()),
        }
    }

    pub fn jid(&self) -> &str {
        &self.jid
    }

    async fn next_pre_key_id(&self) -> Result<u32> {
        let last: Option<i64> = sqlx::query_scalar("SELECT MAX(key_id) FROM wa_pre_keys WHERE jid=?1")
            .bind(&self.jid)
            .fetch_one(&self.db)
            .await
            .context("failed to query next prekey ID")?;
        Ok(last.unwrap_or(0) as u32 + 1)
    }

    async fn insert_pre_key(&self, id: u32, uploaded: bool) -> Result<PreKey> {
        let key = PreKey::generate(id);
        sqlx::query("INSERT INTO wa_pre_keys (jid, key_id, key, uploaded) VALUES (?1, ?2, ?3, ?4)")
            .bind(&self.jid)
            .bind(i64::from(key.key_id))
            .bind(&key.key_pair.private[..])
            .bind(uploaded)
            .execute(&self.db)
            .await?;
        Ok(key)
    }

    /// `getContact` — caller holds the contact cache lock.
    async fn load_contact(&self, cache: &mut HashMap<Jid, ContactInfo>, user: &Jid) -> Result<ContactInfo> {
        if let Some(info) = cache.get(user) {
            return Ok(info.clone());
        }
        let row = sqlx::query(
            "SELECT first_name, full_name, push_name, business_name, redacted_phone
             FROM wa_contacts WHERE our_jid=?1 AND their_jid=?2",
        )
        .bind(&self.jid)
        .bind(user.to_string())
        .fetch_optional(&self.db)
        .await?;
        let info = match row {
            Some(r) => contact_from_row(&r, 0)?,
            None => ContactInfo::default(),
        };
        cache.insert(user.clone(), info.clone());
        Ok(info)
    }

    async fn put_chat_setting<T>(&self, column: &str, chat: &Jid, value: T) -> Result<()>
    where
        T: for<'q> sqlx::Encode<'q, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite> + Send,
    {
        let sql = format!(
            "INSERT INTO wa_chat_settings (our_jid, chat_jid, {column}) VALUES (?1, ?2, ?3)
             ON CONFLICT (our_jid, chat_jid) DO UPDATE SET {column}=excluded.{column}"
        );
        sqlx::query(&sql).bind(&self.jid).bind(chat.to_string()).bind(value).execute(&self.db).await?;
        Ok(())
    }

    async fn delete_like(conn: &mut SqliteConnection, sql: &str, jid: &str, prefix: &str) -> Result<()> {
        sqlx::query(sql).bind(jid).bind(format!("{prefix}:%")).execute(conn).await?;
        Ok(())
    }
}

fn contact_from_row(r: &sqlx::sqlite::SqliteRow, offset: usize) -> Result<ContactInfo> {
    let s = |i: usize| -> Result<String> { Ok(r.try_get::<Option<String>, _>(offset + i)?.unwrap_or_default()) };
    Ok(ContactInfo {
        found: true,
        first_name: s(0)?,
        full_name: s(1)?,
        push_name: s(2)?,
        business_name: s(3)?,
        redacted_phone: s(4)?,
    })
}

#[async_trait]
impl IdentityStore for SqlStore {
    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()> {
        sqlx::query(
            "INSERT INTO wa_identity_keys (our_jid, their_id, identity) VALUES (?1, ?2, ?3)
             ON CONFLICT (our_jid, their_id) DO UPDATE SET identity=excluded.identity",
        )
        .bind(&self.jid)
        .bind(address)
        .bind(&key[..])
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn delete_all_identities(&self, phone: &str) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        Self::delete_like(
            &mut conn,
            "DELETE FROM wa_identity_keys WHERE our_jid=?1 AND their_id LIKE ?2",
            &self.jid,
            phone,
        )
        .await
    }

    async fn delete_identity(&self, address: &str) -> Result<()> {
        sqlx::query("DELETE FROM wa_identity_keys WHERE our_jid=?1 AND their_id=?2")
            .bind(&self.jid)
            .bind(address)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn is_trusted_identity(&self, address: &str, key: [u8; 32]) -> Result<bool> {
        let existing: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT identity FROM wa_identity_keys WHERE our_jid=?1 AND their_id=?2")
                .bind(&self.jid)
                .bind(address)
                .fetch_optional(&self.db)
                .await?;
        match existing {
            // Trust if not known, it'll be saved automatically later.
            None => Ok(true),
            Some(existing) => Ok(array::<32>(existing)? == key),
        }
    }
}

const MIGRATE_SESSIONS: &str = "
    INSERT INTO wa_sessions (our_jid, their_id, session)
    SELECT our_jid, replace(their_id, ?2, ?3), session FROM wa_sessions
    WHERE our_jid=?1 AND their_id LIKE ?2 || ':%'
    ON CONFLICT (our_jid, their_id) DO UPDATE SET session=excluded.session";
const MIGRATE_IDENTITY_KEYS: &str = "
    INSERT INTO wa_identity_keys (our_jid, their_id, identity)
    SELECT our_jid, replace(their_id, ?2, ?3), identity FROM wa_identity_keys
    WHERE our_jid=?1 AND their_id LIKE ?2 || ':%'
    ON CONFLICT (our_jid, their_id) DO UPDATE SET identity=excluded.identity";
const MIGRATE_SENDER_KEYS: &str = "
    INSERT INTO wa_sender_keys (our_jid, chat_id, sender_id, sender_key)
    SELECT our_jid, chat_id, replace(sender_id, ?2, ?3), sender_key FROM wa_sender_keys
    WHERE our_jid=?1 AND sender_id LIKE ?2 || ':%'
    ON CONFLICT (our_jid, chat_id, sender_id) DO UPDATE SET sender_key=excluded.sender_key";

#[async_trait]
impl SessionStore for SqlStore {
    async fn get_session(&self, address: &str) -> Result<Option<Vec<u8>>> {
        let s: Option<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT session FROM wa_sessions WHERE our_jid=?1 AND their_id=?2")
                .bind(&self.jid)
                .bind(address)
                .fetch_optional(&self.db)
                .await?;
        Ok(s.flatten())
    }

    async fn has_session(&self, address: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM wa_sessions WHERE our_jid=?1 AND their_id=?2")
            .bind(&self.jid)
            .bind(address)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.is_some())
    }

    async fn get_many_sessions(&self, addresses: &[String]) -> Result<HashMap<String, Option<Vec<u8>>>> {
        let mut result: HashMap<String, Option<Vec<u8>>> = addresses.iter().map(|a| (a.clone(), None)).collect();
        if addresses.is_empty() {
            return Ok(result);
        }
        let sql = format!(
            "SELECT their_id, session FROM wa_sessions WHERE our_jid=?1 AND their_id IN ({})",
            placeholders(2, addresses.len())
        );
        let mut q = sqlx::query(&sql).bind(&self.jid);
        for a in addresses {
            q = q.bind(a);
        }
        for row in q.fetch_all(&self.db).await? {
            result.insert(row.try_get(0)?, row.try_get(1)?);
        }
        Ok(result)
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()> {
        put_session(&mut *self.db.acquire().await?, &self.jid, address, session).await
    }

    async fn put_many_sessions(&self, sessions: HashMap<String, Vec<u8>>) -> Result<()> {
        let mut tx = self.db.begin().await?;
        for (addr, sess) in &sessions {
            put_session(&mut tx, &self.jid, addr, sess).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_all_sessions(&self, phone: &str) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        Self::delete_like(&mut conn, "DELETE FROM wa_sessions WHERE our_jid=?1 AND their_id LIKE ?2", &self.jid, phone)
            .await
    }

    async fn delete_session(&self, address: &str) -> Result<()> {
        sqlx::query("DELETE FROM wa_sessions WHERE our_jid=?1 AND their_id=?2")
            .bind(&self.jid)
            .bind(address)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn migrate_pn_to_lid(&self, pn: &Jid, lid: &Jid) -> Result<()> {
        let pn_signal = pn.signal_address_user();
        if !self.migrated_pn_sessions.lock().expect("poisoned").insert(pn_signal.clone()) {
            return Ok(());
        }
        let lid_signal = lid.signal_address_user();
        let mut tx = self.db.begin().await?;
        let mut migrate = async |sql: &str, delete_sql: &str, what: &str| -> Result<u64> {
            let n = sqlx::query(sql)
                .bind(&self.jid)
                .bind(&pn_signal)
                .bind(&lid_signal)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("failed to migrate {what}"))?
                .rows_affected();
            Self::delete_like(&mut tx, delete_sql, &self.jid, &pn_signal)
                .await
                .with_context(|| format!("failed to delete extra {what}"))?;
            Ok(n)
        };
        let sessions =
            migrate(MIGRATE_SESSIONS, "DELETE FROM wa_sessions WHERE our_jid=?1 AND their_id LIKE ?2", "sessions")
                .await?;
        let identities = migrate(
            MIGRATE_IDENTITY_KEYS,
            "DELETE FROM wa_identity_keys WHERE our_jid=?1 AND their_id LIKE ?2",
            "identity keys",
        )
        .await?;
        let sender_keys = migrate(
            MIGRATE_SENDER_KEYS,
            "DELETE FROM wa_sender_keys WHERE our_jid=?1 AND sender_id LIKE ?2",
            "sender keys",
        )
        .await?;
        tx.commit().await?;
        if sessions > 0 || identities > 0 || sender_keys > 0 {
            tracing::info!(
                "Migrated {sessions} sessions, {identities} identity keys and {sender_keys} sender keys from {pn_signal} to {lid_signal}"
            );
        } else {
            tracing::debug!("No sessions or sender keys found to migrate from {pn_signal} to {lid_signal}");
        }
        Ok(())
    }
}

async fn put_session(conn: &mut SqliteConnection, jid: &str, address: &str, session: &[u8]) -> Result<()> {
    sqlx::query(
        "INSERT INTO wa_sessions (our_jid, their_id, session) VALUES (?1, ?2, ?3)
         ON CONFLICT (our_jid, their_id) DO UPDATE SET session=excluded.session",
    )
    .bind(jid)
    .bind(address)
    .bind(session)
    .execute(conn)
    .await?;
    Ok(())
}

fn pre_key_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<PreKey> {
    let id: i64 = row.try_get(0)?;
    let private = array::<32>(row.try_get(1)?)?;
    Ok(PreKey::from_private(id as u32, private))
}

#[async_trait]
impl PreKeyStore for SqlStore {
    async fn get_or_gen_pre_keys(&self, count: u32) -> Result<Vec<PreKey>> {
        let _guard = self.pre_key_lock.lock().await;
        let rows =
            sqlx::query("SELECT key_id, key FROM wa_pre_keys WHERE jid=?1 AND uploaded=false ORDER BY key_id LIMIT ?2")
                .bind(&self.jid)
                .bind(i64::from(count))
                .fetch_all(&self.db)
                .await
                .context("failed to query existing prekeys")?;
        let mut keys = rows.iter().map(pre_key_from_row).collect::<Result<Vec<_>>>()?;
        if (keys.len() as u32) < count {
            let mut next = self.next_pre_key_id().await?;
            while (keys.len() as u32) < count {
                keys.push(self.insert_pre_key(next, false).await.context("failed to generate prekey")?);
                next += 1;
            }
        }
        Ok(keys)
    }

    async fn gen_one_pre_key(&self) -> Result<PreKey> {
        let _guard = self.pre_key_lock.lock().await;
        let next = self.next_pre_key_id().await?;
        self.insert_pre_key(next, true).await
    }

    async fn get_pre_key(&self, id: u32) -> Result<Option<PreKey>> {
        let row = sqlx::query("SELECT key_id, key FROM wa_pre_keys WHERE jid=?1 AND key_id=?2")
            .bind(&self.jid)
            .bind(i64::from(id))
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(pre_key_from_row).transpose()
    }

    async fn remove_pre_key(&self, id: u32) -> Result<()> {
        sqlx::query("DELETE FROM wa_pre_keys WHERE jid=?1 AND key_id=?2")
            .bind(&self.jid)
            .bind(i64::from(id))
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn mark_pre_keys_as_uploaded(&self, up_to_id: u32) -> Result<()> {
        sqlx::query("UPDATE wa_pre_keys SET uploaded=true WHERE jid=?1 AND key_id<=?2")
            .bind(&self.jid)
            .bind(i64::from(up_to_id))
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn uploaded_pre_key_count(&self) -> Result<usize> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wa_pre_keys WHERE jid=?1 AND uploaded=true")
            .bind(&self.jid)
            .fetch_one(&self.db)
            .await?;
        Ok(n as usize)
    }
}

#[async_trait]
impl SenderKeyStore for SqlStore {
    async fn put_sender_key(&self, group: &str, user: &str, session: &[u8]) -> Result<()> {
        sqlx::query(
            "INSERT INTO wa_sender_keys (our_jid, chat_id, sender_id, sender_key) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (our_jid, chat_id, sender_id) DO UPDATE SET sender_key=excluded.sender_key",
        )
        .bind(&self.jid)
        .bind(group)
        .bind(user)
        .bind(session)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn get_sender_key(&self, group: &str, user: &str) -> Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT sender_key FROM wa_sender_keys WHERE our_jid=?1 AND chat_id=?2 AND sender_id=?3")
            .bind(&self.jid)
            .bind(group)
            .bind(user)
            .fetch_optional(&self.db)
            .await?)
    }
}

fn sync_key_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<AppStateSyncKey> {
    Ok(AppStateSyncKey { data: row.try_get(0)?, timestamp: row.try_get(1)?, fingerprint: row.try_get(2)? })
}

#[async_trait]
impl AppStateSyncKeyStore for SqlStore {
    async fn put_app_state_sync_key(&self, id: &[u8], key: AppStateSyncKey) -> Result<()> {
        sqlx::query(
            "INSERT INTO wa_app_state_sync_keys (jid, key_id, key_data, timestamp, fingerprint) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (jid, key_id) DO UPDATE
                SET key_data=excluded.key_data, timestamp=excluded.timestamp, fingerprint=excluded.fingerprint
                WHERE excluded.timestamp > wa_app_state_sync_keys.timestamp",
        )
        .bind(&self.jid)
        .bind(id)
        .bind(&key.data)
        .bind(key.timestamp)
        .bind(&key.fingerprint)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn get_app_state_sync_key(&self, id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        let row = sqlx::query(
            "SELECT key_data, timestamp, fingerprint FROM wa_app_state_sync_keys WHERE jid=?1 AND key_id=?2",
        )
        .bind(&self.jid)
        .bind(id)
        .fetch_optional(&self.db)
        .await?;
        row.as_ref().map(sync_key_from_row).transpose()
    }

    async fn get_latest_app_state_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT key_id FROM wa_app_state_sync_keys WHERE jid=?1 ORDER BY timestamp DESC LIMIT 1")
            .bind(&self.jid)
            .fetch_optional(&self.db)
            .await?)
    }

    async fn get_all_app_state_sync_keys(&self) -> Result<Vec<AppStateSyncKey>> {
        let rows = sqlx::query(
            "SELECT key_data, timestamp, fingerprint FROM wa_app_state_sync_keys WHERE jid=?1 ORDER BY timestamp DESC",
        )
        .bind(&self.jid)
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(sync_key_from_row).collect()
    }
}

#[async_trait]
impl AppStateStore for SqlStore {
    async fn put_app_state_version(&self, name: &str, version: u64, hash: [u8; 128]) -> Result<()> {
        sqlx::query(
            "INSERT INTO wa_app_state_version (jid, name, version, hash) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (jid, name) DO UPDATE SET version=excluded.version, hash=excluded.hash",
        )
        .bind(&self.jid)
        .bind(name)
        .bind(version as i64)
        .bind(&hash[..])
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn get_app_state_version(&self, name: &str) -> Result<(u64, [u8; 128])> {
        let row = sqlx::query("SELECT version, hash FROM wa_app_state_version WHERE jid=?1 AND name=?2")
            .bind(&self.jid)
            .bind(name)
            .fetch_optional(&self.db)
            .await?;
        let Some(row) = row else {
            return Ok((0, [0; 128]));
        };
        let version: i64 = row.try_get(0)?;
        let hash = array::<128>(row.try_get(1)?)?;
        if version == 0 {
            bail!("invalid saved app state version 0 for name {name}");
        }
        Ok((version as u64, hash))
    }

    async fn delete_app_state_version(&self, name: &str) -> Result<()> {
        sqlx::query("DELETE FROM wa_app_state_version WHERE jid=?1 AND name=?2")
            .bind(&self.jid)
            .bind(name)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn put_app_state_mutation_macs(
        &self,
        name: &str,
        version: u64,
        mutations: &[AppStateMutationMac],
    ) -> Result<()> {
        if mutations.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        for chunk in mutations.chunks(MUTATION_BATCH_SIZE) {
            let values = (0..chunk.len())
                .map(|i| format!("(?1, ?2, ?3, ?{}, ?{})", 4 + i * 2, 5 + i * 2))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "INSERT INTO wa_app_state_mutation_macs (jid, name, version, index_mac, value_mac) VALUES {values}"
            );
            let mut q = sqlx::query(&sql).bind(&self.jid).bind(name).bind(version as i64);
            for m in chunk {
                q = q.bind(&m.index_mac).bind(&m.value_mac);
            }
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_app_state_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> Result<()> {
        if index_macs.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "DELETE FROM wa_app_state_mutation_macs WHERE jid=?1 AND name=?2 AND index_mac IN ({})",
            placeholders(3, index_macs.len())
        );
        let mut q = sqlx::query(&sql).bind(&self.jid).bind(name);
        for m in index_macs {
            q = q.bind(m);
        }
        q.execute(&self.db).await?;
        Ok(())
    }

    async fn get_app_state_mutation_mac(&self, name: &str, index_mac: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar(
            "SELECT value_mac FROM wa_app_state_mutation_macs
             WHERE jid=?1 AND name=?2 AND index_mac=?3 ORDER BY version DESC LIMIT 1",
        )
        .bind(&self.jid)
        .bind(name)
        .bind(index_mac)
        .fetch_optional(&self.db)
        .await?)
    }
}

#[async_trait]
impl ContactStore for SqlStore {
    async fn put_push_name(&self, user: &Jid, push_name: &str) -> Result<(bool, String)> {
        let mut cache = self.contact_cache.lock().await;
        let cached = self.load_contact(&mut cache, user).await?;
        if cached.push_name == push_name {
            return Ok((false, String::new()));
        }
        sqlx::query(
            "INSERT INTO wa_contacts (our_jid, their_jid, push_name) VALUES (?1, ?2, ?3)
             ON CONFLICT (our_jid, their_jid) DO UPDATE SET push_name=excluded.push_name",
        )
        .bind(&self.jid)
        .bind(user.to_string())
        .bind(push_name)
        .execute(&self.db)
        .await?;
        let entry = cache.entry(user.clone()).or_default();
        entry.push_name = push_name.to_string();
        entry.found = true;
        Ok((true, cached.push_name))
    }

    async fn put_business_name(&self, user: &Jid, business_name: &str) -> Result<(bool, String)> {
        let mut cache = self.contact_cache.lock().await;
        let cached = self.load_contact(&mut cache, user).await?;
        if cached.business_name == business_name {
            return Ok((false, String::new()));
        }
        sqlx::query(
            "INSERT INTO wa_contacts (our_jid, their_jid, business_name) VALUES (?1, ?2, ?3)
             ON CONFLICT (our_jid, their_jid) DO UPDATE SET business_name=excluded.business_name",
        )
        .bind(&self.jid)
        .bind(user.to_string())
        .bind(business_name)
        .execute(&self.db)
        .await?;
        let entry = cache.entry(user.clone()).or_default();
        entry.business_name = business_name.to_string();
        entry.found = true;
        Ok((true, cached.business_name))
    }

    async fn put_contact_name(&self, user: &Jid, first_name: &str, full_name: &str) -> Result<()> {
        let mut cache = self.contact_cache.lock().await;
        let cached = self.load_contact(&mut cache, user).await?;
        if cached.first_name == first_name && cached.full_name == full_name {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO wa_contacts (our_jid, their_jid, first_name, full_name) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (our_jid, their_jid) DO UPDATE SET first_name=excluded.first_name, full_name=excluded.full_name",
        )
        .bind(&self.jid)
        .bind(user.to_string())
        .bind(first_name)
        .bind(full_name)
        .execute(&self.db)
        .await?;
        let entry = cache.entry(user.clone()).or_default();
        entry.first_name = first_name.to_string();
        entry.full_name = full_name.to_string();
        entry.found = true;
        Ok(())
    }

    async fn put_all_contact_names(&self, contacts: Vec<ContactEntry>) -> Result<()> {
        if contacts.is_empty() {
            return Ok(());
        }
        let orig_len = contacts.len();
        let contacts = dedupe_last(contacts, |c| c.jid.clone());
        if orig_len != contacts.len() {
            tracing::warn!("{} duplicate contacts found in PutAllContactNames", orig_len - contacts.len());
        }
        let mut tx = self.db.begin().await?;
        for chunk in contacts.chunks(CONTACT_BATCH_SIZE) {
            let values = (0..chunk.len())
                .map(|i| format!("(?1, ?{}, ?{}, ?{})", 2 + i * 3, 3 + i * 3, 4 + i * 3))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "INSERT INTO wa_contacts (our_jid, their_jid, first_name, full_name) VALUES {values}
                 ON CONFLICT (our_jid, their_jid) DO UPDATE SET first_name=excluded.first_name, full_name=excluded.full_name"
            );
            let mut q = sqlx::query(&sql).bind(&self.jid);
            for c in chunk {
                q = q.bind(c.jid.to_string()).bind(&c.first_name).bind(&c.full_name);
            }
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        // Just clear the cache, fetching push names and business names would be too much effort.
        self.contact_cache.lock().await.clear();
        Ok(())
    }

    async fn put_many_redacted_phones(&self, entries: Vec<RedactedPhoneEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let orig_len = entries.len();
        let entries = dedupe_last(entries, |e| e.jid.clone());
        if orig_len != entries.len() {
            tracing::warn!("{} duplicate contacts found in PutManyRedactedPhones", orig_len - entries.len());
        }
        let mut tx = self.db.begin().await?;
        for chunk in entries.chunks(CONTACT_BATCH_SIZE) {
            let values =
                (0..chunk.len()).map(|i| format!("(?1, ?{}, ?{})", 2 + i * 2, 3 + i * 2)).collect::<Vec<_>>().join(",");
            let sql = format!(
                "INSERT INTO wa_contacts (our_jid, their_jid, redacted_phone) VALUES {values}
                 ON CONFLICT (our_jid, their_jid) DO UPDATE SET redacted_phone=excluded.redacted_phone"
            );
            let mut q = sqlx::query(&sql).bind(&self.jid);
            for e in chunk {
                q = q.bind(e.jid.to_string()).bind(&e.redacted_phone);
            }
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let mut cache = self.contact_cache.lock().await;
        for e in &entries {
            if cache.get(&e.jid).is_some_and(|c| c.redacted_phone == e.redacted_phone) {
                continue;
            }
            cache.remove(&e.jid);
        }
        Ok(())
    }

    async fn get_contact(&self, user: &Jid) -> Result<ContactInfo> {
        let mut cache = self.contact_cache.lock().await;
        self.load_contact(&mut cache, user).await
    }

    async fn get_all_contacts(&self) -> Result<HashMap<Jid, ContactInfo>> {
        let mut cache = self.contact_cache.lock().await;
        let rows = sqlx::query(
            "SELECT their_jid, first_name, full_name, push_name, business_name, redacted_phone FROM wa_contacts WHERE our_jid=?1",
        )
        .bind(&self.jid)
        .fetch_all(&self.db)
        .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for row in rows {
            let jid: Jid = row.try_get::<String, _>(0)?.parse().context("error scanning row")?;
            let info = contact_from_row(&row, 1)?;
            cache.insert(jid.clone(), info.clone());
            out.insert(jid, info);
        }
        Ok(out)
    }
}

#[async_trait]
impl ChatSettingsStore for SqlStore {
    async fn put_muted_until(&self, chat: &Jid, muted_until: Option<SystemTime>) -> Result<()> {
        let val = match muted_until {
            None => 0,
            Some(t) if t == muted_forever() => -1,
            Some(t) => unix_secs(t),
        };
        self.put_chat_setting("muted_until", chat, val).await
    }

    async fn put_pinned(&self, chat: &Jid, pinned: bool) -> Result<()> {
        self.put_chat_setting("pinned", chat, pinned).await
    }

    async fn put_archived(&self, chat: &Jid, archived: bool) -> Result<()> {
        self.put_chat_setting("archived", chat, archived).await
    }

    async fn put_wasa_root_secret_id(&self, chat: &Jid, id: &str) -> Result<()> {
        self.put_chat_setting("wasa_root_secret_id", chat, id.to_string()).await
    }

    async fn get_wasa_root_secret_id(&self, chat: &Jid) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT wasa_root_secret_id FROM wa_chat_settings WHERE our_jid=?1 AND chat_jid=?2")
            .bind(&self.jid)
            .bind(chat.to_string())
            .fetch_optional(&self.db)
            .await?)
    }

    async fn get_chat_settings(&self, chat: &Jid) -> Result<LocalChatSettings> {
        let row =
            sqlx::query("SELECT muted_until, pinned, archived FROM wa_chat_settings WHERE our_jid=?1 AND chat_jid=?2")
                .bind(&self.jid)
                .bind(chat.to_string())
                .fetch_optional(&self.db)
                .await?;
        let Some(row) = row else {
            return Ok(LocalChatSettings::default());
        };
        let muted: i64 = row.try_get(0)?;
        Ok(LocalChatSettings {
            found: true,
            muted_until: match muted {
                m if m < 0 => Some(muted_forever()),
                0 => None,
                m => Some(from_unix_secs(m)),
            },
            pinned: row.try_get(1)?,
            archived: row.try_get(2)?,
        })
    }
}

/// Resolves `?N` to itself or its LID/PN alternative through the global map.
macro_rules! alt_jid_sql {
    ($p:literal, $else:literal) => {
        concat!(
            "(CASE WHEN ",
            $p,
            " LIKE '%@lid' THEN (SELECT pn || '@s.whatsapp.net' FROM wa_lid_map WHERE lid=replace(",
            $p,
            ", '@lid', '')) WHEN ",
            $p,
            " LIKE '%@s.whatsapp.net' THEN (SELECT lid || '@lid' FROM wa_lid_map WHERE pn=replace(",
            $p,
            ", '@s.whatsapp.net', '')) ",
            $else,
            " END)"
        )
    };
}

const PUT_MSG_SECRET: &str = "
    INSERT INTO wa_message_secrets (our_jid, chat_jid, sender_jid, message_id, key) VALUES (?1, ?2, ?3, ?4, ?5)
    ON CONFLICT (our_jid, chat_jid, sender_jid, message_id) DO NOTHING";
const GET_MSG_SECRET: &str = concat!(
    "SELECT key, sender_jid FROM wa_message_secrets WHERE our_jid=?1 AND (chat_jid=?2 OR chat_jid=",
    alt_jid_sql!("?2", ""),
    ") AND message_id=?4 AND (sender_jid=?3 OR sender_jid=",
    alt_jid_sql!("?3", ""),
    ")"
);

#[async_trait]
impl MsgSecretStore for SqlStore {
    async fn put_message_secrets(&self, inserts: &[MessageSecretInsert]) -> Result<()> {
        if inserts.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        for i in inserts {
            if i.chat.is_empty() || i.sender.is_empty() || i.id.is_empty() || i.secret.is_empty() {
                continue;
            }
            sqlx::query(PUT_MSG_SECRET)
                .bind(&self.jid)
                .bind(i.chat.to_non_ad().to_string())
                .bind(i.sender.to_non_ad().to_string())
                .bind(&i.id)
                .bind(&i.secret)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn put_message_secret(&self, chat: &Jid, sender: &Jid, id: &str, secret: &[u8]) -> Result<()> {
        sqlx::query(PUT_MSG_SECRET)
            .bind(&self.jid)
            .bind(chat.to_non_ad().to_string())
            .bind(sender.to_non_ad().to_string())
            .bind(id)
            .bind(secret)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn get_message_secret(&self, chat: &Jid, sender: &Jid, id: &str) -> Result<Option<(Vec<u8>, Jid)>> {
        let row = sqlx::query(GET_MSG_SECRET)
            .bind(&self.jid)
            .bind(chat.to_non_ad().to_string())
            .bind(sender.to_non_ad().to_string())
            .bind(id)
            .fetch_optional(&self.db)
            .await?;
        row.map(|r| -> Result<_> { Ok((r.try_get(0)?, r.try_get::<String, _>(1)?.parse()?)) }).transpose()
    }
}

const GET_PRIVACY_TOKEN: &str = concat!(
    "SELECT token, timestamp, sender_timestamp FROM wa_privacy_tokens WHERE our_jid=?1 AND (their_jid=?2 OR their_jid=",
    alt_jid_sql!("?2", "ELSE ?2"),
    ") ORDER BY timestamp DESC LIMIT 1"
);

#[async_trait]
impl PrivacyTokenStore for SqlStore {
    async fn put_privacy_tokens(&self, tokens: &[PrivacyToken]) -> Result<()> {
        if tokens.is_empty() {
            return Ok(());
        }
        let values = (0..tokens.len())
            .map(|i| format!("(?1, ?{}, ?{}, ?{}, ?{})", 2 + i * 4, 3 + i * 4, 4 + i * 4, 5 + i * 4))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO wa_privacy_tokens (our_jid, their_jid, token, timestamp, sender_timestamp) VALUES {values}
             ON CONFLICT (our_jid, their_jid) DO UPDATE SET
                token=excluded.token,
                timestamp=excluded.timestamp,
                sender_timestamp=COALESCE(excluded.sender_timestamp, wa_privacy_tokens.sender_timestamp)
             WHERE excluded.timestamp >= wa_privacy_tokens.timestamp"
        );
        let mut q = sqlx::query(&sql).bind(&self.jid);
        for t in tokens {
            q = q
                .bind(t.user.to_non_ad().to_string())
                .bind(&t.token)
                .bind(unix_secs(t.timestamp))
                .bind(t.sender_timestamp.map(unix_secs));
        }
        q.execute(&self.db).await?;
        Ok(())
    }

    async fn get_privacy_token(&self, user: &Jid) -> Result<Option<PrivacyToken>> {
        let user = user.to_non_ad();
        let row =
            sqlx::query(GET_PRIVACY_TOKEN).bind(&self.jid).bind(user.to_string()).fetch_optional(&self.db).await?;
        row.map(|r| -> Result<_> {
            Ok(PrivacyToken {
                user,
                token: r.try_get(0)?,
                timestamp: from_unix_secs(r.try_get(1)?),
                sender_timestamp: r.try_get::<Option<i64>, _>(2)?.map(from_unix_secs),
            })
        })
        .transpose()
    }

    async fn delete_expired_privacy_tokens(&self, cutoff: SystemTime) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM wa_privacy_tokens WHERE our_jid=?1 AND timestamp < ?2")
            .bind(&self.jid)
            .bind(unix_secs(cutoff))
            .execute(&self.db)
            .await?
            .rows_affected())
    }
}

#[async_trait]
impl NctSaltStore for SqlStore {
    async fn put_nct_salt(&self, salt: &[u8]) -> Result<()> {
        sqlx::query(
            "INSERT INTO wa_nct_salt (our_jid, salt) VALUES (?1, ?2) ON CONFLICT (our_jid) DO UPDATE SET salt=excluded.salt",
        )
        .bind(&self.jid)
        .bind(salt)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn get_nct_salt(&self) -> Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT salt FROM wa_nct_salt WHERE our_jid=?1")
            .bind(&self.jid)
            .fetch_optional(&self.db)
            .await?)
    }

    async fn delete_nct_salt(&self) -> Result<()> {
        sqlx::query("DELETE FROM wa_nct_salt WHERE our_jid=?1").bind(&self.jid).execute(&self.db).await?;
        Ok(())
    }
}

#[async_trait]
impl EventBuffer for SqlStore {
    async fn get_buffered_event(&self, ciphertext_hash: [u8; 32]) -> Result<Option<BufferedEvent>> {
        let row = sqlx::query(
            "SELECT plaintext, server_timestamp, insert_timestamp FROM wa_event_buffer WHERE our_jid=?1 AND ciphertext_hash=?2",
        )
        .bind(&self.jid)
        .bind(&ciphertext_hash[..])
        .fetch_optional(&self.db)
        .await?;
        row.map(|r| -> Result<_> {
            Ok(BufferedEvent {
                plaintext: r.try_get(0)?,
                server_time: from_unix_secs(r.try_get(1)?),
                insert_time: from_unix_millis(r.try_get(2)?),
            })
        })
        .transpose()
    }

    async fn put_buffered_event(
        &self,
        ciphertext_hash: [u8; 32],
        plaintext: &[u8],
        server_timestamp: SystemTime,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO wa_event_buffer (our_jid, ciphertext_hash, plaintext, server_timestamp, insert_timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&self.jid)
        .bind(&ciphertext_hash[..])
        .bind(plaintext)
        .bind(unix_secs(server_timestamp))
        .bind(unix_millis(SystemTime::now()))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Serializes decryptions for this device.
    ///
    /// Go runs `fn` inside one SQL transaction carried by `context.Context`.
    /// Store calls made by `work` here use the pool directly, so they are
    /// serialized but not atomic yet — a shared transaction handle arrives with
    /// the Signal integration (Phase 4).
    async fn do_decryption_txn<'a>(&'a self, work: TxnFuture<'a>) -> Result<()> {
        let _guard = self.decryption_lock.lock().await;
        work.await
    }

    async fn clear_buffered_event_plaintext(&self, ciphertext_hash: [u8; 32]) -> Result<()> {
        sqlx::query("UPDATE wa_event_buffer SET plaintext=NULL WHERE our_jid=?1 AND ciphertext_hash=?2")
            .bind(&self.jid)
            .bind(&ciphertext_hash[..])
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn delete_old_buffered_hashes(&self) -> Result<()> {
        sqlx::query("DELETE FROM wa_event_buffer WHERE insert_timestamp < ?1")
            .bind(unix_millis(SystemTime::now() - EVENT_BUFFER_TTL))
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn get_outgoing_event(&self, chat: &Jid, alt_chat: &Jid, id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row = sqlx::query(
            "SELECT format, plaintext FROM wa_retry_buffer WHERE our_jid=?1 AND (chat_jid=?2 OR chat_jid=?3) AND message_id=?4",
        )
        .bind(&self.jid)
        .bind(chat.to_string())
        .bind(alt_chat.to_string())
        .bind(id)
        .fetch_optional(&self.db)
        .await?;
        row.map(|r| -> Result<_> { Ok((r.try_get(0)?, r.try_get(1)?)) }).transpose()
    }

    async fn add_outgoing_event(&self, chat: &Jid, id: &str, format: &str, plaintext: &[u8]) -> Result<()> {
        sqlx::query(
            "INSERT INTO wa_retry_buffer (our_jid, chat_jid, message_id, format, plaintext, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (our_jid, chat_jid, message_id) DO UPDATE
                SET format=excluded.format, plaintext=excluded.plaintext, timestamp=excluded.timestamp",
        )
        .bind(&self.jid)
        .bind(chat.to_string())
        .bind(id)
        .bind(format)
        .bind(plaintext)
        .bind(unix_millis(SystemTime::now()))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn delete_old_outgoing_events(&self) -> Result<()> {
        sqlx::query("DELETE FROM wa_retry_buffer WHERE our_jid=?1 AND timestamp < ?2")
            .bind(&self.jid)
            .bind(unix_millis(SystemTime::now() - RETRY_BUFFER_TTL))
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_keeps_last_value_in_first_position() {
        let v = dedupe_last(vec![(1, "a"), (2, "b"), (1, "c")], |t| t.0);
        assert_eq!(v, vec![(1, "c"), (2, "b")]);
    }

    #[test]
    fn unix_conversions_roundtrip() {
        let t = from_unix_secs(1_700_000_000);
        assert_eq!(unix_secs(t), 1_700_000_000);
        assert_eq!(unix_millis(from_unix_millis(1_700_000_000_123)), 1_700_000_000_123);
        assert_eq!(unix_secs(from_unix_secs(-5)), -5);
    }

    #[test]
    fn alt_jid_sql_is_well_formed() {
        assert!(GET_MSG_SECRET.contains("replace(?2, '@lid', '')"));
        assert!(GET_PRIVACY_TOKEN.contains("ELSE ?2 END"));
    }
}
