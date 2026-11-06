import { ComponentFixture, TestBed } from '@angular/core/testing';

import { PLATFORM_DEPS } from '../../core/services/tauri.service';
import { EngineConfig, WhatsappService } from '../../core/services/whatsapp.service';
import { fakeDeps } from '../../testing/fakes';
import { CONFIG_KEYS, SettingsComponent, configValue, providerRows } from './settings.component';

const config = (extra: Partial<EngineConfig> = {}): EngineConfig => ({
  addr: '127.0.0.1:8787',
  auth: true,
  token: '<redacted>',
  cors: ['http://localhost:4200'],
  diagnostic: true,
  dataDir: '/home/u/.local/share/bebok',
  waUrl: null,
  handshakeTimeoutSecs: 20,
  vectorSearch: false,
  ...extra,
});

describe('settings helpers', () => {
  it('lists the providers the engine runs', () => {
    const rows = providerRows(config());
    expect(rows.map((r) => r.name)).toEqual(['Messages store', 'Vector search', 'Transport', 'Auth', 'Event log']);
    expect(rows.find((r) => r.name === 'Vector search')?.status).toBe('off');
    expect(rows.find((r) => r.name === 'Event log')?.status).toBe('on');
    expect(rows.find((r) => r.name === 'Transport')?.status).toBe('default');
  });

  it('flips statuses with the config flags', () => {
    const rows = providerRows(config({ vectorSearch: true, auth: false, waUrl: 'ws://mirror' }));
    expect(rows.find((r) => r.name === 'Vector search')?.status).toBe('on');
    expect(rows.find((r) => r.name === 'Event log')?.status).toBe('on');
    expect(rows.find((r) => r.name === 'Auth')?.status).toBe('off');
    expect(rows.find((r) => r.name === 'Transport')?.detail).toBe('ws://mirror');
  });

  it('has no providers without a config', () => {
    expect(providerRows(null)).toEqual([]);
  });

  it('formats config cells', () => {
    expect(configValue(null)).toBe('—');
    expect(configValue(['a', 'b'])).toBe('a, b');
    expect(configValue([])).toBe('—');
    expect(configValue(20)).toBe('20');
    expect(configValue({ a: 1 })).toBe('{"a":1}');
  });
});

describe('SettingsComponent', () => {
  let fixture: ComponentFixture<SettingsComponent>;
  let wa: WhatsappService;
  const el = () => fixture.nativeElement as HTMLElement;
  const settle = async () => {
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
  };

  async function setup(cfg: EngineConfig, log = { capacity: 1000, entries: [] as unknown[] }): Promise<void> {
    TestBed.configureTestingModule({
      imports: [SettingsComponent],
      providers: [{ provide: PLATFORM_DEPS, useValue: fakeDeps({ stored: { url: 'http://e', token: 't' } }) }],
    });
    wa = TestBed.inject(WhatsappService);
    spyOn(wa, 'config').and.resolveTo(cfg);
    spyOn(wa, 'debugLog').and.resolveTo(
      log as { capacity: number; entries: { seq: number; ts: number; kind: string; message: string }[] },
    );
    spyOn(wa, 'clearDebugLog').and.resolveTo(4);
    wa.engineStatus.set('online');
    fixture = TestBed.createComponent(SettingsComponent);
    await settle();
  }

  it('renders the redacted config keys', async () => {
    await setup(config());
    const dts = Array.from(el().querySelectorAll('[data-testid="config"] dt')).map((n) => n.textContent?.trim());
    expect(dts).toEqual([...CONFIG_KEYS]);
    expect(el().querySelector('[data-testid="config"]')?.textContent).toContain('<redacted>');
  });

  it('renders the provider list with status badges', async () => {
    await setup(config({ vectorSearch: true }));
    const items = el().querySelectorAll('[data-testid="providers"] li');
    expect(items.length).toBe(5);
    expect(el().querySelector('[data-provider="Vector search"]')?.getAttribute('data-status')).toBe('on');
  });

  it('shows the log tail', async () => {
    await setup(config(), {
      capacity: 1000,
      entries: [
        { seq: 1, ts: 1_700_000_000_000, kind: 'engine', message: 'first note' },
        { seq: 2, ts: 1_700_000_001_000, kind: 'event', message: 'connected' },
      ],
    });
    const items = el().querySelectorAll('[data-testid="debug-log"] li');
    expect(Array.from(items).map((i) => i.getAttribute('data-seq'))).toEqual(['2', '1']);
    expect(items[0].textContent).toContain('connected');
    expect(wa.debugLog).toHaveBeenCalled();
  });

  it('shows the log even when the diagnostic flag is off', async () => {
    await setup(config({ diagnostic: false }), {
      capacity: 1000,
      entries: [{ seq: 1, ts: 1_700_000_000_000, kind: 'engine', message: 'note' }],
    });
    expect(el().querySelector('[data-testid="debug-log"]')).not.toBeNull();
    expect(wa.debugLog).toHaveBeenCalled();
  });

  it('clears the log', async () => {
    await setup(config(), { capacity: 1000, entries: [{ seq: 1, ts: 1, kind: 'engine', message: 'x' }] });
    const clear = Array.from(el().querySelectorAll('button')).find((b) => b.textContent?.trim() === 'Clear')!;
    clear.click();
    await settle();
    expect(wa.clearDebugLog).toHaveBeenCalled();
    expect(el().querySelectorAll('[data-testid="debug-log"] li').length).toBe(0);
  });

  it('reports an engine status', async () => {
    await setup(config());
    expect(el().querySelector('[data-testid="engine-status"]')?.textContent).toContain('online');
  });
});
