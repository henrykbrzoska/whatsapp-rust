use std::sync::Arc;

use tokio::sync::broadcast;
use wa_core::WaEvent;
use wa_core::store::sqlite::Container;
use wa_store::Store;

use crate::config::Config;
use crate::connection::Connection;
use crate::debug_log::{DEBUG_LOG_CAPACITY, DebugLog};

pub struct AppState {
    pub config: Config,
    pub store: Arc<Store>,
    pub container: Container,
    pub events: broadcast::Sender<WaEvent>,
    /// The single WhatsApp session this engine owns (and its protocol client).
    pub connection: Connection,
    /// Ring buffer behind `GET /debug/log` (always recorded and served).
    pub debug_log: Arc<DebugLog>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub async fn new(config: Config) -> anyhow::Result<SharedState> {
        std::fs::create_dir_all(&config.data_dir)?;
        let store = Arc::new(Store::open(&config.data_dir.join("wa-store.db"), default_embedder(&config))?);
        let container = Container::open(&config.data_dir.join("wa-core.db")).await?;
        Ok(Self::assemble(config, store, container))
    }

    /// In-memory state for tests.
    pub async fn memory(config: Config) -> anyhow::Result<SharedState> {
        let store = Arc::new(Store::open_memory(None)?);
        let container = Container::open_memory().await?;
        Ok(Self::assemble(config, store, container))
    }

    fn assemble(config: Config, store: Arc<Store>, container: Container) -> SharedState {
        let (events, _) = broadcast::channel(2048);
        let connection = Connection::new();
        let debug_log = Arc::new(DebugLog::new(DEBUG_LOG_CAPACITY, config.token.iter().cloned().collect()));
        crate::debug_log::set_global(&debug_log);
        // Drain any early panics/fatals that happened before the log existed.
        crate::debug_log::drain_fallback_into(&debug_log);
        let state = Arc::new(Self { config, store, container, events, connection, debug_log });
        state.connection.attach(&state);
        // Every node sent to WA goes into the diagnostic ring buffer
        // (`GET /debug/log`) — no RUST_LOG needed. Also mirrored to
        // `tracing` target `wa_outgoing` when enabled.
        let log = Arc::clone(&state.debug_log);
        wa_core::request_log::set_sink(Arc::new(move |direction: wa_core::request_log::Direction, xml: String| {
            log.note(format!("{} {xml}", direction.marker()));
        }));
        state
    }

    /// Publish an event on the bus (SSE / Tauri) and record it in the debug log.
    pub fn emit(&self, ev: WaEvent) {
        self.debug_log.record_event(&ev);
        let _ = self.events.send(ev);
    }
}

#[cfg(feature = "fastembed")]
fn default_embedder(config: &Config) -> Option<Arc<dyn wa_store::Embedder>> {
    match wa_store::embed::MiniLm::new(config.data_dir.join("models")) {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => {
            tracing::warn!("fastembed unavailable, vector search disabled: {e:#}");
            None
        }
    }
}

#[cfg(not(feature = "fastembed"))]
fn default_embedder(_config: &Config) -> Option<Arc<dyn wa_store::Embedder>> {
    None
}
