import { test as base, expect, type Page } from '@playwright/test';

/**
 * Shared E2E scaffolding: the mock engine's address, the client URL that
 * performs the `?engine=<BEBOK_READY>` handshake, and a `mock` fixture that
 * drives the mock engine's test-control API from the Node side.
 */

export const MOCK_PORT = Number(process.env.E2E_MOCK_PORT ?? 8788);
export const MOCK_TOKEN = process.env.E2E_MOCK_TOKEN ?? 'e2e-token';
export const MOCK_ORIGIN = `http://127.0.0.1:${MOCK_PORT}`;

/** The line the real engine prints on stdout (`BEBOK_READY <url>?token=…`). */
export function readyLine(token: string = MOCK_TOKEN): string {
  return `BEBOK_READY ${MOCK_ORIGIN}/?token=${token}`;
}

/**
 * Client URL that pairs with the mock engine (the app strips the parameter, so
 * every full page load needs it again — the specs therefore always navigate
 * through this helper, never to a bare route).
 */
export function engineUrl(path = '/', token: string = MOCK_TOKEN): string {
  const sep = path.includes('?') ? '&' : '?';
  return `${path}${sep}engine=${encodeURIComponent(readyLine(token))}`;
}

export interface MockRequest {
  method: string;
  path: string;
  query: string | null;
  authorization: string | null;
  body: Record<string, unknown> | null;
  status: number;
}

export interface MockApi {
  /** Drops recorded requests + stored messages, back to the fixtures. */
  reset(): Promise<void>;
  requests(): Promise<MockRequest[]>;
  state(): Promise<{ sse: number; connected: boolean; requests: number; chats: { jid: string; last_text: string | null }[] }>;
  /** Publishes one `WaEvent` (API-WHATSAPP.md §2) to every SSE subscriber. */
  emit(event: Record<string, unknown>): Promise<void>;
  /** Marks the (mock) device as paired and publishes `pair_success`. */
  pair(): Promise<void>;
}

/**
 * Records every `EventSource` message in `window.__sseMessages` and whether the
 * stream opened, so a spec can assert what the client received over SSE even
 * when no component renders it yet. Also clears `localStorage` so each test
 * starts from the `?engine=` handshake instead of a previous endpoint.
 */
export async function instrumentSse(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const w = window as unknown as {
      __sseMessages: unknown[];
      __sseOpen: boolean;
      EventSource: typeof EventSource;
    };
    w.__sseMessages = [];
    w.__sseOpen = false;
    try {
      window.localStorage.clear();
    } catch {
      /* storage disabled */
    }
    const Original = w.EventSource;
    w.EventSource = class extends Original {
      constructor(url: string | URL, init?: EventSourceInit) {
        super(url, init);
        this.addEventListener('open', () => {
          w.__sseOpen = true;
        });
        this.addEventListener('message', (ev: MessageEvent) => {
          try {
            w.__sseMessages.push(JSON.parse(String(ev.data)));
          } catch {
            w.__sseMessages.push({ raw: String(ev.data) });
          }
        });
      }
    } as typeof EventSource;
  });
}

/** WaEvents the page received over SSE, newest last. */
export function sseEvents(page: Page): Promise<{ type: string; [k: string]: unknown }[]> {
  return page.evaluate(() => {
    const w = window as unknown as { __sseMessages: { type: string }[] };
    return w.__sseMessages as { type: string; [k: string]: unknown }[];
  });
}

export const test = base.extend<{ mock: MockApi }>({
  // `auto`: the mock engine keeps state between tests (connected / paired /
  // published messages), so every test starts from the fixtures — a pairing test
  // would otherwise leave the client "linked" for the next one.
  mock: [
    async ({ request }, use) => {
      const api: MockApi = {
        async reset() {
          const res = await request.post(`${MOCK_ORIGIN}/__mock/reset`);
          expect(res.ok(), 'mock engine reset').toBeTruthy();
        },
        async requests() {
          const res = await request.get(`${MOCK_ORIGIN}/__mock/requests`);
          expect(res.ok(), 'mock engine request log').toBeTruthy();
          return ((await res.json()) as { requests: MockRequest[] }).requests;
        },
        async state() {
          const res = await request.get(`${MOCK_ORIGIN}/__mock/state`);
          expect(res.ok(), 'mock engine state').toBeTruthy();
          return (await res.json()) as Awaited<ReturnType<MockApi['state']>>;
        },
        async emit(event) {
          const res = await request.post(`${MOCK_ORIGIN}/__mock/emit`, { data: event });
          expect(res.ok(), 'mock engine emit').toBeTruthy();
        },
        async pair() {
          const res = await request.post(`${MOCK_ORIGIN}/__mock/pair`);
          expect(res.ok(), 'mock engine pair').toBeTruthy();
        },
      };
      await api.reset();
      await use(api);
    },
    { auto: true },
  ],
});

export { expect };
