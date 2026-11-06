# scripts — build, release and packaging helpers

| file | entry point | purpose |
|---|---|---|
| `bebok.mjs` | `npm run doctor / engine / client / full-build-dev / full-build-app` | the one developer entry point (see `README.md` §Running) |
| `copy-sidecar.mjs` | `npm run sidecar:copy` | engine binary → `client/src-tauri/binaries/bebok-server-<triple>` |
| `release.mjs` | `npm run release` | version stamping + `SHA256SUMS.txt` generation/verification |

`bebok.mjs --help` (or `npm run full-build-app -- --help`) prints every command and flag;
`release.mjs --help` does the same for the release helper.

## Release flow

```bash
# 1. stamp the version (package.json, client/package.json, tauri.conf.json)
npm run release -- version --bump minor          # or: --set 0.3.0
npm run release -- version --show                # verify all three agree

# 2. build the engine + sidecar + every Tauri bundle, write SHA256SUMS.txt
npm run full-build-app                           # full
npm run full-build-app -- --bundles deb,appimage # one target set
npm run full-build-app -- --skip-engine          # reuse the built sidecar binary

# 3. verify / refresh the checksums without rebuilding
npm run release:checksums                        # (re)write SHA256SUMS.txt
npm run release:checksums -- --verify            # recompute and compare
```

Artifacts land in `client/src-tauri/target/release/bundle/`:

```
bundle/
├── deb/*.deb  rpm/*.rpm  appimage/*.AppImage   (Linux)
├── dmg/*.dmg  macos/*.app                     (macOS — .app dirs are not hashed)
├── msi/*.msi  nsis/*-setup.exe                (Windows)
└── SHA256SUMS.txt                             # <sha256>  <relative path>
```

`SHA256SUMS.txt` is written by `full-build-app` and by `release.mjs`; both use the
same walker (`checksumLines` in `release.mjs`), so a rebuild and a manual
re-checksum always agree. `.app` bundles are directories and are skipped — the
dmg carries the payload.

`version --set` rewrites exactly three files and nothing else:

* `package.json` → `version`
* `client/package.json` → `version`
* `client/src-tauri/tauri.conf.json` → `version`

`engine/Cargo.toml` is reported but **not** touched: the Rust workspace has its
own version and bumping it re-locks `Cargo.lock`. Bump it by hand when the
protocol surface changes (`API-WHATSAPP.md` §7).

`--dry-run` works for every command (`version --bump patch --dry-run`).

## Verifying the `full-build-app` path from a clean checkout

Prerequisites: Node ≥ 20, Rust ≥ 1.88 with `cargo`, and the Tauri system
libraries for the host OS (Linux: `webkit2gtk-4.1`, `libsoup-3.0`, `gtk+-3.0`,
`librsvg-2.0`).

```bash
git clone <repo> && cd whatsapp-angular
npm run doctor              # every required tool ✓ (sidecar + node_modules are optional)
cd client && npm install && cd ..
npm run full-build-app      # cargo --release build → sidecar copy → tauri build → SHA256SUMS.txt
npm run release:checksums -- --verify   # artifacts match the sums
```

The script fails fast with an actionable message at every step:

* missing system library → `tauri build` fails with the package name;
* missing engine binary → `copy-sidecar.mjs` prints both paths it looked in;
* missing icons → `tauri icon` regenerates them from `src-tauri/icons/icon.svg`;
* missing bundles → `no bundles found under …` (the checksum step is skipped,
  not failed).

Partial runs are supported and were exercised here: `--skip-engine` (reuse the
sidecar), `--skip-tauri` (engine only), `--bundles deb,appimage`.
