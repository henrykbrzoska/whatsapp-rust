import { EventSourceLike, PlatformDeps, TauriBridge } from '../core/services/tauri.service';

export class FakeEventSource implements EventSourceLike {
  static instances: FakeEventSource[] = [];
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onopen: ((ev: Event) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  closed = false;

  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }

  emit(data: unknown): void {
    this.onmessage?.(new MessageEvent('message', { data: typeof data === 'string' ? data : JSON.stringify(data) }));
  }

  close(): void {
    this.closed = true;
  }
}

export class MemoryStorage {
  readonly items = new Map<string, string>();
  getItem(k: string): string | null {
    return this.items.get(k) ?? null;
  }
  setItem(k: string, v: string): void {
    this.items.set(k, v);
  }
  removeItem(k: string): void {
    this.items.delete(k);
  }
}

export function jsonResponse(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
}

export interface FakeDeps extends PlatformDeps {
  fetchSpy: jasmine.Spy<(input: string, init?: RequestInit) => Promise<Response>>;
  storageImpl: MemoryStorage;
  replaced: string[];
}

/** Platform deps for tests: no real network, storage or address bar. */
export function fakeDeps(opts: { href?: string; bridge?: TauriBridge; stored?: unknown } = {}): FakeDeps {
  FakeEventSource.instances = [];
  const storage = new MemoryStorage();
  if (opts.stored !== undefined) storage.setItem('bebok.engine', JSON.stringify(opts.stored));
  const replaced: string[] = [];
  const fetchSpy = jasmine
    .createSpy<(input: string, init?: RequestInit) => Promise<Response>>('fetch')
    .and.callFake(async () => jsonResponse({ version: '0.1.0', protocolRef: 'abc' }));
  return {
    fetch: fetchSpy,
    fetchSpy,
    createEventSource: (url) => new FakeEventSource(url),
    storage,
    storageImpl: storage,
    location: { href: opts.href ?? 'http://localhost:4200/' },
    history: {
      state: null,
      replaceState: (_d: unknown, _u: string, url?: string) => {
        replaced.push(url ?? '');
      },
    },
    replaced,
    tauri: opts.bridge ? () => Promise.resolve(opts.bridge!) : null,
  };
}
