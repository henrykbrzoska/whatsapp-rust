import { Component, DestroyRef, computed, effect, inject, signal, untracked } from '@angular/core';
import { DatePipe, JsonPipe } from '@angular/common';

import { TauriService } from '../../core/services/tauri.service';
import {
  DebugLogEntry,
  EngineConfig,
  WhatsappService,
} from '../../core/services/whatsapp.service';

/** One engine capability as Settings lists it. */
export interface ProviderRow {
  /** What the engine uses it for (`transport`, `messages`, …). */
  name: string;
  /** The implementation actually in use. */
  detail: string;
  status: 'on' | 'off' | 'default';
}

/**
 * Derives the provider list from `GET /config`. The engine has no `/providers`
 * route: every backend it can swap (embedder, transport endpoint, auth) is
 * already visible in the redacted config, so the list is computed from it.
 */
export function providerRows(c: EngineConfig | null): ProviderRow[] {
  if (!c) return [];
  return [
    {
      name: 'Messages store',
      detail: 'SQLite + FTS5 (trigram) — local, offline',
      status: 'on',
    },
    {
      name: 'Vector search',
      detail: c.vectorSearch
        ? 'fastembed MiniLM-L6-v2, 384d (sqlite-vec, cosine)'
        : 'disabled — the engine was built without the `fastembed` feature',
      status: c.vectorSearch ? 'on' : 'off',
    },
    {
      name: 'Transport',
      detail: c.waUrl ?? 'WhatsApp Web WebSocket (socket::frame_socket::URL)',
      status: c.waUrl ? 'on' : 'default',
    },
    {
      name: 'Auth',
      detail: c.auth ? 'bearer capability token per launch' : 'disabled (BEBOK_NO_AUTH=1)',
      status: c.auth ? 'on' : 'off',
    },
    {
      name: 'Event log',
      detail: 'ring buffer, 1000 entries (always on)',
      status: 'on',
    },
  ];
}

/** `GET /config` keys in the order the panel shows them. */
export const CONFIG_KEYS = [
  'addr',
  'auth',
  'token',
  'cors',
  'diagnostic',
  'dataDir',
  'waUrl',
  'handshakeTimeoutSecs',
  'vectorSearch',
] as const;

/** A config value as a table cell (`<redacted>` stays visible, not blank). */
export function configValue(v: unknown): string {
  if (v === null || v === undefined) return '—';
  if (Array.isArray(v)) return v.length ? v.join(', ') : '—';
  if (typeof v === 'object') return JSON.stringify(v);
  return String(v);
}

const LOG_POLL_MS = 2000;

@Component({
  selector: 'app-settings',
  imports: [DatePipe, JsonPipe],
  template: `
    <section class="page">
      <h1>Settings</h1>

      <h2>Engine</h2>
      <dl class="kv">
        <dt>Mode</dt>
        <dd>{{ tauri.isTauri ? 'Desktop (Tauri sidecar)' : 'Browser' }}</dd>
        <dt>URL</dt>
        <dd>{{ tauri.endpoint()?.url ?? 'not configured' }}</dd>
        <dt>Token</dt>
        <dd>{{ tauri.endpoint()?.token ? '•••• (stored)' : 'none' }}</dd>
        <dt>Status</dt>
        <dd [attr.data-testid]="'engine-status'">{{ wa.engineStatus() }}</dd>
        <dt>Events</dt>
        <dd>{{ tauri.sseState() }}</dd>
      </dl>

      @if (!tauri.isTauri) {
        <form class="connect" (submit)="$event.preventDefault(); connect()">
          <label for="ready">Paste the <code>BEBOK_READY</code> line printed by the engine</label>
          <input
            id="ready"
            name="ready"
            [value]="readyLine()"
            (input)="readyLine.set($any($event.target).value)"
            placeholder="BEBOK_READY http://127.0.0.1:8787/?token=…"
            autocomplete="off"
            spellcheck="false"
          />
          <div class="row">
            <button type="submit" class="primary">Connect</button>
            <button type="button" (click)="forget()" [disabled]="!tauri.configured()">Forget engine</button>
          </div>
        </form>
      }
      @if (message()) {
        <p [class.error]="isError()" [class.ok]="!isError()" role="status">{{ message() }}</p>
      }

      <h2>Providers</h2>
      @if (providers().length) {
        <ul class="providers" data-testid="providers">
          @for (p of providers(); track p.name) {
            <li [attr.data-provider]="p.name" [attr.data-status]="p.status">
              <span class="provider-name">{{ p.name }}</span>
              <span class="provider-detail">{{ p.detail }}</span>
              <span class="badge" [class.on]="p.status === 'on'" [class.off]="p.status === 'off'">{{ p.status }}</span>
            </li>
          }
        </ul>
      } @else {
        <p class="muted">Connect to an engine to see the providers it runs.</p>
      }

      @if (config(); as c) {
        <h2>Engine config <span class="muted">(secrets redacted)</span></h2>
        <dl class="kv" data-testid="config">
          @for (key of configKeys; track key) {
            <dt>{{ key }}</dt>
            <dd><code>{{ value(c[key]) }}</code></dd>
          }
        </dl>
        <details>
          <summary>Raw JSON</summary>
          <pre>{{ c | json }}</pre>
        </details>
      }

      <h2>Diagnostic log</h2>
      @if (!config()) {
        <p class="muted">Connect to an engine to read its log.</p>
      } @else {
        <div class="row">
          <button type="button" (click)="refreshLog()">Refresh</button>
          <button type="button" (click)="clearLog()">Clear</button>
          <label class="inline">
            <input type="checkbox" [checked]="autoRefresh()" (change)="toggleAutoRefresh($any($event.target).checked)" />
            Auto-refresh
          </label>
          <span class="muted">{{ log().length }} / {{ capacity() }} entries</span>
        </div>
        @if (logError()) {
          <p class="error" role="alert">{{ logError() }}</p>
        }
        @if (log().length) {
          <ol class="log" data-testid="debug-log" reversed>
            @for (e of log(); track e.seq) {
              <li [attr.data-kind]="e.kind" [attr.data-seq]="e.seq">
                <time>{{ e.ts | date: 'HH:mm:ss.SSS' }}</time>
                <span class="kind">{{ e.kind }}</span>
                <span class="msg">{{ e.message }}</span>
              </li>
            }
          </ol>
        } @else {
          <p class="muted">The log is empty.</p>
        }
      }
    </section>
  `,
})
export class SettingsComponent {
  protected readonly tauri = inject(TauriService);
  protected readonly wa = inject(WhatsappService);
  private readonly destroyRef = inject(DestroyRef);

  readonly readyLine = signal('');
  readonly message = signal<string | null>(null);
  readonly isError = signal(false);
  readonly config = signal<EngineConfig | null>(null);
  readonly log = signal<DebugLogEntry[]>([]);
  readonly logError = signal<string | null>(null);
  readonly capacity = signal(0);
  readonly autoRefresh = signal(false);

  readonly providers = computed(() => providerRows(this.config()));
  protected readonly configKeys = CONFIG_KEYS;
  protected readonly value = configValue;

  private timer: ReturnType<typeof setInterval> | null = null;

  constructor() {
    effect(() => {
      if (this.wa.engineStatus() === 'online') untracked(() => void this.loadConfig());
      else {
        this.config.set(null);
        this.log.set([]);
        this.stopTimer();
      }
    });
    this.destroyRef.onDestroy(() => this.stopTimer());
  }

  async connect(): Promise<void> {
    try {
      this.tauri.connectWith(this.readyLine());
    } catch (e) {
      this.show(e instanceof Error ? e.message : String(e), true);
      return;
    }
    this.readyLine.set('');
    await this.wa.restart();
    if (this.wa.engineStatus() === 'online') this.show(`Connected to engine ${this.wa.version()?.version ?? ''}.`, false);
    else this.show(`Saved, but the engine did not answer: ${this.wa.engineError() ?? this.wa.engineStatus()}`, true);
  }

  async forget(): Promise<void> {
    this.tauri.setEndpoint(null);
    await this.wa.restart();
    this.show('Engine forgotten.', false);
  }

  /** Reloads `GET /config`, then the log when diagnostics are on. */
  async loadConfig(): Promise<void> {
    try {
      const c = await this.wa.config();
      this.config.set(c);
      await this.refreshLog();
    } catch {
      this.config.set(null);
    }
  }

  /** `GET /debug/log` — newest first, capped by `limit`. */
  async refreshLog(limit = 200): Promise<void> {
    try {
      const res = await this.wa.debugLog(limit);
      this.capacity.set(res.capacity);
      this.log.set([...res.entries].sort((a, b) => b.seq - a.seq));
      this.logError.set(null);
    } catch (e) {
      this.logError.set(e instanceof Error ? e.message : String(e));
    }
  }

  async clearLog(): Promise<void> {
    try {
      await this.wa.clearDebugLog();
      this.log.set([]);
      this.logError.set(null);
    } catch (e) {
      this.logError.set(e instanceof Error ? e.message : String(e));
    }
  }

  toggleAutoRefresh(on: boolean): void {
    this.autoRefresh.set(on);
    if (on) this.startTimer();
    else this.stopTimer();
  }

  private startTimer(): void {
    this.stopTimer();
    this.timer = setInterval(() => void this.refreshLog(), LOG_POLL_MS);
  }

  private stopTimer(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  private show(msg: string, error: boolean): void {
    this.message.set(msg);
    this.isError.set(error);
  }
}
