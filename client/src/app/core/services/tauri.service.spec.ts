import { TestBed } from '@angular/core/testing';

import { FakeDeps, FakeEventSource, fakeDeps, jsonResponse } from '../../testing/fakes';
import { EngineError, PLATFORM_DEPS, STORAGE_KEY, TauriBridge, TauriService, parseReadyLine } from './tauri.service';

function setup(deps: FakeDeps): TauriService {
  TestBed.configureTestingModule({ providers: [{ provide: PLATFORM_DEPS, useValue: deps }] });
  return TestBed.inject(TauriService);
}

const READY = 'BEBOK_READY http://127.0.0.1:8787/?token=abc123';

describe('parseReadyLine', () => {
  it('parses the full BEBOK_READY line', () => {
    expect(parseReadyLine(READY)).toEqual({ url: 'http://127.0.0.1:8787', token: 'abc123' });
  });

  it('accepts a bare URL and a missing token', () => {
    expect(parseReadyLine('  http://127.0.0.1:5000/  ')).toEqual({ url: 'http://127.0.0.1:5000', token: null });
  });

  it('rejects garbage and non-http schemes', () => {
    expect(parseReadyLine('')).toBeNull();
    expect(parseReadyLine('not a url')).toBeNull();
    expect(parseReadyLine('javascript:alert(1)')).toBeNull();
    expect(parseReadyLine(null)).toBeNull();
  });
});

describe('TauriService (browser mode)', () => {
  it('consumes ?engine=, stores URL + token and cleans the address bar', () => {
    const deps = fakeDeps({ href: `http://localhost:4200/chat/1%40g.us?engine=${encodeURIComponent(READY)}&x=1#top` });
    const svc = setup(deps);

    expect(svc.isTauri).toBeFalse();
    expect(svc.endpoint()).toEqual({ url: 'http://127.0.0.1:8787', token: 'abc123' });
    expect(JSON.parse(deps.storageImpl.getItem(STORAGE_KEY)!)).toEqual({ url: 'http://127.0.0.1:8787', token: 'abc123' });
    expect(deps.replaced).toEqual(['/chat/1%40g.us?x=1#top']);
  });

  it('restores the stored endpoint and leaves a clean URL alone', () => {
    const deps = fakeDeps({ stored: { url: 'http://127.0.0.1:9000', token: 't' } });
    const svc = setup(deps);
    expect(svc.endpoint()).toEqual({ url: 'http://127.0.0.1:9000', token: 't' });
    expect(deps.replaced).toEqual([]);
  });

  it('still cleans the URL when ?engine= is invalid', () => {
    const deps = fakeDeps({ href: 'http://localhost:4200/?engine=nope' });
    const svc = setup(deps);
    expect(svc.endpoint()).toBeNull();
    expect(deps.replaced).toEqual(['/']);
  });

  it('rejects requests until an engine is configured', async () => {
    const svc = setup(fakeDeps());
    await expectAsync(svc.invoke('version')).toBeRejectedWithError(EngineError, /not configured/);
  });

  it('sends Bearer auth and routes search_messages to GET /search', async () => {
    const deps = fakeDeps({ stored: { url: 'http://127.0.0.1:8787', token: 'abc' } });
    deps.fetchSpy.and.resolveTo(jsonResponse({ mode: 'keyword', vector: false, results: [] }));
    const svc = setup(deps);

    await svc.invoke('search_messages', { query: 'dinner plans', mode: 'keyword', chat: '1@g.us', limit: undefined });

    const [url, init] = deps.fetchSpy.calls.mostRecent().args;
    expect(url).toBe('http://127.0.0.1:8787/search?q=dinner+plans&chat=1%40g.us&mode=keyword');
    expect(init?.method).toBe('GET');
    expect((init?.headers as Record<string, string>)['Authorization']).toBe('Bearer abc');
  });

  it('encodes the JID in chat routes', async () => {
    const deps = fakeDeps({ stored: { url: 'http://e', token: null } });
    deps.fetchSpy.and.resolveTo(jsonResponse({ count: 0, first_timestamp: null, last_timestamp: null }));
    const svc = setup(deps);
    await svc.invoke('get_chat_stats', { chat: '1@g.us' });
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/chats/1%40g.us/stats');
    expect((deps.fetchSpy.calls.mostRecent().args[1]?.headers as Record<string, string>)['Authorization']).toBeUndefined();
  });

  it('maps engine errors to EngineError with the JSON message', async () => {
    const deps = fakeDeps({ stored: { url: 'http://e', token: 'bad' } });
    deps.fetchSpy.and.resolveTo(jsonResponse({ error: 'missing or invalid token' }, 401));
    const svc = setup(deps);
    try {
      await svc.invoke('version');
      fail('expected rejection');
    } catch (e) {
      expect(e).toEqual(jasmine.any(EngineError));
      expect((e as EngineError).status).toBe(401);
      expect((e as EngineError).message).toBe('missing or invalid token');
    }
  });

  it('reports network failures as status 0', async () => {
    const deps = fakeDeps({ stored: { url: 'http://e', token: null } });
    deps.fetchSpy.and.rejectWith(new TypeError('Failed to fetch'));
    const svc = setup(deps);
    await expectAsync(svc.invoke('version')).toBeRejectedWith(jasmine.objectContaining({ status: 0 }));
  });

  it('rejects unknown commands', async () => {
    const svc = setup(fakeDeps({ stored: { url: 'http://e', token: null } }));
    await expectAsync(svc.invoke('definitely_not_a_command')).toBeRejectedWithError(/Unknown command/);
  });

  it('listens to wa-event over SSE with the token in the query', async () => {
    // The malformed event below is dropped on purpose — keep the suite quiet.
    const warn = spyOn(console, 'warn');
    const svc = setup(fakeDeps({ stored: { url: 'http://e', token: 'tok' } }));
    const seen: unknown[] = [];
    const unlisten = await svc.listen('wa-event', (e) => seen.push(e));

    const es = FakeEventSource.instances[0];
    expect(es.url).toBe('http://e/events?token=tok');
    expect(svc.sseState()).toBe('connecting');
    es.onopen?.(new Event('open'));
    expect(svc.sseState()).toBe('open');

    es.emit({ type: 'connected' });
    es.emit('{not json');
    expect(seen).toEqual([{ type: 'connected' }]);
    expect(warn).toHaveBeenCalledWith('Dropping malformed SSE event', jasmine.any(SyntaxError));

    unlisten();
    expect(es.closed).toBeTrue();
    expect(svc.sseState()).toBe('idle');
  });

  it('treats shell events as no-ops in the browser', async () => {
    const svc = setup(fakeDeps({ stored: { url: 'http://e', token: null } }));
    const unlisten = await svc.listen('engine-exit', () => undefined);
    expect(FakeEventSource.instances.length).toBe(0);
    expect(() => unlisten()).not.toThrow();
  });

  it('connectWith validates input and setEndpoint(null) forgets it', () => {
    const deps = fakeDeps();
    const svc = setup(deps);
    expect(() => svc.connectWith('hello')).toThrowError(/BEBOK_READY/);
    svc.connectWith(READY);
    expect(svc.configured()).toBeTrue();
    svc.setEndpoint(null);
    expect(svc.configured()).toBeFalse();
    expect(deps.storageImpl.getItem(STORAGE_KEY)).toBeNull();
  });
});

describe('TauriService (Tauri mode)', () => {
  function bridge(): jasmine.SpyObj<TauriBridge> {
    const b = jasmine.createSpyObj<TauriBridge>('TauriBridge', ['invoke', 'listen']);
    b.invoke.and.callFake(async <T>(cmd: string) => (cmd === 'engine_ready' ? 'BEBOK_READY http://127.0.0.1:41234/?token=side' : null) as T);
    b.listen.and.resolveTo(() => undefined);
    return b;
  }

  it('asks the shell for the sidecar URL once and does not persist it', async () => {
    const b = bridge();
    const deps = fakeDeps({ bridge: b });
    const svc = setup(deps);
    expect(svc.isTauri).toBeTrue();

    await Promise.all([svc.invoke('version'), svc.invoke('list_chats')]);

    expect(b.invoke.calls.allArgs().filter(([c]) => c === 'engine_ready').length).toBe(1);
    expect(deps.fetchSpy.calls.argsFor(0)[0]).toBe('http://127.0.0.1:41234/version');
    expect((deps.fetchSpy.calls.argsFor(0)[1]?.headers as Record<string, string>)['Authorization']).toBe('Bearer side');
    expect(deps.storageImpl.getItem(STORAGE_KEY)).toBeNull();
  });

  it('explains a bare 404 as an engine that is older than this client', async () => {
    const deps = fakeDeps({ stored: { url: 'http://e', token: 't' } });
    const svc = setup(deps);
    deps.fetchSpy.and.resolveTo(new Response('Not Found', { status: 404 }));

    await expectAsync(svc.request('POST', '/appstate/sync')).toBeRejectedWith(
      jasmine.objectContaining({ status: 404, message: jasmine.stringContaining('/appstate/sync') }),
    );
    await expectAsync(svc.request('POST', '/appstate/sync')).toBeRejectedWith(
      jasmine.objectContaining({ message: jasmine.stringContaining('Restart the engine') }),
    );
  });

  it("keeps the engine's own 404 message (an unknown id is not an old build)", async () => {
    const deps = fakeDeps({ stored: { url: 'http://e', token: 't' } });
    const svc = setup(deps);
    deps.fetchSpy.and.resolveTo(jsonResponse({ error: 'unknown message M1' }, 404));

    await expectAsync(svc.request('GET', '/media/M1')).toBeRejectedWith(
      jasmine.objectContaining({ status: 404, message: 'unknown message M1' }),
    );
  });

  it('sends native commands and shell events through the bridge', async () => {
    const b = bridge();
    const svc = setup(fakeDeps({ bridge: b }));
    const handler = () => undefined;

    await svc.invoke('engine_ready');
    await svc.listen('engine-exit', handler);

    expect(b.invoke).toHaveBeenCalledWith('engine_ready', {});
    expect(b.listen).toHaveBeenCalledWith('engine-exit', handler);
  });
});
