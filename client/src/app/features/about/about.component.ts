import { Component, computed, effect, inject, signal, untracked } from '@angular/core';
import { DatePipe } from '@angular/common';

import type { ChatListEntry } from '../../types/specta';
import { TauriService } from '../../core/services/tauri.service';
import { WhatsappService } from '../../core/services/whatsapp.service';

export const CLIENT_VERSION = '0.1.0';
export const APP_AUTHOR = 'Henryk Brzoska';

/** Aggregate of the local library, derived from `GET /chats`. */
export interface LibraryStats {
  chats: number;
  groups: number;
  newsletters: number;
  /** Sum of the per-chat message counts. */
  messages: number;
  /** Unix seconds of the newest message, `null` for an empty library. */
  lastActivity: number | null;
}

/** Totals over the chat list — one engine call instead of one per chat. */
export function libraryStats(entries: readonly ChatListEntry[]): LibraryStats {
  let groups = 0;
  let newsletters = 0;
  let messages = 0;
  let lastActivity: number | null = null;
  for (const c of entries) {
    if (c.jid.endsWith('@g.us')) groups++;
    else if (c.jid.endsWith('@newsletter')) newsletters++;
    messages += c.count;
    if (c.last_timestamp && (lastActivity === null || c.last_timestamp > lastActivity)) {
      lastActivity = c.last_timestamp;
    }
  }
  return { chats: entries.length, groups, newsletters, messages, lastActivity };
}

@Component({
  selector: 'app-about',
  imports: [DatePipe],
  template: `
    <section class="page">
      <h1>About</h1>
      <p>A local WhatsApp client — Angular (standalone, signals) + Tauri 2 + a Rust engine.</p>

      <h2>Versions</h2>
      <dl class="kv">
        <dt>Author</dt>
        <dd>{{ author }}</dd>
        <dt>Client</dt>
        <dd data-testid="client-version">{{ clientVersion }}</dd>
        <dt>Engine</dt>
        <dd data-testid="engine-version">{{ wa.version()?.version ?? 'not connected' }}</dd>
        <dt>Protocol ref</dt>
        <dd><code data-testid="protocol-ref">{{ wa.version()?.protocolRef ?? '—' }}</code></dd>
        <dt>Mode</dt>
        <dd>{{ tauri.isTauri ? 'Desktop (Tauri sidecar)' : 'Browser' }}</dd>
        <dt>Engine URL</dt>
        <dd>{{ tauri.endpoint()?.url ?? 'not configured' }}</dd>
      </dl>

      <h2>Local library</h2>
      @if (stats(); as s) {
        <dl class="kv" data-testid="stats">
          <dt>Chats</dt>
          <dd>{{ s.chats }} ({{ s.groups }} groups, {{ s.newsletters }} newsletters)</dd>
          <dt>Messages</dt>
          <dd>{{ s.messages }}</dd>
          <dt>Last activity</dt>
          <dd>{{ s.lastActivity ? (s.lastActivity * 1000 | date: 'medium') : '—' }}</dd>
        </dl>
      } @else if (loading()) {
        <p class="muted">Reading the local library…</p>
      } @else {
        <p class="muted">Connect to an engine to see the local library size.</p>
      }

      <p class="muted">Third-party licenses: see <code>THIRD_PARTY_NOTICES.md</code>.</p>
    </section>
  `,
})
export class AboutComponent {
  protected readonly wa = inject(WhatsappService);
  protected readonly tauri = inject(TauriService);
  protected readonly clientVersion = CLIENT_VERSION;
  protected readonly author = APP_AUTHOR;
  protected readonly stats = signal<LibraryStats | null>(null);
  protected readonly loading = computed(() => this.wa.engineStatus() === 'online' && this.stats() === null);

  constructor() {
    effect(() => {
      if (this.wa.engineStatus() === 'online') untracked(() => void this.loadStats());
      else this.stats.set(null);
    });
  }

  async loadStats(): Promise<void> {
    try {
      this.stats.set(libraryStats(await this.wa.listChats()));
    } catch {
      this.stats.set(null);
    }
  }
}
