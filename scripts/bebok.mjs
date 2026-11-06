#!/usr/bin/env node
// Developer entry point (run from the repo root via `npm run <cmd>`):
//
//   doctor          check rustc/cargo, node/npm, tauri cli, OS deps
//   engine          cargo run the engine (prints BEBOK_READY)
//   client          ng serve the client
//   full-build-dev  engine + ng serve, prints/opens http://localhost:<client-port>/?engine=<BEBOK_READY>
//   full-build-app  engine release + sidecar copy + Tauri bundles + SHA256SUMS.txt
//
// Flags: --port N (engine, default 8787), --client-port N (default 4200),
//        --no-auth, --diagnostic, --open, --tauri,
//        --bundles a,b (full-build-app), --skip-engine, --skip-tauri.

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

import { copySidecar } from './copy-sidecar.mjs';
import { checksumLines } from './release.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const engineDir = join(root, 'engine');
const clientDir = join(root, 'client');
const tauriDir = join(clientDir, 'src-tauri');
const isWin = process.platform === 'win32';
const children = new Set();

// ---------------------------------------------------------------- helpers

const USAGE = `usage: node scripts/bebok.mjs <doctor|engine|client|full-build-dev|full-build-app> [flags]

  doctor          check rustc/cargo, node/npm, tauri cli, OS deps
  engine          cargo run the engine (prints BEBOK_READY)
  client          ng serve the client
  full-build-dev  engine + ng serve, prints/opens http://localhost:<client-port>/?engine=<BEBOK_READY>
  full-build-app  engine release + sidecar copy + Tauri bundles + SHA256SUMS.txt

flags
  --port N          engine port (default 8787, 0 = random)
  --client-port N   ng serve port (default 4200)
  --no-auth         start the engine without the capability token
  --diagnostic      enable the engine event log (BEBOK_DIAGNOSTIC)
  --open            open the browser on full-build-dev
  --tauri           run \`tauri dev\` instead of the browser pair
  --bundles a,b     full-build-app: bundle targets passed to \`tauri build\`
  --skip-engine     full-build-app: reuse the existing engine binary
  --skip-tauri      full-build-app: build the engine only (no bundles)
  --help            this text

Release version stamping + checksum verification: node scripts/release.mjs --help`;

function parseFlags(argv) {
  const f = { port: 8787, clientPort: 4200, noAuth: false, diagnostic: false, open: false, tauri: false, bundles: null, skipEngine: false, skipTauri: false, help: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const num = (name) => {
      const v = Number(argv[++i]);
      if (!Number.isInteger(v) || v < 0 || v > 65535) die(`${name} needs a port number`);
      return v;
    };
    switch (a) {
      case '--port': f.port = num('--port'); break;
      case '--client-port': f.clientPort = num('--client-port'); break;
      case '--no-auth': f.noAuth = true; break;
      case '--diagnostic': f.diagnostic = true; break;
      case '--open': f.open = true; break;
      case '--tauri': f.tauri = true; break;
      case '--bundles': f.bundles = argv[++i]; break;
      case '--skip-engine': f.skipEngine = true; break;
      case '--skip-tauri': f.skipTauri = true; break;
      case '--help':
      case '-h': f.help = true; break;
      default: die(`unknown flag: ${a} (try --help)`);
    }
  }
  return f;
}

function die(msg) {
  console.error(`bebok: ${msg}`);
  killAll();
  process.exit(1);
}

function log(msg) {
  console.error(`\x1b[32m[bebok]\x1b[0m ${msg}`);
}

/** Runs a command to completion with inherited stdio; dies on failure. */
function run(cmd, args, opts = {}) {
  log(`$ ${cmd} ${args.join(' ')}${opts.cwd ? `   (in ${relative(root, opts.cwd) || '.'})` : ''}`);
  const r = spawnSync(cmd, args, { stdio: 'inherit', shell: isWin, ...opts });
  if (r.status !== 0) die(`${cmd} ${args.join(' ')} failed (exit ${r.status ?? r.signal})`);
}

function start(cmd, args, opts = {}) {
  const child = spawn(cmd, args, { shell: isWin, ...opts });
  children.add(child);
  child.on('exit', () => children.delete(child));
  return child;
}

function killAll() {
  for (const c of children) {
    try {
      if (isWin) spawnSync('taskkill', ['/pid', String(c.pid), '/T', '/F']);
      else c.kill('SIGTERM');
    } catch {
      /* already gone */
    }
  }
}

for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => {
    killAll();
    process.exit(130);
  });
}

function which(cmd) {
  const r = spawnSync(isWin ? 'where' : 'which', [cmd], { encoding: 'utf8' });
  return r.status === 0 ? r.stdout.split(/\r?\n/)[0].trim() : null;
}

function version(cmd, args = ['--version']) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', shell: isWin });
  return r.status === 0 ? (r.stdout || r.stderr).trim().split('\n')[0] : null;
}

function engineArgs(f, port) {
  const args = ['run', '-p', 'bebok-server', '--', '--port', String(port)];
  if (f.noAuth) args.push('--no-auth');
  if (f.diagnostic) args.push('--diagnostic');
  return args;
}

function ensureClientDeps() {
  if (!existsSync(join(clientDir, 'node_modules'))) run('npm', ['install'], { cwd: clientDir });
}

function openUrl(url) {
  const [cmd, args] =
    process.platform === 'darwin' ? ['open', [url]] : isWin ? ['cmd', ['/c', 'start', '', url]] : ['xdg-open', [url]];
  spawn(cmd, args, { stdio: 'ignore', detached: true }).unref();
}

/** Starts the engine and resolves with its BEBOK_READY line. */
function startEngine(f) {
  log('starting engine…');
  const child = start('cargo', engineArgs(f, f.port), { cwd: engineDir, stdio: ['ignore', 'pipe', 'inherit'] });
  return new Promise((resolveReady, reject) => {
    const rl = createInterface({ input: child.stdout });
    rl.on('line', (line) => {
      if (line.startsWith('BEBOK_READY ')) resolveReady(line.trim());
      else process.stdout.write(`${line}\n`);
    });
    child.on('exit', (code) => reject(new Error(`engine exited (code ${code}) before BEBOK_READY`)));
  });
}

function waitForPort(port, timeoutMs = 180_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((ok, fail) => {
    const tick = async () => {
      try {
        const r = await fetch(`http://localhost:${port}/`);
        if (r.status < 500) return ok();
      } catch {
        /* not up yet */
      }
      if (Date.now() > deadline) return fail(new Error(`nothing answered on :${port} after ${timeoutMs / 1000}s`));
      setTimeout(tick, 500);
    };
    tick();
  });
}

function ensureIcons() {
  if (existsSync(join(tauriDir, 'icons', 'icon.ico')) && existsSync(join(tauriDir, 'icons', 'icon.icns'))) return;
  log('generating Tauri icons from src-tauri/icons/icon.svg');
  run('npx', ['tauri', 'icon', 'src-tauri/icons/icon.svg', '-o', 'src-tauri/icons'], { cwd: clientDir });
}

// ---------------------------------------------------------------- commands

function doctor() {
  let ok = true;
  const check = (label, value, required = true, hint = '') => {
    const pass = Boolean(value);
    if (!pass && required) ok = false;
    const mark = pass ? '\x1b[32m✓\x1b[0m' : required ? '\x1b[31m✗\x1b[0m' : '\x1b[33m!\x1b[0m';
    console.log(`${mark} ${label.padEnd(22)} ${value || hint}`);
  };

  const node = process.versions.node;
  check('node >= 20', Number(node.split('.')[0]) >= 20 ? `v${node}` : null, true, `v${node} is too old`);
  check('npm', version('npm', ['-v']));
  check('rustc >= 1.88', version('rustc'), true, 'install via https://rustup.rs');
  check('cargo', version('cargo'));
  check('client node_modules', existsSync(join(clientDir, 'node_modules')) ? 'installed' : null, false, 'run: cd client && npm install');
  const tauriBin = join(clientDir, 'node_modules', '.bin', isWin ? 'tauri.cmd' : 'tauri');
  check('tauri cli', existsSync(tauriBin) ? version(tauriBin, ['--version']) : null, false, 'comes with client npm install');

  if (process.platform === 'linux') {
    const pkgConfig = which('pkg-config');
    check('pkg-config', pkgConfig, false, 'needed to detect Tauri system libraries');
    if (pkgConfig) {
      for (const lib of ['webkit2gtk-4.1', 'libsoup-3.0', 'gtk+-3.0', 'librsvg-2.0']) {
        const r = spawnSync('pkg-config', ['--modversion', lib], { encoding: 'utf8' });
        check(lib, r.status === 0 ? r.stdout.trim() : null, false, 'missing — only needed for Tauri builds');
      }
    }
  } else if (process.platform === 'darwin') {
    check('xcode-select', version('xcode-select', ['-p']), false, 'run: xcode-select --install');
  } else if (isWin) {
    check('cl.exe (MSVC)', which('cl'), false, 'install Visual Studio Build Tools (or run from a dev prompt)');
  }

  const sidecar = existsSync(join(tauriDir, 'binaries'))
    ? readdirSync(join(tauriDir, 'binaries')).find((n) => n.startsWith('bebok-server-'))
    : null;
  check('sidecar binary', sidecar, false, 'run: npm run full-build-app (or cargo build --release + npm run sidecar:copy)');
  console.log(ok ? '\nAll required tools found.' : '\nSome required tools are missing.');
  process.exit(ok ? 0 : 1);
}

function engine(f) {
  run('cargo', engineArgs(f, f.port), { cwd: engineDir });
}

function client(f) {
  ensureClientDeps();
  run('npx', ['ng', 'serve', '--port', String(f.clientPort)], { cwd: clientDir });
}

async function fullBuildDev(f) {
  if (f.tauri) {
    ensureClientDeps();
    run('cargo', ['build', '-p', 'bebok-server'], { cwd: engineDir });
    const { dest } = copySidecar({ profile: 'debug' });
    log(`sidecar → ${relative(root, dest)}`);
    ensureIcons();
    run('npm', ['run', 'tauri:dev'], { cwd: clientDir });
    return;
  }

  ensureClientDeps();
  let ready;
  try {
    ready = await startEngine(f);
  } catch (e) {
    die(e.message);
  }
  log(ready.replace(/token=[^&\s]+/, 'token=<redacted>'));

  log(`starting ng serve on :${f.clientPort}…`);
  const ng = start('npx', ['ng', 'serve', '--port', String(f.clientPort)], { cwd: clientDir, stdio: 'inherit' });
  ng.on('exit', (code) => die(`ng serve exited (code ${code})`));
  await waitForPort(f.clientPort).catch((e) => die(e.message));

  const url = `http://localhost:${f.clientPort}/?engine=${encodeURIComponent(ready)}`;
  log(`client ready — open:\n\n    ${url}\n`);
  if (f.open) openUrl(url);
  log('Ctrl+C stops engine and client.');
}

function fullBuildApp(f) {
  if (!f.skipEngine) {
    run('cargo', ['build', '--release', '-p', 'bebok-server'], { cwd: engineDir });
    const { dest } = copySidecar({ profile: 'release' });
    log(`sidecar → ${relative(root, dest)}`);
  }
  if (f.skipTauri) return;
  ensureClientDeps();
  ensureIcons();
  const args = ['run', 'tauri:build'];
  if (f.bundles) args.push('--', '--bundles', f.bundles);
  run('npm', args, { cwd: clientDir });

  const bundleDir = join(tauriDir, 'target', 'release', 'bundle');
  // Same walker + hashing as `node scripts/release.mjs checksums`, so a rebuild
  // and a manual re-checksum always agree.
  const lines = checksumLines(bundleDir);
  if (!lines.length) {
    log(`no bundles found under ${relative(root, bundleDir)}`);
    return;
  }
  const sums = join(bundleDir, 'SHA256SUMS.txt');
  writeFileSync(sums, `${lines.join('\n')}\n`);
  log(`bundles:\n${lines.map((l) => `    ${l}`).join('\n')}\n  → ${relative(root, sums)}`);
}

// ---------------------------------------------------------------- main

const [cmd, ...rest] = process.argv.slice(2);
const flags = parseFlags(rest);
if (flags.help || !cmd || cmd === '--help' || cmd === '-h' || cmd === 'help') {
  console.log(USAGE);
  process.exit(0);
}
switch (cmd) {
  case 'doctor': doctor(); break;
  case 'engine': engine(flags); break;
  case 'client': client(flags); break;
  case 'full-build-dev': await fullBuildDev(flags); break;
  case 'full-build-app': fullBuildApp(flags); break;
  default:
    console.error(USAGE);
    process.exit(1);
}
