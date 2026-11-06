import {
  Component,
  DestroyRef,
  computed,
  effect,
  inject,
  input,
  output,
  signal,
  untracked,
} from '@angular/core';

import { shortJid } from '../../core/format';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { GROUPS_API, GroupInfo, sortParticipants } from './groups.api';

@Component({
  selector: 'app-group-info',
  template: `
    <section class="page group-info">
      @if (error()) {
        <p class="error" role="alert">{{ error() }}</p>
      }
      @if (loading()) {
        <p class="muted">Loading group…</p>
      } @else if (info(); as g) {
        <h1 data-testid="group-name">{{ g.name || shortJid(g.jid) }}</h1>
        @if (g.topic) {
          <p class="topic" data-testid="group-topic">{{ g.topic }}</p>
        } @else {
          <p class="topic muted" data-testid="group-topic">No description</p>
        }

        <h2>{{ g.participants.length }} participants</h2>
        <ul class="participants" data-testid="participants">
          @for (p of participants(); track p.jid) {
            <li [attr.data-jid]="p.jid">
              <span class="name">{{ p.displayName || p.phoneNumber || shortJid(p.jid) }}</span>
              @if (p.isSuperAdmin) {
                <span class="role">Owner</span>
              } @else if (p.isAdmin) {
                <span class="role">Admin</span>
              }
            </li>
          }
        </ul>

        <div class="invite">
          @if (inviteLink(); as link) {
            <code data-testid="invite-link">{{ link }}</code>
            <button type="button" data-testid="copy-invite" (click)="copyInvite()">
              {{ copied() ? 'Copied' : 'Copy invite link' }}
            </button>
          } @else {
            <button type="button" data-testid="load-invite" (click)="loadInvite()">Show invite link</button>
          }
        </div>

        <button type="button" class="danger" data-testid="leave" [disabled]="leaving()" (click)="leave()">
          {{ leaving() ? 'Leaving…' : 'Leave group' }}
        </button>
      }
    </section>
  `,
})
export class GroupInfoComponent {
  private readonly api = inject(GROUPS_API);
  private readonly wa = inject(WhatsappService);
  protected readonly shortJid = shortJid;

  readonly jid = input.required<string>();
  /** Emitted with the group JID after a successful leave. */
  readonly left = output<string>();

  readonly info = signal<GroupInfo | null>(null);
  readonly inviteLink = signal<string | null>(null);
  readonly loading = signal(false);
  readonly leaving = signal(false);
  readonly copied = signal(false);
  readonly error = signal<string | null>(null);

  /** Owner first, then admins, then everyone else by name. */
  readonly participants = computed(() => sortParticipants(this.info()?.participants ?? []));

  /** Group currently rendered (the routed `jid` input; `null` before binding). */
  private currentJid: string | null = null;

  constructor() {
    effect(() => {
      const jid = this.jid();
      this.currentJid = jid;
      untracked(() => void this.load(jid));
    });
    // A `group_info` notification for this group means the cache changed —
    // re-read the metadata (the engine's group cache is authoritative).
    const off = this.wa.onEvent((e) => {
      if (e.type === 'group_info' && e.info.jid === this.currentJid) void this.load(e.info.jid);
    });
    inject(DestroyRef).onDestroy(off);
  }

  async load(jid = this.jid()): Promise<void> {
    this.loading.set(true);
    this.error.set(null);
    this.inviteLink.set(null);
    try {
      this.info.set(await this.api.getGroupInfo(jid));
    } catch (e) {
      this.error.set(message(e));
    } finally {
      this.loading.set(false);
    }
  }

  async loadInvite(): Promise<void> {
    this.error.set(null);
    try {
      this.inviteLink.set(await this.api.getGroupInviteLink(this.jid()));
    } catch (e) {
      this.error.set(message(e));
    }
  }

  async copyInvite(): Promise<void> {
    const link = this.inviteLink();
    if (!link) return;
    try {
      await navigator.clipboard.writeText(link);
      this.copied.set(true);
    } catch (e) {
      this.error.set(`Could not copy the link: ${message(e)}`);
    }
  }

  async leave(): Promise<void> {
    this.leaving.set(true);
    this.error.set(null);
    try {
      await this.api.leaveGroup(this.jid());
      this.left.emit(this.jid());
    } catch (e) {
      this.error.set(message(e));
    } finally {
      this.leaving.set(false);
    }
  }
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
