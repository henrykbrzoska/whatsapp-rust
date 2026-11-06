import { Component, DestroyRef, computed, effect, inject, input, signal, untracked, viewChild } from '@angular/core';
import { RouterLink } from '@angular/router';

import { formatTimestamp, initial, isGroupJid, shortJid } from '../../core/format';
import { ChatStats, EXPORT_FORMATS, ExportFormat, Jid, StoredMessage, WaEvent } from '../../core/models';
import { ContactNames, baseJid } from '../../core/services/contact-names';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { ComposerComponent } from '../messages/composer.component';
import { MessageListComponent } from '../messages/message-list.component';
import { SearchBarComponent } from '../search/search-bar.component';
import { PresenceStore } from './chat-list.models';

/** How many messages one page of history holds. */
const PAGE_SIZE = 50;
/** How many messages one `POST /history/fetch` asks the phone for. */
const HISTORY_FETCH_COUNT = 200;

/**
 * One chat: the header, the message thread and the composer — the whole
 * right-hand pane of the app.
 *
 * The thread is read from the engine's local database
 * (`GET /chats/:jid/messages`), so it opens instantly and works offline; live
 * messages are appended from the `wa-event` stream. Sending goes through
 * `POST /send/message`, which stores the sent message and hands it back, so
 * the bubble that appears is the row the engine kept.
 *
 * Search and "download full chat history" live behind the header's icons:
 * this pane is a conversation first.
 */
@Component({
  selector: 'app-chat',
  imports: [RouterLink, SearchBarComponent, MessageListComponent, ComposerComponent],
  template: `
    <section class="thread" [attr.data-chat]="jid()">
      <header class="thread-header">
        <a routerLink="/" class="back" aria-label="Back to chats">←</a>
        <span class="avatar" aria-hidden="true">
          {{ initial(title()) }}
          @if (presenceState(); as p) {
            <span class="presence-dot" [class]="p" [attr.data-presence]="p" [title]="p"></span>
          }
        </span>
        <div class="who">
          <h1 data-testid="chat-title">{{ title() }}</h1>
          <span class="muted subtitle" data-testid="chat-subtitle">{{ subtitle() }}</span>
        </div>

        <button
          type="button"
          class="icon"
          data-testid="toggle-search"
          [class.active]="showSearch()"
          [attr.aria-pressed]="showSearch()"
          title="Search in this chat"
          aria-label="Search in this chat"
          (click)="showSearch.set(!showSearch())"
        >
          🔍
        </button>

        <details class="menu" #menu>
          <summary title="Chat menu" aria-label="Chat menu">⋮</summary>
          <div class="menu-panel">
            @if (stats(); as s) {
              <p class="muted stats">
                {{ s.count }} {{ s.count === 1 ? 'message' : 'messages' }}
                @if (s.count > 0) {
                  · {{ formatTimestamp(s.first_timestamp) }} – {{ formatTimestamp(s.last_timestamp) }}
                }
              </p>
            }
            <p class="muted jid">{{ jid() }}</p>
            @if (isGroup()) {
              <a [routerLink]="['/group', jid()]">Group info</a>
            }
            <div class="export">
              <label>
                Format
                <select
                  data-testid="export-format"
                  [value]="format()"
                  (change)="format.set($any($event.target).value)"
                  aria-label="Export format"
                >
                  @for (f of formats; track f) {
                    <option [value]="f">{{ f.toUpperCase() }}</option>
                  }
                </select>
              </label>
              <button type="button" class="primary" data-testid="export" (click)="download()" [disabled]="exporting()">
                {{ exporting() ? 'Exporting…' : 'Download full chat history' }}
              </button>
            </div>
          </div>
        </details>
      </header>

      @if (showSearch()) {
        <div class="thread-search">
          <app-search-bar [chat]="jid()" placeholder="Search this chat" />
        </div>
      }

      @if (exporting()) {
        <div class="export-progress">
          @if (progress(); as p) {
            @if (p.total > 0) {
              <progress [value]="p.exported" [max]="p.total" aria-label="Export progress"></progress>
              <span class="muted">{{ p.exported }} / {{ p.total }}</span>
            } @else {
              <progress aria-label="Export progress"></progress>
              <span class="muted">Preparing export…</span>
            }
          } @else {
            <progress aria-label="Export progress"></progress>
          }
        </div>
      }
      @if (exportMessage()) {
        <p class="ok" role="status">{{ exportMessage() }}</p>
      }
      @if (error()) {
        <p class="error" role="alert">{{ error() }}</p>
      }

      @if (loading() && messages().length === 0) {
        <p class="muted thread-empty">Loading messages…</p>
      } @else if (messages().length === 0) {
        <div class="thread-empty">
          <p class="muted">No messages in this chat yet.</p>
          <p class="muted">Say hello — the message goes out as soon as the engine is connected.</p>
        </div>
      } @else {
        <app-message-list
          [messages]="messages()"
          [hasMore]="hasMore()"
          [loadingOlder]="loadingOlder()"
          [isGroup]="isGroup()"
          [names]="names.names()"
          [canFetch]="canFetch()"
          [fetchState]="fetchState()"
          [fetchNote]="fetchNote()"
          (loadOlder)="loadOlder()"
          (fetchOlder)="fetchOlder()"
        />
      }

      <app-composer
        [sending]="sending()"
        [disabled]="!canSend()"
        [error]="sendError()"
        [placeholder]="composerHint()"
        (send)="sendMessage($event)"
        (typing)="announceTyping($event)"
      />
    </section>
  `,
})
export class ChatComponent {
  private readonly wa = inject(WhatsappService);
  protected readonly names = inject(ContactNames);
  private readonly presence = inject(PresenceStore);
  protected readonly formatTimestamp = formatTimestamp;
  protected readonly initial = initial;
  protected readonly formats = EXPORT_FORMATS;

  /** Route parameter `chat/:jid` (component input binding). */
  readonly jid = input.required<Jid>();

  private readonly list = viewChild(MessageListComponent);
  private readonly composer = viewChild(ComposerComponent);

  readonly messages = signal<StoredMessage[]>([]);
  readonly hasMore = signal(false);
  readonly loading = signal(false);
  readonly loadingOlder = signal(false);
  readonly sending = signal(false);
  readonly sendError = signal<string | null>(null);
  readonly stats = signal<ChatStats | null>(null);
  readonly format = signal<ExportFormat>('json');
  readonly exporting = signal(false);
  readonly exportMessage = signal<string | null>(null);
  readonly error = signal<string | null>(null);
  readonly showSearch = signal(false);
  readonly progress = computed(() => this.wa.exportProgress()[this.jid()] ?? null);
  /** `waiting` while a `POST /history/fetch` request is out for this chat. */
  readonly fetchState = signal<'idle' | 'waiting'>('idle');
  /** What the last fetch did — the thread shows it above the oldest bubble. */
  readonly fetchNote = signal<string | null>(null);
  /** Incremented by every [`open`]; see the guard in [`current`]. */
  private openGeneration = 0;

  readonly isGroup = computed(() => isGroupJid(this.jid()));
  /** The contact / group name, falling back to the JID's user part. */
  readonly title = computed(() => this.names.lookup(this.jid()) ?? shortJid(this.jid()));
  readonly presenceState = computed(() => (this.isGroup() ? undefined : this.presence.get(this.jid())));
  readonly subtitle = computed(() => {
    const state = this.presenceState();
    if (state === 'composing') return 'typing…';
    if (state === 'recording') return 'recording audio…';
    if (this.isGroup()) return 'Group chat';
    if (state === 'online') return 'online';
    return this.jid().split('@')[0];
  });
  /** Sending needs a live, paired session. */
  readonly canSend = computed(() => this.wa.engineStatus() === 'online' && this.wa.connectionState() === 'connected');
  /**
   * The local pages are exhausted, but the phone can still be asked.
   *
   * `POST /history/fetch` has to name a message to go back from, so a chat
   * with nothing stored cannot be anchored — and it needs a live session.
   */
  readonly canFetch = computed(() => !this.hasMore() && this.messages().length > 0 && this.canSend());
  readonly composerHint = computed(() => {
    if (this.canSend()) return 'Type a message';
    if (this.wa.engineStatus() !== 'online') return 'Engine offline — messages cannot be sent';
    switch (this.wa.connectionState()) {
      case 'connecting':
        return 'Connecting to WhatsApp…';
      case 'logged_out':
      case 'qr':
        return 'Pair this device on the Login page to send messages';
      default:
        return this.wa.paired() ? 'Reconnecting to WhatsApp…' : 'Pair this device on the Login page to send messages';
    }
  });

  constructor() {
    // (Re)load whenever the route JID changes or the engine comes online.
    effect(() => {
      const jid = this.jid();
      if (this.wa.engineStatus() !== 'online') return;
      untracked(() => void this.open(jid));
    });

    const off = this.wa.onEvent((e) => this.onEngineEvent(e));
    inject(DestroyRef).onDestroy(off);
  }

  /**
   * Open a chat: its newest page, the stats and the names of its people.
   *
   * Every open gets a generation, and a page is only adopted while its own
   * generation is still the current one — the route effect can start a second
   * open for the *same* chat (the engine coming online, a re-entered route),
   * and a slow first page must not wipe the newer one.
   */
  async open(jid: Jid): Promise<void> {
    const generation = ++this.openGeneration;
    this.messages.set([]);
    this.hasMore.set(false);
    this.sendError.set(null);
    this.error.set(null);
    this.fetchState.set('idle');
    this.fetchNote.set(null);
    this.loading.set(true);
    void this.names.load();
    void this.loadStats(jid);
    if (this.isGroup()) void this.loadGroupName(jid);
    try {
      const page = await this.wa.listMessages(jid, { limit: PAGE_SIZE });
      // Ignore a page that arrived after the user moved on, or after a newer
      // open of the same chat already filled the thread.
      if (!this.current(jid, generation)) return;
      const messages = page?.messages ?? [];
      this.names.rememberFromMessages(messages);
      this.messages.set(messages);
      this.hasMore.set(page?.hasMore ?? false);
      void this.markRead();
    } catch (e) {
      if (this.current(jid, generation)) this.error.set(`Could not load messages: ${message(e)}`);
    } finally {
      if (this.current(jid, generation)) this.loading.set(false);
    }
  }

  /** `true` while `jid`'s `generation`-th open is still the one on screen. */
  private current(jid: Jid, generation: number): boolean {
    return jid === this.jid() && generation === this.openGeneration;
  }

  /** One page further back, triggered by scrolling to the top. */
  async loadOlder(): Promise<void> {
    const oldest = this.messages()[0];
    if (!oldest || this.loadingOlder() || !this.hasMore()) return;
    const jid = this.jid();
    const generation = this.openGeneration;
    this.loadingOlder.set(true);
    try {
      const page = await this.wa.listMessages(jid, {
        before: { timestamp: oldest.timestamp, id: oldest.id },
        limit: PAGE_SIZE,
      });
      if (!this.current(jid, generation)) return;
      const older = page?.messages ?? [];
      this.names.rememberFromMessages(older);
      if (older.length > 0) this.messages.update((current) => [...older, ...current]);
      this.hasMore.set(page?.hasMore ?? false);
    } catch (e) {
      if (this.current(jid, generation)) this.error.set(`Could not load earlier messages: ${message(e)}`);
    } finally {
      this.loadingOlder.set(false);
    }
  }

  /**
   * The local history is exhausted — ask our own phone for what came before.
   *
   * This is the second step of "load earlier": the engine sends a peer
   * request and the phone answers minutes later with an encrypted blob, so
   * nothing arrives synchronously. The thread stays in `waiting` until a
   * `history_imported` event lands.
   */
  async fetchOlder(): Promise<void> {
    if (this.fetchState() === 'waiting') return;
    const jid = this.jid();
    this.fetchState.set('waiting');
    this.fetchNote.set(null);
    try {
      const report = await this.wa.fetchHistory(jid, HISTORY_FETCH_COUNT);
      if (jid !== this.jid()) return;
      if (report.requested.length === 0) {
        this.fetchState.set('idle');
        this.fetchNote.set(report.skipped[0] ?? 'WhatsApp had nothing older to send.');
        return;
      }
      this.fetchNote.set(`Asked the phone for ${report.count} older message(s) — this can take a few minutes.`);
    } catch (e) {
      if (jid !== this.jid()) return;
      this.fetchState.set('idle');
      this.fetchNote.set(`Could not ask for older messages: ${message(e)}`);
    }
  }

  /**
   * An imported history blob can hold messages older than the thread's first
   * bubble, so walk one page further back and prepend what arrived. The
   * message list keeps the reading position on the row that was first.
   */
  private async absorbImport(): Promise<void> {
    const jid = this.jid();
    const generation = this.openGeneration;
    const oldest = this.messages()[0];
    const waiting = this.fetchState() === 'waiting';
    try {
      const page = await this.wa.listMessages(jid, {
        before: oldest ? { timestamp: oldest.timestamp, id: oldest.id } : null,
        limit: PAGE_SIZE,
      });
      if (!this.current(jid, generation)) return;
      const older = page?.messages ?? [];
      this.names.rememberFromMessages(older);
      if (older.length > 0) this.messages.update((current) => [...older, ...current]);
      this.hasMore.set(page?.hasMore ?? false);
      if (waiting) {
        this.fetchNote.set(
          older.length > 0
            ? `${older.length} older message(s) arrived.`
            : 'The phone sent history, but nothing older for this chat.',
        );
      }
      void this.loadStats(jid);
    } catch (e) {
      if (this.current(jid, generation)) this.error.set(`Could not load the imported messages: ${message(e)}`);
    } finally {
      if (this.current(jid, generation) && waiting) this.fetchState.set('idle');
    }
  }

  /**
   * The group's subject, so the header reads "Family" instead of
   * `Group 1203…`. Needs a live session; without one the JID stays.
   */
  private async loadGroupName(jid: Jid): Promise<void> {
    if (this.names.lookup(jid)) return;
    try {
      const info = await this.wa.groupInfo(jid);
      if (info?.name) this.names.remember(jid, info.name);
    } catch {
      // No session (or not a member any more) — the fallback label stands.
    }
  }

  async loadStats(jid: Jid): Promise<void> {
    try {
      const stats = await this.wa.chatStats(jid);
      if (jid === this.jid()) this.stats.set(stats);
    } catch (e) {
      this.error.set(message(e));
    }
  }

  /** Send the draft; the engine answers with the row it stored. */
  async sendMessage(text: string): Promise<void> {
    this.sending.set(true);
    this.sendError.set(null);
    try {
      const result = await this.wa.sendMessage(this.jid(), text);
      if (result?.message) this.append(result.message);
      this.composer()?.clear();
      this.list()?.scrollToBottom();
      void this.loadStats(this.jid());
    } catch (e) {
      this.sendError.set(`Not sent: ${message(e)}`);
    } finally {
      this.sending.set(false);
    }
  }

  /** Mirror the draft as a typing indicator, like the phone client does. */
  async announceTyping(typing: boolean): Promise<void> {
    if (!this.canSend()) return;
    try {
      await this.wa.sendChatPresence(this.jid(), typing ? 'composing' : 'paused');
    } catch {
      // A failed presence update must never interrupt typing.
    }
  }

  async download(): Promise<void> {
    this.exporting.set(true);
    this.error.set(null);
    this.exportMessage.set(null);
    try {
      const { filename, size, path } = await this.wa.exportHistory(this.jid(), this.format());
      this.exportMessage.set(`Saved ${filename} (${formatSize(size)})${path ? ` to ${path}` : ''}.`);
    } catch (e) {
      this.error.set(`Export failed: ${message(e)}`);
    } finally {
      this.exporting.set(false);
    }
  }

  /** Live events for this chat: new messages, and our own echoes. */
  private onEngineEvent(e: WaEvent): void {
    // A blob the phone sent can hold messages older than this thread's first
    // bubble — whether we asked for it or it came with the initial sync.
    if (e.type === 'history_imported') {
      if (e.stored > 0 || this.fetchState() === 'waiting') void this.absorbImport();
      return;
    }
    if (e.type !== 'message') return;
    if (baseJid(e.info.chat) !== baseJid(this.jid())) return;
    this.append({
      id: e.info.id,
      chat_jid: baseJid(e.info.chat),
      sender: e.info.sender,
      timestamp: e.info.timestamp,
      type: e.info.type || 'text',
      text: e.text,
      from_me: e.info.isFromMe,
      push_name: e.info.pushName || null,
    });
    if (!e.info.isFromMe) void this.markRead([e.info.id], e.info.sender);
  }

  /** Append (or replace, on a re-delivery) one message. */
  private append(row: StoredMessage): void {
    this.messages.update((current) => {
      const index = current.findIndex((m) => m.id === row.id);
      if (index >= 0) {
        const next = [...current];
        next[index] = row;
        return next;
      }
      // Live messages arrive in order; a late one still lands in place.
      const at = current.findIndex((m) => m.timestamp > row.timestamp);
      if (at < 0) return [...current, row];
      return [...current.slice(0, at), row, ...current.slice(at)];
    });
    void this.loadStats(this.jid());
  }

  /** Acknowledge incoming messages (best effort — a read receipt is not UI). */
  private async markRead(ids?: readonly string[], sender?: Jid): Promise<void> {
    if (!this.canSend()) return;
    const incoming = ids ?? this.messages().filter((m) => !m.from_me).map((m) => m.id);
    if (incoming.length === 0) return;
    try {
      await this.wa.markRead(this.jid(), incoming.slice(-100), this.isGroup() ? sender : undefined);
    } catch {
      // Receipts are fire-and-forget; a failure must not disturb reading.
    }
  }
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
