//! Local chat database (see `ARCHITECTURE.md` §6).
//!
//! * `messages` — one row per message, indexed by `(chat_jid, timestamp)`.
//! * `messages_fts` — FTS5 external-content index (trigram tokenizer).
//! * `message_embeddings` — sqlite-vec `vec0` table (384d, cosine), keyed by
//!   the `messages` rowid.
//!
//! Search is keyword (BM25), vector (KNN) or hybrid (Reciprocal Rank Fusion).
//! Without an embedder the vector side is simply empty, so hybrid degrades to
//! keyword search.

pub mod embed;
pub mod export;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Once};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use specta::Type;

pub use embed::{DIM, Embedder, HashEmbedder};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS messages(
    id        TEXT PRIMARY KEY,
    chat_jid  TEXT NOT NULL,
    sender    TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    type      TEXT NOT NULL,
    text      TEXT,
    raw_json  TEXT,
    from_me   INTEGER NOT NULL DEFAULT 0,
    push_name TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_chat_ts ON messages(chat_jid, timestamp, id);

-- Decrypted attachment bytes, kept only long enough for the UI to show them
-- (see `Store::purge_media_blobs`). Not a cache of record: the bytes can
-- always be downloaded again from WhatsApp.
CREATE TABLE IF NOT EXISTS media_blobs(
    message_id TEXT PRIMARY KEY,
    mime       TEXT NOT NULL,
    bytes      BLOB NOT NULL,
    fetched_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_media_blobs_fetched ON media_blobs(fetched_at);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    text, chat_jid UNINDEXED, content='messages', content_rowid='rowid', tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, text, chat_jid) VALUES (new.rowid, new.text, new.chat_jid);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text, chat_jid) VALUES ('delete', old.rowid, old.text, old.chat_jid);
    DELETE FROM message_embeddings WHERE rowid = old.rowid;
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text, chat_jid) VALUES ('delete', old.rowid, old.text, old.chat_jid);
    INSERT INTO messages_fts(rowid, text, chat_jid) VALUES (new.rowid, new.text, new.chat_jid);
END;
"#;

const RRF_K: f64 = 60.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct StoredMessage {
    pub id: String,
    pub chat_jid: String,
    pub sender: String,
    pub timestamp: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<String>,
    /// `true` for a message this device sent (`MessageInfo::is_from_me`).
    /// Older rows (written before the column existed) default to `false`.
    #[serde(default)]
    pub from_me: bool,
    /// The sender's own display name (`MessageInfo::push_name`, the `notify`
    /// attribute). The only name available for a chat that is not in the
    /// address book — which is every `@lid` chat before contact sync runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    #[default]
    Hybrid,
    Keyword,
    Vector,
}

impl std::str::FromStr for SearchMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hybrid" | "" => Ok(Self::Hybrid),
            "keyword" => Ok(Self::Keyword),
            "vector" => Ok(Self::Vector),
            o => Err(format!("unknown search mode `{o}`")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SearchHit {
    pub score: f64,
    pub message: StoredMessage,
}

#[derive(Debug, Clone, Serialize, PartialEq, Type)]
pub struct ChatStats {
    pub count: i64,
    pub first_timestamp: Option<i64>,
    pub last_timestamp: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Type)]
pub struct ChatSummary {
    pub jid: String,
    pub count: i64,
    pub last_timestamp: i64,
    pub last_text: Option<String>,
    /// The most recent push name an incoming message of this chat carried —
    /// the chat's own name when the address book has no row for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Chat-settings / contact read path for the UI (Phase 5, Task E).
//
// `wa-store` deliberately does not depend on `wa-core`: the authoritative
// mute/pin/archive state lives in `wa_chat_settings` (wa-core
// `ChatSettingsStore`) and names in `wa_contacts` (wa-core `ContactStore`).
// `bebok-server` reads those async traits, converts each row with
// [`ChatSettingsView::from_local`] / [`ContactView`], and hands the maps to
// [`Store::list_chats_with_meta`]. No schema here changes.
// ---------------------------------------------------------------------------

/// Unix seconds of wa-core `muted_forever()` (9999-12-31T23:59:59Z).
pub const MUTED_FOREVER: i64 = 253_402_300_799;

/// UI view of wa-core `LocalChatSettings` (rows that are not `found` map to the default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ChatSettingsView {
    /// Unix seconds; `None` = not muted, [`MUTED_FOREVER`] = muted forever.
    pub muted_until: Option<i64>,
    pub pinned: bool,
    pub archived: bool,
}

impl ChatSettingsView {
    /// Build from the fields of wa-core `LocalChatSettings`.
    pub fn from_local(muted_until: Option<std::time::SystemTime>, pinned: bool, archived: bool) -> Self {
        Self { muted_until: muted_until.map(system_time_to_unix), pinned, archived }
    }

    /// Muted at `now` (unix seconds)? A mute that already expired is not a mute.
    pub fn is_muted(&self, now: i64) -> bool {
        self.muted_until.is_some_and(|t| t > now)
    }

    pub fn is_muted_forever(&self) -> bool {
        self.muted_until.is_some_and(|t| t >= MUTED_FOREVER)
    }
}

/// UI view of wa-core `ContactInfo` (also usable for group names from the group cache).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ContactView {
    pub first_name: String,
    pub full_name: String,
    pub push_name: String,
    pub business_name: String,
}

impl ContactView {
    /// A view carrying just a display name (e.g. a group subject).
    pub fn named(name: impl Into<String>) -> Self {
        Self { full_name: name.into(), ..Self::default() }
    }

    /// Address-book name first, then business name, then push name.
    pub fn display_name(&self) -> Option<&str> {
        [&self.full_name, &self.first_name, &self.business_name, &self.push_name]
            .into_iter()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
    }
}

/// Which chats `list_chats_with_meta` returns, by archive state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveFilter {
    /// Archived and unarchived chats.
    #[default]
    All,
    /// Only chats that are not archived (main chat list).
    Active,
    /// Only archived chats.
    Archived,
}

impl std::str::FromStr for ArchiveFilter {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "all" | "" => Ok(Self::All),
            "active" | "false" => Ok(Self::Active),
            "archived" | "true" => Ok(Self::Archived),
            o => Err(format!("unknown archive filter `{o}`")),
        }
    }
}

/// One chat-list row: message summary + local settings + display name.
#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct ChatListEntry {
    pub jid: String,
    pub name: Option<String>,
    pub count: i64,
    pub last_timestamp: i64,
    pub last_text: Option<String>,
    /// Muted at the time of the call (expired mutes are `false`).
    pub muted: bool,
    pub muted_until: Option<i64>,
    pub pinned: bool,
    pub archived: bool,
}

/// Merge chat summaries with settings and contacts, keyed by chat JID string.
///
/// Order: pinned first, then most recent activity, then JID (stable for ties).
pub fn merge_chat_list(
    summaries: Vec<ChatSummary>,
    settings: &HashMap<String, ChatSettingsView>,
    contacts: &HashMap<String, ContactView>,
    now: i64,
    filter: ArchiveFilter,
) -> Vec<ChatListEntry> {
    let mut out: Vec<ChatListEntry> = summaries
        .into_iter()
        .filter_map(|s| {
            let cs = settings.get(&s.jid).copied().unwrap_or_default();
            let keep = match filter {
                ArchiveFilter::All => true,
                ArchiveFilter::Active => !cs.archived,
                ArchiveFilter::Archived => cs.archived,
            };
            keep.then(|| ChatListEntry {
                // Address book first, then the name the chat itself announced.
                name: contacts
                    .get(&s.jid)
                    .and_then(|c| c.display_name())
                    .map(str::to_owned)
                    .or_else(|| s.push_name.clone().filter(|n| !n.trim().is_empty())),
                muted: cs.is_muted(now),
                muted_until: cs.muted_until,
                pinned: cs.pinned,
                archived: cs.archived,
                jid: s.jid,
                count: s.count,
                last_timestamp: s.last_timestamp,
                last_text: s.last_text,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.pinned.cmp(&a.pinned).then(b.last_timestamp.cmp(&a.last_timestamp)).then_with(|| a.jid.cmp(&b.jid))
    });
    out
}

/// `SystemTime` → unix seconds (pre-epoch clamps to negative seconds, overflow saturates).
pub fn system_time_to_unix(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_secs()).map(|s| -s).unwrap_or(i64::MIN),
    }
}

/// Current unix time in seconds (for [`Store::list_chats_with_meta`]).
pub fn unix_now() -> i64 {
    system_time_to_unix(std::time::SystemTime::now())
}

pub struct Store {
    conn: Mutex<Connection>,
    embedder: Option<Arc<dyn Embedder>>,
}

fn register_sqlite_vec() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: registers the statically linked sqlite-vec entry point for
        // every connection opened afterwards; signature matches the
        // `sqlite3_auto_extension` callback ABI.
        unsafe {
            #[allow(clippy::missing_transmute_annotations)]
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())));
        }
    });
}

impl Store {
    pub fn open(path: &Path, embedder: Option<Arc<dyn Embedder>>) -> anyhow::Result<Self> {
        register_sqlite_vec();
        Self::init(Connection::open(path)?, embedder)
    }

    pub fn open_memory(embedder: Option<Arc<dyn Embedder>>) -> anyhow::Result<Self> {
        register_sqlite_vec();
        Self::init(Connection::open_in_memory()?, embedder)
    }

    fn init(conn: Connection, embedder: Option<Arc<dyn Embedder>>) -> anyhow::Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS message_embeddings USING vec0(embedding float[{DIM}] distance_metric=cosine);"
        ))?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn), embedder })
    }

    pub fn has_embedder(&self) -> bool {
        self.embedder.is_some()
    }

    /// Insert or replace messages in one transaction (FTS via triggers,
    /// embeddings computed inline when an embedder is configured).
    pub fn insert_messages(&self, msgs: &[StoredMessage]) -> anyhow::Result<()> {
        let embeddings = match &self.embedder {
            Some(e) => {
                let texts: Vec<&str> = msgs.iter().map(|m| m.text.as_deref().unwrap_or("")).collect();
                Some(e.embed(&texts)?)
            }
            None => None,
        };
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for (i, m) in msgs.iter().enumerate() {
            tx.execute("DELETE FROM messages WHERE id = ?1", params![m.id])?;
            tx.execute(
                "INSERT INTO messages(id, chat_jid, sender, timestamp, type, text, raw_json, from_me, push_name)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![m.id, m.chat_jid, m.sender, m.timestamp, m.kind, m.text, m.raw_json, m.from_me, m.push_name],
            )?;
            if let (Some(embs), Some(_)) = (&embeddings, &m.text) {
                let rowid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO message_embeddings(rowid, embedding) VALUES (?1, ?2)",
                    params![rowid, f32_bytes(&embs[i])],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_message(&self, msg: &StoredMessage) -> anyhow::Result<()> {
        self.insert_messages(std::slice::from_ref(msg))
    }

    /// Store decrypted attachment bytes for `message_id`.
    ///
    /// The blob is a convenience for the UI, not a record: [`purge_media_blobs`]
    /// drops it after an hour and the next view downloads it again.
    ///
    /// [`purge_media_blobs`]: Self::purge_media_blobs
    pub fn put_media_blob(&self, message_id: &str, mime: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO media_blobs(message_id, mime, bytes, fetched_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(message_id) DO UPDATE SET mime = ?2, bytes = ?3, fetched_at = ?4",
            params![message_id, mime, bytes, unix_now()],
        )?;
        Ok(())
    }

    /// The cached bytes of one attachment, with their MIME type.
    pub fn get_media_blob(&self, message_id: &str) -> anyhow::Result<Option<(String, Vec<u8>)>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT mime, bytes FROM media_blobs WHERE message_id = ?1", params![message_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?)
    }

    /// Drop every cached attachment that is at least `max_age_secs` old
    /// (`0` empties the cache). Returns how many went.
    pub fn purge_media_blobs(&self, max_age_secs: i64) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let cutoff = unix_now() - max_age_secs.max(0);
        Ok(conn.execute("DELETE FROM media_blobs WHERE fetched_at <= ?1", params![cutoff])?)
    }

    /// How many attachments are cached right now (diagnostics).
    pub fn count_media_blobs(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM media_blobs", [], |r| r.get(0))?)
    }

    /// Insert messages that came from a history-sync blob, keeping whatever is
    /// already stored.
    ///
    /// A live message is richer than its history copy (it was decrypted here,
    /// with the sender's device and the push name of the moment), so an
    /// existing row wins — unlike [`Self::insert_messages`], which replaces.
    /// Returns how many rows were new.
    pub fn insert_messages_if_new(&self, msgs: &[StoredMessage]) -> anyhow::Result<usize> {
        let mut fresh = Vec::with_capacity(msgs.len());
        {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare_cached("SELECT 1 FROM messages WHERE id = ?1")?;
            for m in msgs {
                if !stmt.exists(params![m.id])? {
                    fresh.push(m.clone());
                }
            }
        }
        if fresh.is_empty() {
            return Ok(0);
        }
        self.insert_messages(&fresh)?;
        Ok(fresh.len())
    }

    pub fn delete_chat(&self, chat: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM media_blobs WHERE message_id IN (SELECT id FROM messages WHERE chat_jid = ?1)",
            params![chat],
        )?;
        Ok(conn.execute("DELETE FROM messages WHERE chat_jid = ?1", params![chat])?)
    }

    pub fn chat_stats(&self, chat: &str) -> anyhow::Result<ChatStats> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*), MIN(timestamp), MAX(timestamp) FROM messages WHERE chat_jid = ?1",
            params![chat],
            |r| Ok(ChatStats { count: r.get(0)?, first_timestamp: r.get(1)?, last_timestamp: r.get(2)? }),
        )?)
    }

    /// Every chat with at least one message, most recent activity first.
    pub fn list_chats(&self) -> anyhow::Result<Vec<ChatSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT chat_jid, COUNT(*), MAX(timestamp),
                    (SELECT text FROM messages m2 WHERE m2.chat_jid = m.chat_jid
                     ORDER BY timestamp DESC, id DESC LIMIT 1),
                    (SELECT push_name FROM messages m3 WHERE m3.chat_jid = m.chat_jid
                     AND m3.from_me = 0 AND m3.push_name IS NOT NULL AND m3.push_name <> ''
                     ORDER BY timestamp DESC, id DESC LIMIT 1)
             FROM messages m GROUP BY chat_jid ORDER BY MAX(timestamp) DESC, chat_jid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ChatSummary {
                jid: r.get(0)?,
                count: r.get(1)?,
                last_timestamp: r.get(2)?,
                last_text: r.get(3)?,
                push_name: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Distinct chat JIDs with at least one message — the keys `bebok-server`
    /// must look up in `ChatSettingsStore` before [`Self::list_chats_with_meta`].
    pub fn chat_jids(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("wa-store connection mutex poisoned"))?;
        let mut stmt = conn.prepare_cached("SELECT DISTINCT chat_jid FROM messages ORDER BY chat_jid")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// [`Self::list_chats`] enriched with mute/pin/archive and display names
    /// (see [`merge_chat_list`] for ordering and filtering).
    pub fn list_chats_with_meta(
        &self,
        settings: &HashMap<String, ChatSettingsView>,
        contacts: &HashMap<String, ContactView>,
        now: i64,
        filter: ArchiveFilter,
    ) -> anyhow::Result<Vec<ChatListEntry>> {
        Ok(merge_chat_list(self.list_chats()?, settings, contacts, now, filter))
    }

    pub fn get_message(&self, id: &str) -> anyhow::Result<Option<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, chat_jid, sender, timestamp, type, text, raw_json, from_me, push_name FROM messages WHERE id = ?1",
                params![id],
                row_to_msg,
            )
            .optional()?)
    }

    /// Rows stored under a stanza kind (`media`, `text`) that still have their
    /// payload, as `(id, raw_json)`.
    ///
    /// An incoming stanza only says `type="media"`, so older rows never got a
    /// real attachment kind and the UI cannot tell a photo from a voice note.
    /// The payload can: decoding it is wa-core's job, so this only hands over
    /// the candidates (see `bebok-server::repair_message_kinds`).
    pub fn messages_with_payload(&self, kinds: &[&str]) -> anyhow::Result<Vec<(String, String)>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let placeholders = (1..=kinds.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, raw_json FROM messages WHERE raw_json IS NOT NULL AND type IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(kinds), |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Correct one message's kind. Returns `false` when no row matched.
    ///
    /// The FTS trigger re-indexes the row on update, so the search index stays
    /// in step.
    pub fn set_message_kind(&self, id: &str, kind: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("UPDATE messages SET type = ?2 WHERE id = ?1", params![id, kind])? > 0)
    }

    /// Every distinct `chat_jid` in the message table.
    pub fn chat_jids_stored(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT DISTINCT chat_jid FROM messages")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Move every message of `from` into `to`, keeping what `to` already has.
    ///
    /// The same conversation can end up under two JIDs — live messages arrive
    /// LID-addressed, a history blob comes back in phone-number form — and
    /// then the thread the user reads is missing half of itself. A message ID
    /// that exists in both is dropped from `from`: the copy already in `to`
    /// was stored by the live path, which knows more (the sender's device, the
    /// push name).
    ///
    /// Returns how many rows moved. The FTS trigger re-indexes them.
    pub fn merge_chat(&self, from: &str, to: &str) -> anyhow::Result<usize> {
        if from == to {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE chat_jid = ?1 AND id IN (SELECT id FROM messages WHERE chat_jid = ?2)",
            params![from, to],
        )?;
        let moved = tx.execute("UPDATE messages SET chat_jid = ?2 WHERE chat_jid = ?1", params![from, to])?;
        tx.commit()?;
        Ok(moved)
    }

    /// Keyset page of a chat ordered by `(timestamp, id)`, strictly after `after`.
    pub fn page(
        &self,
        chat: &str,
        after: Option<(i64, &str)>,
        from: Option<i64>,
        to: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let (ats, aid) = after.unwrap_or((i64::MIN, ""));
        let mut stmt = conn.prepare_cached(
            "SELECT id, chat_jid, sender, timestamp, type, text, raw_json, from_me, push_name FROM messages
             WHERE chat_jid = ?1 AND (timestamp, id) > (?2, ?3)
               AND timestamp >= ?4 AND timestamp <= ?5
             ORDER BY timestamp, id LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            params![chat, ats, aid, from.unwrap_or(i64::MIN), to.unwrap_or(i64::MAX), limit as i64],
            row_to_msg,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The newest `limit` messages of a chat, strictly *older* than `before`
    /// (a `(timestamp, id)` keyset cursor), returned oldest-first.
    ///
    /// This is the backward page a message view needs: open a chat with
    /// `before: None` to get its tail, then pass the first row of the page you
    /// hold to walk further back. Ordering matches [`Self::page`]
    /// (`(timestamp, id)`), so the two cursors are interchangeable.
    pub fn page_before(
        &self,
        chat: &str,
        before: Option<(i64, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let (bts, bid) = before.unwrap_or((i64::MAX, "\u{10ffff}"));
        let mut stmt = conn.prepare_cached(
            "SELECT id, chat_jid, sender, timestamp, type, text, raw_json, from_me, push_name FROM messages
             WHERE chat_jid = ?1 AND (timestamp, id) < (?2, ?3)
             ORDER BY timestamp DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![chat, bts, bid, limit as i64], row_to_msg)?;
        let mut page: Vec<StoredMessage> = rows.collect::<Result<_, _>>()?;
        page.reverse();
        Ok(page)
    }

    /// How many messages of a chat are strictly older than `before`.
    /// Lets the message view tell "load older" from "you are at the top".
    pub fn count_before(&self, chat: &str, before: Option<(i64, &str)>) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let (bts, bid) = before.unwrap_or((i64::MAX, "\u{10ffff}"));
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE chat_jid = ?1 AND (timestamp, id) < (?2, ?3)",
            params![chat, bts, bid],
            |r| r.get(0),
        )?)
    }

    pub fn search(
        &self,
        query: &str,
        chat: Option<&str>,
        limit: usize,
        mode: SearchMode,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let pool = limit * 4;
        let keyword = if mode != SearchMode::Vector { self.keyword_ids(query, chat, pool)? } else { Vec::new() };
        let vector = if mode != SearchMode::Keyword { self.vector_ids(query, chat, pool)? } else { Vec::new() };

        let mut scores: HashMap<i64, f64> = HashMap::new();
        for list in [&keyword, &vector] {
            for (rank, rowid) in list.iter().enumerate() {
                *scores.entry(*rowid).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }
        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(limit);

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, chat_jid, sender, timestamp, type, text, raw_json, from_me, push_name FROM messages WHERE rowid = ?1",
        )?;
        let mut hits = Vec::with_capacity(ranked.len());
        for (rowid, score) in ranked {
            if let Some(message) = stmt.query_row(params![rowid], row_to_msg).optional()? {
                hits.push(SearchHit { score, message });
            }
        }
        Ok(hits)
    }

    fn keyword_ids(&self, query: &str, chat: Option<&str>, limit: usize) -> anyhow::Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        // The trigram tokenizer cannot match terms shorter than 3 chars.
        if query.chars().count() < 3 {
            let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
            let mut stmt = conn.prepare_cached(
                "SELECT rowid FROM messages WHERE text LIKE ?1 ESCAPE '\\'
                 AND (?2 IS NULL OR chat_jid = ?2) ORDER BY timestamp DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![like, chat, limit as i64], |r| r.get(0))?;
            return Ok(rows.collect::<Result<_, _>>()?);
        }
        let fts = fts_query(query);
        let mut stmt = conn.prepare_cached(
            "SELECT f.rowid FROM messages_fts f JOIN messages m ON m.rowid = f.rowid
             WHERE messages_fts MATCH ?1 AND (?2 IS NULL OR m.chat_jid = ?2)
             ORDER BY bm25(messages_fts) LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![fts, chat, limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn vector_ids(&self, query: &str, chat: Option<&str>, limit: usize) -> anyhow::Result<Vec<i64>> {
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        let q = embedder.embed(&[query])?.pop().unwrap_or_default();
        let conn = self.conn.lock().unwrap();
        // Over-fetch when filtering by chat since vec0 KNN runs before the join.
        let k = if chat.is_some() { limit * 8 } else { limit };
        let mut stmt = conn.prepare_cached(
            "SELECT e.rowid FROM message_embeddings e JOIN messages m ON m.rowid = e.rowid
             WHERE e.embedding MATCH ?1 AND k = ?2 AND (?3 IS NULL OR m.chat_jid = ?3)
             ORDER BY e.distance LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![f32_bytes(&q), k as i64, chat, limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// Quote every whitespace-separated term so user input can't inject FTS syntax.
fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .filter(|t| t.chars().count() >= 3)
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Additive migrations for databases created by an older build. The schema
/// itself is `CREATE … IF NOT EXISTS`, so only new columns need this.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    if !has_column(conn, "messages", "from_me")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN from_me INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !has_column(conn, "messages", "push_name")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN push_name TEXT;")?;
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn row_to_msg(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        id: r.get(0)?,
        chat_jid: r.get(1)?,
        sender: r.get(2)?,
        timestamp: r.get(3)?,
        kind: r.get(4)?,
        text: r.get(5)?,
        raw_json: r.get(6)?,
        from_me: r.get(7)?,
        push_name: r.get(8)?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn msg(id: &str, chat: &str, ts: i64, text: &str) -> StoredMessage {
        StoredMessage {
            id: id.into(),
            chat_jid: chat.into(),
            sender: "1@s.whatsapp.net".into(),
            timestamp: ts,
            kind: "text".into(),
            text: Some(text.into()),
            raw_json: None,
            from_me: false,
            push_name: None,
        }
    }

    fn seeded(embed: bool) -> Store {
        let e: Option<Arc<dyn Embedder>> = embed.then(|| Arc::new(HashEmbedder) as _);
        let s = Store::open_memory(e).unwrap();
        s.insert_messages(&[
            msg("a", "1@g.us", 10, "Dinner tonight at 7?"),
            msg("b", "1@g.us", 20, "Meeting moved to tomorrow"),
            msg("c", "2@s.whatsapp.net", 30, "dinner at mom's on Sunday"),
            msg("d", "2@s.whatsapp.net", 40, "ok"),
        ])
        .unwrap();
        s
    }

    #[test]
    fn keyword_search_and_chat_filter() {
        let s = seeded(false);
        let hits = s.search("dinner", None, 10, SearchMode::Keyword).unwrap();
        let mut ids: Vec<_> = hits.iter().map(|h| h.message.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["a", "c"]);
        let hits = s.search("dinner", Some("1@g.us"), 10, SearchMode::Keyword).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message.id, "a");
    }

    #[test]
    fn list_chats_most_recent_first() {
        let s = seeded(false);
        let chats = s.list_chats().unwrap();
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0].jid, "2@s.whatsapp.net");
        assert_eq!(chats[0].count, 2);
        assert_eq!(chats[0].last_timestamp, 40);
        assert_eq!(chats[0].last_text.as_deref(), Some("ok"));
        assert_eq!(chats[1].jid, "1@g.us");
    }

    #[test]
    fn short_query_uses_like() {
        let s = seeded(false);
        let hits = s.search("ok", None, 10, SearchMode::Keyword).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message.id, "d");
    }

    #[test]
    fn fts_injection_is_quoted() {
        let s = seeded(false);
        assert!(s.search("kol\" OR \"x", None, 10, SearchMode::Keyword).is_ok());
        assert!(s.search("NEAR(", None, 10, SearchMode::Keyword).is_ok());
    }

    #[test]
    fn vector_and_hybrid_search() {
        let s = seeded(true);
        let hits = s.search("dinner", None, 2, SearchMode::Vector).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.message.id == "a" || h.message.id == "c"));
        let hits = s.search("dinner", None, 10, SearchMode::Hybrid).unwrap();
        assert!(matches!(hits[0].message.id.as_str(), "a" | "c"));
    }

    #[test]
    fn hybrid_without_embedder_is_keyword() {
        let s = seeded(false);
        let hits = s.search("meeting", None, 10, SearchMode::Hybrid).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message.id, "b");
        assert!(s.search("meeting", None, 10, SearchMode::Vector).unwrap().is_empty());
    }

    #[test]
    fn replace_delete_and_stats() {
        let s = seeded(true);
        s.insert_message(&msg("a", "1@g.us", 11, "edited content")).unwrap();
        assert!(s.search("dinner", Some("1@g.us"), 10, SearchMode::Keyword).unwrap().is_empty());
        assert_eq!(s.get_message("a").unwrap().unwrap().timestamp, 11);
        let st = s.chat_stats("1@g.us").unwrap();
        assert_eq!(st, ChatStats { count: 2, first_timestamp: Some(11), last_timestamp: Some(20) });
        assert_eq!(s.delete_chat("1@g.us").unwrap(), 2);
        assert_eq!(s.chat_stats("1@g.us").unwrap().count, 0);
        // FTS entries are gone…
        assert!(s.search("edited", None, 10, SearchMode::Keyword).unwrap().is_empty());
        // …and so are the embeddings (KNN still returns the nearest *remaining*
        // messages from other chats, so hybrid is never empty — only check that
        // nothing from the deleted chat comes back).
        let hits = s.search("edited", None, 10, SearchMode::Hybrid).unwrap();
        assert!(hits.iter().all(|h| h.message.chat_jid != "1@g.us"));
        let n: i64 =
            s.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM message_embeddings", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn keyset_paging() {
        let s = seeded(false);
        let p1 = s.page("2@s.whatsapp.net", None, None, None, 1).unwrap();
        assert_eq!(p1[0].id, "c");
        let last = p1.last().unwrap();
        let p2 = s.page("2@s.whatsapp.net", Some((last.timestamp, &last.id)), None, None, 10).unwrap();
        assert_eq!(p2.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["d"]);
        let ranged = s.page("2@s.whatsapp.net", None, Some(35), None, 10).unwrap();
        assert_eq!(ranged.len(), 1);
    }

    #[test]
    fn page_before_walks_backwards() {
        let s = seeded(false);
        // The tail of the chat, oldest-first inside the page.
        let tail = s.page_before("1@g.us", None, 1).unwrap();
        assert_eq!(tail.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["b"]);
        assert_eq!(s.count_before("1@g.us", Some((tail[0].timestamp, &tail[0].id))).unwrap(), 1);

        let older = s.page_before("1@g.us", Some((tail[0].timestamp, &tail[0].id)), 10).unwrap();
        assert_eq!(older.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(s.count_before("1@g.us", Some((older[0].timestamp, &older[0].id))).unwrap(), 0);
        assert!(s.page_before("1@g.us", Some((older[0].timestamp, &older[0].id)), 10).unwrap().is_empty());

        // A whole chat in one page comes back in ascending order.
        let all = s.page_before("1@g.us", None, 50).unwrap();
        assert_eq!(all.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn from_me_roundtrips_and_old_rows_default_false() {
        let s = Store::open_memory(None).unwrap();
        let mut mine = msg("m1", "2@s.whatsapp.net", 50, "sent by me");
        mine.from_me = true;
        s.insert_messages(&[mine, msg("m2", "2@s.whatsapp.net", 60, "theirs")]).unwrap();
        let page = s.page_before("2@s.whatsapp.net", None, 10).unwrap();
        assert_eq!(page.iter().map(|m| m.from_me).collect::<Vec<_>>(), [true, false]);
        assert!(s.get_message("m1").unwrap().unwrap().from_me);
    }

    #[test]
    fn migrate_adds_from_me_to_a_legacy_database() {
        let dir = std::env::temp_dir().join(format!("wa-store-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        let _ = std::fs::remove_file(&path);
        {
            // A database as an older build left it: no `from_me` column.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages(id TEXT PRIMARY KEY, chat_jid TEXT NOT NULL, sender TEXT NOT NULL,
                     timestamp INTEGER NOT NULL, type TEXT NOT NULL, text TEXT, raw_json TEXT);
                 INSERT INTO messages VALUES ('old', '2@s.whatsapp.net', '2@s.whatsapp.net', 5, 'text', 'hi', NULL);",
            )
            .unwrap();
        }
        let s = Store::open(&path, None).unwrap();
        let row = s.get_message("old").unwrap().unwrap();
        assert!(!row.from_me);
        std::fs::remove_file(&path).ok();
    }

    fn settings(muted_until: Option<i64>, pinned: bool, archived: bool) -> ChatSettingsView {
        ChatSettingsView { muted_until, pinned, archived }
    }

    #[test]
    fn chat_jids_distinct_sorted() {
        let s = seeded(false);
        assert_eq!(s.chat_jids().unwrap(), ["1@g.us", "2@s.whatsapp.net"]);
        assert!(Store::open_memory(None).unwrap().chat_jids().unwrap().is_empty());
    }

    #[test]
    fn list_with_meta_defaults_without_settings() {
        let s = seeded(false);
        let rows = s.list_chats_with_meta(&HashMap::new(), &HashMap::new(), 100, ArchiveFilter::All).unwrap();
        assert_eq!(rows.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(), ["2@s.whatsapp.net", "1@g.us"]);
        assert!(rows.iter().all(|r| !r.muted && !r.pinned && !r.archived && r.name.is_none()));
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].last_text.as_deref(), Some("ok"));
    }

    #[test]
    fn list_with_meta_pinned_first_then_recency() {
        let s = seeded(false);
        s.insert_message(&msg("e", "3@s.whatsapp.net", 5, "oldest")).unwrap();
        let mut st = HashMap::new();
        st.insert("3@s.whatsapp.net".to_string(), settings(None, true, false));
        let rows = s.list_chats_with_meta(&st, &HashMap::new(), 0, ArchiveFilter::All).unwrap();
        let order: Vec<_> = rows.iter().map(|r| r.jid.as_str()).collect();
        assert_eq!(order, ["3@s.whatsapp.net", "2@s.whatsapp.net", "1@g.us"]);
        assert!(rows[0].pinned);
    }

    #[test]
    fn list_with_meta_archive_filter() {
        let s = seeded(false);
        let mut st = HashMap::new();
        st.insert("1@g.us".to_string(), settings(None, false, true));
        let active = s.list_chats_with_meta(&st, &HashMap::new(), 0, ArchiveFilter::Active).unwrap();
        assert_eq!(active.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(), ["2@s.whatsapp.net"]);
        let archived = s.list_chats_with_meta(&st, &HashMap::new(), 0, ArchiveFilter::Archived).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].archived);
        assert_eq!(s.list_chats_with_meta(&st, &HashMap::new(), 0, ArchiveFilter::All).unwrap().len(), 2);
    }

    #[test]
    fn mute_expiry_and_forever() {
        let now = 1_000;
        assert!(settings(Some(2_000), false, false).is_muted(now));
        assert!(!settings(Some(1_000), false, false).is_muted(now));
        assert!(!settings(Some(10), false, false).is_muted(now));
        assert!(!settings(None, false, false).is_muted(now));
        let forever = settings(Some(MUTED_FOREVER), false, false);
        assert!(forever.is_muted(now) && forever.is_muted_forever());
        assert!(!settings(Some(2_000), false, false).is_muted_forever());

        let s = seeded(false);
        let mut st = HashMap::new();
        st.insert("1@g.us".to_string(), settings(Some(500), false, false));
        st.insert("2@s.whatsapp.net".to_string(), settings(Some(MUTED_FOREVER), false, false));
        let rows = s.list_chats_with_meta(&st, &HashMap::new(), now, ArchiveFilter::All).unwrap();
        let by: HashMap<_, _> = rows.iter().map(|r| (r.jid.as_str(), r)).collect();
        assert!(!by["1@g.us"].muted);
        assert_eq!(by["1@g.us"].muted_until, Some(500));
        assert!(by["2@s.whatsapp.net"].muted);
    }

    #[test]
    fn from_local_converts_system_time() {
        use std::time::{Duration, UNIX_EPOCH};
        let forever = UNIX_EPOCH + Duration::new(MUTED_FOREVER as u64, 999_999_999);
        let v = ChatSettingsView::from_local(Some(forever), true, false);
        assert_eq!(v, settings(Some(MUTED_FOREVER), true, false));
        assert_eq!(ChatSettingsView::from_local(None, false, true), settings(None, false, true));
        assert_eq!(system_time_to_unix(UNIX_EPOCH - Duration::from_secs(5)), -5);
        assert!(unix_now() > 1_700_000_000);
    }

    #[test]
    fn contact_display_name_priority() {
        let mut c = ContactView { push_name: "Pushy".into(), ..Default::default() };
        assert_eq!(c.display_name(), Some("Pushy"));
        c.business_name = "Biz Ltd".into();
        assert_eq!(c.display_name(), Some("Biz Ltd"));
        c.first_name = "Ann".into();
        assert_eq!(c.display_name(), Some("Ann"));
        c.full_name = "Ann Smith".into();
        assert_eq!(c.display_name(), Some("Ann Smith"));
        assert_eq!(ContactView { full_name: "  ".into(), ..Default::default() }.display_name(), None);
        assert_eq!(ContactView::named("Family").display_name(), Some("Family"));

        let s = seeded(false);
        let mut contacts = HashMap::new();
        contacts.insert("2@s.whatsapp.net".to_string(), c);
        contacts.insert("1@g.us".to_string(), ContactView::named("Family"));
        let rows = s.list_chats_with_meta(&HashMap::new(), &contacts, 0, ArchiveFilter::All).unwrap();
        assert_eq!(rows[0].name.as_deref(), Some("Ann Smith"));
        assert_eq!(rows[1].name.as_deref(), Some("Family"));
    }

    #[test]
    fn a_chat_is_named_by_its_push_name_when_the_address_book_is_empty() {
        let s = Store::open_memory(None).unwrap();
        let mut incoming = msg("p1", "192680906768485@lid", 10, "hi");
        incoming.sender = "192680906768485@lid".into();
        incoming.push_name = Some("Kasia".into());
        let mut mine = msg("p2", "192680906768485@lid", 20, "hello");
        mine.from_me = true;
        mine.push_name = Some("Me".into());
        s.insert_messages(&[incoming, mine]).unwrap();

        // The summary carries the name the chat announced, never our own.
        let summary = &s.list_chats().unwrap()[0];
        assert_eq!(summary.push_name.as_deref(), Some("Kasia"));

        let rows = s.list_chats_with_meta(&HashMap::new(), &HashMap::new(), 0, ArchiveFilter::All).unwrap();
        assert_eq!(rows[0].name.as_deref(), Some("Kasia"), "no contact row, so the push name names the chat");

        // The address book still wins when it has an entry.
        let contacts = HashMap::from([("192680906768485@lid".to_owned(), ContactView::named("Kasia Nowak"))]);
        let rows = s.list_chats_with_meta(&HashMap::new(), &contacts, 0, ArchiveFilter::All).unwrap();
        assert_eq!(rows[0].name.as_deref(), Some("Kasia Nowak"));
    }

    #[test]
    fn media_blobs_are_cached_replaced_and_purged() {
        let s = seeded(false);
        s.put_media_blob("a", "image/jpeg", b"first").unwrap();
        assert_eq!(s.get_media_blob("a").unwrap(), Some(("image/jpeg".to_owned(), b"first".to_vec())));
        assert_eq!(s.count_media_blobs().unwrap(), 1);

        // A second download of the same message replaces the bytes.
        s.put_media_blob("a", "image/png", b"second").unwrap();
        assert_eq!(s.get_media_blob("a").unwrap(), Some(("image/png".to_owned(), b"second".to_vec())));
        assert_eq!(s.count_media_blobs().unwrap(), 1);

        assert_eq!(s.get_media_blob("missing").unwrap(), None);

        // A fresh blob survives the hourly purge; age 0 empties the cache.
        assert_eq!(s.purge_media_blobs(3600).unwrap(), 0);
        assert_eq!(s.purge_media_blobs(0).unwrap(), 1);
        assert_eq!(s.count_media_blobs().unwrap(), 0);
    }

    #[test]
    fn deleting_a_chat_drops_its_cached_media() {
        let s = seeded(false);
        s.put_media_blob("a", "image/jpeg", b"in 1@g.us").unwrap();
        s.put_media_blob("c", "image/jpeg", b"in 2@s.whatsapp.net").unwrap();
        s.delete_chat("1@g.us").unwrap();
        assert_eq!(s.get_media_blob("a").unwrap(), None, "the blob of a deleted message goes too");
        assert!(s.get_media_blob("c").unwrap().is_some(), "another chat keeps its media");
    }

    #[test]
    fn a_history_import_never_overwrites_a_live_message() {
        let s = seeded(false);
        // "a" is already stored (live, with a push name); "new" is not.
        let mut live_copy = msg("a", "1@g.us", 10, "history copy without a name");
        live_copy.push_name = None;
        let fresh = msg("new", "1@g.us", 5, "older message from the blob");

        assert_eq!(s.insert_messages_if_new(&[live_copy, fresh]).unwrap(), 1, "only the unknown id is written");
        assert_eq!(s.get_message("a").unwrap().unwrap().text.as_deref(), Some("Dinner tonight at 7?"));
        assert_eq!(s.get_message("new").unwrap().unwrap().text.as_deref(), Some("older message from the blob"));
        assert_eq!(s.insert_messages_if_new(&[]).unwrap(), 0);
    }

    #[test]
    fn archive_filter_parse_and_wire() {
        assert_eq!("".parse::<ArchiveFilter>().unwrap(), ArchiveFilter::All);
        assert_eq!("active".parse::<ArchiveFilter>().unwrap(), ArchiveFilter::Active);
        assert_eq!("true".parse::<ArchiveFilter>().unwrap(), ArchiveFilter::Archived);
        assert!("nope".parse::<ArchiveFilter>().is_err());
        let row = merge_chat_list(
            vec![ChatSummary { jid: "x".into(), count: 1, last_timestamp: 1, last_text: None, push_name: None }],
            &HashMap::new(),
            &HashMap::new(),
            0,
            ArchiveFilter::All,
        );
        let v = serde_json::to_value(&row[0]).unwrap();
        for key in ["jid", "name", "count", "last_timestamp", "last_text", "muted", "muted_until", "pinned", "archived"]
        {
            assert!(v.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn file_db_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("store.db");
        Store::open(&p, None).unwrap().insert_message(&msg("x", "c", 1, "hello world")).unwrap();
        let s = Store::open(&p, None).unwrap();
        assert_eq!(s.search("hello", None, 5, SearchMode::Keyword).unwrap().len(), 1);
    }

    /// The candidates for the stored-kind repair, and the update itself.
    #[test]
    fn rows_with_a_payload_can_have_their_kind_corrected() {
        let s = Store::open_memory(None).unwrap();
        let kinded = |id: &str, ts: i64, kind: &str, raw: Option<&str>| StoredMessage {
            kind: kind.to_owned(),
            raw_json: raw.map(str::to_owned),
            ..msg(id, "1@g.us", ts, id)
        };
        let rows = vec![
            kinded("a", 10, "media", Some("aabb")),
            kinded("b", 20, "text", Some("ccdd")),
            kinded("c", 30, "image", Some("eeff")),
            kinded("d", 40, "media", None),
        ];
        // `d` keeps no payload: nothing to recover from.
        s.insert_messages(&rows).unwrap();

        let mut found = s.messages_with_payload(&["media", "text"]).unwrap();
        found.sort();
        assert_eq!(
            found,
            [("a".to_owned(), "aabb".to_owned()), ("b".to_owned(), "ccdd".to_owned())],
            "only stanza kinds that still have a payload"
        );
        assert!(s.messages_with_payload(&[]).unwrap().is_empty(), "no kinds, no work");

        assert!(s.set_message_kind("a", "image").unwrap());
        assert_eq!(s.get_message("a").unwrap().unwrap().kind, "image");
        assert!(!s.set_message_kind("nope", "image").unwrap(), "an unknown id changes nothing");

        // The corrected row is no longer a candidate.
        assert_eq!(s.messages_with_payload(&["media"]).unwrap(), Vec::new());
    }

    /// The same conversation under two JIDs: live messages arrive LID-keyed,
    /// a history blob comes back in phone-number form.
    #[test]
    fn merging_a_chat_keeps_the_live_copy_of_a_shared_id() {
        let s = Store::open_memory(None).unwrap();
        let pn = "48600765779@s.whatsapp.net";
        let lid = "192680906768485@lid";
        let mut live = msg("shared", lid, 50, "live copy");
        live.push_name = Some("Lukasz".to_owned());
        s.insert_messages(&[
            msg("old-1", pn, 10, "from the history blob"),
            msg("old-2", pn, 20, "also history"),
            msg("shared", pn, 50, "history copy"),
            msg("other", "1@g.us", 30, "untouched"),
        ])
        .unwrap();
        s.insert_messages(&[live, msg("new-1", lid, 60, "live")]).unwrap();

        assert_eq!(s.merge_chat(pn, lid).unwrap(), 2, "the two history-only rows move");

        let thread: Vec<String> = s.page(lid, None, None, None, 10).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(thread, ["old-1", "old-2", "shared", "new-1"], "one thread, in order");
        assert!(s.page(pn, None, None, None, 10).unwrap().is_empty(), "nothing left under the phone number");
        let shared = s.get_message("shared").unwrap().unwrap();
        assert_eq!(shared.push_name.as_deref(), Some("Lukasz"), "the live copy survived, not the history one");
        assert_eq!(s.page("1@g.us", None, None, None, 10).unwrap().len(), 1, "other chats untouched");

        assert_eq!(s.merge_chat(lid, lid).unwrap(), 0, "merging a chat into itself is a no-op");
        assert_eq!(s.merge_chat("nobody@s.whatsapp.net", lid).unwrap(), 0);
    }

    #[test]
    fn stored_chat_jids_are_listed_once_each() {
        let s = Store::open_memory(None).unwrap();
        s.insert_messages(&[
            msg("a", "1@g.us", 10, "x"),
            msg("b", "1@g.us", 20, "y"),
            msg("c", "2@s.whatsapp.net", 30, "z"),
        ])
        .unwrap();
        let mut chats = s.chat_jids_stored().unwrap();
        chats.sort();
        assert_eq!(chats, ["1@g.us", "2@s.whatsapp.net"]);
    }

    /// The merge has to keep the search index usable: a moved row is still
    /// findable, and under its new chat filter.
    #[test]
    fn a_merged_row_stays_searchable_under_the_new_chat() {
        let s = Store::open_memory(None).unwrap();
        let pn = "48600765779@s.whatsapp.net";
        let lid = "192680906768485@lid";
        s.insert_messages(&[msg("h1", pn, 10, "dinner at the lake")]).unwrap();
        s.merge_chat(pn, lid).unwrap();

        let hits = s.search("dinner", Some(lid), 10, SearchMode::Keyword).unwrap();
        assert_eq!(hits.len(), 1, "found under the chat it was merged into");
        assert_eq!(hits[0].message.chat_jid, lid);
        assert!(s.search("dinner", Some(pn), 10, SearchMode::Keyword).unwrap().is_empty());
    }
}
