import path from 'node:path';

import { defineConfig, devices } from '@playwright/test';

/**
 * Playwright config for the browser-mode E2E suite.
 *
 * Two servers, both started by Playwright (or reused when they already run):
 *   1. the mock engine (`e2e/mock-engine.mjs`, port 8788) — HTTP + SSE per
 *      `API-WHATSAPP.md`, no Rust / WhatsApp needed;
 *   2. the Angular dev server (`client`, port 4200 — the same port
 *      `scripts/bebok.mjs` and `client/src-tauri/tauri.conf.json` `devUrl` use).
 *      There is no dev-server proxy in this project: the client talks to the
 *      engine directly, the URL + capability token come from the
 *      `?engine=<BEBOK_READY …>` handshake (`tauri.service.ts`), which is exactly
 *      what the specs navigate with.
 *
 * Run: `npm --prefix e2e install && npx --prefix e2e playwright test`
 * (or `npm run e2e` from the repo root). Ports are overridable:
 * E2E_CLIENT_PORT, E2E_MOCK_PORT, E2E_MOCK_TOKEN.
 */

const CLIENT_PORT = Number(process.env.E2E_CLIENT_PORT ?? 4200);
const MOCK_PORT = Number(process.env.E2E_MOCK_PORT ?? 8788);
const MOCK_TOKEN = process.env.E2E_MOCK_TOKEN ?? 'e2e-token';
const isCi = Boolean(process.env.CI);

export default defineConfig({
  testDir: path.join(import.meta.dirname, 'tests'),
  outputDir: path.join(import.meta.dirname, 'test-results'),
  // The mock engine keeps per-test state, so the suite runs serially.
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  retries: isCi ? 1 : 0,
  forbidOnly: isCi,
  reporter: isCi ? [['list'], ['html', { open: 'never', outputFolder: path.join(import.meta.dirname, 'playwright-report') }]] : [['list']],
  use: {
    baseURL: `http://localhost:${CLIENT_PORT}`,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'off',
    actionTimeout: 15_000,
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  webServer: [
    {
      command: `node mock-engine.mjs --port ${MOCK_PORT} --token ${MOCK_TOKEN}`,
      url: `http://127.0.0.1:${MOCK_PORT}/`,
      reuseExistingServer: !isCi,
      timeout: 30_000,
      stdout: 'pipe',
      stderr: 'pipe',
    },
    {
      command: `npm --prefix client start -- --port ${CLIENT_PORT}`,
      cwd: path.resolve(import.meta.dirname, '..'),
      url: `http://localhost:${CLIENT_PORT}/`,
      reuseExistingServer: !isCi,
      timeout: 180_000,
      stdout: 'pipe',
      stderr: 'pipe',
    },
  ],
});
