//! Guards the generated TypeScript bindings against drift: the committed
//! `client/src/app/types/specta.ts` must match what the exporter produces.
//!
//! Run `npm run specta` after changing a wire type.

use std::path::PathBuf;

#[test]
fn committed_typescript_bindings_are_up_to_date() {
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../client/src/app/types/specta.ts");
    let expected = std::fs::read_to_string(&committed)
        .unwrap_or_else(|e| panic!("cannot read {}: {e} — run `npm run specta`", committed.display()));

    let tmp = std::env::temp_dir().join(format!("specta-check-{}.ts", std::process::id()));
    bebok_server::bindings::export(&tmp.to_string_lossy()).expect("specta export failed");
    let generated = std::fs::read_to_string(&tmp).expect("cannot read generated bindings");
    let _ = std::fs::remove_file(&tmp);

    assert_eq!(
        generated, expected,
        "client/src/app/types/specta.ts is stale — run `npm run specta` and commit the result"
    );
}
