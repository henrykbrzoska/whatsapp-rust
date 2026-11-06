//! Port of `Go store/` — traits, `Device` and the SQLite implementation.

pub mod device;
pub mod sqlite;
pub mod traits;

pub use device::Device;
pub use traits::*;
