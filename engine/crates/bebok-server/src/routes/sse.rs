//! `GET /events` — `WaEvent` stream as Server-Sent Events (browser mode
//! counterpart of the Tauri `wa-event`).

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use crate::state::SharedState;

pub async fn events(State(s): State<SharedState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(s.events.subscribe()).filter_map(|ev| async move {
        // Lagged receivers simply skip the dropped events.
        let ev = ev.ok()?;
        Some(Ok(Event::default().json_data(ev).ok()?))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
