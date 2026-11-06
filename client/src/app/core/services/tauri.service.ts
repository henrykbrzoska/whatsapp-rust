import { Injectable, InjectionToken, computed, inject, signal } from '@angular/core';

/**
 * Transport to the engine, identical API in both run modes:
 *
 * * **Browser** (`ng serve`): the engine URL + token come from the
 *   `?engine=<url-encoded BEBOK_READY line>` handshake (or Settings) and are
 *   kept in `localStorage`; commands go over `fetch` with
 *   `Authorization: Bearer`, `wa-event` arrives over SSE (`GET /events`).
 * * **Tauri**: the shell spawns the engine as a sidecar and exposes the
 *   BEBOK_READY line through the native `engine_ready` command. Engine
 *   commands still travel over HTTP to the sidecar (they move to native IPC
 *   in Phase 3); shell events (`engine-exit`, …) use native `listen`.
 */

export interface EngineEndpoint {
  /** Origin of the engine, e.g. `http://127.0.0.1:8787` (no trailing slash). */
  url: string;
  token: string | null;
}

export interface TauriBridge {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  listen<T>(event: string, handler: (payload: T) => void): Promise<() => void>;
}

export interface EventSourceLike {
  onmessage: ((ev: MessageEvent) => void) | null;
  onopen: ((ev: Event) => void) | null;
  onerror: ((ev: Event) => void) | null;
  close(): void;
}

export interface PlatformDeps {
  fetch: (input: string, init?: RequestInit) => Promise<Response>;
  createEventSource: ((url: string) => EventSourceLike) | null;
  storage: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null;
  location: { readonly href: string };
  history: { replaceState(data: unknown, unused: string, url?: string): void; readonly state: unknown };
  /** `null` outside Tauri. */
  tauri: (() => Promise<TauriBridge>) | null;
}

async function loadTauriBridge(): Promise<TauriBridge> {
  const [core, event] = await Promise.all([import('@tauri-apps/api/core'), import('@tauri-apps/api/event')]);
  return {
    invoke: <T>(cmd: string, args?: Record<string, unknown>) => core.invoke<T>(cmd, args),
    listen: <T>(name: string, handler: (payload: T) => void) => event.listen<T>(name, (e) => handler(e.payload)),
  };
}

function defaultDeps(): PlatformDeps {
  const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
  let storage: PlatformDeps['storage'] = null;
  try {
    storage = window.localStorage;
  } catch {
    storage = null;
  }
  return {
    fetch: (input, init) => window.fetch(input, init),
    createEventSource: typeof EventSource === 'undefined' ? null : (url) => new EventSource(url),
    storage,
    location: window.location,
    history: window.history,
    tauri: isTauri ? loadTauriBridge : null,
  };
}

export const PLATFORM_DEPS = new InjectionToken<PlatformDeps>('PLATFORM_DEPS', {
  providedIn: 'root',
  factory: defaultDeps,
});

export const STORAGE_KEY = 'bebok.engine';
export const ENGINE_PARAM = 'engine';
/** Event name of the engine's `WaEvent` stream (Tauri event / SSE). */
export const WA_EVENT = 'wa-event';

/** Commands implemented by the Tauri shell itself (not by the engine). */
export const NATIVE_COMMANDS: ReadonlySet<string> = new Set(['engine_ready', 'export_chat_history']);

/** `GET /chats/:jid/history/export` — the HTTP side of `export_chat_history`. */
export const exportHistoryPath = (chat: string) => `/chats/${encodeURIComponent(chat)}/history/export`;

export class EngineError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = 'EngineError';
  }
}

type Args = Record<string, unknown>;
type Query = Record<string, string | number | boolean | null | undefined>;

interface CommandRoute {
  method: 'GET' | 'POST' | 'DELETE';
  path: (a: Args) => string;
  query?: (a: Args) => Query;
  /** JSON request body (POST commands). */
  body?: (a: Args) => unknown;
}

const chatPath = (a: Args) => `/chats/${encodeURIComponent(String(a['chat']))}`;

/** Command name (Tauri IPC contract, `API-WHATSAPP.md`) → HTTP route. */
export const COMMAND_ROUTES: Readonly<Record<string, CommandRoute>> = {
  version: { method: 'GET', path: () => '/version' },
  get_config: { method: 'GET', path: () => '/config' },
  is_connected: { method: 'GET', path: () => '/connected' },
  is_logged_in: { method: 'GET', path: () => '/logged_in' },
  status: { method: 'GET', path: () => '/status' },
  connect: { method: 'POST', path: () => '/connect' },
  disconnect: { method: 'POST', path: () => '/disconnect' },
  logout: { method: 'POST', path: () => '/logout' },
  list_chats: { method: 'GET', path: () => '/chats' },
  search_messages: {
    method: 'GET',
    path: () => '/search',
    query: (a) => ({ q: a['query'] as string, chat: a['chat'] as string, limit: a['limit'] as number, mode: a['mode'] as string }),
  },
  get_chat_stats: { method: 'GET', path: (a) => `${chatPath(a)}/stats` },
  list_messages: {
    method: 'GET',
    path: (a) => `${chatPath(a)}/messages`,
    query: (a) => ({ before: a['before'] as number, beforeId: a['beforeId'] as string, limit: a['limit'] as number }),
  },
  send_message: { method: 'POST', path: () => '/send/message', body: (a) => ({ to: a['to'], text: a['text'] }) },
  mark_read: {
    method: 'POST',
    path: () => '/receipt/read',
    body: (a) => ({ chat: a['chat'], ids: a['ids'], sender: a['sender'] ?? undefined }),
  },
  delete_chat_messages: { method: 'DELETE', path: (a) => `${chatPath(a)}/messages` },
  fetch_history: {
    method: 'POST',
    path: () => '/history/fetch',
    body: (a) => ({ chat: a['chat'] ?? undefined, count: a['count'] ?? undefined }),
  },
  media_info: { method: 'GET', path: (a) => `/media/${encodeURIComponent(String(a['id']))}/info` },
};

/**
 * Parses a `BEBOK_READY http://127.0.0.1:8787/?token=…` line (the prefix is
 * optional, so a bare URL works too). Returns `null` for anything that is not
 * an http(s) URL.
 */
export function parseReadyLine(input: string | null | undefined): EngineEndpoint | null {
  if (!input) return null;
  const candidate = input
    .trim()
    .replace(/^BEBOK_READY\s+/i, '')
    .split(/\s+/)[0];
  if (!candidate) return null;
  let url: URL;
  try {
    url = new URL(candidate);
  } catch {
    return null;
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;
  return { url: `${url.protocol}//${url.host}`, token: url.searchParams.get('token') || null };
}

function buildQuery(query: Query | undefined): string {
  if (!query) return '';
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined && v !== null && v !== '') params.set(k, String(v));
  }
  const s = params.toString();
  return s ? `?${s}` : '';
}

export type SseState = 'idle' | 'connecting' | 'open' | 'reconnecting';

@Injectable({ providedIn: 'root' })
export class TauriService {
  private readonly deps = inject(PLATFORM_DEPS);
  private bridge: Promise<TauriBridge> | null = null;
  private endpointRequest: Promise<EngineEndpoint> | null = null;

  /** Running inside the Tauri WebView. */
  readonly isTauri = this.deps.tauri !== null;
  readonly endpoint = signal<EngineEndpoint | null>(null);
  readonly configured = computed(() => this.endpoint() !== null);
  readonly sseState = signal<SseState>('idle');

  constructor() {
    this.endpoint.set(this.loadStored());
    this.consumeEngineParam();
  }

  /**
   * `?engine=<BEBOK_READY>` handshake: store URL + token like a manual
   * Connect, then strip the parameter from the address bar so the token does
   * not linger in history or screenshots.
   */
  private consumeEngineParam(): void {
    let url: URL;
    try {
      url = new URL(this.deps.location.href);
    } catch {
      return;
    }
    if (!url.searchParams.has(ENGINE_PARAM)) return;
    const parsed = parseReadyLine(url.searchParams.get(ENGINE_PARAM));
    if (parsed) this.setEndpoint(parsed);
    url.searchParams.delete(ENGINE_PARAM);
    this.deps.history.replaceState(this.deps.history.state, '', `${url.pathname}${url.search}${url.hash}`);
  }

  private loadStored(): EngineEndpoint | null {
    const raw = this.deps.storage?.getItem(STORAGE_KEY);
    if (!raw) return null;
    try {
      const v = JSON.parse(raw) as Partial<EngineEndpoint>;
      return typeof v.url === 'string' ? { url: v.url, token: typeof v.token === 'string' ? v.token : null } : null;
    } catch {
      return null;
    }
  }

  setEndpoint(endpoint: EngineEndpoint | null): void {
    this.endpoint.set(endpoint);
    if (this.isTauri) return; // the sidecar URL changes every launch — never persist it
    if (endpoint) this.deps.storage?.setItem(STORAGE_KEY, JSON.stringify(endpoint));
    else this.deps.storage?.removeItem(STORAGE_KEY);
  }

  /** Settings → "Connect": accepts a BEBOK_READY line or a bare URL. */
  connectWith(readyLine: string): EngineEndpoint {
    const parsed = parseReadyLine(readyLine);
    if (!parsed) throw new Error('Not a valid engine URL — paste the BEBOK_READY line printed by the engine.');
    this.setEndpoint(parsed);
    return parsed;
  }

  private tauri(): Promise<TauriBridge> {
    if (!this.deps.tauri) return Promise.reject(new Error('Not running inside Tauri'));
    this.bridge ??= this.deps.tauri();
    return this.bridge;
  }

  /** Resolves the engine endpoint, asking the Tauri shell for it if needed. */
  ready(): Promise<EngineEndpoint> {
    const current = this.endpoint();
    if (current) return Promise.resolve(current);
    if (!this.isTauri) {
      return Promise.reject(new EngineError(0, 'Engine not configured — paste the BEBOK_READY URL in Settings.'));
    }
    this.endpointRequest ??= this.tauri()
      .then((t) => t.invoke<string>('engine_ready'))
      .then((line) => {
        const parsed = parseReadyLine(line);
        if (!parsed) throw new EngineError(0, `Unexpected engine_ready reply: ${line}`);
        this.setEndpoint(parsed);
        return parsed;
      })
      .finally(() => (this.endpointRequest = null));
    return this.endpointRequest;
  }

  /**
   * `GET /media/{id}` as a URL an `<img>`, `<video>` or `<audio>` element can
   * load directly.
   *
   * Those elements cannot send an `Authorization` header, so the capability
   * token travels as `?token=` — the engine accepts both. Returns `''` while
   * no endpoint is known yet, which keeps the element from firing a request at
   * the page's own origin.
   */
  mediaUrl(messageId: string): string {
    const ep = this.endpoint();
    if (!ep) return '';
    const query = ep.token ? `?token=${encodeURIComponent(ep.token)}` : '';
    return `${ep.url}/media/${encodeURIComponent(messageId)}${query}`;
  }

  /** Raw HTTP call to the engine. Throws `EngineError` on non-2xx. */
  async request(
    method: string,
    path: string,
    opts: { query?: Query; body?: unknown; signal?: AbortSignal } = {},
  ): Promise<Response> {
    const ep = await this.ready();
    const headers: Record<string, string> = {};
    if (ep.token) headers['Authorization'] = `Bearer ${ep.token}`;
    let body: string | undefined;
    if (opts.body !== undefined) {
      headers['Content-Type'] = 'application/json';
      body = JSON.stringify(opts.body);
    }
    let res: Response;
    try {
      res = await this.deps.fetch(`${ep.url}${path}${buildQuery(opts.query)}`, { method, headers, body, signal: opts.signal });
    } catch (e) {
      throw new EngineError(0, `Engine unreachable at ${ep.url} (${e instanceof Error ? e.message : String(e)})`);
    }
    if (!res.ok) {
      let message = `${res.status} ${res.statusText}`.trim();
      let explained = false;
      try {
        const err = (await res.json()) as { error?: string };
        if (err?.error) {
          message = err.error;
          explained = true;
        }
      } catch {
        /* non-JSON error body */
      }
      // The engine answers its own 404s with `{"error": …}`. A bare 404 means
      // the route does not exist in the running build — almost always a
      // client that is newer than the engine next to it.
      if (!explained && res.status === 404) {
        message =
          `This engine build has no ${path} endpoint. Restart the engine ` +
          `(npm run full-build-dev) and paste the new BEBOK_READY link.`;
      }
      throw new EngineError(res.status, message);
    }
    return res;
  }

  /**
   * Invoke a command from the IPC contract. Shell commands go to Tauri; engine
   * commands are routed over HTTP via `COMMAND_ROUTES`.
   */
  async invoke<T>(cmd: string, args: Args = {}): Promise<T> {
    if (NATIVE_COMMANDS.has(cmd)) return (await this.tauri()).invoke<T>(cmd, args);
    const route = COMMAND_ROUTES[cmd];
    if (!route) throw new Error(`Unknown command: ${cmd}`);
    const res = await this.request(route.method, route.path(args), {
      query: route.query?.(args),
      body: route.body?.(args),
    });
    const text = await res.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }

  /**
   * Subscribe to an event. `wa-event` comes from the engine (SSE in both
   * modes); other names are Tauri shell events (no-op in the browser).
   */
  async listen<T>(event: string, handler: (payload: T) => void): Promise<() => void> {
    if (event !== WA_EVENT) {
      if (!this.isTauri) return () => undefined;
      return (await this.tauri()).listen<T>(event, handler);
    }
    const create = this.deps.createEventSource;
    if (!create) throw new Error('EventSource is not available');
    const ep = await this.ready();
    const query = ep.token ? `?token=${encodeURIComponent(ep.token)}` : '';
    this.sseState.set('connecting');
    const es = create(`${ep.url}/events${query}`);
    es.onopen = () => this.sseState.set('open');
    // EventSource reconnects on its own; just surface the state.
    es.onerror = () => this.sseState.set('reconnecting');
    es.onmessage = (m) => {
      try {
        handler(JSON.parse(m.data) as T);
      } catch (e) {
        console.warn('Dropping malformed SSE event', e);
      }
    };
    return () => {
      es.close();
      this.sseState.set('idle');
    };
  }
}
