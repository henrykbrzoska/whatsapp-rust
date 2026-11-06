import { Component, inject, input, signal } from '@angular/core';
import { RouterLink } from '@angular/router';

import { formatTimestamp, shortJid } from '../../core/format';
import { Jid, SEARCH_MODES, SearchHit, SearchMode } from '../../core/models';
import { ContactNames } from '../../core/services/contact-names';
import { WhatsappService } from '../../core/services/whatsapp.service';

/** Search bar with a keyword / vector / hybrid toggle (`GET /search`). */
@Component({
  selector: 'app-search-bar',
  imports: [RouterLink],
  template: `
    <form class="search-bar" role="search" (submit)="$event.preventDefault(); run()">
      <input
        type="search"
        name="q"
        [placeholder]="placeholder()"
        [value]="query()"
        (input)="query.set($any($event.target).value)"
        aria-label="Search messages"
      />
      <div class="mode-toggle" role="group" aria-label="Search mode">
        @for (m of modes; track m) {
          <button type="button" [class.active]="mode() === m" [attr.aria-pressed]="mode() === m" (click)="setMode(m)">
            {{ m }}
          </button>
        }
      </div>
      <button type="submit" class="primary" [disabled]="loading()">Search</button>
    </form>

    @if (error()) {
      <p class="error" role="alert">{{ error() }}</p>
    }
    @if (searched() && !error()) {
      @if (vectorUnavailable()) {
        <p class="hint">
          Vector search is unavailable (engine built without <code>fastembed</code>)@if (mode() === 'hybrid') {
            — hybrid falls back to keyword matches}.
        </p>
      }
      @if (results().length === 0) {
        <p class="muted">No messages match “{{ lastQuery() }}”.</p>
      } @else {
        <ul class="search-results">
          @for (hit of results(); track hit.message.id) {
            <li>
              <div class="meta">
                @if (!chat()) {
                  <a [routerLink]="['/chat', hit.message.chat_jid]">{{ nameOf(hit.message.chat_jid) }}</a> ·
                }
                <span>{{ hit.message.from_me ? 'You' : nameOf(hit.message.sender) }}</span> ·
                <time>{{ formatTimestamp(hit.message.timestamp) }}</time>
                <span class="score" title="Relevance score">{{ hit.score.toFixed(3) }}</span>
              </div>
              <div class="text">{{ hit.message.text ?? '(' + hit.message.type + ')' }}</div>
            </li>
          }
        </ul>
      }
    }
  `,
})
export class SearchBarComponent {
  private readonly wa = inject(WhatsappService);
  private readonly names = inject(ContactNames);

  /** Restrict the search to one chat (`?chat=`); `null` = all chats. */
  readonly chat = input<Jid | null>(null);
  readonly placeholder = input('Search messages');

  protected readonly modes = SEARCH_MODES;
  protected readonly formatTimestamp = formatTimestamp;
  protected readonly shortJid = shortJid;

  readonly query = signal('');
  readonly mode = signal<SearchMode>('hybrid');

  /** A contact / group name when one is known, the JID's user part otherwise. */
  protected nameOf(jid: Jid): string {
    return this.names.nameOf(jid);
  }
  readonly results = signal<SearchHit[]>([]);
  readonly loading = signal(false);
  readonly error = signal<string | null>(null);
  readonly searched = signal(false);
  readonly lastQuery = signal('');
  readonly vectorUnavailable = signal(false);

  setMode(m: SearchMode): void {
    this.mode.set(m);
    if (this.query().trim()) void this.run();
  }

  async run(): Promise<void> {
    const q = this.query().trim();
    if (!q) {
      this.results.set([]);
      this.searched.set(false);
      this.error.set(null);
      return;
    }
    this.loading.set(true);
    this.error.set(null);
    try {
      const res = await this.wa.search(q, { mode: this.mode(), chat: this.chat() });
      this.results.set(res.results);
      this.vectorUnavailable.set(this.mode() !== 'keyword' && !res.vector);
      this.lastQuery.set(q);
      this.searched.set(true);
    } catch (e) {
      this.error.set(e instanceof Error ? e.message : String(e));
    } finally {
      this.loading.set(false);
    }
  }
}
