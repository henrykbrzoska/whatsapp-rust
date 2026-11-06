//! bebok-server — HTTP + SSE front of the WhatsApp engine.
//!
//! stdout carries exactly one line, `BEBOK_READY http://<addr>/?token=…`;
//! everything else (logs) goes to stderr.

pub mod appstate_sync;
pub mod auth;
pub mod bindings;
pub mod config;
pub mod connection;
pub mod debug_log;
pub mod history_import;
pub mod routes;
pub mod state;

use tokio::net::TcpListener;

pub use config::Config;

pub async fn run(config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.addr).await?;
    let bound = listener.local_addr()?;
    if config.token.is_none() {
        tracing::warn!("BEBOK_NO_AUTH: API is UNAUTHENTICATED — local development only");
    }
    let ready = config.ready_line(bound);
    let state = state::AppState::new(config).await?;
    tracing::info!(data_dir = %state.config.data_dir.display(), "engine listening on http://{bound}");
    // Cached attachments expire on their own, not only when a chat is opened.
    routes::history::spawn_media_purge(&state);
    // Rows stored under the stanza kind (`type="media"`) get their real
    // attachment kind back, so the thread can render them.
    history_import::spawn_message_kind_repair(&state);
    // A PN-keyed chat whose LID we already know belongs to the LID thread.
    history_import::spawn_chat_merge(&state);
    println!("{ready}");
    axum::serve(listener, routes::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
