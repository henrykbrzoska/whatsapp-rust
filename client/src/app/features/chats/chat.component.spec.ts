import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';

import { WhatsappService } from '../../core/services/whatsapp.service';
import { PLATFORM_DEPS } from '../../core/services/tauri.service';
import { EXPORT_FORMATS } from '../../core/models';
import { FakeDeps, fakeDeps } from '../../testing/fakes';
import { ChatComponent } from './chat.component';

describe('ChatComponent', () => {
  let fixture: ComponentFixture<ChatComponent>;
  let wa: WhatsappService;
  let deps: FakeDeps;
  const fetchSpy = () => deps.fetchSpy;

  const el = () => fixture.nativeElement as HTMLElement;
  const button = () => el().querySelector<HTMLButtonElement>('[data-testid="export"]')!;
  const formatPicker = () => el().querySelector<HTMLSelectElement>('[data-testid="export-format"]')!;
  const selectFormat = (format: string) => {
    formatPicker().value = format;
    formatPicker().dispatchEvent(new Event('change'));
  };

  /**
   * Lets the export finish: `Response.blob()` resolves outside the Angular
   * zone, so `whenStable()` alone would return before the download completes.
   */
  async function flush(): Promise<void> {
    for (let i = 0; i < 3; i++) await new Promise((resolve) => setTimeout(resolve));
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
  }

  beforeEach(async () => {
    deps = fakeDeps({ stored: { url: 'http://e', token: 't' } });
    TestBed.configureTestingModule({
      imports: [ChatComponent],
      providers: [provideRouter([]), { provide: PLATFORM_DEPS, useValue: deps }],
    });
    wa = TestBed.inject(WhatsappService);
    wa.engineStatus.set('online');
    spyOn(wa, 'chatStats').and.resolveTo({ count: 3, first_timestamp: 1_700_000_000, last_timestamp: 1_700_000_100 });
    fixture = TestBed.createComponent(ChatComponent);
    fixture.componentRef.setInput('jid', '1@g.us');
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
  });

  it('shows the chat stats and a download button', () => {
    expect(wa.chatStats).toHaveBeenCalledWith('1@g.us');
    expect(el().querySelector('.stats')?.textContent).toContain('3 messages');
    expect(button().textContent).toContain('Download full chat history');
  });

  it('offers JSON, JSONL and CSV in the format picker', () => {
    const options = Array.from(formatPicker().querySelectorAll('option')).map((o) => o.textContent?.trim());
    expect(options).toEqual(['JSON', 'JSONL', 'CSV']);
    expect(formatPicker().value).toBe('json');
  });

  it('exports with the selected format', async () => {
    const exp = spyOn(wa, 'exportHistory').and.resolveTo({ filename: 'chat-1@g.us.csv', size: 2048 });
    selectFormat('csv');

    button().click();
    await flush();

    expect(exp).toHaveBeenCalledWith('1@g.us', 'csv');
    expect(el().querySelector('.ok')?.textContent).toContain('Saved chat-1@g.us.csv (2.0 KB)');
  });

  it('reports export failures', async () => {
    spyOn(wa, 'exportHistory').and.rejectWith(new Error('boom'));
    button().click();
    await flush();
    expect(el().querySelector('.error')?.textContent).toContain('Export failed: boom');
    expect(button().disabled).toBeFalse();
    expect(el().querySelector('progress')).toBeNull();
  });

  for (const format of EXPORT_FORMATS) {
    it(`the button requests GET /chats/:jid/history/export?format=${format}`, async () => {
      const filename = `chat-1@g.us.${format}`;
      fetchSpy().and.resolveTo(new Response('data', { headers: { 'Content-Disposition': `attachment; filename="${filename}"` } }));
      const save = spyOn(wa, 'saveBlob');
      selectFormat(format);

      button().click();
      await flush();

      expect(fetchSpy().calls.mostRecent().args[0]).toBe(
        `http://e/chats/1%40g.us/history/export?format=${format}`,
      );
      const [blob, savedAs] = save.calls.mostRecent().args;
      expect(savedAs).toBe(filename, 'saves under the Content-Disposition name');
      expect(blob.size).toBe(4);
      expect(el().querySelector('.ok')?.textContent).toContain(`Saved ${filename}`);
    });
  }

  it('renders the engine error message when the export request fails', async () => {
    fetchSpy().and.resolveTo(new Response(JSON.stringify({ error: 'unknown format: xml' }), { status: 400 }));
    button().click();
    await flush();

    expect(el().querySelector('[role="alert"]')?.textContent).toContain('Export failed: unknown format: xml');
    expect(el().querySelector('.ok')).toBeNull();
    expect(el().querySelector('progress')).toBeNull();
    expect(button().disabled).toBeFalse();
  });

  describe('load earlier → fetch from WhatsApp', () => {
    const stored = (id: string, timestamp: number) => ({
      id,
      chat_jid: '1@g.us',
      sender: 'a@s.whatsapp.net',
      timestamp,
      type: 'text',
      text: id,
      from_me: false,
    });

    /** A chat with one page that holds everything the engine has locally. */
    async function openWith(messages: ReturnType<typeof stored>[], hasMore = false): Promise<jasmine.Spy> {
      const list = spyOn(wa, 'listMessages').and.resolveTo({ messages, hasMore });
      wa.connectionState.set('connected');
      await fixture.componentInstance.open('1@g.us');
      await flush();
      return list;
    }

    it('offers the local page while the engine still has older messages', async () => {
      await openWith([stored('m2', 1_700_000_100)], true);
      expect(el().querySelector('[data-testid="load-older"]')).toBeTruthy();
      expect(el().querySelector('[data-testid="fetch-older"]')).toBeNull();
    });

    it('asks WhatsApp once the local pages are exhausted, and waits', async () => {
      await openWith([stored('m2', 1_700_000_100)]);
      const fetchHistory = spyOn(wa, 'fetchHistory').and.resolveTo({
        count: 200,
        requested: [{ chat: '1@g.us', oldestId: 'm2', oldestTimestamp: 1_700_000_100, requestId: 'r1' }],
        skipped: [],
      });

      el().querySelector<HTMLButtonElement>('[data-testid="fetch-older"]')!.click();
      await flush();

      expect(fetchHistory).toHaveBeenCalledWith('1@g.us', 200);
      expect(el().querySelector('[data-testid="fetch-waiting"]')).withContext('nothing arrives at once').toBeTruthy();
      expect(el().querySelector('[data-testid="fetch-older"]')).toBeNull();
    });

    it('prepends what the imported blob brought and says how much', async () => {
      const list = await openWith([stored('m2', 1_700_000_100)]);
      spyOn(wa, 'fetchHistory').and.resolveTo({
        count: 200,
        requested: [{ chat: '1@g.us', oldestId: 'm2', oldestTimestamp: 1_700_000_100, requestId: 'r1' }],
        skipped: [],
      });
      el().querySelector<HTMLButtonElement>('[data-testid="fetch-older"]')!.click();
      await flush();

      // The phone answers minutes later; the engine imports and announces it.
      list.and.resolveTo({ messages: [stored('m0', 1_699_999_000), stored('m1', 1_699_999_500)], hasMore: true });
      wa.handleEvent({ type: 'history_imported', messages: 2, stored: 2, conversations: 1, named: 1, endOfHistory: false });
      await flush();

      expect(list.calls.mostRecent().args[1]).toEqual({
        before: { timestamp: 1_700_000_100, id: 'm2' },
        limit: 50,
      });
      expect(fixture.componentInstance.messages().map((m) => m.id)).toEqual(['m0', 'm1', 'm2']);
      expect(el().querySelector('[data-testid="fetch-note"]')?.textContent).toContain('2 older message(s) arrived');
      expect(el().querySelector('[data-testid="load-older"]')).withContext('more pages again').toBeTruthy();
    });

    it('reports the reason when no request could go out', async () => {
      await openWith([stored('m2', 1_700_000_100)]);
      spyOn(wa, 'fetchHistory').and.resolveTo({
        count: 200,
        requested: [],
        skipped: ['1@g.us: no messages to ask from'],
      });

      el().querySelector<HTMLButtonElement>('[data-testid="fetch-older"]')!.click();
      await flush();

      expect(el().querySelector('[data-testid="fetch-note"]')?.textContent).toContain('no messages to ask from');
      expect(el().querySelector('[data-testid="fetch-waiting"]')).toBeNull();
      expect(el().querySelector('[data-testid="fetch-older"]')).withContext('ready to try again').toBeTruthy();
    });

    it('reports a failed request without getting stuck in waiting', async () => {
      await openWith([stored('m2', 1_700_000_100)]);
      spyOn(wa, 'fetchHistory').and.rejectWith(new Error('not connected'));

      el().querySelector<HTMLButtonElement>('[data-testid="fetch-older"]')!.click();
      await flush();

      expect(el().querySelector('[data-testid="fetch-note"]')?.textContent).toContain('not connected');
      expect(el().querySelector('[data-testid="fetch-waiting"]')).toBeNull();
    });

    it('hides the fetch button without a live session', async () => {
      await openWith([stored('m2', 1_700_000_100)]);
      wa.connectionState.set('disconnected');
      fixture.detectChanges();
      expect(el().querySelector('[data-testid="fetch-older"]')).toBeNull();
    });
  });

  it('shows progress while exporting and disables the button', async () => {
    let finish!: (r: { filename: string; size: number; path?: string }) => void;
    spyOn(wa, 'exportHistory').and.returnValue(new Promise((resolve) => (finish = resolve)));

    button().click();
    fixture.detectChanges();
    expect(button().disabled).toBeTrue();
    expect(button().textContent).toContain('Exporting…');
    expect(el().querySelector('progress')).toBeTruthy();

    wa.handleEvent({ type: 'history_export_progress', chat: '1@g.us', exported: 500, total: 1200 });
    fixture.detectChanges();
    const bar = el().querySelector('progress') as HTMLProgressElement;
    expect(bar.value).toBe(500);
    expect(bar.max).toBe(1200);
    expect(el().textContent).toContain('500 / 1200');

    finish({ filename: 'chat-1@g.us.json', size: 10, path: '/home/u/Downloads/chat-1@g.us.json' });
    await flush();

    expect(el().querySelector('progress')).toBeNull();
    expect(el().querySelector('.ok')?.textContent).toContain('to /home/u/Downloads/chat-1@g.us.json');
    expect(button().disabled).toBeFalse();
  });
});
