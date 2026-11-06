import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';

import { PLATFORM_DEPS } from '../../core/services/tauri.service';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { fakeDeps } from '../../testing/fakes';
import type { ChatPresenceData, MessageSource } from '../../types/specta';
import { ChatListComponent, syncSummary } from './chat-list.component';
import type { AppStateSyncReport } from '../../core/services/whatsapp.service';
import { ChatListEntry, isMuted, PresenceStore, sortChats } from './chat-list.models';

const chat = (jid: string, last_timestamp: number, extra: Partial<ChatListEntry> = {}): ChatListEntry => ({
  jid,
  name: null,
  count: 1,
  last_timestamp,
  last_text: `hi ${jid}`,
  muted: false,
  muted_until: null,
  pinned: false,
  archived: false,
  ...extra,
});

const source = (chat: string): MessageSource => ({
  chat,
  sender: chat,
  senderAlt: '',
  recipientAlt: '',
  isFromMe: false,
  isGroup: chat.endsWith('@g.us'),
  addressingMode: '',
});

describe('chat-list sorting and flags', () => {
  it('puts pinned chats first, then sorts by recency', () => {
    const sorted = sortChats([
      chat('old@s.whatsapp.net', 100),
      chat('new@s.whatsapp.net', 300),
      chat('pin-old@s.whatsapp.net', 50, { pinned: true }),
      chat('pin-new@s.whatsapp.net', 250, { pinned: true }),
      chat('mid@s.whatsapp.net', 200),
    ]);
    expect(sorted.map((c) => c.jid.split('@')[0])).toEqual(['pin-new', 'pin-old', 'new', 'mid', 'old']);
  });

  it('does not mutate its input', () => {
    const input = [chat('a@s.whatsapp.net', 1), chat('b@s.whatsapp.net', 2)];
    sortChats(input);
    expect(input[0].jid).toBe('a@s.whatsapp.net');
  });

  it('treats -1 as muted forever and past timestamps as unmuted', () => {
    expect(isMuted({ muted: true }, 1000)).toBeTrue();
    expect(isMuted({ muted_until: -1 }, 1000)).toBeTrue();
    expect(isMuted({ muted_until: 2000 }, 1000)).toBeTrue();
    expect(isMuted({ muted_until: 500 }, 1000)).toBeFalse();
    expect(isMuted({}, 1000)).toBeFalse();
  });
});

describe('ChatListComponent', () => {
  let fixture: ComponentFixture<ChatListComponent>;
  let wa: WhatsappService;
  const el = () => fixture.nativeElement as HTMLElement;
  const row = (jid: string) => el().querySelector<HTMLElement>(`li[data-jid="${jid}"]`)!;

  beforeEach(async () => {
    TestBed.configureTestingModule({
      imports: [ChatListComponent],
      providers: [provideRouter([]), { provide: PLATFORM_DEPS, useValue: fakeDeps({ stored: { url: 'http://e', token: 't' } }) }],
    });
    wa = TestBed.inject(WhatsappService);
    spyOn(wa, 'listChats').and.resolveTo([
      chat('a@s.whatsapp.net', 300),
      chat('b@s.whatsapp.net', 100, { pinned: true }),
      chat('c@g.us', 200, { muted: true, muted_until: -1, archived: true }),
    ]);
    wa.engineStatus.set('online');
    fixture = TestBed.createComponent(ChatListComponent);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
  });

  describe('the Fetch history button', () => {
    const fetchButton = () => el().querySelector<HTMLButtonElement>('[data-testid="fetch-history"]')!;
    const result = () => el().querySelector('[data-testid="fetch-result"]')?.textContent;

    async function click(): Promise<void> {
      fetchButton().click();
      fixture.detectChanges();
      await fixture.whenStable();
      fixture.detectChanges();
    }

    it('asks for every known chat at once', async () => {
      const fetchHistory = spyOn(wa, 'fetchHistory').and.resolveTo({
        count: 200,
        requested: [
          { chat: 'a@s.whatsapp.net', oldestId: 'm1', oldestTimestamp: 1, requestId: 'r1' },
          { chat: 'c@g.us', oldestId: 'm2', oldestTimestamp: 2, requestId: 'r2' },
        ],
        skipped: [],
      });

      await click();

      expect(fetchHistory).toHaveBeenCalledWith();
      expect(result()).toContain('200 older message(s) in 2 chat(s)');
      expect(fetchButton().disabled).toBeFalse();
    });

    it('names the chats it could not anchor on', async () => {
      spyOn(wa, 'fetchHistory').and.resolveTo({
        count: 200,
        requested: [{ chat: 'a@s.whatsapp.net', oldestId: 'm1', oldestTimestamp: 1, requestId: 'r1' }],
        skipped: ['b@s.whatsapp.net: no messages to ask from'],
      });

      await click();

      expect(result()).toContain('Skipped 1');
      expect(result()).toContain('no messages to ask from');
    });

    it('reports a failure instead of staying busy', async () => {
      spyOn(wa, 'fetchHistory').and.rejectWith(new Error('not connected'));
      await click();
      expect(result()).toContain('Could not ask for history: not connected');
      expect(fetchButton().disabled).toBeFalse();
    });

    it('reloads the list when an imported blob brought messages', async () => {
      const list = wa.listChats as jasmine.Spy;
      const before = list.calls.count();
      wa.handleEvent({ type: 'history_imported', messages: 9, stored: 9, conversations: 2, named: 2, endOfHistory: false });
      await fixture.whenStable();
      expect(list.calls.count()).toBeGreaterThan(before);

      // An import that stored nothing is not worth a reload.
      const after = list.calls.count();
      wa.handleEvent({ type: 'history_imported', messages: 0, stored: 0, conversations: 0, named: 0, endOfHistory: true });
      await fixture.whenStable();
      expect(list.calls.count()).toBe(after);
    });
  });

  it('renders pinned first, then by recency', () => {
    const jids = Array.from(el().querySelectorAll('li[data-jid]')).map((li) => li.getAttribute('data-jid'));
    expect(jids).toEqual(['b@s.whatsapp.net', 'a@s.whatsapp.net', 'c@g.us']);
  });

  it('shows pin, mute and archive badges only where set', () => {
    expect(row('b@s.whatsapp.net').querySelector('[data-testid="pinned"]')).not.toBeNull();
    expect(row('b@s.whatsapp.net').querySelector('[data-testid="muted"]')).toBeNull();
    expect(row('c@g.us').querySelector('[data-testid="muted"]')).not.toBeNull();
    expect(row('c@g.us').querySelector('[data-testid="archived"]')).not.toBeNull();
    expect(row('a@s.whatsapp.net').querySelector('.flag')).toBeNull();
  });

  it('opens every row as a conversation and offers the create flow', () => {
    // Group details live behind the chat menu now, so a group row opens the
    // thread like any other chat.
    expect(row('c@g.us').querySelector('a')?.getAttribute('href')).toContain('/chat/');
    expect(row('a@s.whatsapp.net').querySelector('a')?.getAttribute('href')).toContain('/chat/');
    expect(el().querySelector('[data-testid="new-group"]')).not.toBeNull();
  });

  it('shows a presence dot driven by presence and chat_presence events', () => {
    const presence = TestBed.inject(PresenceStore);
    expect(row('a@s.whatsapp.net').querySelector('.presence-dot')).toBeNull();

    presence.applyEvent({ type: 'presence', presence: { from: 'a@s.whatsapp.net', unavailable: false, lastSeen: null } });
    fixture.detectChanges();
    expect(row('a@s.whatsapp.net').querySelector('.presence-dot')?.getAttribute('data-presence')).toBe('online');

    const typing: ChatPresenceData = { source: source('a@s.whatsapp.net'), state: 'composing', media: '' };
    presence.applyEvent({ type: 'chat_presence', chatPresence: typing });
    fixture.detectChanges();
    expect(row('a@s.whatsapp.net').querySelector('.presence-dot')?.classList).toContain('composing');

    presence.applyEvent({ type: 'presence', presence: { from: 'a@s.whatsapp.net', unavailable: true, lastSeen: null } });
    fixture.detectChanges();
    expect(row('a@s.whatsapp.net').querySelector('.presence-dot')?.getAttribute('data-presence')).toBe('offline');
  });

  it('feeds the presence store from the wa-event stream', () => {
    const presence = TestBed.inject(PresenceStore);
    const typing: ChatPresenceData = { source: source('a@s.whatsapp.net'), state: 'composing', media: 'audio' };
    wa.handleEvent({ type: 'chat_presence', chatPresence: typing });
    expect(presence.get('a@s.whatsapp.net')).toBe('recording');

    // Unrelated events leave presence untouched.
    wa.handleEvent({ type: 'connected' });
    expect(presence.get('a@s.whatsapp.net')).toBe('recording');
  });
});

describe('syncSummary', () => {
  const report = (over: Partial<AppStateSyncReport> = {}): AppStateSyncReport => ({
    collections: [],
    contacts: 0,
    chatSettings: 0,
    errors: [],
    skipped: [],
    requestedKeys: [],
    ...over,
  });

  it('reports what was applied', () => {
    expect(syncSummary(report({ contacts: 12, chatSettings: 3 }))).toBe(
      'Synced 12 contact name(s) and 3 chat setting(s).',
    );
  });

  it('puts an error first — it explains everything else', () => {
    const summary = syncSummary(report({ errors: ['regular: not connected'], contacts: 1 }));
    expect(summary).toContain('Sync incomplete');
    expect(summary).toContain('regular: not connected');
  });

  it('explains the missing app-state keys instead of claiming a clean sync', () => {
    // This is the real shape of the bug: the patches arrive and decrypt to
    // nothing, because only the phone has the keys.
    const summary = syncSummary(report({ requestedKeys: ['aabb', 'ccdd'] }));
    expect(summary).toContain('missing 2 app-state key(s)');
    expect(summary).toContain('try Sync names again');
  });

  it('names what was skipped when nothing could be applied', () => {
    const summary = syncSummary(
      report({ skipped: ['regular: needs 1 app state key(s) this device does not have'] }),
    );
    expect(summary).toContain('Nothing could be applied');
    expect(summary).toContain('needs 1 app state key(s)');
  });

  it('does not nag about skips once names actually arrived', () => {
    expect(syncSummary(report({ contacts: 4, skipped: ['regular_low: malformed mutation index []'] }))).toBe(
      'Synced 4 contact name(s) and 0 chat setting(s).',
    );
  });
});
