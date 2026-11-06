//! Helpers shared by the `binary` tests (not part of the public API).

use std::io::Write;

/// Compresses `data` with zlib, like `compress/zlib` does in Go.
pub fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("zlib write");
    encoder.finish().expect("zlib finish")
}
