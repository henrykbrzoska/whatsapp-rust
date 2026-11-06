//! SQLite implementation of the store (port of `Go store/sqlstore/`).
//!
//! * [`Container`] — database + devices (`container.go`)
//! * [`SqlStore`] — per-device stores (`store.go`)
//! * [`CachedLidMap`] — the global LID ↔ PN map (`lidmap.go`)

mod container;
mod lid_map;
mod store;

pub use container::{Container, SCHEMA_VERSION};
pub use lid_map::CachedLidMap;
pub use store::SqlStore;

#[cfg(test)]
mod tests;
