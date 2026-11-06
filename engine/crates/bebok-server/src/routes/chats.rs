//! Local database endpoints (`API-WHATSAPP.md` §3.8): chat list (with live
//! mute/pin/archive), search, history export, stats, delete.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use wa_core::WaEvent;
use wa_core::types::jid::Jid;
use wa_store::export::{Exporter, Format, PAGE_SIZE};
use wa_store::{ArchiveFilter, ChatSettingsView, ContactView, SearchMode, Store, StoredMessage};

use super::error;
use crate::state::SharedState;

fn internal(e: impl std::fmt::Display) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub(super) async fn blocking<T: Send + 'static>(
    store: &Arc<Store>,
    f: impl FnOnce(&Store) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, Response> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || f(&store)).await.map_err(internal)?.map_err(internal)
}

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    chat: Option<String>,
    limit: Option<usize>,
    mode: Option<String>,
}

pub async fn search(State(s): State<SharedState>, Query(p): Query<SearchParams>) -> Result<Json<Value>, Response> {
    let mode: SearchMode =
        p.mode.as_deref().unwrap_or("hybrid").parse().map_err(|e: String| error(StatusCode::BAD_REQUEST, e))?;
    let limit = p.limit.unwrap_or(20).clamp(1, 200);
    let chat = p.chat.filter(|c| !c.is_empty());
    let q = p.q;
    let hits = blocking(&s.store, move |st| st.search(&q, chat.as_deref(), limit, mode)).await?;
    Ok(Json(json!({ "mode": mode, "vector": s.store.has_embedder(), "results": hits })))
}

#[derive(Deserialize)]
pub struct ExportParams {
    format: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
}

struct ExportCursor {
    exporter: Exporter,
    after: Option<(i64, String)>,
    exported: u64,
    total: u64,
    done: bool,
    started: bool,
}

pub async fn export(
    State(s): State<SharedState>,
    Path(jid): Path<String>,
    Query(p): Query<ExportParams>,
) -> Result<Response, Response> {
    let format: Format =
        p.format.as_deref().unwrap_or("json").parse().map_err(|e: String| error(StatusCode::BAD_REQUEST, e))?;
    let chat = jid.clone();
    let total = blocking(&s.store, move |st| st.chat_stats(&chat)).await?.count as u64;

    let cursor =
        ExportCursor { exporter: Exporter::new(format), after: None, exported: 0, total, done: false, started: false };
    let (from, to) = (p.from, p.to);
    let state = s.clone();
    let chat = jid.clone();
    let stream = futures::stream::unfold(cursor, move |mut c| {
        let state = state.clone();
        let chat = chat.clone();
        async move {
            if c.done {
                return None;
            }
            if !c.started {
                c.started = true;
                let head = c.exporter.begin();
                return Some((Ok::<_, std::io::Error>(head), c));
            }
            let store = state.store.clone();
            let (ch, after) = (chat.clone(), c.after.clone());
            let page = tokio::task::spawn_blocking(move || {
                store.page(&ch, after.as_ref().map(|(t, i)| (*t, i.as_str())), from, to, PAGE_SIZE)
            })
            .await
            .map_err(std::io::Error::other)
            .and_then(|r| r.map_err(std::io::Error::other));
            let page = match page {
                Ok(p) => p,
                Err(e) => {
                    c.done = true;
                    return Some((Err(e), c));
                }
            };
            if page.is_empty() {
                c.done = true;
                return Some((Ok(c.exporter.end()), c));
            }
            let last = page.last().unwrap();
            c.after = Some((last.timestamp, last.id.clone()));
            c.exported += page.len() as u64;
            state.emit(WaEvent::HistoryExportProgress { chat: chat.clone(), exported: c.exported, total: c.total });
            Some((Ok(c.exporter.chunk(&page)), c))
        }
    });

    let filename = format!("chat-{}.{}", sanitize(&jid), format.extension());
    Ok((
        [
            (header::CONTENT_TYPE, format.content_type().to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
            (header::HeaderName::from_static("x-total-count"), total.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

fn sanitize(jid: &str) -> String {
    jid.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@') { c } else { '_' }).collect()
}

/// Query of `GET /chats?archived=`.
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    /// `all` (default), `active`/`false` (main list) or `archived`/`true`.
    archived: Option<String>,
}

/// `GET /chats` — `ChatListEntry[]`: the message summaries merged with the
/// live mute/pin/archive state (`ChatSettingsStore`) and display names
/// (`ContactStore`). Pinned first, then most recent. Works offline.
pub async fn list(State(s): State<SharedState>, Query(p): Query<ListParams>) -> Result<Json<Value>, Response> {
    let filter: ArchiveFilter =
        p.archived.as_deref().unwrap_or("all").parse().map_err(|e: String| error(StatusCode::BAD_REQUEST, e))?;
    let jids = blocking(&s.store, |st| st.chat_jids()).await?;
    let (settings, contacts) = chat_meta(&s, &jids).await?;
    let now = wa_store::unix_now();
    let chats = blocking(&s.store, move |st| st.list_chats_with_meta(&settings, &contacts, now, filter)).await?;
    Ok(Json(serde_json::to_value(chats).map_err(internal)?))
}

type ChatMeta = (HashMap<String, ChatSettingsView>, HashMap<String, ContactView>);

/// Settings and display names for `jids`, keyed by the JID string as stored
/// in wa-store. A store error for one chat degrades to the defaults (logged)
/// rather than failing the whole list.
async fn chat_meta(s: &SharedState, jids: &[String]) -> Result<ChatMeta, Response> {
    let device = super::groups::current_device(s).await?;
    let mut settings = HashMap::new();
    for raw in jids {
        let Ok(jid) = Jid::parse(raw) else { continue };
        match device.chat_settings.get_chat_settings(&jid).await {
            Ok(cs) if cs.found => {
                settings.insert(raw.clone(), ChatSettingsView::from_local(cs.muted_until, cs.pinned, cs.archived));
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("chat settings for {raw} unavailable: {err}"),
        }
    }
    let contacts = match device.contacts.get_all_contacts().await {
        Ok(all) => all
            .into_iter()
            .map(|(jid, c)| {
                let view = ContactView {
                    first_name: c.first_name,
                    full_name: c.full_name,
                    push_name: c.push_name,
                    business_name: c.business_name,
                };
                (jid.to_string(), view)
            })
            .collect(),
        Err(err) => {
            tracing::warn!("contacts unavailable for the chat list: {err}");
            HashMap::new()
        }
    };
    Ok((settings, contacts))
}

pub async fn stats(State(s): State<SharedState>, Path(jid): Path<String>) -> Result<Json<Value>, Response> {
    let st = blocking(&s.store, move |st| st.chat_stats(&jid)).await?;
    Ok(Json(serde_json::to_value(st).map_err(internal)?))
}

pub async fn delete_messages(State(s): State<SharedState>, Path(jid): Path<String>) -> Result<Json<Value>, Response> {
    let deleted = blocking(&s.store, move |st| st.delete_chat(&jid)).await?;
    Ok(Json(json!({ "deleted": deleted })))
}

/// Diagnostic only: bulk-insert messages so search/export can be exercised
/// before the real message pipeline exists.
pub async fn seed(State(s): State<SharedState>, Json(msgs): Json<Vec<StoredMessage>>) -> Result<Json<Value>, Response> {
    let n = msgs.len();
    blocking(&s.store, move |st| st.insert_messages(&msgs)).await?;
    Ok(Json(json!({ "inserted": n })))
}
