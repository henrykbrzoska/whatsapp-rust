// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! App-state sync: the address book and the chat flags.
//!
//! `wa-core` can fetch and apply every app-state collection
//! ([`Client::fetch_app_state`]), but nothing used to call it, so a freshly
//! paired engine had an empty [`ContactStore`](wa_core::store::traits::ContactStore)
//! and the UI could only show raw JIDs. the Go client leaves this to the
//! application (`FetchAppState` is public API, `appstate.go:31`) — for this
//! engine the application is the server, so the sync runs here:
//!
//! * once per connection, when the client reports [`WaEvent::Connected`]
//!   (throttled, so a reconnect loop cannot hammer the server);
//! * on demand through `POST /appstate/sync` (the client's "refresh
//!   contacts & chats" action), optionally as a full re-sync.
//!
//! Progress is published as [`WaEvent::AppStateSyncProgress`] per collection
//! and noted in the debug log, so a browser client can watch it happen.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use wa_core::WaEvent;
use wa_core::appstate::WAPatchName;
use wa_core::client::appstate::{AppStateSkip, SyncAction};

use crate::state::SharedState;

/// Don't re-sync more often than this on repeated `connected` events.
const MIN_SECONDS_BETWEEN_AUTO_SYNCS: i64 = 60;
/// Don't ask the phone for the same app-state key more than once a day
/// (Go `requestMissingAppStateKeys`, `appStateKeyRequests`).
const MIN_SECONDS_BETWEEN_KEY_REQUESTS: i64 = 24 * 60 * 60;

/// Unix seconds of the last automatic sync (0 = never).
static LAST_AUTO_SYNC: AtomicI64 = AtomicI64::new(0);
/// Unix seconds of the last request per app-state key ID (hex).
static LAST_KEY_REQUESTS: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

/// What one [`sync`] run did, per collection.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    /// Collections that were fetched, with the number of applied mutations.
    pub collections: Vec<CollectionReport>,
    /// Contact names that were written (the names the UI shows).
    pub contacts: usize,
    /// Chat flag changes (mute / pin / archive) that were applied.
    pub chat_settings: usize,
    /// Collections that could not be synced, with the reason.
    pub errors: Vec<String>,
    /// What was fetched but could not be applied — above all the app-state
    /// sync keys this device does not have. Without this the UI would report
    /// "0 contacts, 0 errors" for a sync that in fact applied nothing.
    pub skipped: Vec<String>,
    /// App-state key IDs (hex) this run asked the phone for.
    pub requested_keys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionReport {
    pub name: String,
    pub applied: usize,
    /// `true` when the server still has more patches than one run fetched.
    pub has_more: bool,
}

/// Sync every app-state collection. `full` drops the stored versions first and
/// asks for a fresh snapshot (Go `FetchAppState(..., fullSync: true, ...)`).
pub async fn sync(app: &SharedState, full: bool) -> SyncReport {
    let Some(client) = app.connection.client() else {
        return SyncReport { errors: vec!["not connected".to_owned()], ..Default::default() };
    };
    let mut report = SyncReport::default();
    let mut missing_keys: Vec<Vec<u8>> = Vec::new();
    let names = WAPatchName::all();
    let total = names.len();
    for (index, name) in names.into_iter().enumerate() {
        match client.fetch_app_state(&name, full).await {
            Ok(fetched) => {
                for skip in &fetched.skipped {
                    match skip {
                        AppStateSkip::MissingKeys(ids) => {
                            for id in ids {
                                if !missing_keys.contains(id) {
                                    missing_keys.push(id.clone());
                                }
                            }
                            report.skipped.push(format!(
                                "{}: needs {} app state key(s) this device does not have",
                                name.as_str(),
                                ids.len()
                            ));
                        }
                        AppStateSkip::MalformedIndex(index) => {
                            report.skipped.push(format!("{}: malformed mutation index {index:?}", name.as_str()));
                        }
                    }
                }
                for action in &fetched.actions {
                    match action {
                        SyncAction::Contact { .. } => report.contacts += 1,
                        SyncAction::Mute { .. } | SyncAction::Pin { .. } | SyncAction::Archive { .. } => {
                            report.chat_settings += 1
                        }
                        SyncAction::Unhandled { .. } => {}
                    }
                }
                report.collections.push(CollectionReport {
                    name: name.as_str().to_owned(),
                    applied: fetched.actions.len(),
                    has_more: fetched.has_more_patches,
                });
                app.emit(WaEvent::AppStateSyncProgress {
                    name: name.as_str().to_owned(),
                    applied: (index + 1) as u64,
                    total: total as u64,
                });
            }
            Err(err) => {
                let message = format!("{}: {err:#}", name.as_str());
                tracing::warn!("app state sync failed for {message}");
                report.errors.push(message);
            }
        }
    }
    // A patch is encrypted with a key the primary phone owns; without it
    // nothing can be applied, and the only way to get one is to ask (Go
    // `requestMissingAppStateKeys`). The answer is an `AppStateSyncKeyShare`
    // protocol message, which the client persists on its own — so the next
    // sync is the one that applies the mutations.
    let now = wa_store::unix_now();
    let to_request = due_key_requests(&missing_keys, now);
    if !to_request.is_empty() {
        match client.request_app_state_keys(&to_request).await {
            Ok(Some(id)) => {
                mark_key_requests(&to_request, now);
                report.requested_keys = to_request.iter().map(hex::encode).collect();
                app.debug_log.note(format!(
                    "asked the phone for {} app state key(s) ({}), request {id}",
                    to_request.len(),
                    report.requested_keys.join(", ")
                ));
            }
            Ok(None) => {}
            Err(err) => {
                // Not marked: the next sync tries again instead of waiting a
                // day for a request the server never accepted.
                let message = format!("could not ask for the app state keys: {err:#}");
                tracing::warn!("{message}");
                report.errors.push(message);
            }
        }
    }
    app.debug_log.note(format!(
        "app state sync{}: {} contact(s), {} chat setting(s), {} skipped, {} error(s)",
        if full { " (full)" } else { "" },
        report.contacts,
        report.chat_settings,
        report.skipped.len(),
        report.errors.len()
    ));
    report
}

/// The key IDs that may be asked for now, against the process-wide history.
fn due_key_requests(ids: &[Vec<u8>], now: i64) -> Vec<Vec<u8>> {
    let mut guard = LAST_KEY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    due_keys(guard.get_or_insert_with(HashMap::new), ids, now)
}

/// Record that the phone was asked for these keys. Called **after** a
/// successful send: a request the server rejected must not block the retry
/// for a day.
fn mark_key_requests(ids: &[Vec<u8>], now: i64) {
    let mut guard = LAST_KEY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(HashMap::new);
    for id in ids {
        seen.insert(hex::encode(id), now);
    }
}

/// One request per key per [`MIN_SECONDS_BETWEEN_KEY_REQUESTS`], so a
/// collection that stays unapplied cannot turn every sync into a peer
/// message. `seen` is the request history — a parameter, so the rule can be
/// tested without the process-wide state.
fn due_keys(seen: &HashMap<String, i64>, ids: &[Vec<u8>], now: i64) -> Vec<Vec<u8>> {
    ids.iter()
        .filter(|id| now - seen.get(&hex::encode(id)).copied().unwrap_or(0) >= MIN_SECONDS_BETWEEN_KEY_REQUESTS)
        .cloned()
        .collect()
}

/// Run [`sync`] in the background after a `connected` event, at most once per
/// [`MIN_SECONDS_BETWEEN_AUTO_SYNCS`]. Called from the client's event sink,
/// which must not block.
pub fn spawn_after_connect(app: &Arc<crate::state::AppState>) {
    let now = wa_store::unix_now();
    let last = LAST_AUTO_SYNC.load(Ordering::Relaxed);
    if now - last < MIN_SECONDS_BETWEEN_AUTO_SYNCS {
        return;
    }
    LAST_AUTO_SYNC.store(now, Ordering::Relaxed);
    let app = Arc::clone(app);
    tokio::spawn(async move {
        // The `connected` event fires on `<success>`; the collections are
        // fetched over the same socket, so no extra wait is needed.
        let report = sync(&app, false).await;
        if report.contacts > 0 || report.chat_settings > 0 {
            tracing::info!(
                "app state sync applied {} contacts and {} chat settings",
                report.contacts,
                report.chat_settings
            );
        }
    });
}

/// Test hook: forget the throttles so a test can trigger another auto sync.
#[cfg(test)]
pub(crate) fn reset_throttle() {
    LAST_AUTO_SYNC.store(0, Ordering::Relaxed);
    *LAST_KEY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::state::AppState;

    #[tokio::test]
    async fn without_a_session_the_report_says_not_connected() {
        let cfg = Config::parse(["--no-auth".to_string()], |_| None).unwrap();
        let app = AppState::memory(cfg).await.unwrap();
        let report = sync(&app, false).await;
        assert_eq!(report.errors, ["not connected"]);
        assert!(report.collections.is_empty());
        assert_eq!((report.contacts, report.chat_settings), (0, 0));
    }

    #[test]
    fn a_key_is_asked_for_once_a_day() {
        // A local history, so this test shares nothing with its neighbours.
        let mut seen = HashMap::new();
        let a = vec![1u8, 2, 3];
        let b = vec![4u8, 5, 6];
        let now = 1_700_000_000;

        assert_eq!(due_keys(&seen, &[a.clone(), b.clone()], now), vec![a.clone(), b.clone()]);
        assert!(due_keys(&seen, &[], now).is_empty());

        // Only a request that actually went out counts.
        for id in [&a, &b] {
            seen.insert(hex::encode(id), now);
        }
        assert!(due_keys(&seen, &[a.clone(), b.clone()], now + 60).is_empty(), "not again a minute later");

        // A day later the phone is asked again — a key that never arrives
        // would otherwise leave the address book empty for good.
        assert_eq!(
            due_keys(&seen, std::slice::from_ref(&a), now + MIN_SECONDS_BETWEEN_KEY_REQUESTS),
            vec![a],
            "a day later it is worth another request"
        );
        assert!(due_keys(&seen, &[b], now + MIN_SECONDS_BETWEEN_KEY_REQUESTS - 1).is_empty());
    }

    /// A request the server rejected (`<error code="479">`) must not block the
    /// retry: the keys stay due until one is accepted.
    #[test]
    fn a_rejected_request_does_not_burn_the_daily_slot() {
        let seen = HashMap::new();
        let id = vec![9u8, 9, 9];
        let now = 1_700_000_000;
        assert_eq!(due_keys(&seen, std::slice::from_ref(&id), now), vec![id.clone()]);
        // Nothing was marked, because nothing was sent.
        assert_eq!(due_keys(&seen, std::slice::from_ref(&id), now + 1), vec![id]);
    }

    #[tokio::test]
    async fn the_auto_sync_is_throttled() {
        reset_throttle();
        let cfg = Config::parse(["--no-auth".to_string()], |_| None).unwrap();
        let app = AppState::memory(cfg).await.unwrap();
        spawn_after_connect(&app);
        let first = LAST_AUTO_SYNC.load(Ordering::Relaxed);
        assert!(first > 0, "the first connect records a sync");
        spawn_after_connect(&app);
        assert_eq!(LAST_AUTO_SYNC.load(Ordering::Relaxed), first, "a second connect within a minute is skipped");
    }
}
