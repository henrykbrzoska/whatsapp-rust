import { Injectable, inject, signal } from '@angular/core';

import { Jid, StoredMessage } from '../models';
import { shortJid } from '../format';
import { WhatsappService } from './whatsapp.service';

/**
 * Display names by JID, so the UI shows "Ann" instead of
 * `48111@s.whatsapp.net`.
 *
 * Three sources, in this order:
 *
 * 1. `GET /contacts` — the address book the engine synced (app state), already
 *    resolved server-side to `displayName` (address book → business → push).
 * 2. the `pushName` of every incoming message (`wa-event`), which covers
 *    people who are not in the address book — the engine stores it too, this
 *    just avoids a refetch;
 * 3. [`remember`] for a name the caller resolved itself (a group subject).
 *
 * Lookups fall back to the JID's user part, never to an empty label. A JID
 * with a device suffix (`48111:3@s.whatsapp.net`) resolves to its base user.
 */
@Injectable({ providedIn: 'root' })
export class ContactNames {
  private readonly wa = inject(WhatsappService);

  /** JID (device suffix stripped) → display name. */
  readonly names = signal<Readonly<Record<Jid, string>>>({});
  private loading: Promise<void> | null = null;

  constructor() {
    // Push names arrive with the messages themselves.
    this.wa.onEvent((e) => {
      if (e.type !== 'message') return;
      const { sender, pushName, isFromMe } = e.info;
      if (!isFromMe && pushName) this.remember(sender, pushName);
    });
  }

  /** Load `GET /contacts` once (or again with `force`). Never throws. */
  load(force = false): Promise<void> {
    if (force) this.loading = null;
    this.loading ??= this.wa
      .contacts()
      .then((contacts) => {
        if (!Array.isArray(contacts)) return;
        const next: Record<Jid, string> = { ...this.names() };
        for (const c of contacts) {
          const name = c.displayName ?? c.fullName ?? c.pushName ?? '';
          if (name) next[baseJid(c.jid)] = name;
        }
        this.names.set(next);
      })
      .catch(() => {
        // Names are a nicety: a chat must open without the address book.
        this.loading = null;
      });
    return this.loading;
  }

  /**
   * Learn the names a page of stored messages carries. `push_name` is the only
   * name an `@lid` chat has before contact sync runs, and the engine keeps it
   * per message — so a thread shows real names straight from the database.
   */
  rememberFromMessages(messages: readonly StoredMessage[]): void {
    const next: Record<Jid, string> = {};
    for (const m of messages) {
      const name = m.push_name?.trim();
      if (!name || m.from_me) continue;
      const key = baseJid(m.sender);
      if (this.names()[key] !== name) next[key] = name;
    }
    if (Object.keys(next).length > 0) this.names.update((n) => ({ ...n, ...next }));
  }

  /** Store one resolved name (a push name, or a group subject). */
  remember(jid: Jid, name: string): void {
    const key = baseJid(jid);
    const trimmed = name.trim();
    if (!key || !trimmed || this.names()[key] === trimmed) return;
    this.names.update((n) => ({ ...n, [key]: trimmed }));
  }

  /** The best known name for `jid`, falling back to its user part. */
  nameOf(jid: Jid): string {
    return this.lookup(jid) ?? shortJid(jid);
  }

  /** The known name, or `null` when only the JID is known. */
  lookup(jid: Jid): string | null {
    return this.names()[baseJid(jid)] ?? null;
  }
}

/** `48111:3@s.whatsapp.net` → `48111@s.whatsapp.net` (Go `JID.ToNonAD`). */
export function baseJid(jid: Jid): Jid {
  const [user = '', server = ''] = jid.split('@');
  const base = user.split(':')[0];
  return server ? `${base}@${server}` : base;
}
