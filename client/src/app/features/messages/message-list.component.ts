import { AfterViewChecked, Component, ElementRef, computed, effect, inject, input, output, signal, viewChild } from '@angular/core';

import { StoredMessage } from '../../core/models';
import type { MediaRender, TextSegment } from '../../core/format';
import { formatClock, formatDay, initial, linkify, mediaLabel, mediaRender, sameDay, shortJid } from '../../core/format';
import { WhatsappService } from '../../core/services/whatsapp.service';

/** One rendered line of the thread: a day separator or a message bubble. */
export type Row =
  | { key: string; kind: 'day'; timestamp: number; label: string }
  | { key: string; kind: 'msg'; timestamp: number; message: StoredMessage; head: boolean; tail: boolean };

/** Rendered above and below the viewport, in pixels. */
const OVERSCAN = 400;
/** Row heights before they are measured (a bubble, a day separator). */
const MSG_ESTIMATE = 56;
const DAY_ESTIMATE = 36;
/** Distance from the top that asks the parent for an older page. */
const NEAR_TOP = 300;
/** Distance from the bottom that still counts as "following the thread". */
const NEAR_BOTTOM = 80;
/**
 * How many change-detection passes a scroll correction may re-apply while the
 * measured row heights settle. Bounded on purpose: an unbounded correction
 * pins the thread and makes it unscrollable.
 */
const SETTLE_PASSES = 3;

/**
 * The message thread: day separators, bubbles aligned by author, and a
 * virtual scroll that only renders the rows around the viewport.
 *
 * The window is driven by measured row heights (bubbles wrap, so fixed-height
 * virtual scrolling would misplace them): every rendered row reports its real
 * height, the rows outside the window are represented by two spacer divs, and
 * the scrollbar therefore matches the whole thread.
 *
 * The component only renders — loading pages and sending stay with the parent
 * (`ChatComponent`). It asks for more history through [`loadOlder`] when the
 * user scrolls near the top, keeps the view pinned to the newest message while
 * the reader is at the bottom, and keeps the reading position steady when an
 * older page is prepended.
 */
@Component({
  selector: 'app-message-list',
  template: `
    <div
      #scroller
      class="thread-scroll"
      data-testid="thread"
      role="log"
      aria-live="polite"
      aria-label="Messages"
      (scroll)="onScroll()"
    >
      <div class="older" data-testid="older">
        @if (loadingOlder()) {
          <span class="muted">Loading earlier messages…</span>
        } @else if (hasMore()) {
          <button type="button" class="link" data-testid="load-older" (click)="loadOlder.emit()">
            Load earlier messages
          </button>
        } @else if (fetchState() === 'waiting') {
          <span class="muted" data-testid="fetch-waiting">Waiting for the phone to send older messages…</span>
        } @else if (canFetch()) {
          <button type="button" class="link" data-testid="fetch-older" (click)="fetchOlder.emit()">
            Fetch older messages from WhatsApp
          </button>
        } @else if (!fetchNote()) {
          <span class="muted">Beginning of this conversation.</span>
        }
        <!-- What the last fetch did, whichever state the thread is in. -->
        @if (fetchNote()) {
          <span class="muted fetch-note" data-testid="fetch-note">{{ fetchNote() }}</span>
        }
      </div>

      <div class="pad" [style.height.px]="padTop()" aria-hidden="true"></div>

      @for (row of window(); track row.key) {
        @if (row.kind === 'day') {
          <div class="day" [attr.data-row-key]="row.key">
            <span>{{ row.label }}</span>
          </div>
        } @else {
          <div
            class="bubble-row"
            [attr.data-row-key]="row.key"
            [attr.data-id]="row.message.id"
            [class.mine]="row.message.from_me"
            [class.theirs]="!row.message.from_me"
            [class.tail]="row.tail"
          >
            @if (showAuthor(row)) {
              <span class="author-avatar" aria-hidden="true">{{ initial(authorName(row.message)) }}</span>
            }
            <div class="bubble">
              @if (showAuthor(row)) {
                <span class="author">{{ authorName(row.message) }}</span>
              }
              @if (render(row.message); as kind) {
                @if (broken(row.message.id)) {
                  <span class="text media" data-testid="media-failed">{{ mediaLabel(row.message.type) }}</span>
                  <button type="button" class="link media-retry" data-testid="media-retry" (click)="retryMedia(row.message.id)">
                    Could not load — retry
                  </button>
                } @else if (kind === 'image') {
                  <a class="media-link" [href]="mediaSrc(row.message.id)" target="_blank" rel="noopener noreferrer">
                    <img
                      class="media-image"
                      data-testid="media-image"
                      [src]="mediaSrc(row.message.id)"
                      [alt]="mediaLabel(row.message.type)"
                      loading="lazy"
                      (error)="mediaFailed(row.message.id)"
                    />
                  </a>
                } @else if (kind === 'video') {
                  <video
                    class="media-video"
                    data-testid="media-video"
                    [src]="mediaSrc(row.message.id)"
                    [loop]="row.message.type === 'gif'"
                    [muted]="row.message.type === 'gif'"
                    controls
                    preload="metadata"
                    (error)="mediaFailed(row.message.id)"
                  ></video>
                } @else if (kind === 'audio') {
                  <audio
                    class="media-audio"
                    data-testid="media-audio"
                    [src]="mediaSrc(row.message.id)"
                    controls
                    preload="none"
                    (error)="mediaFailed(row.message.id)"
                  ></audio>
                } @else {
                  <a
                    class="media-doc"
                    data-testid="media-doc"
                    [href]="mediaSrc(row.message.id)"
                    target="_blank"
                    rel="noopener noreferrer"
                    download
                    >{{ mediaLabel(row.message.type) }}</a
                  >
                }
                @if (row.message.text) {
                  <span class="text caption" data-testid="media-caption">
                    @for (part of segments(row.message.text); track $index) {
                      @if (part.href) {
                        <a [href]="part.href" target="_blank" rel="noopener noreferrer nofollow">{{ part.text }}</a>
                      } @else {
                        {{ part.text }}
                      }
                    }
                  </span>
                }
              } @else if (row.message.text) {
                <span class="text">
                  @for (part of segments(row.message.text); track $index) {
                    @if (part.href) {
                      <a [href]="part.href" target="_blank" rel="noopener noreferrer nofollow">{{ part.text }}</a>
                    } @else {
                      {{ part.text }}
                    }
                  }
                </span>
              } @else {
                <span class="text media">{{ mediaLabel(row.message.type) }}</span>
              }
              <time class="clock" [attr.datetime]="row.timestamp">{{ formatClock(row.timestamp) }}</time>
            </div>
          </div>
        }
      }

      <div class="pad" [style.height.px]="padBottom()" aria-hidden="true"></div>
    </div>
  `,
})
export class MessageListComponent implements AfterViewChecked {
  private readonly wa = inject(WhatsappService);
  protected readonly formatClock = formatClock;
  protected readonly mediaLabel = mediaLabel;
  protected readonly initial = initial;

  /** The thread, oldest message first. */
  readonly messages = input<readonly StoredMessage[]>([]);
  /** `true` when the engine has older messages than `messages[0]`. */
  readonly hasMore = input(false);
  /** `true` while the parent is fetching an older page. */
  readonly loadingOlder = input(false);
  /** Group threads label every bubble with its author. */
  readonly isGroup = input(false);
  /** Display names by JID (contacts); the JID's user part is the fallback. */
  readonly names = input<Readonly<Record<string, string>>>({});
  /**
   * `true` when the local history is exhausted but the phone could still be
   * asked for more (`POST /history/fetch` needs a live session).
   */
  readonly canFetch = input(false);
  /** `idle` | `waiting` (a request is out, the blob arrives minutes later). */
  readonly fetchState = input<'idle' | 'waiting'>('idle');
  /** One line about the last fetch ("12 older messages arrived", an error…). */
  readonly fetchNote = input<string | null>(null);

  /** The user scrolled near the top and wants older history. */
  readonly loadOlder = output<void>();
  /** The local history is exhausted — ask WhatsApp for more. */
  readonly fetchOlder = output<void>();

  private readonly scroller = viewChild.required<ElementRef<HTMLElement>>('scroller');

  /** Measured row heights by row key, and a version to recompute offsets. */
  private readonly heights = new Map<string, number>();
  private readonly measured = signal(0);

  private readonly scrollTop = signal(0);
  private readonly viewport = signal(0);
  /** `true` while the reader is at the bottom — new messages scroll into view. */
  private readonly following = signal(true);
  /** Remaining passes of the "scroll to the newest message" correction. */
  private bottomPasses = SETTLE_PASSES;
  /** Set when an older page was prepended: the row to keep in place. */
  private anchor: { key: string; scrollTop: number } | null = null;
  /** Remaining passes of the "keep the anchor row in place" correction. */
  private anchorPasses = 0;
  private lastFirstKey: string | null = null;

  readonly rows = computed<Row[]>(() => buildRows(this.messages(), this.isGroup()));

  /** Running offset of every row plus the total height (length `n + 1`). */
  private readonly offsets = computed<number[]>(() => {
    this.measured();
    const rows = this.rows();
    const out = new Array<number>(rows.length + 1);
    out[0] = 0;
    for (let i = 0; i < rows.length; i++) {
      const estimate = rows[i].kind === 'day' ? DAY_ESTIMATE : MSG_ESTIMATE;
      out[i + 1] = out[i] + (this.heights.get(rows[i].key) ?? estimate);
    }
    return out;
  });

  /** `[start, end)` of the rendered window. */
  private readonly range = computed<[number, number]>(() => {
    const offsets = this.offsets();
    const n = offsets.length - 1;
    if (n === 0) return [0, 0];
    const top = Math.max(0, this.scrollTop() - OVERSCAN);
    const bottom = this.scrollTop() + this.viewport() + OVERSCAN;
    const start = Math.max(0, indexAt(offsets, top));
    let end = indexAt(offsets, bottom) + 1;
    if (end > n) end = n;
    return [start, Math.max(end, start + 1)];
  });

  readonly window = computed<Row[]>(() => {
    const [start, end] = this.range();
    return this.rows().slice(start, end);
  });

  readonly padTop = computed(() => this.offsets()[this.range()[0]] ?? 0);
  readonly padBottom = computed(() => {
    const offsets = this.offsets();
    return Math.max(0, offsets[offsets.length - 1] - (offsets[this.range()[1]] ?? 0));
  });

  constructor() {
    effect(() => {
      const rows = this.rows();
      const firstKey = rows[0]?.key ?? null;
      // A prepend keeps the previous first row — hold it in place. Everything
      // else (a new message, a chat switch) follows the bottom.
      if (this.lastFirstKey && firstKey !== this.lastFirstKey && rows.some((r) => r.key === this.lastFirstKey)) {
        this.anchor = { key: this.lastFirstKey, scrollTop: this.scroller().nativeElement.scrollTop };
        this.anchorPasses = SETTLE_PASSES;
        this.stickToBottom(false);
      } else if (this.following()) {
        this.stickToBottom(true);
      }
      this.lastFirstKey = firstKey;
    });
  }

  ngAfterViewChecked(): void {
    const changed = this.measureRows();
    if (changed) this.measured.update((v) => v + 1);
    const el = this.scroller().nativeElement;
    if (this.viewport() !== el.clientHeight) this.viewport.set(el.clientHeight);

    // Both corrections below move `scrollTop`, which re-renders the window and
    // brings new rows to measure — so each is allowed a small, fixed number of
    // passes. Without that budget a measurement that never settles would keep
    // re-applying the correction on every change-detection run and the thread
    // could not be scrolled at all.
    if (this.anchorPasses > 0 && this.anchor) {
      this.anchorPasses--;
      const index = this.rows().findIndex((r) => r.key === this.anchor!.key);
      if (index > 0) el.scrollTop = this.anchor.scrollTop + this.offsets()[index];
      if (!changed || this.anchorPasses === 0) this.clearAnchor();
      return;
    }
    if (this.bottomPasses > 0) {
      this.bottomPasses--;
      el.scrollTop = el.scrollHeight;
      if (!changed || this.bottomPasses === 0) this.stickToBottom(false);
    }
  }

  /** Arm (or disarm) the "keep the newest message in view" correction. */
  private stickToBottom(on: boolean): void {
    this.bottomPasses = on ? SETTLE_PASSES : 0;
  }

  private clearAnchor(): void {
    this.anchor = null;
    this.anchorPasses = 0;
  }

  /** Read the real height of every rendered row. Returns `true` on a change. */
  private measureRows(): boolean {
    const el = this.scroller().nativeElement;
    let changed = false;
    for (const node of Array.from(el.querySelectorAll<HTMLElement>('[data-row-key]'))) {
      const key = node.dataset['rowKey'];
      if (!key) continue;
      const height = node.offsetHeight;
      if (height > 0 && Math.abs((this.heights.get(key) ?? -1) - height) > 0.5) {
        this.heights.set(key, height);
        changed = true;
      }
    }
    return changed;
  }

  onScroll(): void {
    const el = this.scroller().nativeElement;
    this.scrollTop.set(el.scrollTop);
    this.viewport.set(el.clientHeight);
    this.following.set(el.scrollHeight - el.scrollTop - el.clientHeight <= NEAR_BOTTOM);
    // The reader is in charge now: drop any pending correction, or it would
    // pull the view back while they scroll.
    this.stickToBottom(false);
    this.clearAnchor();
    if (el.scrollTop <= NEAR_TOP && this.hasMore() && !this.loadingOlder()) this.loadOlder.emit();
  }

  /** Jump to the newest message (the composer does this after sending). */
  scrollToBottom(): void {
    this.following.set(true);
    this.stickToBottom(true);
    const el = this.scroller().nativeElement;
    el.scrollTop = el.scrollHeight;
  }

  protected showAuthor(row: Extract<Row, { kind: 'msg' }>): boolean {
    return this.isGroup() && !row.message.from_me && row.head;
  }

  /** Contact name, then the name the sender announced, then the JID's user. */
  protected authorName(m: StoredMessage): string {
    return this.names()[m.sender] ?? m.push_name?.trim() ?? shortJid(m.sender);
  }

  /**
   * The element this message renders as, or `null` for a label.
   *
   * A message only renders as media when the engine kept its payload
   * (`raw_json`): rows stored before that change cannot be downloaded, so
   * asking for their bytes would always 404.
   */
  protected render(m: StoredMessage): MediaRender | null {
    if (!m.raw_json) return null;
    return mediaRender(m.type);
  }

  /**
   * `GET /media/{id}` for one message. The `retry` parameter only exists to
   * defeat the browser's cache of a failed response.
   */
  protected mediaSrc(id: string): string {
    const url = this.wa.mediaUrl(id);
    const attempt = this.retried()[id];
    if (!url || !attempt) return url;
    return `${url}${url.includes('?') ? '&' : '?'}retry=${attempt}`;
  }

  /** `true` when this message's media failed to load. */
  protected broken(id: string): boolean {
    return this.failed()[id] === true;
  }

  /**
   * The first `GET /media/{id}` downloads from WhatsApp and can fail (502) —
   * fall back to the label with a retry instead of a broken image icon.
   */
  protected mediaFailed(id: string): void {
    this.failed.update((m) => ({ ...m, [id]: true }));
  }

  protected retryMedia(id: string): void {
    this.retried.update((m) => ({ ...m, [id]: (m[id] ?? 0) + 1 }));
    this.failed.update((m) => {
      const next = { ...m };
      delete next[id];
      return next;
    });
  }

  /** Message IDs whose media failed, and how often each was retried. */
  private readonly failed = signal<Record<string, boolean>>({});
  private readonly retried = signal<Record<string, number>>({});

  /** Message body split into text and links (memoised per body). */
  protected segments(text: string): TextSegment[] {
    let parts = this.segmentCache.get(text);
    if (!parts) {
      parts = linkify(text);
      // The thread is windowed, so the cache only ever holds live bodies plus
      // the recently scrolled-past ones.
      if (this.segmentCache.size > 500) this.segmentCache.clear();
      this.segmentCache.set(text, parts);
    }
    return parts;
  }

  private readonly segmentCache = new Map<string, TextSegment[]>();
}

/**
 * Messages → rows: a day separator whenever the calendar day changes, and
 * `head`/`tail` flags marking the first and last bubble of a run by the same
 * author (the run gets one name label and one tail).
 */
export function buildRows(messages: readonly StoredMessage[], isGroup: boolean): Row[] {
  const rows: Row[] = [];
  for (let i = 0; i < messages.length; i++) {
    const m = messages[i];
    const previous = messages[i - 1];
    const next = messages[i + 1];
    if (!previous || !sameDay(previous.timestamp, m.timestamp)) {
      rows.push({ key: `day-${m.timestamp}-${m.id}`, kind: 'day', timestamp: m.timestamp, label: formatDay(m.timestamp) });
    }
    const sameRun = (a: StoredMessage | undefined, b: StoredMessage) =>
      !!a && a.from_me === b.from_me && (!isGroup || a.sender === b.sender) && sameDay(a.timestamp, b.timestamp);
    rows.push({
      key: `msg-${m.id}`,
      kind: 'msg',
      timestamp: m.timestamp,
      message: m,
      head: !sameRun(previous, m),
      tail: !sameRun(next, m),
    });
  }
  return rows;
}

/** Index of the row that covers `y`, by binary search over the offsets. */
function indexAt(offsets: number[], y: number): number {
  let lo = 0;
  let hi = offsets.length - 2;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (offsets[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}
