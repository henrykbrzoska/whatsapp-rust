//! TypeScript binding export (`client/src/app/types/specta.ts`).
//!
//! Wire types opt in by deriving `specta::Type` with the `export` feature, which
//! registers them in `specta::export::TYPES` from a `ctor`. That registration is
//! only linked in when something in the binary references the type, so
//! [`touch_wire_types`] must run before [`export`].

use specta::ts::{BigIntExportBehavior, ExportConfiguration};
use specta::{DefOpts, NamedType, TypeDefs};
use wa_core::types::events::WaEvent;
use wa_store::{ChatStats, ChatSummary, SearchHit, SearchMode, StoredMessage};

/// References every exported type so the linker keeps its `ctor` registration.
pub fn touch_wire_types() -> usize {
    fn touch<T: NamedType>() {
        let mut map = TypeDefs::default();
        let _ = T::reference(DefOpts { parent_inline: false, type_map: &mut map }, &[]);
    }
    touch::<WaEvent>();
    touch::<StoredMessage>();
    touch::<SearchMode>();
    touch::<SearchHit>();
    touch::<ChatStats>();
    touch::<ChatSummary>();
    6
}

/// Registers the wire types and writes the TypeScript bindings to `path`.
///
/// `i64`/`u64` wire values are plain JSON numbers (`API-WHATSAPP.md`), so they
/// are exported as `number` rather than `bigint`.
pub fn export(path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let touched = touch_wire_types();
    let conf = ExportConfiguration::new().bigint(BigIntExportBehavior::Number);
    specta::export::ts_with_cfg(path, &conf)?;
    sort_declarations(path)?;
    Ok(touched)
}

/// Specta emits types in `ctor` registration order, which depends on linking.
/// Sort the declarations so the output is deterministic.
fn sort_declarations(path: &str) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut blocks: Vec<&str> = text.split("\n\n").map(str::trim).filter(|b| !b.is_empty()).collect();
    let header = blocks.remove(0);
    blocks.sort();
    let mut out = format!("{header}\n\n");
    for b in blocks {
        out.push_str(b);
        out.push_str("\n\n");
    }
    std::fs::write(path, out)
}
