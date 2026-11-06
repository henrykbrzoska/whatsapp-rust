import { Component, inject, output, signal } from '@angular/core';

import { GROUPS_API, GroupInfo, parseParticipants } from './groups.api';

/** Create-group flow: name + participant list → `POST /group/create`. */
@Component({
  selector: 'app-group-create',
  template: `
    <section class="page group-create">
      <h1>New group</h1>
      <form (submit)="submit($event)">
        <label>
          Group name
          <input data-testid="group-name" [value]="name()" (input)="name.set(value($event))" maxlength="100" />
        </label>
        <label>
          Participants (phone numbers or JIDs, comma or newline separated)
          <textarea data-testid="group-participants" [value]="participants()" (input)="participants.set(value($event))"></textarea>
        </label>
        @if (error()) {
          <p class="error" role="alert">{{ error() }}</p>
        }
        <button type="submit" data-testid="create" [disabled]="busy() || !name().trim()">
          {{ busy() ? 'Creating…' : 'Create group' }}
        </button>
      </form>
    </section>
  `,
})
export class GroupCreateComponent {
  private readonly api = inject(GROUPS_API);

  /** Emitted with the new group's metadata (`POST /group/create` reply). */
  readonly created = output<GroupInfo>();
  readonly name = signal('');
  readonly participants = signal('');
  readonly busy = signal(false);
  readonly error = signal<string | null>(null);

  protected value(ev: Event): string {
    return (ev.target as HTMLInputElement | HTMLTextAreaElement).value;
  }

  async submit(ev?: Event): Promise<void> {
    ev?.preventDefault();
    const name = this.name().trim();
    if (!name) return;
    this.busy.set(true);
    this.error.set(null);
    try {
      this.created.emit(await this.api.createGroup(name, parseParticipants(this.participants())));
    } catch (e) {
      this.error.set(e instanceof Error ? e.message : String(e));
    } finally {
      this.busy.set(false);
    }
  }
}
