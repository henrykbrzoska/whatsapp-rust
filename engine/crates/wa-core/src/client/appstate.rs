// Ported from the Go client (`appstate.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Client-side app state sync — fetch, verify, decode, apply.
//!
//! This is the glue Task A deliberately left out: it needs the socket
//! (`send_iq`), the device stores and the mutation dispatcher. Go
//! counterparts: `Client.fetchAppState`, `fetchAppStatePatches`,
//! `applyAppStatePatches`, `handleAppStateNotification` and `dispatchAppState`
//! (`Go appstate.go`, `Go notification.go`).
//!
//! Boundary contract (docs/PROMPT-PHASE-5.md §2.1):
//!
//! * [`Client::fetch_app_state`] — `fetch_app_state(name, full_sync) -> Vec<SyncAction>`
//! * [`Client::apply_app_state_patch`] — verifies the snapshot/patch MACs
//!   **before** the first store write, then stores the version/LTHash/mutation
//!   MACs and dispatches the mutations
//! * [`dispatch_mutation`] — the store writes (`ChatSettingsStore`,
//!   `ContactStore`); `get_chat_settings` is the existing trait, reused as-is
//! * [`Client::handle_app_state_notification`] — the `server_sync` notification
//! * [`Client::handle_app_state_sync_key_share`] — Go `handleAppStateSyncKeyShare`:
//!   persists the primary device's sync keys, then re-fetches every collection
//!
//! A patch that names a key this device does not have (yet) stays unapplied
//! with [`AppStateError::KeyNotFound`] and the missing IDs are reported through
//! [`AppStateSkip::MissingKeys`] — nothing is faked and no unverified mutation
//! is written. Once the key share arrives, the re-fetch applies it.

use std::collections::HashMap;

use anyhow::Result;
use prost::Message as _;
use wa_proto::wa_web_protobufs_e2e::AppStateSyncKeyShare;
use wa_proto::wa_web_protobufs_server_sync::{ExternalBlobReference, SyncdMutation, SyncdPatch};

use crate::appstate::decode::decode_mutation;
use crate::appstate::keys::{AppStateKeyCache, missing_key_ids, persist_app_state_sync_key_share};
use crate::appstate::recovery::{
    index_mac_of_mutation, lthash_of, mutation_operation_of, value_mac_of_mutation, verify_patch_chained,
    verify_snapshot,
};
use crate::appstate::{
    AppStateError, HashState, Mutation, PatchList, SyncdOperation, WAPatchName, decode::decode_patch_list,
};
use crate::binary::node::{Node, NodeContent};
use crate::client::download::{Downloadable, MediaType};
use crate::client::{Client, InfoQuery, IqKind, server_jid};
use crate::store::traits::{AppStateMutationMac, ChatSettingsStore, ContactStore, LocalChatSettings, muted_forever};
use crate::types::jid::Jid;

/// App state IQ namespace (`w:sync:app:state`).
pub const APP_STATE_NAMESPACE: &str = "w:sync:app:state";

/// One applied app state mutation, as the frontend sees it.
///
/// Deliberately a small, `serde`-free summary: the Tauri/SSE event layer turns
/// it into its own event type (that file is main-thread territory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// `regular_high` — mute state changed.
    Mute {
        /// The chat the mutation is about.
        chat: Jid,
        /// The new mute state, `None` = not muted.
        muted_until: Option<std::time::SystemTime>,
        /// `true` when the mutation arrived during a full sync.
        from_full_sync: bool,
    },
    /// `regular_low` — pinned state changed.
    Pin {
        /// The chat the mutation is about.
        chat: Jid,
        /// The new pinned state.
        pinned: bool,
        /// `true` when the mutation arrived during a full sync.
        from_full_sync: bool,
    },
    /// `regular_low` — archived state changed.
    Archive {
        /// The chat the mutation is about.
        chat: Jid,
        /// The new archived state.
        archived: bool,
        /// `true` when the mutation arrived during a full sync.
        from_full_sync: bool,
    },
    /// `critical_unblock_low` — a contact was renamed.
    Contact {
        /// The contact JID.
        chat: Jid,
        /// The contact's first name.
        first_name: String,
        /// The contact's full name.
        full_name: String,
        /// `true` when the mutation arrived during a full sync.
        from_full_sync: bool,
    },
    /// An index this engine reads but does not apply yet (starred messages,
    /// labels, deletes, …). The event layer may forward it; nothing is stored.
    Unhandled {
        /// The collection the mutation belonged to.
        name: WAPatchName,
        /// The mutation's index.
        index: Vec<String>,
        /// The operation (`SET`/`REMOVE`).
        operation: SyncdOperation,
    },
}

/// Something the engine could not apply, but that must not be dropped silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStateSkip {
    /// The patch names sync keys this device does not have (NOTE: key
    /// shares). The IDs are what an `APP_STATE_SYNC_KEY_REQUEST` would ask for.
    MissingKeys(Vec<Vec<u8>>),
    /// A mutation's index is too short for its type — ignored, like Go.
    MalformedIndex(Vec<String>),
}

/// The outcome of applying one [`PatchList`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppliedAppState {
    /// The collection state after the patches.
    pub state: HashState,
    /// The mutations the frontend should hear about.
    pub actions: Vec<SyncAction>,
    /// Mutations that could not be applied (empty in the happy path).
    pub skipped: Vec<AppStateSkip>,
}

/// The result of a full [`Client::fetch_app_state`] run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FetchedAppState {
    /// The final state of the collection.
    pub state: HashState,
    /// Every action of every applied patch, in server order.
    pub actions: Vec<SyncAction>,
    /// What could not be applied.
    pub skipped: Vec<AppStateSkip>,
    /// `has_more_patches` of the last response — the caller may fetch again.
    pub has_more_patches: bool,
}

/// The result of [`Client::handle_app_state_sync_key_share`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyShareOutcome {
    /// The key IDs that were written to the store, in share order.
    pub stored_key_ids: Vec<Vec<u8>>,
    /// Every collection that was re-fetched, with its result.
    pub fetched: Vec<(WAPatchName, FetchedAppState)>,
}

/// `dispatchAppState` — writes one decoded mutation into the local stores.
///
/// `SET` mutations of `mute`, `pin_v1`, `archive` and `contact` are applied;
/// every other index (and every `REMOVE`) is reported as
/// [`SyncAction::Unhandled`] without a store write, which mirrors Go for the
/// indices this engine does not implement yet.
pub async fn dispatch_mutation(
    chat_settings: &dyn ChatSettingsStore,
    contacts: &dyn ContactStore,
    name: &WAPatchName,
    mutation: &Mutation,
    from_full_sync: bool,
) -> Result<Option<SyncAction>> {
    let index = &mutation.index;
    let first = index.first().map(String::as_str);
    let chat = index.get(1).and_then(|raw| raw.parse::<Jid>().ok());

    if mutation.operation != SyncdOperation::Set {
        // A REMOVE only ever drops local state we do not keep in these stores.
        return Ok(Some(SyncAction::Unhandled {
            name: name.clone(),
            index: index.clone(),
            operation: mutation.operation,
        }));
    }

    let Some(action) = mutation.action.as_ref() else {
        return Ok(Some(SyncAction::Unhandled {
            name: name.clone(),
            index: index.clone(),
            operation: mutation.operation,
        }));
    };

    match first {
        Some(crate::appstate::INDEX_MUTE) => {
            let Some(chat) = chat else {
                return Ok(Some(SyncAction::Unhandled {
                    name: name.clone(),
                    index: index.clone(),
                    operation: mutation.operation,
                }));
            };
            let mute = action.mute_action.unwrap_or_default();
            let muted = mute.muted.unwrap_or_default();
            let muted_until = if !muted {
                None
            } else {
                match mute.mute_end_timestamp.unwrap_or_default() {
                    // Go: `MuteEndTimestamp < 0` means "muted forever".
                    end if end < 0 => Some(muted_forever()),
                    end => Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(end.max(0) as u64)),
                }
            };
            chat_settings.put_muted_until(&chat, muted_until).await?;
            Ok(Some(SyncAction::Mute { chat, muted_until, from_full_sync }))
        }
        Some(crate::appstate::INDEX_PIN) => {
            let Some(chat) = chat else {
                return Ok(Some(SyncAction::Unhandled {
                    name: name.clone(),
                    index: index.clone(),
                    operation: mutation.operation,
                }));
            };
            let pinned = action.pin_action.as_ref().and_then(|pin| pin.pinned).unwrap_or_default();
            chat_settings.put_pinned(&chat, pinned).await?;
            Ok(Some(SyncAction::Pin { chat, pinned, from_full_sync }))
        }
        Some(crate::appstate::INDEX_ARCHIVE) => {
            let Some(chat) = chat else {
                return Ok(Some(SyncAction::Unhandled {
                    name: name.clone(),
                    index: index.clone(),
                    operation: mutation.operation,
                }));
            };
            let archived = action.archive_chat_action.as_ref().and_then(|archive| archive.archived).unwrap_or_default();
            chat_settings.put_archived(&chat, archived).await?;
            Ok(Some(SyncAction::Archive { chat, archived, from_full_sync }))
        }
        Some(crate::appstate::INDEX_CONTACT) | Some(crate::appstate::INDEX_LID_CONTACT) => {
            let Some(contact) = action.contact_action.as_ref() else {
                return Ok(Some(SyncAction::Unhandled {
                    name: name.clone(),
                    index: index.clone(),
                    operation: mutation.operation,
                }));
            };
            // A `lid_contact` names the LID in index[1] and (sometimes) the PN in
            // its own field; prefer whichever parsed.
            let target = chat
                .or_else(|| contact.pn_jid.as_deref().and_then(|raw| raw.parse::<Jid>().ok()))
                .or_else(|| contact.lid_jid.as_deref().and_then(|raw| raw.parse::<Jid>().ok()));
            let Some(target) = target else {
                return Ok(Some(SyncAction::Unhandled {
                    name: name.clone(),
                    index: index.clone(),
                    operation: mutation.operation,
                }));
            };
            let first_name = contact.first_name.clone().unwrap_or_default();
            let full_name = contact.full_name.clone().unwrap_or_default();
            contacts.put_contact_name(&target, &first_name, &full_name).await?;
            Ok(Some(SyncAction::Contact { chat: target, first_name, full_name, from_full_sync }))
        }
        _ => {
            Ok(Some(SyncAction::Unhandled { name: name.clone(), index: index.clone(), operation: mutation.operation }))
        }
    }
}

/// Every external blob a `<collection>` node points at: its `<snapshot>`,
/// plus one per `<patch>` that keeps its mutations outside the stanza.
///
/// A payload that does not decode is left out — [`decode_patch_list`] reports
/// it with the right error instead of failing the download.
fn external_references(collection: &Node) -> Vec<ExternalBlobReference> {
    let mut out = Vec::new();
    if let Some(snapshot) = collection.optional_child_by_tag(&["snapshot"])
        && let NodeContent::Bytes(raw) = &snapshot.content
        && let Ok(reference) = ExternalBlobReference::decode(raw.as_slice())
    {
        out.push(reference);
    }
    if let Some(patches) = collection.optional_child_by_tag(&["patches"]) {
        for patch in patches.children() {
            if patch.tag != "patch" {
                continue;
            }
            if let NodeContent::Bytes(raw) = &patch.content
                && let Ok(decoded) = SyncdPatch::decode(raw.as_slice())
                && let Some(reference) = decoded.external_mutations
            {
                out.push(reference);
            }
        }
    }
    out
}

/// `ExternalBlobReference` → [`Downloadable`], so the media path can fetch and
/// verify it like any attachment. App state blobs carry no MIME type.
fn external_downloadable(reference: &ExternalBlobReference) -> Downloadable {
    Downloadable {
        direct_path: reference.direct_path().to_owned(),
        media_key: reference.media_key().to_vec(),
        file_sha256: reference.file_sha256().to_vec(),
        file_enc_sha256: reference.file_enc_sha256().to_vec(),
        file_length: reference.file_size_bytes(),
        mime_type: String::new(),
    }
}

impl Client {
    /// `fetchAppStatePatches` — one `w:sync:app:state` IQ.
    pub async fn fetch_app_state_patches(
        &self,
        name: &WAPatchName,
        from_version: u64,
        snapshot: bool,
    ) -> Result<PatchList> {
        let mut collection = Node::new("collection").attr("name", name.as_str()).attr("return_snapshot", snapshot);
        if !snapshot {
            collection = collection.attr("version", from_version);
        }
        let response = self
            .send_iq(InfoQuery {
                namespace: APP_STATE_NAMESPACE.into(),
                kind: IqKind::Set,
                to: Some(server_jid()),
                content: vec![Node::new("sync").with_content(vec![collection])],
                ..Default::default()
            })
            .await?;
        let collection = response.optional_child_by_tag(&["sync", "collection"]).cloned().ok_or_else(|| {
            crate::client::ClientError::MissingElement { tag: "collection", parent: "app state patch response" }
        })?;

        // A snapshot (and a large patch) does not fit in a stanza: the server
        // sends an `ExternalBlobReference` and the bytes live on the media CDN.
        // The decoder is synchronous, so the blobs are downloaded first and
        // handed to it through a lookup — Go downloads them inline.
        let blobs = self.download_external_blobs(&collection).await?;
        let lookup = |reference: &ExternalBlobReference| -> Result<Vec<u8>, AppStateError> {
            blobs
                .get(reference.file_enc_sha256())
                .cloned()
                .ok_or(AppStateError::InvalidPatchList("an external blob was not downloaded"))
        };
        Ok(decode_patch_list(&collection, Some(&lookup))?)
    }

    /// Downloads every external blob a `<collection>` references, keyed by its
    /// `fileEncSHA256` (which is what identifies one blob on the CDN).
    ///
    /// Go `downloadExternalAppStateBlob` → `Download` with `MediaAppState`;
    /// the same verified media path as an attachment, so a tampered blob is
    /// rejected before it is decoded.
    async fn download_external_blobs(&self, collection: &Node) -> Result<HashMap<Vec<u8>, Vec<u8>>> {
        let mut blobs: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for reference in external_references(collection) {
            let key = reference.file_enc_sha256().to_vec();
            if blobs.contains_key(&key) {
                continue;
            }
            let data =
                self.download_media(&external_downloadable(&reference), MediaType::AppState).await.map_err(|err| {
                    anyhow::anyhow!(
                        "failed to download an external app state blob ({} bytes): {err}",
                        reference.file_size_bytes()
                    )
                })?;
            blobs.insert(key, data);
        }
        Ok(blobs)
    }

    /// `Client.fetchAppState` — sync one collection and return its actions.
    ///
    /// `full_sync` drops the stored version first and asks for a snapshot, like
    /// Go. The stored collection state is read from the device's
    /// [`AppStateStore`](crate::store::traits::AppStateStore).
    pub async fn fetch_app_state(&self, name: &WAPatchName, full_sync: bool) -> Result<FetchedAppState> {
        if full_sync {
            self.device()
                .await
                .app_state
                .delete_app_state_version(name.as_str())
                .await
                .map_err(|err| AppStateError::Store(err.to_string()))?;
        }
        let (version, hash) = self
            .device()
            .await
            .app_state
            .get_app_state_version(name.as_str())
            .await
            .map_err(|err| AppStateError::Store(err.to_string()))?;
        let full_sync = full_sync || version == 0;
        let mut state = HashState::from_parts(version, hash);

        let keys = AppStateKeyCache::new();
        let mut out = FetchedAppState::default();
        let mut want_snapshot = full_sync;
        // A server that never stops asking for another page must not hang us.
        const MAX_PAGES: usize = 64;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(AppStateError::InvalidPatchList("the server kept sending more app state patches").into());
            }
            let list = self.fetch_app_state_patches(name, state.version, want_snapshot).await?;
            if !want_snapshot && list.snapshot.is_some() {
                return Err(
                    AppStateError::InvalidPatchList("the server returned a snapshot that was not asked for").into()
                );
            }
            want_snapshot = false;
            let applied = self.apply_app_state_patch(name, state.clone(), &list, &keys, full_sync).await?;
            state = applied.state;
            out.actions.extend(applied.actions);
            out.skipped.extend(applied.skipped);
            out.has_more_patches = list.has_more_patches;
            if !list.has_more_patches {
                break;
            }
        }
        out.state = state;
        Ok(out)
    }

    /// `applyAppStatePatches` — the boundary contract of Phase 5 §2.1.
    ///
    /// The order is the point of this function:
    /// 1. verify every patch/snapshot MAC and decode every mutation (content and
    ///    index MAC) — no store writes yet;
    /// 2. store the new version, LTHash and mutation MACs;
    /// 3. write the mutations into `ChatSettingsStore`/`ContactStore`.
    ///
    /// A snapshot in `list` replaces the collection state instead of patching it.
    pub async fn apply_app_state_patch(
        &self,
        name: &WAPatchName,
        state: HashState,
        list: &PatchList,
        keys: &AppStateKeyCache,
        from_full_sync: bool,
    ) -> Result<AppliedAppState> {
        let device = self.device().await;

        // 1. Verify and decode everything.
        let mut new_state = state.clone();
        let mut raw_mutations: Vec<SyncdMutation> = Vec::new();
        // Value MACs of the mutations already verified in this batch, so a patch
        // chain in one response does not depend on unwritten store rows.
        let mut known: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();

        if let Some(snapshot) = &list.snapshot {
            let Some(key_id) = snapshot.key_id.as_ref().and_then(|key_id| key_id.id.clone()) else {
                return Err(AppStateError::InvalidMutation("the snapshot has no key ID").into());
            };
            let expanded = match keys.get(device.app_state_keys.as_ref(), &key_id).await {
                Ok(expanded) => expanded,
                Err(err) => return self.skip_or_fail(name, list, err.into()).await,
            };
            let verified = match verify_snapshot(name, snapshot, &expanded, true) {
                Ok(verified) => verified,
                Err(err) => return self.skip_or_fail(name, list, err).await,
            };
            for warning in &verified.warnings {
                tracing::warn!("app state {name} snapshot hash warning: {warning}");
            }
            new_state = verified.state;
            for record in &snapshot.records {
                raw_mutations
                    .push(SyncdMutation { operation: Some(SyncdOperation::Set as i32), record: Some(record.clone()) });
            }
        }

        for patch in &list.patches {
            let Some(key_id) = patch.key_id.as_ref().and_then(|key_id| key_id.id.clone()) else {
                return Err(AppStateError::InvalidMutation("the patch has no key ID").into());
            };
            let expanded = match keys.get(device.app_state_keys.as_ref(), &key_id).await {
                Ok(expanded) => expanded,
                Err(err) => return self.skip_or_fail(name, list, err.into()).await,
            };
            let verified =
                match verify_patch_chained(name, patch, &new_state, &expanded, device.app_state.as_ref(), true, &known)
                    .await
                {
                    Ok(verified) => verified,
                    Err(err) => return self.skip_or_fail(name, list, err).await,
                };
            for warning in &verified.warnings {
                tracing::warn!("app state {name} patch hash warning: {warning}");
            }
            new_state = verified.state;
            for mutation in &patch.mutations {
                if let (Some(index_mac), Ok(value_mac)) =
                    (index_mac_of_mutation(mutation)?, value_mac_of_mutation(mutation))
                {
                    let value = if mutation_operation_of(mutation) == SyncdOperation::Set {
                        Some(value_mac.to_vec())
                    } else {
                        None
                    };
                    known.insert(index_mac.to_vec(), value);
                }
                raw_mutations.push(mutation.clone());
            }
        }

        // Decode every mutation with the key it names.
        let mut decoded: Vec<Mutation> = Vec::with_capacity(raw_mutations.len());
        // The *final* MAC per index: a later SET or REMOVE of the same index wins,
        // so storing them cannot collide on the (name, version, index_mac) key.
        let mut final_macs: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
        let mut order: Vec<Vec<u8>> = Vec::new();
        for raw in &raw_mutations {
            let key_id = raw
                .record
                .as_ref()
                .and_then(|record| record.key_id.as_ref())
                .and_then(|key_id| key_id.id.clone())
                .ok_or(AppStateError::InvalidMutation("mutation has no key ID"))?;
            let expanded = keys.get(device.app_state_keys.as_ref(), &key_id).await?;
            let mutation = decode_mutation(raw, &expanded, true)?;
            if !final_macs.contains_key(&mutation.index_mac) {
                order.push(mutation.index_mac.clone());
            }
            let value = if mutation.operation == SyncdOperation::Set { Some(mutation.value_mac.clone()) } else { None };
            final_macs.insert(mutation.index_mac.clone(), value);
            decoded.push(mutation);
        }

        // 2. Persist the collection state and the mutation MACs.
        device
            .app_state
            .put_app_state_version(name.as_str(), new_state.version, new_state.hash)
            .await
            .map_err(|err| AppStateError::Store(err.to_string()))?;
        let set_macs: Vec<AppStateMutationMac> = order
            .iter()
            .filter_map(|index_mac| {
                final_macs
                    .get(index_mac)
                    .cloned()
                    .flatten()
                    .map(|value_mac| AppStateMutationMac { index_mac: index_mac.clone(), value_mac })
            })
            .collect();
        let removed_index_macs: Vec<Vec<u8>> =
            order.iter().filter(|index_mac| matches!(final_macs.get(*index_mac), Some(None))).cloned().collect();
        if !set_macs.is_empty() {
            device
                .app_state
                .put_app_state_mutation_macs(name.as_str(), new_state.version, &set_macs)
                .await
                .map_err(|err| AppStateError::Store(err.to_string()))?;
        }
        if !removed_index_macs.is_empty() {
            device
                .app_state
                .delete_app_state_mutation_macs(name.as_str(), &removed_index_macs)
                .await
                .map_err(|err| AppStateError::Store(err.to_string()))?;
        }

        // 3. Apply the mutations.
        let mut out = AppliedAppState { state: new_state, ..Default::default() };
        for mutation in &decoded {
            if let Some(action) = dispatch_mutation(
                device.chat_settings.as_ref(),
                device.contacts.as_ref(),
                name,
                mutation,
                from_full_sync,
            )
            .await?
            {
                out.actions.push(action);
            }
        }
        Ok(out)
    }

    /// Turns a missing-key failure into [`AppStateSkip::MissingKeys`]; every
    /// other error is fatal.
    async fn skip_or_fail(&self, name: &WAPatchName, list: &PatchList, err: anyhow::Error) -> Result<AppliedAppState> {
        match err.downcast_ref::<AppStateError>() {
            Some(AppStateError::KeyNotFound) => {
                let device = self.device().await;
                // The collection stays unsynced until a key share for these IDs
                // arrives (`handle_app_state_sync_key_share` re-fetches it).
                let missing = missing_key_ids(device.app_state_keys.as_ref(), list).await?;
                tracing::warn!(
                    "app state {name} needs {} sync key(s) this device does not have; leaving the collection unsynced",
                    missing.len()
                );
                Ok(AppliedAppState { skipped: vec![AppStateSkip::MissingKeys(missing)], ..Default::default() })
            }
            _ => Err(err),
        }
    }

    /// `handleAppStateNotification` — the `server_sync` notification.
    ///
    /// One `<collection name="..." version="..."/>` per changed collection; each
    /// one is fetched immediately.
    pub async fn handle_app_state_notification(&self, node: &Node) -> Result<Vec<FetchedAppState>> {
        if node.attrs.get("type").and_then(|value| value.as_str()) != Some("server_sync") {
            return Err(AppStateError::InvalidPatchList("not a server_sync notification").into());
        }
        let mut out = Vec::new();
        for collection in node.children_by_tag("collection") {
            let Some(raw_name) = collection.attrs.get("name").and_then(|value| value.as_str()) else {
                continue;
            };
            let version = collection
                .attrs
                .get("version")
                .and_then(|value| value.as_str())
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or_default();
            let name = WAPatchName::new(raw_name);
            tracing::debug!("app state {name} changed on the server (version {version})");
            match self.fetch_app_state(&name, false).await {
                Ok(fetched) => out.push(fetched),
                Err(err) => {
                    // A disconnect right before a remote logout must stop the loop.
                    if is_disconnect(&err) {
                        tracing::debug!("not syncing {name} after a disconnect: {err}");
                        break;
                    }
                    // The collection stays unsynced until the primary device's
                    // key share arrives (`handle_app_state_sync_key_share`).
                    if matches!(err.downcast_ref::<AppStateError>(), Some(AppStateError::KeyNotFound)) {
                        tracing::warn!("app state {name} needs a sync key this device does not have");
                        continue;
                    }
                    tracing::error!("failed to sync app state {name} after a notification: {err}");
                }
            }
        }
        Ok(out)
    }

    /// Go `handleAppStateSyncKeyShare` — stores the keys of an
    /// `AppStateSyncKeyShare` protocol message, then re-fetches every
    /// collection (`appstate.AllPatchNames`).
    ///
    /// `only_if_not_synced` mirrors Go's `onlyResyncIfNotSynced`: `true` skips
    /// collections that already have a stored version; Go passes `false` when
    /// the share answers one of our own key requests. A failed fetch is logged
    /// and the next collection is tried; a disconnect stops the loop.
    pub async fn handle_app_state_sync_key_share(
        &self,
        share: &AppStateSyncKeyShare,
        only_if_not_synced: bool,
    ) -> Result<KeyShareOutcome> {
        let device = self.device().await;
        tracing::debug!("got {} new app state keys", share.keys.len());
        let stored = persist_app_state_sync_key_share(device.app_state_keys.as_ref(), share).await;
        let mut out = KeyShareOutcome { stored_key_ids: stored, fetched: Vec::new() };
        for name in WAPatchName::all() {
            if only_if_not_synced {
                let (version, _) = device
                    .app_state
                    .get_app_state_version(name.as_str())
                    .await
                    .map_err(|err| AppStateError::Store(err.to_string()))?;
                if version != 0 {
                    tracing::debug!("not re-fetching app state {name} after a key share: already synced");
                    continue;
                }
            }
            match self.fetch_app_state(&name, false).await {
                Ok(fetched) => out.fetched.push((name, fetched)),
                Err(err) if is_disconnect(&err) => {
                    tracing::debug!("not syncing {name} after a key share: {err}");
                    break;
                }
                Err(err) => tracing::error!("failed to fetch app state {name} after a key share: {err}"),
            }
        }
        Ok(out)
    }

    /// `get_chat_settings` — the existing trait, exposed where a caller only has
    /// a `Client` at hand.
    pub async fn get_chat_settings(&self, chat: &Jid) -> Result<LocalChatSettings> {
        let settings = self.device().await.chat_settings.get_chat_settings(chat).await?;
        Ok(settings)
    }
}

/// `true` when the sync failed because the connection went away.
fn is_disconnect(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<crate::client::ClientError>(),
        Some(crate::client::ClientError::NotConnected | crate::client::ClientError::Disconnected { .. })
    )
}

/// The LTHash of a set of value MACs — re-exported for callers that verify a
/// snapshot themselves.
pub fn app_state_lthash<'a>(value_macs: impl Iterator<Item = &'a [u8]>) -> Result<[u8; 128], AppStateError> {
    lthash_of(value_macs)
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use wa_proto::wa_web_protobuf_sync_action::{
        ArchiveChatAction, ContactAction, MuteAction, PinAction, SyncActionValue,
    };
    use wa_proto::wa_web_protobufs_server_sync::{SyncdSnapshot, SyncdVersion};

    use super::*;
    use crate::appstate::keys::store_generated_app_state_sync_key;
    use crate::appstate::{ExpandedAppStateKeys, MutationInfo, PatchInfo, encode_patch};
    use crate::client::ClientConfig;
    use crate::store::sqlite::{Container, SqlStore};

    /// The `<collection>` of a patch response, with optional external blobs.
    fn collection_node(snapshot: Option<ExternalBlobReference>, patches: Vec<SyncdPatch>) -> Node {
        let mut children = Vec::new();
        if let Some(reference) = snapshot {
            children.push(Node::new("snapshot").with_content(reference.encode_to_vec()));
        }
        if !patches.is_empty() {
            children.push(Node::new("patches").with_content(
                patches.into_iter().map(|p| Node::new("patch").with_content(p.encode_to_vec())).collect::<Vec<_>>(),
            ));
        }
        Node::new("collection").attr("name", "regular").with_content(children)
    }

    fn blob(enc_sha: u8) -> ExternalBlobReference {
        ExternalBlobReference {
            media_key: Some(vec![7; 32]),
            direct_path: Some("/v/t62/blob?ccb=11-4".to_owned()),
            handle: Some("h".to_owned()),
            file_size_bytes: Some(1234),
            file_sha256: Some(vec![1; 32]),
            file_enc_sha256: Some(vec![enc_sha; 32]),
        }
    }

    #[test]
    fn external_references_cover_the_snapshot_and_every_patch() {
        let patches = vec![
            SyncdPatch { external_mutations: Some(blob(2)), ..Default::default() },
            // A patch that carries its mutations inline has nothing to fetch.
            SyncdPatch::default(),
            SyncdPatch { external_mutations: Some(blob(3)), ..Default::default() },
        ];
        let found = external_references(&collection_node(Some(blob(1)), patches));

        assert_eq!(
            found.iter().map(|r| r.file_enc_sha256()[0]).collect::<Vec<_>>(),
            [1, 2, 3],
            "the snapshot first, then the patches in order"
        );
    }

    #[test]
    fn a_collection_without_external_blobs_needs_no_download() {
        assert!(external_references(&collection_node(None, vec![SyncdPatch::default()])).is_empty());
        assert!(external_references(&Node::new("collection")).is_empty());
    }

    #[test]
    fn an_undecodable_payload_is_left_to_the_decoder() {
        // Not a protobuf at all: the download step skips it, and
        // `decode_patch_list` is the one that reports the malformed node.
        let node = Node::new("collection")
            .with_content(vec![Node::new("snapshot").with_content(vec![0xff_u8, 0xff, 0xff, 0xff])]);
        assert!(external_references(&node).is_empty());
    }

    #[test]
    fn a_reference_becomes_a_verifiable_downloadable() {
        let media = external_downloadable(&blob(9));
        assert_eq!(media.direct_path, "/v/t62/blob?ccb=11-4");
        assert_eq!(media.media_key, vec![7; 32]);
        assert_eq!(media.file_enc_sha256, vec![9; 32], "what identifies the blob on the CDN");
        assert_eq!(media.file_sha256, vec![1; 32], "the plaintext hash is still checked");
        assert_eq!(media.file_length, 1234);
        assert!(media.mime_type.is_empty(), "an app state blob has no MIME type");

        // An empty reference must not panic — the download then fails cleanly.
        let empty = external_downloadable(&ExternalBlobReference::default());
        assert!(empty.direct_path.is_empty() && empty.media_key.is_empty());
    }

    /// A `Client` whose device uses in-memory stores — no socket involved.
    async fn client() -> (std::sync::Arc<Client>, std::sync::Arc<SqlStore>, Container) {
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
        let store = std::sync::Arc::new(SqlStore::new(container.pool().clone(), &Jid::new_ad("1", 0, 1)));
        let client = Client::new(device, ClientConfig::default(), std::sync::Arc::new(|_| {}));
        (client, store, container)
    }

    fn mute_value(muted: bool, end: i64) -> SyncActionValue {
        SyncActionValue {
            mute_action: Some(MuteAction { muted: Some(muted), mute_end_timestamp: Some(end), ..Default::default() }),
            ..Default::default()
        }
    }

    fn pin_value(pinned: bool) -> SyncActionValue {
        SyncActionValue { pin_action: Some(PinAction { pinned: Some(pinned) }), ..Default::default() }
    }

    fn archive_value(archived: bool) -> SyncActionValue {
        SyncActionValue {
            archive_chat_action: Some(ArchiveChatAction { archived: Some(archived), message_range: None }),
            ..Default::default()
        }
    }

    fn contact_value(first: &str, full: &str) -> SyncActionValue {
        SyncActionValue {
            contact_action: Some(ContactAction {
                first_name: Some(first.into()),
                full_name: Some(full.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Encodes one mutable patch on top of `state`, returning the `PatchList`
    /// the server would have answered with.
    fn patch_list(
        key_id: &[u8],
        keys: &ExpandedAppStateKeys,
        state: &HashState,
        name: &WAPatchName,
        mutations: Vec<MutationInfo>,
    ) -> (PatchList, HashState) {
        let info = PatchInfo { timestamp_millis: 1_700_000_000_000, name: name.clone(), mutations };
        let encoded = encode_patch(key_id, keys, state, &info, |_, _| Ok(None)).expect("encode patch");
        let mut list = PatchList::new(name.clone());
        list.patches = vec![encoded.patch];
        (list, encoded.state)
    }

    /// The trailing 32 bytes of a mutation's value blob — its value MAC.
    fn wire_value_mac(mutation: &SyncdMutation) -> Vec<u8> {
        let blob = mutation
            .record
            .as_ref()
            .and_then(|record| record.value.as_ref())
            .and_then(|value| value.blob.as_deref())
            .expect("the mutation has a value blob");
        blob[blob.len() - 32..].to_vec()
    }

    fn mutation(index: Vec<&str>, value: SyncActionValue) -> MutationInfo {
        MutationInfo { index: index.into_iter().map(str::to_string).collect(), version: 2, value }
    }

    #[tokio::test]
    async fn apply_app_state_patch_writes_mute_pin_and_archive_into_the_chat_settings_store() {
        let (client, store, _container) = client().await;
        let device = client.device().await;
        let mut rng = StdRng::seed_from_u64(71);
        let key = store_generated_app_state_sync_key(device.app_state_keys.as_ref(), &mut rng).await.expect("key");
        let keys = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let chat: Jid = "123@s.whatsapp.net".parse().expect("jid");

        let name: WAPatchName = WAPatchName::REGULAR_HIGH.into();
        let (list, expected) = patch_list(
            &key.key_id,
            &keys,
            &HashState::new(),
            &name,
            vec![
                mutation(vec!["mute", "123@s.whatsapp.net"], mute_value(true, 1_700_000_600_000)),
                mutation(vec!["pin_v1", "123@s.whatsapp.net"], pin_value(true)),
                mutation(vec!["archive", "123@s.whatsapp.net"], archive_value(true)),
            ],
        );

        let cache = AppStateKeyCache::new();
        let applied = client.apply_app_state_patch(&name, HashState::new(), &list, &cache, true).await.expect("apply");

        assert_eq!(applied.state, expected, "the applied state must be the encoded state");
        assert_eq!(applied.actions.len(), 3, "one action per mutation");
        assert!(matches!(applied.actions[0], SyncAction::Mute { from_full_sync: true, .. }));
        assert!(matches!(applied.actions[1], SyncAction::Pin { pinned: true, .. }));
        assert!(matches!(applied.actions[2], SyncAction::Archive { archived: true, .. }));

        let settings = client.get_chat_settings(&chat).await.expect("chat settings");
        assert_eq!(
            settings.muted_until,
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_600_000)),
            "the mute end timestamp becomes a SystemTime"
        );
        assert!(settings.pinned);
        assert!(settings.archived);

        // The collection version/LTHash and the mutation MACs were stored too.
        let (version, hash) = store.get_app_state_version(name.as_str()).await.expect("version");
        assert_eq!((version, hash), (expected.version, expected.hash));
    }

    #[tokio::test]
    async fn a_muted_forever_mutation_stores_the_forever_sentinel() {
        let (client, _store, _container) = client().await;
        let device = client.device().await;
        let mut rng = StdRng::seed_from_u64(72);
        let key = store_generated_app_state_sync_key(device.app_state_keys.as_ref(), &mut rng).await.expect("key");
        let keys = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let name: WAPatchName = WAPatchName::REGULAR_HIGH.into();

        // Go: `MuteEndTimestamp < 0` means "muted forever".
        let (list, _state) = patch_list(
            &key.key_id,
            &keys,
            &HashState::new(),
            &name,
            vec![mutation(vec!["mute", "123@s.whatsapp.net"], mute_value(true, -1))],
        );
        let muted_mac = wire_value_mac(
            list.patches.first().and_then(|patch| patch.mutations.first()).expect("the patch has one mutation"),
        );
        let applied = client
            .apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), false)
            .await
            .expect("apply");
        assert_eq!(
            applied.actions[0],
            SyncAction::Mute {
                chat: "123@s.whatsapp.net".parse().expect("jid"),
                muted_until: Some(muted_forever()),
                from_full_sync: false,
            }
        );
        let settings = client.get_chat_settings(&"123@s.whatsapp.net".parse().expect("jid")).await.expect("settings");
        assert_eq!(settings.muted_until, Some(muted_forever()));

        // A later unmute clears it again; the encoder resolves the previous
        // value MAC the same way the verifier does (from `applied.state`).
        let info = PatchInfo {
            timestamp_millis: 1_700_000_000_001,
            name: name.clone(),
            mutations: vec![mutation(vec!["mute", "123@s.whatsapp.net"], mute_value(false, 0))],
        };
        let encoded = encode_patch(&key.key_id, &keys, &applied.state, &info, |_, _| Ok(Some(muted_mac.clone())))
            .expect("encode unmute");
        let mut list = PatchList::new(name.clone());
        list.patches = vec![encoded.patch];
        client
            .apply_app_state_patch(&name, applied.state, &list, &AppStateKeyCache::new(), false)
            .await
            .expect("apply unmute");
        let settings = client.get_chat_settings(&"123@s.whatsapp.net".parse().expect("jid")).await.expect("settings");
        assert_eq!(settings.muted_until, None);
    }

    #[tokio::test]
    async fn a_contact_mutation_writes_the_contact_store() {
        let (client, _store, _container) = client().await;
        let device = client.device().await;
        let mut rng = StdRng::seed_from_u64(73);
        let key = store_generated_app_state_sync_key(device.app_state_keys.as_ref(), &mut rng).await.expect("key");
        let keys = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let name: WAPatchName = WAPatchName::CRITICAL_UNBLOCK_LOW.into();

        let (list, _state) = patch_list(
            &key.key_id,
            &keys,
            &HashState::new(),
            &name,
            vec![mutation(vec!["contact", "456@s.whatsapp.net"], contact_value("Alice", "Alice Doe"))],
        );
        let applied = client
            .apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), true)
            .await
            .expect("apply");
        assert_eq!(
            applied.actions[0],
            SyncAction::Contact {
                chat: "456@s.whatsapp.net".parse().expect("jid"),
                first_name: "Alice".into(),
                full_name: "Alice Doe".into(),
                from_full_sync: true,
            }
        );

        let contact = device.contacts.get_contact(&"456@s.whatsapp.net".parse().expect("jid")).await.expect("contact");
        assert!(contact.found);
        assert_eq!(contact.first_name, "Alice");
        assert_eq!(contact.full_name, "Alice Doe");
    }

    #[tokio::test]
    async fn a_tampered_patch_writes_nothing_at_all() {
        let (client, store, _container) = client().await;
        let device = client.device().await;
        let mut rng = StdRng::seed_from_u64(74);
        let key = store_generated_app_state_sync_key(device.app_state_keys.as_ref(), &mut rng).await.expect("key");
        let keys = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let name: WAPatchName = WAPatchName::REGULAR_HIGH.into();
        let (mut list, _state) = patch_list(
            &key.key_id,
            &keys,
            &HashState::new(),
            &name,
            vec![mutation(vec!["mute", "123@s.whatsapp.net"], mute_value(true, 1))],
        );
        // Corrupt the mutation so the content MAC no longer matches.
        if let Some(blob) = list
            .patches
            .first_mut()
            .and_then(|patch| patch.mutations.first_mut())
            .and_then(|mutation| mutation.record.as_mut())
            .and_then(|record| record.value.as_mut())
            .and_then(|value| value.blob.as_mut())
        {
            let last = blob.len() - 1;
            blob[last] ^= 0xFF;
        }

        let result =
            client.apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), false).await;
        assert!(result.is_err(), "a tampered mutation must fail verification");
        let err = result.expect_err("err");
        assert!(
            matches!(
                err.downcast_ref::<AppStateError>(),
                Some(AppStateError::MismatchingContentMac | AppStateError::MismatchingLtHash)
            ),
            "a MAC check must be what fails, got: {err}"
        );

        // No store write happened: version, chat settings and contacts are untouched.
        let (version, hash) = store.get_app_state_version(name.as_str()).await.expect("version");
        assert_eq!((version, hash), (0, [0u8; 128]));
        let settings = client.get_chat_settings(&"123@s.whatsapp.net".parse().expect("jid")).await.expect("settings");
        assert!(!settings.found, "the chat settings row must not exist yet");
    }

    #[tokio::test]
    async fn a_patch_with_an_unknown_sync_key_is_skipped_with_the_missing_ids() {
        let (client, store, _container) = client().await;
        let mut rng = StdRng::seed_from_u64(75);
        let key = store_generated_app_state_sync_key(client.device().await.app_state_keys.as_ref(), &mut rng)
            .await
            .expect("key");
        let keys = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let name: WAPatchName = WAPatchName::REGULAR_HIGH.into();
        let (list, _state) = patch_list(
            &key.key_id,
            &keys,
            &HashState::new(),
            &name,
            vec![mutation(vec!["pin_v1", "123@s.whatsapp.net"], pin_value(true))],
        );
        // A key we do not have: emulate a patch encrypted with the phone's key.
        let foreign = b"phone!".to_vec();
        let (list, _state) = patch_list(
            &foreign,
            &ExpandedAppStateKeys::expand(&[9u8; 32]).expect("expand"),
            &HashState::new(),
            &name,
            list.patches
                .first()
                .map(|patch| {
                    patch
                        .mutations
                        .iter()
                        .map(|_| mutation(vec!["pin_v1", "123@s.whatsapp.net"], pin_value(true)))
                        .collect()
                })
                .unwrap_or_default(),
        );

        let applied = client
            .apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), false)
            .await
            .expect("a missing key is a skip, not a failure");
        assert_eq!(applied.skipped, vec![AppStateSkip::MissingKeys(vec![foreign])]);
        assert!(applied.actions.is_empty());

        // The collection stays unsynced, with a reason, until a key share arrives.
        let (version, hash) = store.get_app_state_version(name.as_str()).await.expect("version");
        assert_eq!((version, hash), (0, [0u8; 128]), "nothing is written without the sync key");
    }

    #[tokio::test]
    async fn a_full_sync_snapshot_replaces_the_collection_state() {
        let (client, store, _container) = client().await;
        let device = client.device().await;
        let mut rng = StdRng::seed_from_u64(76);
        let key = store_generated_app_state_sync_key(device.app_state_keys.as_ref(), &mut rng).await.expect("key");
        let keys = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let name: WAPatchName = WAPatchName::REGULAR_HIGH.into();

        // One encrypted record, exactly as the encoder writes a mutation.
        let info = PatchInfo {
            timestamp_millis: 1,
            name: name.clone(),
            mutations: vec![mutation(vec!["mute", "123@s.whatsapp.net"], mute_value(true, 1_700_000_000_000))],
        };
        let encoded = encode_patch(&key.key_id, &keys, &HashState::new(), &info, |_, _| Ok(None)).expect("encode");
        let snapshot = SyncdSnapshot {
            version: Some(SyncdVersion { version: Some(4) }),
            records: encoded.patch.mutations.iter().filter_map(|mutation| mutation.record.clone()).collect(),
            mac: Some(vec![0u8; 32]),
            key_id: Some(wa_proto::wa_web_protobufs_server_sync::KeyId { id: Some(key.key_id.clone()) }),
        };
        // The snapshot MAC covers the collection hash the records produce.
        let mut snapshot_mac_state = HashState::new();
        snapshot_mac_state.version = 4;
        snapshot_mac_state.update_hash(&encoded.patch.mutations, |_, _| Ok(None)).expect("hash the snapshot records");
        let mut snapshot = snapshot;
        snapshot.mac = Some(snapshot_mac_state.generate_snapshot_mac(&name, &keys.snapshot_mac));

        let mut list = PatchList::new(name.clone());
        list.snapshot = Some(snapshot);
        let applied = client
            .apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), true)
            .await
            .expect("apply snapshot");

        assert_eq!(applied.state.version, 4);
        assert!(matches!(applied.actions[0], SyncAction::Mute { .. }));
        let (version, hash) = store.get_app_state_version(name.as_str()).await.expect("version");
        assert_eq!((version, hash), (4, applied.state.hash));
        let settings = client.get_chat_settings(&"123@s.whatsapp.net".parse().expect("jid")).await.expect("settings");
        assert!(settings.muted_until.is_some());
    }

    #[tokio::test]
    async fn handle_app_state_notification_rejects_other_notifications() {
        let (client, _store, _container) = client().await;
        let node = Node::new("notification").attr("type", "server_sync");
        // Not connected: the fetch fails, but the notification is accepted and
        // reported as "nothing synced" instead of panicking.
        let result = client.handle_app_state_notification(&node).await;
        assert!(result.is_ok(), "a server_sync notification on a disconnected client is not an error");

        let wrong = Node::new("notification").attr("type", "encrypt");
        assert!(client.handle_app_state_notification(&wrong).await.is_err());
    }

    #[tokio::test]
    async fn a_patch_chain_in_one_response_uses_the_in_batch_macs() {
        let (client, store, _container) = client().await;
        let device = client.device().await;
        let mut rng = StdRng::seed_from_u64(77);
        let key = store_generated_app_state_sync_key(device.app_state_keys.as_ref(), &mut rng).await.expect("key");
        let keys = ExpandedAppStateKeys::expand(&key.key.data).expect("expand");
        let name: WAPatchName = WAPatchName::REGULAR_HIGH.into();

        let first = PatchInfo {
            timestamp_millis: 1,
            name: name.clone(),
            mutations: vec![mutation(vec!["pin_v1", "123@s.whatsapp.net"], pin_value(true))],
        };
        let patch1 = encode_patch(&key.key_id, &keys, &HashState::new(), &first, |_, _| Ok(None)).expect("patch 1");
        let first_mac = patch1.patch.mutations[0]
            .record
            .as_ref()
            .and_then(|record| record.value.as_ref())
            .and_then(|value| value.blob.as_ref())
            .map(|blob| blob[blob.len() - 32..].to_vec())
            .expect("value mac");

        // Patch 2 flips the same chat back; it needs patch 1's value MAC.
        let second = PatchInfo {
            timestamp_millis: 2,
            name: name.clone(),
            mutations: vec![mutation(vec!["pin_v1", "123@s.whatsapp.net"], pin_value(false))],
        };
        let patch2 = encode_patch(&key.key_id, &keys, &patch1.state, &second, |_, _| Ok(Some(first_mac.clone())))
            .expect("patch 2");

        let mut list = PatchList::new(name.clone());
        list.patches = vec![patch1.patch.clone(), patch2.patch.clone()];
        let applied = client
            .apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), false)
            .await
            .expect("both patches must verify without a store round-trip");
        assert_eq!(applied.state, patch2.state);
        // Two mutations, both applied in order: pinned then unpinned.
        assert_eq!(applied.actions.len(), 2);
        let settings = client.get_chat_settings(&"123@s.whatsapp.net".parse().expect("jid")).await.expect("settings");
        assert!(!settings.pinned, "the last mutation wins");
        let (version, _hash) = store.get_app_state_version(name.as_str()).await.expect("version");
        assert_eq!(version, 2);
    }

    use crate::store::traits::AppStateStore;

    #[tokio::test]
    async fn a_key_share_unlocks_a_patch_that_was_skipped_for_a_missing_key() {
        use wa_proto::wa_web_protobufs_e2e::{AppStateSyncKey as ShareKey, AppStateSyncKeyData, AppStateSyncKeyId};

        let (client, store, _container) = client().await;
        let name: WAPatchName = WAPatchName::REGULAR_LOW.into();
        let phone_key_id = b"phone!".to_vec();
        let phone_key = [5u8; 32];
        let keys = ExpandedAppStateKeys::expand(&phone_key).expect("expand");
        let (list, _state) = patch_list(
            &phone_key_id,
            &keys,
            &HashState::new(),
            &name,
            vec![mutation(vec!["pin_v1", "123@s.whatsapp.net"], pin_value(true))],
        );

        // Before the share: skipped, nothing written.
        let skipped = client
            .apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), false)
            .await
            .expect("skip");
        assert_eq!(skipped.skipped, vec![AppStateSkip::MissingKeys(vec![phone_key_id.clone()])]);

        // The primary device shares its key. The client is not connected, so the
        // re-fetch stops at the first collection; the keys are stored regardless.
        let share = AppStateSyncKeyShare {
            keys: vec![ShareKey {
                key_id: Some(AppStateSyncKeyId { key_id: Some(phone_key_id.clone()) }),
                key_data: Some(AppStateSyncKeyData {
                    key_data: Some(phone_key.to_vec()),
                    fingerprint: None,
                    timestamp: Some(1_700_000_000_000),
                }),
            }],
        };
        let outcome = client.handle_app_state_sync_key_share(&share, true).await.expect("key share");
        assert_eq!(outcome.stored_key_ids, vec![phone_key_id.clone()]);
        assert!(outcome.fetched.is_empty(), "no socket, so nothing was fetched");

        // After the share: the same patch verifies, decodes and applies.
        let applied = client
            .apply_app_state_patch(&name, HashState::new(), &list, &AppStateKeyCache::new(), false)
            .await
            .expect("apply with the shared key");
        assert!(applied.skipped.is_empty());
        assert!(matches!(applied.actions.as_slice(), [SyncAction::Pin { pinned: true, .. }]));
        let (version, _) = store.get_app_state_version(name.as_str()).await.expect("version");
        assert_eq!(version, applied.state.version);
        let settings = client.get_chat_settings(&"123@s.whatsapp.net".parse().expect("jid")).await.expect("settings");
        assert!(settings.pinned);
    }
}
