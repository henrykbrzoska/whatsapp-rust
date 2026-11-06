#!/usr/bin/env sh
# Alias for `npm run full-build-dev`; `./dev.sh tauri` runs the Tauri dev shell.
cd "$(dirname "$0")" || exit 1
if [ "$1" = "tauri" ]; then
  shift
  exec node scripts/bebok.mjs full-build-dev --tauri "$@"
fi
exec node scripts/bebok.mjs full-build-dev "$@"
