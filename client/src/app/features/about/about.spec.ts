import { ComponentFixture, TestBed } from '@angular/core/testing';

import { PLATFORM_DEPS } from '../../core/services/tauri.service';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { fakeDeps } from '../../testing/fakes';
import type { ChatListEntry } from '../../types/specta';
import { AboutComponent, CLIENT_VERSION, libraryStats } from './about.component';

const chat = (jid: string, count: number, last_timestamp: number): ChatListEntry => ({
  jid,
  name: null,
  count,
  last_timestamp,
  last_text: null,
  muted: false,
  muted_until: null,
  pinned: false,
  archived: false,
});

describe('libraryStats', () => {
  it('counts chats, groups, newsletters and messages', () => {
    const s = libraryStats([
      chat('1@s.whatsapp.net', 5, 100),
      chat('2@g.us', 3, 300),
      chat('3@newsletter', 2, 200),
    ]);
    expect(s.chats).toBe(3);
    expect(s.groups).toBe(1);
    expect(s.newsletters).toBe(1);
    expect(s.messages).toBe(10);
    expect(s.lastActivity).toBe(300);
  });

  it('reports an empty library', () => {
    expect(libraryStats([])).toEqual({ chats: 0, groups: 0, newsletters: 0, messages: 0, lastActivity: null });
  });
});

describe('AboutComponent', () => {
  let fixture: ComponentFixture<AboutComponent>;
  let wa: WhatsappService;
  const el = () => fixture.nativeElement as HTMLElement;
  const settle = async () => {
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
  };

  async function setup(online: boolean): Promise<void> {
    TestBed.configureTestingModule({
      imports: [AboutComponent],
      providers: [{ provide: PLATFORM_DEPS, useValue: fakeDeps({ stored: { url: 'http://e', token: 't' } }) }],
    });
    wa = TestBed.inject(WhatsappService);
    spyOn(wa, 'listChats').and.resolveTo([chat('1@s.whatsapp.net', 4, 1_700_000_000), chat('2@g.us', 6, 1_700_000_500)]);
    wa.version.set(online ? { version: '0.1.0', protocolRef: 'deadbeef' } : null);
    wa.engineStatus.set(online ? 'online' : 'offline');
    fixture = TestBed.createComponent(AboutComponent);
    await settle();
  }

  it('shows the client version, engine version and protocol ref', async () => {
    await setup(true);
    expect(el().querySelector('[data-testid="client-version"]')?.textContent?.trim()).toBe(CLIENT_VERSION);
    expect(el().querySelector('[data-testid="engine-version"]')?.textContent?.trim()).toBe('0.1.0');
    expect(el().querySelector('[data-testid="protocol-ref"]')?.textContent?.trim()).toBe('deadbeef');
  });

  it('shows the local library totals', async () => {
    await setup(true);
    expect(wa.listChats).toHaveBeenCalled();
    const text = el().querySelector('[data-testid="stats"]')?.textContent ?? '';
    expect(text).toContain('10');
  });

  it('says so when the engine is offline', async () => {
    await setup(false);
    expect(el().querySelector('[data-testid="stats"]')).toBeNull();
    expect(wa.listChats).not.toHaveBeenCalled();
    expect(el().textContent).toContain('not connected');
  });
});
