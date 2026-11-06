#!/usr/bin/env node
// Copies the engine binary into client/src-tauri/binaries/ with the
// target-triple suffix Tauri's `externalBin` expects:
//   engine/target/[<triple>/]release/bebok-server[.exe]
//     → client/src-tauri/binaries/bebok-server-<triple>[.exe]
//
// Usage: node scripts/copy-sidecar.mjs [--target <triple>] [--profile release|debug]
// The triple defaults to $TAURI_ENV_TARGET_TRIPLE, then `rustc -vV` host.

import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, existsSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

export function hostTriple() {
  const out = execFileSync('rustc', ['-vV'], { encoding: 'utf8' });
  const line = out.split('\n').find((l) => l.startsWith('host:'));
  if (!line) throw new Error('could not read the host triple from `rustc -vV`');
  return line.slice('host:'.length).trim();
}

export function copySidecar({ target, profile = 'release' } = {}) {
  const triple = target || process.env.TAURI_ENV_TARGET_TRIPLE || hostTriple();
  const ext = triple.includes('windows') ? '.exe' : '';
  const candidates = [
    join(root, 'engine', 'target', triple, profile, `bebok-server${ext}`),
    join(root, 'engine', 'target', profile, `bebok-server${ext}`),
  ];
  const src = candidates.find(existsSync);
  if (!src) {
    throw new Error(
      `engine binary not found (looked in:\n  ${candidates.join('\n  ')})\n` +
        `build it first: cd engine && cargo build ${profile === 'release' ? '--release ' : ''}-p bebok-server`,
    );
  }
  const destDir = join(root, 'client', 'src-tauri', 'binaries');
  mkdirSync(destDir, { recursive: true });
  const dest = join(destDir, `bebok-server-${triple}${ext}`);
  copyFileSync(src, dest);
  if (!ext) chmodSync(dest, 0o755);
  return { src, dest, triple };
}

function parseArgs(argv) {
  const opts = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--target') opts.target = argv[++i];
    else if (argv[i] === '--profile') opts.profile = argv[++i];
    else throw new Error(`unknown argument: ${argv[i]}`);
  }
  return opts;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const { src, dest } = copySidecar(parseArgs(process.argv.slice(2)));
    console.log(`sidecar: ${src} → ${dest}`);
  } catch (e) {
    console.error(`copy-sidecar: ${e.message}`);
    process.exit(1);
  }
}
