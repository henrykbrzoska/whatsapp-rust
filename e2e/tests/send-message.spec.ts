import { MOCK_ORIGIN, MOCK_TOKEN, engineUrl, expect, instrumentSse, sseEvents, test } from '../fixtures';

const ALICE = '48123123123@s.whatsapp.net';

/**
 * Flow 2 — sending a message.
 *
 * The composer is not built yet (`chat.component.ts`: "The message view arrives
 * in Phase 4") and `bebok-server` has no `/send/message` route
 * (`docs/PROMPT-PHASE-4.md`, §3 of TODO.md), so this spec drives the documented
 * contract (`API-WHATSAPP.md` §3.2: `send_message(to, text)` → `MessageID`)
 * against the mock from the browser, then asserts the *client* reflects the
 * result: the engine echo arrives as a `message` event over SSE and the chat
 * list preview shows the new text after a reload. The composer assertion is a
 * `fixme` for Phase 4.
 */
test.describe('send message', () => {
  test('POST /send/message stores the message and the chat list shows it', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl());

    // Boot state: the fixtures land in the chat list.
    const aliceRow = page.locator(`li[data-jid="${ALICE}"]`);
    await expect(aliceRow).toContainText('Hello from the mock engine');

    // The send path exactly as the client would call it (Bearer token, §1.2).
    const reply = await page.evaluate(
      async ({ origin, token, to }) => {
        const res = await fetch(`${origin}/send/message`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
          body: JSON.stringify({ to, text: 'E2E: sent from Playwright' }),
        });
        return { status: res.status, body: (await res.json()) as { id?: string } };
      },
      { origin: MOCK_ORIGIN, token: MOCK_TOKEN, to: ALICE },
    );
    expect(reply.status).toBe(200);
    expect(reply.body.id, 'send returns a MessageID').toBeTruthy();

    // The engine recorded the command with the capability token.
    const sent = (await mock.requests()).find((r) => r.path === '/send/message');
    expect(sent?.authorization).toBe('Bearer e2e-token');
    expect(sent?.body).toMatchObject({ to: ALICE, text: 'E2E: sent from Playwright' });

    // The engine echoes our own message back (`message` event, §2) → SSE.
    await expect.poll(async () => (await sseEvents(page)).filter((e) => e.type === 'message').length).toBe(1);

    // Reload: `GET /chats` now reports the new last message in the preview.
    await page.goto(engineUrl());
    await expect(page.locator(`li[data-jid="${ALICE}"]`)).toContainText('E2E: sent from Playwright');
  });

  test('an empty message is rejected by the engine contract', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl());
    await expect(page.locator('.engine-status')).toHaveAttribute('data-status', 'online');

    const status = await page.evaluate(
      async ({ origin, token, to }) => {
        const res = await fetch(`${origin}/send/message`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
          body: JSON.stringify({ to, text: '' }),
        });
        return res.status;
      },
      { origin: MOCK_ORIGIN, token: MOCK_TOKEN, to: ALICE },
    );
    expect(status).toBe(400);
    expect((await mock.requests()).some((r) => r.path === '/send/message' && r.status === 400)).toBe(true);
  });

  // Blocked on the Phase 4 composer: no `sendMessage` method exists on
  // `WhatsappService` yet, so there is no UI entry point to drive.
  test.fixme('the composer sends the typed text', async ({ page }) => {
    await instrumentSse(page);
    await page.goto(engineUrl(`/chat/${encodeURIComponent(ALICE)}`));
    await page.getByTestId('composer-input').fill('typed into the composer');
    await page.getByTestId('composer-send').click();
    await expect(page.getByRole('status')).toContainText('sending');
  });
});
