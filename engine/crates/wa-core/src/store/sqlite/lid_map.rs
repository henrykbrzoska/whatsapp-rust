// Ported from the Go client (`store/sqlstore/lidmap.go`), © Tulir Asokan,
// MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `CachedLIDMap` — the **global** LID ↔ PN map. One instance per
//! [`Container`](super::Container), shared by every device: a per-device copy
//! would go stale and drop messages (`AGENTS.md` §8).

use std::collections::HashMap;

use anyhow::bail;
use async_trait::async_trait;
use sqlx::{Row, SqliteConnection, SqlitePool};
use tokio::sync::RwLock;

use crate::store::traits::{LidMapping, LidStore, Result};
use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid};

const DELETE_EXISTING_LID_MAPPING: &str = "DELETE FROM wa_lid_map WHERE (lid<>?1 AND pn=?2)";
const PUT_LID_MAPPING: &str = "
    INSERT INTO wa_lid_map (lid, pn) VALUES (?1, ?2)
    ON CONFLICT (lid) DO UPDATE SET pn=excluded.pn WHERE wa_lid_map.pn<>excluded.pn
";
const GET_LID_FOR_PN: &str = "SELECT lid FROM wa_lid_map WHERE pn=?1";
const GET_PN_FOR_LID: &str = "SELECT pn FROM wa_lid_map WHERE lid=?1";
const GET_ALL_LID_MAPPINGS: &str = "SELECT lid, pn FROM wa_lid_map";

#[derive(Default)]
struct Cache {
    /// PN user → LID user. An empty value caches "known not to exist".
    pn_to_lid: HashMap<String, String>,
    lid_to_pn: HashMap<String, String>,
    filled: bool,
}

impl Cache {
    fn insert(&mut self, lid: &str, pn: &str) {
        self.pn_to_lid.insert(pn.to_string(), lid.to_string());
        self.lid_to_pn.insert(lid.to_string(), pn.to_string());
    }
}

#[derive(Clone, Copy)]
enum Direction {
    PnToLid,
    LidToPn,
}

pub struct CachedLidMap {
    db: SqlitePool,
    cache: RwLock<Cache>,
}

fn valid_pair(lid: &Jid, pn: &Jid) -> bool {
    lid.server == HIDDEN_USER_SERVER && pn.server == DEFAULT_USER_SERVER
}

impl CachedLidMap {
    pub fn new(db: SqlitePool) -> Self {
        Self { db, cache: RwLock::new(Cache::default()) }
    }

    /// `FillCache` — loads every mapping; afterwards misses don't hit the DB.
    pub async fn fill_cache(&self) -> Result<()> {
        let mut cache = self.cache.write().await;
        let rows = sqlx::query(GET_ALL_LID_MAPPINGS).fetch_all(&self.db).await;
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                cache.filled = false;
                return Err(e.into());
            }
        };
        for row in rows {
            let (lid, pn): (String, String) = (row.try_get(0)?, row.try_get(1)?);
            cache.insert(&lid, &pn);
        }
        cache.filled = true;
        Ok(())
    }

    async fn get_mapping(&self, source: &Jid, dir: Direction) -> Result<Option<Jid>> {
        let (target_server, query) = match dir {
            Direction::PnToLid => (HIDDEN_USER_SERVER, GET_LID_FOR_PN),
            Direction::LidToPn => (DEFAULT_USER_SERVER, GET_PN_FOR_LID),
        };
        let to_jid = |user: &str| {
            (!user.is_empty()).then(|| Jid {
                user: user.to_string(),
                device: source.device,
                ..Jid::new("", target_server)
            })
        };
        {
            let cache = self.cache.read().await;
            let map = match dir {
                Direction::PnToLid => &cache.pn_to_lid,
                Direction::LidToPn => &cache.lid_to_pn,
            };
            if let Some(target) = map.get(&source.user) {
                return Ok(to_jid(target));
            }
            if cache.filled {
                return Ok(None);
            }
        }
        let mut cache = self.cache.write().await;
        let target: String =
            sqlx::query_scalar(query).bind(&source.user).fetch_optional(&self.db).await?.unwrap_or_default();
        let Cache { pn_to_lid, lid_to_pn, .. } = &mut *cache;
        let (forward, backward) = match dir {
            Direction::PnToLid => (pn_to_lid, lid_to_pn),
            Direction::LidToPn => (lid_to_pn, pn_to_lid),
        };
        forward.insert(source.user.clone(), target.clone());
        if !target.is_empty() {
            backward.insert(target.clone(), source.user.clone());
        }
        Ok(to_jid(&target))
    }

    /// Writes one mapping (caller holds the cache write lock and a transaction).
    async fn put_locked(cache: &mut Cache, conn: &mut SqliteConnection, lid: &Jid, pn: &Jid) -> Result<()> {
        if !valid_pair(lid, pn) {
            bail!("invalid PutLIDMapping call {lid}/{pn}");
        }
        sqlx::query(DELETE_EXISTING_LID_MAPPING).bind(&lid.user).bind(&pn.user).execute(&mut *conn).await?;
        sqlx::query(PUT_LID_MAPPING).bind(&lid.user).bind(&pn.user).execute(&mut *conn).await?;
        let old_lid = cache.pn_to_lid.get(&pn.user).cloned().unwrap_or_default();
        let old_pn = cache.lid_to_pn.get(&lid.user).cloned().unwrap_or_default();
        cache.insert(&lid.user, &pn.user);
        if !old_pn.is_empty() && old_pn != pn.user && cache.pn_to_lid.get(&old_pn) == Some(&lid.user) {
            cache.pn_to_lid.remove(&old_pn);
        }
        if !old_lid.is_empty() && old_lid != lid.user && cache.lid_to_pn.get(&old_lid) == Some(&pn.user) {
            cache.lid_to_pn.remove(&old_lid);
        }
        Ok(())
    }
}

#[async_trait]
impl LidStore for CachedLidMap {
    async fn put_many_lid_mappings(&self, mappings: &[LidMapping]) -> Result<()> {
        let mut cache = self.cache.write().await;
        // Drop invalid and already-known entries, then dedupe (the last entry per PN wins).
        let mut order: Vec<&str> = Vec::new();
        let mut latest: HashMap<&str, &LidMapping> = HashMap::new();
        for m in mappings {
            if !valid_pair(&m.lid, &m.pn) {
                tracing::debug!(lid = %m.lid, pn = %m.pn, "Ignoring invalid entry in PutManyLIDMappings");
                continue;
            }
            if cache.pn_to_lid.get(&m.pn.user) == Some(&m.lid.user) {
                continue;
            }
            if latest.insert(&m.pn.user, m).is_none() {
                order.push(&m.pn.user);
            }
        }
        if order.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        for pn in order {
            let m = latest[pn];
            Self::put_locked(&mut cache, &mut tx, &m.lid, &m.pn).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn put_lid_mapping(&self, lid: &Jid, pn: &Jid) -> Result<()> {
        if !valid_pair(lid, pn) {
            bail!("invalid PutLIDMapping call {lid}/{pn}");
        }
        let mut cache = self.cache.write().await;
        if cache.pn_to_lid.get(&pn.user) == Some(&lid.user) {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        Self::put_locked(&mut cache, &mut tx, lid, pn).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_pn_for_lid(&self, lid: &Jid) -> Result<Option<Jid>> {
        if lid.server != HIDDEN_USER_SERVER {
            bail!("invalid GetPNForLID call with non-LID JID {lid}");
        }
        self.get_mapping(lid, Direction::LidToPn).await
    }

    async fn get_lid_for_pn(&self, pn: &Jid) -> Result<Option<Jid>> {
        if pn.server != DEFAULT_USER_SERVER {
            bail!("invalid GetLIDForPN call with non-PN JID {pn}");
        }
        self.get_mapping(pn, Direction::PnToLid).await
    }

    async fn get_many_lids_for_pns(&self, pns: &[Jid]) -> Result<HashMap<Jid, Jid>> {
        let mut result = HashMap::with_capacity(pns.len());
        let mut missing: HashMap<String, Vec<&Jid>> = HashMap::new();
        {
            let cache = self.cache.read().await;
            for pn in pns.iter().filter(|p| p.server == DEFAULT_USER_SERVER) {
                match cache.pn_to_lid.get(&pn.user) {
                    Some(lid) if !lid.is_empty() => {
                        result.insert(pn.clone(), Jid { user: lid.clone(), ..pn.with_server(HIDDEN_USER_SERVER) });
                    }
                    _ if !cache.filled => missing.entry(pn.user.clone()).or_default().push(pn),
                    _ => {}
                }
            }
        }
        if missing.is_empty() {
            return Ok(result);
        }

        let mut cache = self.cache.write().await;
        let placeholders = (1..=missing.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT lid, pn FROM wa_lid_map WHERE pn IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for user in missing.keys() {
            q = q.bind(user);
        }
        for row in q.fetch_all(&self.db).await? {
            let (lid, pn): (String, String) = (row.try_get(0)?, row.try_get(1)?);
            cache.insert(&lid, &pn);
            for dev in missing.get(&pn).into_iter().flatten() {
                result.insert((*dev).clone(), Jid { user: lid.clone(), ..dev.with_server(HIDDEN_USER_SERVER) });
            }
        }
        Ok(result)
    }
}
