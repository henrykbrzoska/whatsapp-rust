import { Component, computed, inject, signal } from '@angular/core';
import { takeUntilDestroyed } from '@angular/core/rxjs-interop';
import { NavigationEnd, Router, RouterOutlet } from '@angular/router';
import { filter, map, startWith } from 'rxjs';

import { WhatsappService } from '../../core/services/whatsapp.service';
import { ChatListComponent } from './chat-list.component';

/**
 * The two-pane chat layout: the chat list on the left, the open conversation
 * on the right — WhatsApp Web's shape, and the reason the app reads as a
 * messenger rather than a page of search results.
 *
 * One viewport, two panes, no page scrolling: each pane scrolls on its own.
 * Below the layout breakpoint only one pane is visible at a time, and
 * `chat-open` (set while a `chat/:jid` child is active) decides which.
 */
@Component({
  selector: 'app-chats-shell',
  imports: [ChatListComponent, RouterOutlet],
  template: `
    <div class="chat-shell" [class.chat-open]="chatOpen()">
      <aside class="sidebar" aria-label="Chats">
        <app-chat-list />
      </aside>
      <section class="pane">
        <router-outlet />
      </section>
    </div>
  `,
})
export class ChatsShellComponent {
  private readonly router = inject(Router);
  /** `true` while a conversation is open (hides the list on small screens). */
  readonly chatOpen = signal(false);

  constructor() {
    this.router.events
      .pipe(
        filter((e) => e instanceof NavigationEnd),
        map(() => this.router.url.startsWith('/chat/')),
        startWith(this.router.url.startsWith('/chat/')),
        takeUntilDestroyed(),
      )
      .subscribe((open) => this.chatOpen.set(open));
  }
}

/**
 * The right-hand pane with no chat selected — WhatsApp Web's "select a chat"
 * state, plus a nudge towards pairing when the device is not connected yet.
 */
@Component({
  selector: 'app-no-chat',
  template: `
    <div class="no-chat">
      <span class="no-chat-mark" aria-hidden="true">💬</span>
      <h2>ngWhatsapp</h2>
      @if (connected()) {
        <p class="muted">Pick a conversation on the left to read it and reply.</p>
      } @else {
        <p class="muted">Pick a conversation on the left — or pair this device to start receiving messages.</p>
      }
    </div>
  `,
})
export class NoChatComponent {
  private readonly wa = inject(WhatsappService);
  readonly connected = computed(() => this.wa.connectionState() === 'connected');
}
