import { engineUrl, expect, instrumentSse, sseEvents, test } from '../fixtures';

/**
 * Flow 1 — pairing/QR.
 *
 * Transport half (top three specs): the `?engine=<BEBOK_READY>` handshake,
 * `GET /version` with the capability token, and `WaEvent::Qr` delivered over
 * SSE. UI half (bottom four): the pairing screen renders the active code,
 * advances the rotation, and only claims "paired" once the device is linked.
 *
 * The path a spec has to walk (mirrors the manual run):
 *   `/?engine=<BEBOK_READY …>` → nav "Login" → `/login`
 *   → click `[data-testid=connect]` (POST /connect)
 *   → engine publishes `qr { codes: [...] }` over SSE
 *   → `[data-testid=qr-code]` (image) + `[data-testid=qr-raw]` (payload)
 *   → `[data-testid=qr-status]` counts down and advances the code
 *   → `pair_success` swaps the screen for `[data-testid=paired]`.
 */
test.describe('QR / pairing', () => {
  test('pairs with the engine from the BEBOK_READY handshake', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl());

    const status = page.locator('.engine-status');
    await expect(status).toHaveAttribute('data-status', 'online');
    await expect(status).toContainText('Engine 0.1.0');

    // The capability token travels as `Authorization: Bearer …` (§1.2).
    const requests = await mock.requests();
    const version = requests.find((r) => r.path === '/version');
    expect(version, 'GET /version was requested').toBeTruthy();
    expect(version?.authorization).toBe('Bearer e2e-token');
    expect(version?.status).toBe(200);
  });

  test('the token is enforced, and the client surfaces a bad one', async ({ page }) => {
    await instrumentSse(page);
    await page.goto(engineUrl('/', 'definitely-not-the-token'));

    const status = page.locator('.engine-status');
    await expect(status).toHaveAttribute('data-status', 'unauthorized');
    await expect(page.getByRole('alert')).toContainText('rejected the token');
  });

  test('QR codes reach the client over SSE', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl());
    await expect(page.locator('.engine-status')).toHaveAttribute('data-status', 'online');

    // The client subscribes to `GET /events` right after a successful ping.
    await expect.poll(() => page.evaluate(() => (window as unknown as { __sseOpen: boolean }).__sseOpen)).toBe(true);
    await expect.poll(async () => (await mock.state()).sse).toBe(1);

    // `POST /connect` is what the engine answers with `QR { codes }` (§2.1).
    await page.evaluate(async () => {
      const endpoint = JSON.parse(window.localStorage.getItem('bebok.engine') ?? '{}') as { url: string; token: string | null };
      await fetch(`${endpoint.url}/connect`, {
        method: 'POST',
        headers: endpoint.token ? { Authorization: `Bearer ${endpoint.token}` } : {},
      });
    });

    await expect.poll(async () => (await sseEvents(page)).filter((e) => e.type === 'qr').length).toBe(1);
    const qr = (await sseEvents(page)).find((e) => e.type === 'qr');
    expect(qr?.codes).toEqual(['2@MOCK-QR-FIRST,AA,BB', '2@MOCK-QR-SECOND,CC,DD']);

    // And an explicitly published event arrives too (engine → SSE → client).
    await mock.emit({ type: 'qr', codes: ['2@SPEC-EMITTED,EE,FF'] });
    await expect.poll(async () => (await sseEvents(page)).filter((e) => e.type === 'qr').length).toBe(2);
  });

  // The pairing screen (`features/auth/login.component.ts`) is the only place
  // that renders `WhatsappService.qrCodes`. Everything below goes through the
  // UI, no `page.evaluate` shortcuts.
  test('renders the active QR code and advances the rotation', async ({ page }) => {
    await instrumentSse(page);
    await page.goto(engineUrl('/login'));

    await expect(page.locator('[data-testid="pair-status"]')).toContainText('Engine: online');

    // 1. Connect from the screen (POST /connect → engine publishes `qr`).
    await page.locator('[data-testid="connect"]').click();

    // 2. Exactly one QR is on screen — the rotation is a countdown, not a list.
    const code = page.locator('[data-testid="qr-code"]');
    await expect(code).toHaveCount(1);
    await expect(code).toBeVisible();
    await expect(code).toHaveAttribute('alt', 'WhatsApp QR code');

    // 3. The engine sends the whole rotation in one event; the first code shows.
    await expect(page.locator('[data-testid="qr-raw"]')).toHaveText('2@MOCK-QR-FIRST,AA,BB');
    await expect(page.locator('[data-testid="qr-status"]')).toContainText('Code 1 of 2');

    // 4. The label counts down and the screen advances on its own (20 s per
    //    code, `QR_CHANNEL_TIMEOUT`); no click needed between rotations.
    await expect(page.locator('[data-testid="qr-raw"]')).toHaveText('2@MOCK-QR-SECOND,CC,DD', { timeout: 30_000 });
    await expect(page.locator('[data-testid="qr-status"]')).toContainText('Code 2 of 2');
    await expect(code).toHaveCount(1);
  });

  test('a new `qr` event restarts the rotation at code 1', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl('/login'));
    await page.locator('[data-testid="connect"]').click();
    await expect(page.locator('[data-testid="qr-raw"]')).toHaveText('2@MOCK-QR-FIRST,AA,BB');

    await mock.emit({ type: 'qr', codes: ['2@RENDERED,GG,HH'] });

    await expect(page.locator('[data-testid="qr-raw"]')).toHaveText('2@RENDERED,GG,HH');
    await expect(page.locator('[data-testid="qr-status"]')).toContainText('Code 1 of 1');
    await expect(page.locator('[data-testid="qr-code"]')).toHaveCount(1);
  });

  test('shows "paired" only after pairing, never on a bare socket', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl('/login'));
    await page.locator('[data-testid="connect"]').click();

    // Connected socket + unpaired device: the QR stays, the "paired" banner
    // must not appear (`/status` reports connected=true, loggedIn=false).
    await expect(page.locator('[data-testid="qr-code"]')).toBeVisible();
    await expect(page.locator('[data-testid="paired"]')).toHaveCount(0);

    // Finishing the pairing publishes `pair_success`, exactly like the engine.
    await mock.pair();

    await expect(page.locator('[data-testid="paired"]')).toBeVisible();
    await expect(page.locator('[data-testid="qr-code"]')).toHaveCount(0);
    await expect(page.locator('[data-testid="pair-status"]')).toHaveCount(0);
  });

  test('the QR image is a 280px QR of the raw payload', async ({ page }) => {
    await instrumentSse(page);
    await page.goto(engineUrl('/login'));
    await page.locator('[data-testid="connect"]').click();

    const code = page.locator('[data-testid="qr-code"]');
    await expect(code).toBeVisible();
    const raw = await page.locator('[data-testid="qr-raw"]').textContent();
    // The image is generated from exactly the string the engine sent (a real
    // engine sends `https://wa.me/settings/linked_devices#…`).
    await expect(code).toHaveAttribute('src', `https://api.qrserver.com/v1/create-qr-code/?size=280x280&data=${encodeURIComponent(raw ?? '')}`);
    await expect(code).toHaveAttribute('width', '280');
    await expect(code).toHaveAttribute('height', '280');
  });

});
