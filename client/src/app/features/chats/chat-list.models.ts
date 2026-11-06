// Chat-list helpers for Phase 5 Task F.
//
// The shapes come from the generated `specta` types: `GET /chats` returns
// `ChatListEntry` (summary + mute/pin/archive flags) and presence arrives as
// `presence` / `chat_presence` on the `wa-event` stream. Only the helpers below
// are hand-written.

import { DestroyRef, Injectable, inject, signal } from '@angular/core';

import { Jid, WaEvent } from '../../core/models';
import { WhatsappService } from '../../core/services/whatsapp.service';
import type { ChatListEntry, ChatPresenceData, PresenceData } from '../../types/specta';

export type { ChatListEntry, ChatPresenceData, PresenceData };

export type PresenceState = 'online' | 'offline' | 'composing' | 'recording';

/** The subset of `ChatListEntry` that decides "muted" (tests may pass a partial row). */
export type ChatFlags = Partial<Pick<ChatListEntry, 'muted' | 'muted_until'>>;

/** Muted now: the flag is set, the mute never expires (`-1`) or ends in the future. */
export function isMuted(c: ChatFlags, nowSec = Date.now() / 1000): boolean {
  if (c.muted) return true;
  const until = c.muted_until ?? 0;
  return until === -1 || until > nowSec;
}

/** Pinned first, then by last message time (newest first), then by JID. */
export function sortChats<T extends ChatListEntry>(chats: readonly T[]): T[] {
  return [...chats].sort((a, b) => {
    const pa = a.pinned ? 1 : 0;
    const pb = b.pinned ? 1 : 0;
    if (pa !== pb) return pb - pa;
    const r = b.last_timestamp - a.last_timestamp;
    return r !== 0 ? r : a.jid.localeCompare(b.jid);
  });
}

/**
 * Presence per JID, fed from the `wa-event` stream (`presence` for a contact,
 * `chat_presence` for typing/recording in one chat).
 */
@Injectable({ providedIn: 'root' })
export class PresenceStore {
  private readonly wa = inject(WhatsappService);
  readonly states = signal<Record<Jid, PresenceState>>({});

  constructor() {
    const off = this.wa.onEvent((e) => this.applyEvent(e));
    inject(DestroyRef).onDestroy(off);
  }

  /** Route one engine event; anything else is ignored. */
  applyEvent(ev: WaEvent): void {
    switch (ev.type) {
      case 'presence':
        this.applyPresence(ev.presence);
        break;
      case 'chat_presence':
        this.applyChatPresence(ev.chatPresence);
        break;
      default:
        break;
    }
  }

  applyPresence(p: PresenceData): void {
    this.set(p.from, p.unavailable ? 'offline' : 'online');
  }

  applyChatPresence(cp: ChatPresenceData): void {
    const jid = cp.source.chat;
    if (cp.state === 'composing') {
      this.set(jid, cp.media === 'audio' ? 'recording' : 'composing');
    } else {
      // `paused`: typing stopped — the contact is still online.
      this.set(jid, 'online');
    }
  }

  get(jid: Jid): PresenceState | undefined {
    return this.states()[jid];
  }

  clear(): void {
    this.states.set({});
  }

  private set(jid: Jid, state: PresenceState): void {
    this.states.update((s) => ({ ...s, [jid]: state }));
  }
}
