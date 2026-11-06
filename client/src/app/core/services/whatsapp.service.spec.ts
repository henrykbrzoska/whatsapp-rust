import { TestBed } from '@angular/core/testing';

import { FakeDeps, FakeEventSource, fakeDeps, jsonResponse } from '../../testing/fakes';
import { EXPORT_FORMATS } from '../models';
import { PLATFORM_DEPS, TauriBridge, TauriService } from './tauri.service';
import { WhatsappService, filenameFromDisposition } from './whatsapp.service';

function setup(deps: FakeDeps): { wa: WhatsappService; tauri: TauriService } {
  TestBed.configureTestingModule({ providers: [{ provide: PLATFORM_DEPS, useValue: deps }] });
  return { wa: TestBed.inject(WhatsappService), tauri: TestBed.inject(TauriService) };
}

const configured = () => fakeDeps({ stored: { url: 'http://e', token: 'tok' } });

describe('filenameFromDisposition', () => {
  it('reads quoted, bare and RFC 5987 forms', () => {
    expect(filenameFromDisposition('attachment; filename="chat-1@g.us.json"')).toBe('chat-1@g.us.json');
    expect(filenameFromDisposition('attachment; filename=a.csv')).toBe('a.csv');
    expect(filenameFromDisposition("attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.csv")).toBe('résumé.csv');
    expect(filenameFromDisposition(null)).toBeNull();
    expect(filenameFromDisposition('inline')).toBeNull();
  });
});

describe('WhatsappService', () => {
  it('starts as unconfigured without an engine and never calls fetch', async () => {
    const deps = fakeDeps();
    const { wa } = setup(deps);
    await wa.start();
    expect(wa.engineStatus()).toBe('unconfigured');
    expect(deps.fetchSpy).not.toHaveBeenCalled();
    expect(FakeEventSource.instances.length).toBe(0);
  });

  it('start() pings /version and subscribes to wa-event once', async () => {
    const deps = configured();
    const { wa } = setup(deps);
    await Promise.all([wa.start(), wa.start()]);
    await wa.start();

    expect(wa.engineStatus()).toBe('online');
    expect(wa.version()).toEqual({ version: '0.1.0', protocolRef: 'abc' });
    expect(FakeEventSource.instances.length).toBe(1);
    expect(FakeEventSource.instances[0].url).toBe('http://e/events?token=tok');
  });

  it('marks a rejected token as unauthorized and a dead engine as offline', async () => {
    const deps = configured();
    const { wa } = setup(deps);
    deps.fetchSpy.and.resolveTo(jsonResponse({ error: 'missing or invalid token' }, 401));
    expect(await wa.ping()).toBeFalse();
    expect(wa.engineStatus()).toBe('unauthorized');

    deps.fetchSpy.and.rejectWith(new TypeError('Failed to fetch'));
    await wa.ping();
    expect(wa.engineStatus()).toBe('offline');
    expect(wa.engineError()).toContain('unreachable');
  });

  it('drives connectionState and qrCodes from wa-event', async () => {
    const { wa } = setup(configured());
    await wa.start();
    const es = FakeEventSource.instances[0];

    es.emit({ type: 'qr', codes: ['a', 'b'] });
    expect(wa.connectionState()).toBe('qr');
    expect(wa.qrCodes()).toEqual(['a', 'b']);

    es.emit({ type: 'connected' });
    expect(wa.connectionState()).toBe('connected');
    expect(wa.qrCodes()).toEqual([]);

    es.emit({ type: 'disconnected' });
    expect(wa.connectionState()).toBe('disconnected');

    es.emit({ type: 'logged_out', onConnect: false, reason: 401 });
    expect(wa.connectionState()).toBe('logged_out');
  });

  /** `GET /version` then `GET /status`, with the status the test wants. */
  function engineWith(status: Record<string, unknown>): FakeDeps {
    const deps = configured();
    deps.fetchSpy.and.callFake(async (url: string) => {
      if (url.includes('/status')) return jsonResponse(status);
      if (url.includes('/connect')) return jsonResponse({});
      return jsonResponse({ version: '0.1.0', protocolRef: 'abc' });
    });
    return deps;
  }

  it('adopts the engine session state on start, so a paired device is not asked to pair again', async () => {
    const deps = engineWith({ state: 'connected', connected: true, loggedIn: true, error: null, attempts: 1 });
    const { wa } = setup(deps);
    await wa.start();

    expect(wa.connectionState()).toBe('connected');
    expect(wa.paired()).toBeTrue();
    expect(deps.fetchSpy.calls.all().some((c) => String(c.args[0]).includes('/connect'))).toBeFalse();
  });

  it('reconnects a paired engine that lost its socket (e.g. after a restart)', async () => {
    const deps = engineWith({ state: 'disconnected', connected: false, loggedIn: true, error: null, attempts: 0 });
    const { wa } = setup(deps);
    await wa.start();

    const connects = deps.fetchSpy.calls.all().filter((c) => String(c.args[0]).includes('/connect'));
    expect(connects.length).toBe(1, 'POST /connect brings the session back');
    expect(wa.paired()).toBeTrue();
    expect(wa.connectionState()).toBe('connecting');
  });

  it('never auto-connects an engine with no paired device', async () => {
    const deps = engineWith({ state: 'disconnected', connected: false, loggedIn: false, error: null, attempts: 0 });
    const { wa } = setup(deps);
    await wa.start();

    expect(deps.fetchSpy.calls.all().some((c) => String(c.args[0]).includes('/connect'))).toBeFalse();
    expect(wa.paired()).toBeFalse();
    expect(wa.connectionState()).toBe('disconnected');
  });

  it('does not fight a session another instance took over (conflict type=replaced)', async () => {
    const deps = engineWith({
      state: 'disconnected',
      connected: false,
      loggedIn: true,
      error: 'another session took over this device (conflict type=replaced)',
      attempts: 1,
      replaced: true,
    });
    const { wa } = setup(deps);
    await wa.start();

    // Reconnecting here would kick the other engine, which would kick us back.
    expect(deps.fetchSpy.calls.all().some((c) => String(c.args[0]).includes('/connect'))).toBeFalse();
    expect(wa.connectionState()).toBe('disconnected');
    expect(wa.engineError()).toContain('conflict type=replaced');
  });

  it('says why the socket closed when another login replaced this session', () => {
    const deps = engineWith({ state: 'connected', connected: true, loggedIn: true, error: null, attempts: 1 });
    const { wa } = setup(deps);

    wa.handleEvent({ type: 'stream_replaced' });

    expect(wa.connectionState()).toBe('disconnected');
    expect(wa.engineError()).toContain('took over this device');
  });

  it('sends a message and reads one page of a thread', async () => {
    const deps = configured();
    const { wa } = setup(deps);
    const row = { id: 'M1', chat_jid: '1@g.us', sender: 'me@s.whatsapp.net', timestamp: 7, type: 'text', text: 'hi', from_me: true };
    deps.fetchSpy.and.callFake(async (url: string, init?: RequestInit) => {
      if (url.includes('/send/message')) return jsonResponse({ id: 'M1', timestamp: 7, chat: '1@g.us', sender: 'me@s.whatsapp.net', message: row, body: init?.body });
      return jsonResponse({ messages: [row], hasMore: true });
    });

    const sent = await wa.sendMessage('1@g.us', 'hi');
    expect(sent.message.text).toBe('hi');
    const send = deps.fetchSpy.calls.all().find((c) => String(c.args[0]).includes('/send/message'))!;
    expect(send.args[1]?.method).toBe('POST');
    expect(JSON.parse(String(send.args[1]?.body))).toEqual({ to: '1@g.us', text: 'hi' });

    const page = await wa.listMessages('1@g.us', { before: { timestamp: 7, id: 'M1' }, limit: 20 });
    expect(page.hasMore).toBeTrue();
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe(
      'http://e/chats/1%40g.us/messages?before=7&beforeId=M1&limit=20',
    );
  });

  it('asks for older history, for one chat or for all of them', async () => {
    const deps = configured();
    const { wa } = setup(deps);
    // A fresh Response per call — a body can only be read once.
    deps.fetchSpy.and.callFake(async () =>
      jsonResponse({
        count: 200,
        requested: [{ chat: '1@g.us', oldestId: 'M1', oldestTimestamp: 7, requestId: 'R1' }],
        skipped: ['2@g.us: no messages to ask from'],
      }),
    );

    const report = await wa.fetchHistory('1@g.us', 200);
    const call = deps.fetchSpy.calls.mostRecent();
    expect(call.args[0]).toBe('http://e/history/fetch');
    expect(call.args[1]?.method).toBe('POST');
    expect(JSON.parse(String(call.args[1]?.body))).toEqual({ chat: '1@g.us', count: 200 });
    expect(report.requested[0].requestId).toBe('R1');
    expect(report.skipped.length).toBe(1);

    await wa.fetchHistory();
    expect(JSON.parse(String(deps.fetchSpy.calls.mostRecent().args[1]?.body)))
      .withContext('no chat = every known chat')
      .toEqual({});
  });

  it('builds a media URL a media element can load, and reads the info', async () => {
    const deps = configured();
    const { wa } = setup(deps);

    // An <img> cannot send an Authorization header, so the token is in the query.
    expect(wa.mediaUrl('M1')).toBe('http://e/media/M1?token=tok');
    expect(wa.mediaUrl('a/b?c')).toBe('http://e/media/a%2Fb%3Fc?token=tok');

    deps.fetchSpy.and.callFake(async () =>
      jsonResponse({ id: 'M1', kind: 'image', available: true, cached: false, mimeType: 'image/jpeg', size: 1024 }),
    );
    const info = await wa.mediaInfo('M1');
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/media/M1/info');
    expect(info.mimeType).toBe('image/jpeg');
  });

  it('has no media URL before the engine endpoint is known', () => {
    const { wa } = setup(fakeDeps());
    expect(wa.mediaUrl('M1')).withContext('never a request at the page’s own origin').toBe('');
  });

  it('tracks history_export_progress per chat', () => {
    const { wa } = setup(configured());
    wa.handleEvent({ type: 'history_export_progress', chat: '1@g.us', exported: 500, total: 1200 });
    expect(wa.exportProgress()['1@g.us']).toEqual({ exported: 500, total: 1200 });
  });

  it('records history_sync notifications and flags a running sync', () => {
    const { wa } = setup(configured());
    const ev = {
      type: 'history_sync' as const,
      messageId: 'm1',
      chat: '1@s.whatsapp.net',
      sender: 's',
      conversationTimestamp: 1_700_000_000,
      syncType: 'recent',
    };
    wa.handleEvent(ev);
    expect(wa.lastHistorySync()).toEqual(ev);
    expect(wa.historySyncing()).toBeTrue();

    wa.handleEvent({ type: 'connected' });
    expect(wa.historySyncing()).toBeFalse();
  });

  it('debugLog() reads /debug/log with limit and since', async () => {
    const deps = configured();
    deps.fetchSpy.and.resolveTo(jsonResponse({ capacity: 1000, entries: [{ seq: 3, ts: 1, kind: 'engine', message: 'hi' }] }));
    const { wa } = setup(deps);

    const log = await wa.debugLog(50, 2);

    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/debug/log?limit=50&since=2');
    expect(log.entries[0].message).toBe('hi');
    expect(log.capacity).toBe(1000);
  });

  it('clearDebugLog() sends DELETE /debug/log', async () => {
    const deps = configured();
    deps.fetchSpy.and.resolveTo(jsonResponse({ cleared: 7 }));
    const { wa } = setup(deps);

    expect(await wa.clearDebugLog()).toBe(7);
    expect(deps.fetchSpy.calls.mostRecent().args[1]?.method).toBe('DELETE');
  });

  it('reads contacts, pictures and the blocklist', async () => {
    const deps = configured();
    deps.fetchSpy.and.callFake(async () => jsonResponse([{ jid: '1@s.whatsapp.net', found: true, displayName: 'Ann' }]));
    const { wa } = setup(deps);

    const contacts = await wa.contacts();
    expect(contacts[0].displayName).toBe('Ann');
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/contacts');

    await wa.contact('1@s.whatsapp.net');
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/contact?jid=1%40s.whatsapp.net');

    await wa.picture('1@s.whatsapp.net', { preview: true });
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe(
      'http://e/picture?jid=1%40s.whatsapp.net&preview=true',
    );

    deps.fetchSpy.and.callFake(async () => jsonResponse({ dhash: 'd', addressingMode: 'lid', items: [] }));
    await wa.blocklist();
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/blocklist');
  });

  it('block() and unblock() POST the jid and return the new blocklist', async () => {
    const deps = configured();
    deps.fetchSpy.and.callFake(async () =>
      jsonResponse({ dhash: 'd', addressingMode: 'lid', items: [{ jid: '2@lid', pn: null, active: true }] }),
    );
    const { wa } = setup(deps);

    const after = await wa.block('2@lid');

    const call = deps.fetchSpy.calls.mostRecent();
    expect(call.args[0]).toBe('http://e/block');
    expect(call.args[1]?.method).toBe('POST');
    expect(call.args[1]?.body).toBe('{"jid":"2@lid"}');
    expect(after.items[0].active).toBeTrue();

    await wa.unblock('2@lid');
    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/unblock');
  });

  it('search() forwards query, mode and chat to search_messages', async () => {
    const { wa, tauri } = setup(configured());
    const invoke = spyOn(tauri, 'invoke').and.resolveTo({ mode: 'vector', vector: true, results: [] });

    await wa.search('  dinner ', { mode: 'vector', chat: '1@g.us' });

    expect(invoke).toHaveBeenCalledWith('search_messages', {
      query: 'dinner',
      mode: 'vector',
      chat: '1@g.us',
      limit: undefined,
    });
  });

  it('search() with an empty query does not hit the engine', async () => {
    const { wa, tauri } = setup(configured());
    const invoke = spyOn(tauri, 'invoke');
    const res = await wa.search('   ', { mode: 'keyword' });
    expect(res).toEqual({ mode: 'keyword', vector: false, results: [] });
    expect(invoke).not.toHaveBeenCalled();
  });

  it('exportHistory() downloads the blob under the Content-Disposition name', async () => {
    const deps = configured();
    const { wa } = setup(deps);
    const body = '{"id":"m1"}\n{"id":"m2"}\n';
    deps.fetchSpy.and.resolveTo(
      new Response(body, {
        status: 200,
        headers: { 'Content-Type': 'application/x-ndjson', 'Content-Disposition': 'attachment; filename="chat-1@g.us.jsonl"' },
      }),
    );
    const save = spyOn(wa, 'saveBlob');

    const result = await wa.exportHistory('1@g.us', 'jsonl');

    expect(deps.fetchSpy.calls.mostRecent().args[0]).toBe('http://e/chats/1%40g.us/history/export?format=jsonl');
    expect(result).toEqual({ filename: 'chat-1@g.us.jsonl', size: body.length });
    const [blob, name] = save.calls.mostRecent().args;
    expect(name).toBe('chat-1@g.us.jsonl');
    expect(await blob.text()).toBe(body);
    expect(wa.exportProgress()['1@g.us']).toBeUndefined();
  });

  for (const format of EXPORT_FORMATS) {
    it(`exportHistory('${format}') in browser mode sends an authorized GET with format=${format}`, async () => {
      const deps = configured();
      const { wa } = setup(deps);
      spyOn(wa, 'saveBlob');
      deps.fetchSpy.and.resolveTo(
        new Response('x', { headers: { 'Content-Disposition': `attachment; filename="chat-1@g.us.${format}"` } }),
      );

      const result = await wa.exportHistory('1@g.us', format);

      const [url, init] = deps.fetchSpy.calls.mostRecent().args;
      expect(url).toBe(`http://e/chats/1%40g.us/history/export?format=${format}`);
      expect(init?.method).toBe('GET');
      expect((init?.headers as Record<string, string>)['Authorization']).toBe('Bearer tok');
      expect(result.filename).toBe(`chat-1@g.us.${format}`);
    });

    it(`exportHistory('${format}') in Tauri mode invokes the native export_chat_history command`, async () => {
      const bridge = jasmine.createSpyObj<TauriBridge>('TauriBridge', ['invoke', 'listen']);
      const saved = { filename: `chat-1@g.us.${format}`, path: `/home/u/Downloads/chat-1@g.us.${format}`, size: 42 };
      bridge.invoke.and.callFake(async <T>(cmd: string) => (cmd === 'export_chat_history' ? saved : null) as T);
      const deps = fakeDeps({ bridge });
      const { wa } = setup(deps);
      const save = spyOn(wa, 'saveBlob');

      expect(await wa.exportHistory('1@g.us', format)).toEqual(saved);

      expect(bridge.invoke).toHaveBeenCalledWith('export_chat_history', { chat: '1@g.us', format });
      expect(deps.fetchSpy).not.toHaveBeenCalled();
      expect(save).not.toHaveBeenCalled();
      expect(wa.exportProgress()).toEqual({});
    });
  }

  it('exportHistory() in Tauri mode surfaces native command errors', async () => {
    const bridge = jasmine.createSpyObj<TauriBridge>('TauriBridge', ['invoke', 'listen']);
    bridge.invoke.and.rejectWith(new Error('engine unreachable'));
    const { wa } = setup(fakeDeps({ bridge }));
    await expectAsync(wa.exportHistory('1@g.us', 'csv')).toBeRejectedWithError(/engine unreachable/);
    expect(wa.exportProgress()).toEqual({});
  });

  it('exportHistory() falls back to a generated name and surfaces errors', async () => {
    const deps = configured();
    const { wa } = setup(deps);
    const save = spyOn(wa, 'saveBlob');

    deps.fetchSpy.and.resolveTo(new Response('a,b\n', { status: 200 }));
    expect((await wa.exportHistory('2@s.whatsapp.net', 'csv')).filename).toBe('chat-2@s.whatsapp.net.csv');

    deps.fetchSpy.and.resolveTo(jsonResponse({ error: 'unknown format' }, 400));
    await expectAsync(wa.exportHistory('2@s.whatsapp.net', 'csv')).toBeRejectedWithError(/unknown format/);
    expect(save).toHaveBeenCalledTimes(1);
    expect(wa.exportProgress()).toEqual({});
  });
});
