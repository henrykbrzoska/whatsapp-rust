import { Component, DestroyRef, computed, effect, inject, signal, untracked } from '@angular/core';
import { RouterLink, RouterLinkActive } from '@angular/router';

import { formatTimestamp, initial, shortJid } from '../../core/format';
import { ContactNames } from '../../core/services/contact-names';
import type { AppStateSyncReport } from '../../core/services/whatsapp.service';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { SearchBarComponent } from '../search/search-bar.component';
import { ChatListEntry, isMuted, PresenceStore, sortChats } from './chat-list.models';

@Component({
  selector: 'app-chat-list',
  imports: [RouterLink, RouterLinkActive, SearchBarComponent],
  template: `
    <section class="page chat-pane">
      <header class="page-header">
        <h1>Chats</h1>
        <div class="row">
          <button
            type="button"
            class="button"
            data-testid="sync-contacts"
            [disabled]="syncing()"
            title="Pull the address book and chat flags from WhatsApp, so chats show names instead of IDs"
            (click)="syncContacts()"
          >
            {{ syncing() ? 'Syncing…' : 'Sync names' }}
          </button>
          <button
            type="button"
            class="button"
            data-testid="fetch-history"
            [disabled]="fetching()"
            title="Ask the phone for older messages in every chat it already knows. A chat with no stored message cannot be asked about — those arrive with the initial history sync at pairing."
            (click)="fetchHistory()"
          >
            {{ fetching() ? 'Asking…' : 'Fetch history' }}
          </button>
          <a class="button" data-testid="new-group" routerLink="/group/new">New group</a>
        </div>
      </header>
      @if (syncMessage()) {
        <p class="muted sync-result" role="status" data-testid="sync-result">{{ syncMessage() }}</p>
      }
      @if (fetchMessage()) {
        <p class="muted sync-result" role="status" data-testid="fetch-result">{{ fetchMessage() }}</p>
      }
      <details class="sidebar-search">
        <summary>Search all messages</summary>
        <app-search-bar placeholder="Search all chats" />
      </details>

      @if (error()) {
        <p class="error" role="alert">{{ error() }}</p>
      } @else if (loading()) {
        <p class="muted">Loading chats…</p>
      } @else if (chats().length === 0) {
        <p class="muted empty">
          No chats yet. Pair the device on the <a routerLink="/login">Login</a> page — conversations appear here as
          soon as the engine receives them.
        </p>
      } @else {
        <ul class="chat-list">
          @for (c of sorted(); track c.jid) {
            <li [attr.data-jid]="c.jid" [class.pinned]="c.pinned" [class.archived]="c.archived">
              <a [routerLink]="linkFor(c.jid)" routerLinkActive="active">
                <span class="avatar" aria-hidden="true">
                  {{ initial(nameOf(c)) }}
                  @if (presence.get(c.jid); as p) {
                    <span class="presence-dot" [class]="p" [attr.data-presence]="p" [title]="p"></span>
                  }
                </span>
                <span class="body">
                  <span class="title">{{ nameOf(c) }}</span>
                  <span class="preview">{{ c.last_text ?? '' }}</span>
                </span>
                <span class="side">
                  <time>{{ formatTimestamp(c.last_timestamp) }}</time>
                  <span class="flags">
                    @if (c.pinned) {
                      <span class="flag pin" data-testid="pinned" title="Pinned" aria-label="Pinned">📌</span>
                    }
                    @if (isMuted(c)) {
                      <span class="flag mute" data-testid="muted" title="Muted" aria-label="Muted">🔇</span>
                    }
                    @if (c.archived) {
                      <span class="flag archive" data-testid="archived" title="Archived" aria-label="Archived">🗄</span>
                    }
                    <span class="badge" title="Messages">{{ c.count }}</span>
                  </span>
                </span>
              </a>
            </li>
          }
        </ul>
      }
    </section>
  `,
})
export class ChatListComponent {
  private readonly wa = inject(WhatsappService);
  protected readonly presence = inject(PresenceStore);
  private readonly names = inject(ContactNames);
  protected readonly formatTimestamp = formatTimestamp;
  protected readonly shortJid = shortJid;
  protected readonly initial = initial;
  protected readonly isMuted = (c: ChatListEntry) => isMuted(c);

  readonly chats = signal<ChatListEntry[]>([]);
  readonly sorted = computed(() => sortChats(this.chats()));
  readonly loading = signal(false);
  readonly error = signal<string | null>(null);
  readonly syncing = signal(false);
  readonly syncMessage = signal<string | null>(null);
  readonly fetching = signal(false);
  readonly fetchMessage = signal<string | null>(null);

  constructor() {
    // Reload once the engine comes online (e.g. after Settings → Connect).
    effect(() => {
      if (this.wa.engineStatus() !== 'online') return;
      untracked(() => {
        void this.load();
        void this.names.load();
      });
    });
    // A new message changes the preview and the order of the list; an
    // imported history blob changes the counts (and can add whole chats).
    const off = this.wa.onEvent((e) => {
      if (this.wa.engineStatus() !== 'online') return;
      if (e.type === 'message' || (e.type === 'history_imported' && e.stored > 0)) void this.load();
    });
    inject(DestroyRef).onDestroy(off);
  }

  /** Every row opens the conversation; group details sit in the chat menu. */
  linkFor(jid: string): string[] {
    return ['/chat', jid];
  }

  /**
   * The row label: the name the engine resolved (address book / push name),
   * then anything the client learned since, then the JID's user part — never
   * a bare ID when a name is known.
   */
  nameOf(c: ChatListEntry): string {
    return c.name || this.names.lookup(c.jid) || shortJid(c.jid);
  }

  /**
   * Pull contacts and chat flags from WhatsApp, then reload the list. The
   * engine does this once per connection on its own; this button is for
   * "the names are still missing" (a chat that was never in the address
   * book, or a sync that ran before the key share arrived).
   */
  async syncContacts(): Promise<void> {
    this.syncing.set(true);
    this.syncMessage.set(null);
    try {
      const report = await this.wa.syncAppState();
      this.syncMessage.set(syncSummary(report));
      await this.names.load(true);
      await this.load();
    } catch (e) {
      this.syncMessage.set(`Sync failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      this.syncing.set(false);
    }
  }

  /**
   * `POST /history/fetch` for every chat the engine has a message for.
   *
   * The request has to name a message to go back from, so a chat with nothing
   * stored cannot be anchored — the complete chat list arrives with the
   * initial history sync at pairing, which the engine imports on its own.
   * The blobs land minutes later as `history_imported` events.
   */
  async fetchHistory(): Promise<void> {
    this.fetching.set(true);
    this.fetchMessage.set(null);
    try {
      const report = await this.wa.fetchHistory();
      const asked = `Asked the phone for ${report.count} older message(s) in ${report.requested.length} chat(s).`;
      this.fetchMessage.set(
        report.skipped.length > 0
          ? `${asked} Skipped ${report.skipped.length}: ${report.skipped.slice(0, 3).join('; ')}`
          : `${asked} They arrive in the background — the chats update themselves.`,
      );
    } catch (e) {
      this.fetchMessage.set(`Could not ask for history: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      this.fetching.set(false);
    }
  }

  async load(): Promise<void> {
    this.loading.set(true);
    this.error.set(null);
    try {
      const chats = await this.wa.listChats();
      // The engine resolved these names (address book / push name) — share
      // them so the open conversation and the search results use them too.
      for (const c of chats) if (c.name) this.names.remember(c.jid, c.name);
      this.chats.set(chats);
    } catch (e) {
      this.error.set(e instanceof Error ? e.message : String(e));
    } finally {
      this.loading.set(false);
    }
  }
}

/**
 * One honest line about what an app-state sync did.
 *
 * "0 contacts, no errors" is the shape of a sync whose patches could not be
 * decrypted: the mutations are encrypted with keys only the primary phone
 * has, and until a key share arrives nothing can be applied. The engine asks
 * for them (once a day per key), so the useful message is "ask again in a
 * moment", not "synced 0".
 */
export function syncSummary(report: AppStateSyncReport): string {
    const applied = `Synced ${report.contacts} contact name(s) and ${report.chatSettings} chat setting(s).`;
    if (report.errors.length > 0) return `Sync incomplete: ${report.errors.join('; ')}`;
    if (report.requestedKeys?.length) {
      return (
        `${applied} This device is still missing ${report.requestedKeys.length} app-state key(s); ` +
        `the phone was asked for them — try Sync names again in a few seconds.`
      );
    }
    if (report.contacts === 0 && report.skipped?.length) {
      return `Nothing could be applied: ${report.skipped.join('; ')}`;
    }
    return applied;
}
