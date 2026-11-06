#!/usr/bin/env node
// Release helper for ngWhatsapp (run from the repo root):
//
//   node scripts/release.mjs version --show
//   node scripts/release.mjs version --set 0.2.0 [--dry-run]
//   node scripts/release.mjs version --bump patch|minor|major [--dry-run]
//   node scripts/release.mjs checksums [--verify] [--dir <bundle-dir>] [--dry-run]
//   node scripts/release.mjs all --set 0.2.0        # stamp + checksums
//
// What it does
//   * version  stamp the same semver into package.json, client/package.json and
//              client/src-tauri/tauri.conf.json (and report engine versions it
//              does NOT touch — the Rust workspace has its own version).
//   * checksums hash every Tauri bundle artifact (.deb, .rpm, .AppImage, .dmg,
//              .msi, .exe) into SHA256SUMS.txt inside the bundle directory —
//              the same file `npm run full-build-app` writes — and, with
//              --verify, recompute and compare against the existing one.
//
// Bundles are produced by `npm run full-build-app` (cargo release build +
// sidecar copy + `tauri build`). This script never builds anything itself.
// Exit codes: 0 ok, 1 error (bad flag / checksum mismatch / missing bundles).

import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** Files `version` stamps (semver only — nothing else is rewritten). */
export const VERSION_TARGETS = [
  { path: 'package.json', read: (t) => JSON.parse(t).version, write: (t, v) => replaceField(t, 'version', v) },
  { path: 'client/package.json', read: (t) => JSON.parse(t).version, write: (t, v) => replaceField(t, 'version', v) },
  {
    path: 'client/src-tauri/tauri.conf.json',
    read: (t) => JSON.parse(t).version,
    write: (t, v) => replaceField(t, 'version', v),
  },
];

/** Versions that live in the repo but stay out of a client release. */
export const VERSION_REPORT_ONLY = ['engine/Cargo.toml'];

const BUNDLE_EXTS = /\.(deb|rpm|AppImage|dmg|msi|exe)$/i;
const DEFAULT_BUNDLE_DIR = 'client/src-tauri/target/release/bundle';

// ---------------------------------------------------------------- helpers

function die(msg) {
  console.error(`release: ${msg}`);
  process.exit(1);
}

function log(msg) {
  console.log(`release: ${msg}`);
}

/** Rewrites `"<field>": "…"` once, keeping the surrounding formatting. */
function replaceField(text, field, value) {
  const re = new RegExp(`("${field}"\\s*:\\s*")[^"]*(")`);
  if (!re.test(text)) throw new Error(`no "${field}" field found`);
  return text.replace(re, `$1${value}$2`);
}

export function isValidSemver(v) {
  return /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(v.trim());
}

export function bumpVersion(current, kind) {
  const m = /^(\d+)\.(\d+)\.(\d+)/.exec(current.trim());
  if (!m) throw new Error(`not a semver: ${current}`);
  let [major, minor, patch] = [Number(m[1]), Number(m[2]), Number(m[3])];
  if (kind === 'major') [major, minor, patch] = [major + 1, 0, 0];
  else if (kind === 'minor') [minor, patch] = [minor + 1, 0];
  else if (kind === 'patch') patch += 1;
  else throw new Error(`unknown bump kind: ${kind} (use patch, minor or major)`);
  return `${major}.${minor}.${patch}`;
}

/** Reads the versions currently in the tree (`null` when unparsable). */
export function readVersions() {
  return VERSION_TARGETS.map((t) => {
    const abs = join(root, t.path);
    const text = existsSync(abs) ? readFileSync(abs, 'utf8') : '';
    let version = null;
    try {
      version = text ? t.read(text) : null;
    } catch {
      version = null;
    }
    return { path: t.path, version, present: text !== '' };
  });
}

/** Collects bundle artifacts, depth-first (`.app` dirs are skipped: hash the dmg). */
export function walkArtifacts(dir, out = []) {
  if (!existsSync(dir)) return out;
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) {
      if (!name.endsWith('.app')) walkArtifacts(p, out);
    } else if (BUNDLE_EXTS.test(name)) {
      out.push(p);
    }
  }
  return out;
}

/** `sha256  relative/path` lines for every artifact under `dir` (sorted by path). */
export function checksumLines(dir) {
  return walkArtifacts(dir)
    .map((p) => ({
      path: relative(dir, p).split(sep).join('/'),
      sha: createHash('sha256').update(readFileSync(p)).digest('hex'),
    }))
    .sort((a, b) => a.path.localeCompare(b.path))
    .map((e) => `${e.sha}  ${e.path}`);
}

/** `SHA256SUMS.txt` entries keyed by relative path. */
export function parseSums(text) {
  const map = new Map();
  for (const line of text.split('\n')) {
    const m = /^([0-9a-f]{64})\s+\*?(.+)$/i.exec(line.trim());
    if (m) map.set(m[2].split(sep).join('/'), m[1].toLowerCase());
  }
  return map;
}

// ---------------------------------------------------------------- commands

function help() {
  console.log(`usage: node scripts/release.mjs <command> [flags]

commands
  version    stamp / show the release version
             --show                 print the version in every stamped file
             --set <semver>         write this version everywhere
             --bump patch|minor|major   derive the next version from package.json
  checksums  write SHA256SUMS.txt for the Tauri bundles
             --dir <dir>            bundle directory (default ${DEFAULT_BUNDLE_DIR})
             --verify               recompute and compare with the existing file
  all        version --set/--bump, then checksums

flags
  --dry-run  print what would change, write nothing
  --help     this text

Bundles come from: npm run full-build-app
Exit codes: 0 ok, 1 error.`);
}

function cmdVersion(opts) {
  const versions = readVersions();
  if (opts.show || (!opts.set && !opts.bump)) {
    for (const v of versions) console.log(`  ${v.path.padEnd(34)} ${v.version ?? '(missing)'}`);
    const mismatch = new Set(versions.map((v) => v.version));
    if (mismatch.size > 1) log('warning: stamped files disagree on the version');
    for (const p of VERSION_REPORT_ONLY) {
      const abs = join(root, p);
      if (!existsSync(abs)) continue;
      const m = /^\s*version\s*=\s*"([^"]+)"/m.exec(readFileSync(abs, 'utf8'));
      log(`${p} (not stamped, Rust workspace): ${m ? m[1] : 'unversioned'}`);
    }
    return;
  }

  let next = opts.set;
  if (opts.bump) {
    const current = versions.find((v) => v.path === 'package.json')?.version;
    if (!current) die(`cannot bump: package.json has no parsable version`);
    try {
      next = bumpVersion(current, opts.bump);
    } catch (e) {
      die(e.message);
    }
  }
  if (!isValidSemver(next)) die(`not a semver version: ${next}`);

  log(`target version: ${next}`);
  for (const t of VERSION_TARGETS) {
    const abs = join(root, t.path);
    const before = existsSync(abs) ? readFileSync(abs, 'utf8') : null;
    if (before === null) {
      log(`  skip ${t.path} (missing)`);
      continue;
    }
    const after = t.write(before, next);
    const old = versions.find((v) => v.path === t.path)?.version ?? '?';
    if (after === before) {
      log(`  ${t.path}: already ${next}`);
      continue;
    }
    if (opts.dryRun) log(`  would stamp ${t.path}: ${old} → ${next}`);
    else {
      writeFileSync(abs, after);
      log(`  stamped ${t.path}: ${old} → ${next}`);
    }
  }
  log('now rebuild with: npm run full-build-app');
}

function cmdChecksums(opts) {
  const dir = opts.dir ? resolve(root, opts.dir) : join(root, DEFAULT_BUNDLE_DIR);
  const relDir = relative(root, dir) || '.';
  if (!existsSync(dir)) {
    die(`bundle directory not found: ${relDir}\nbuild the bundles first: npm run full-build-app`);
  }
  const lines = checksumLines(dir);
  const sumsPath = join(dir, 'SHA256SUMS.txt');

  if (opts.verify) {
    if (!existsSync(sumsPath)) die(`no SHA256SUMS.txt in ${relDir} (run: node scripts/release.mjs checksums)`);
    const expected = parseSums(readFileSync(sumsPath, 'utf8'));
    const actual = parseSums(`${lines.join('\n')}\n`);
    const problems = [];
    for (const [file, sha] of actual) {
      if (!expected.has(file)) problems.push(`missing from SHA256SUMS.txt: ${file}`);
      else if (expected.get(file) !== sha) problems.push(`checksum mismatch: ${file}`);
    }
    for (const file of expected.keys()) if (!actual.has(file)) problems.push(`listed but absent: ${file}`);
    if (problems.length) {
      console.error(problems.map((p) => `  ✗ ${p}`).join('\n'));
      die(`verification failed (${problems.length} problem${problems.length === 1 ? '' : 's'})`);
    }
    log(`verified ${actual.size} artifact${actual.size === 1 ? '' : 's'} in ${relDir}`);
    return;
  }

  if (!lines.length) {
    log(`no bundle artifacts (${BUNDLE_EXTS.source}) under ${relDir}`);
    log('build them first: npm run full-build-app');
    return;
  }
  const text = `${lines.join('\n')}\n`;
  if (opts.dryRun) {
    log(`would write ${relative(root, sumsPath)}:\n${lines.map((l) => `    ${l}`).join('\n')}`);
    return;
  }
  writeFileSync(sumsPath, text);
  log(`${lines.length} artifact${lines.length === 1 ? '' : 's'} → ${relative(root, sumsPath)}`);
  console.log(lines.map((l) => `    ${l}`).join('\n'));
}

// ---------------------------------------------------------------- main

function parseArgs(argv) {
  const opts = { command: null, show: false, set: null, bump: null, dir: null, verify: false, dryRun: false };
  const [command, ...rest] = argv;
  if (!command || command === '--help' || command === '-h') return { ...opts, help: true };
  opts.command = command;
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    switch (a) {
      case '--show': opts.show = true; break;
      case '--set': opts.set = rest[++i]; break;
      case '--bump': opts.bump = rest[++i]; break;
      case '--dir': opts.dir = rest[++i]; break;
      case '--verify': opts.verify = true; break;
      case '--dry-run': opts.dryRun = true; break;
      case '--help':
      case '-h': return { ...opts, help: true };
      default: die(`unknown flag: ${a} (try --help)`);
    }
  }
  return opts;
}

const isMain = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    help();
    process.exit(0);
  }
  try {
    switch (opts.command) {
      case 'version':
        cmdVersion(opts);
        break;
      case 'checksums':
        cmdChecksums(opts);
        break;
      case 'all':
        cmdVersion(opts);
        cmdChecksums(opts);
        break;
      default:
        die(`unknown command: ${opts.command} (try --help)`);
    }
  } catch (e) {
    die(e instanceof Error ? e.message : String(e));
  }
}
