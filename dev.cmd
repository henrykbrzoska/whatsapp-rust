@echo off
rem Alias for `npm run full-build-dev`; `dev.cmd tauri` runs the Tauri dev shell.
cd /d "%~dp0"
if "%1"=="tauri" (
  node scripts\bebok.mjs full-build-dev --tauri %2 %3 %4 %5 %6 %7 %8 %9
) else (
  node scripts\bebok.mjs full-build-dev %*
)
