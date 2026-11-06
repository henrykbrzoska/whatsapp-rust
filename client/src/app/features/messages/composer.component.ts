import { Component, ElementRef, computed, input, output, signal, viewChild } from '@angular/core';

/**
 * The message composer: a growing text box, Enter to send, Shift+Enter for a
 * newline.
 *
 * It owns the draft and nothing else — the parent (`ChatComponent`) performs
 * the send and reports back through `sending` and `error`, so a failed send
 * keeps the text the user typed.
 */
@Component({
  selector: 'app-composer',
  template: `
    <form class="composer" (submit)="submit($event)">
      @if (error()) {
        <p class="error composer-error" role="alert">{{ error() }}</p>
      }
      <div class="composer-row">
        <textarea
          #box
          data-testid="composer-input"
          rows="1"
          [value]="draft()"
          [disabled]="disabled()"
          [attr.aria-label]="'Message'"
          [placeholder]="placeholder()"
          (input)="onInput($event)"
          (keydown)="onKeydown($event)"
          (blur)="typing.emit(false)"
        ></textarea>
        <button
          type="submit"
          class="primary send"
          data-testid="composer-send"
          [disabled]="!canSend()"
          [attr.aria-label]="sending() ? 'Sending' : 'Send'"
          [title]="sending() ? 'Sending…' : 'Send (Enter)'"
        >
          {{ sending() ? '…' : '➤' }}
        </button>
      </div>
    </form>
  `,
})
export class ComposerComponent {
  /** `true` while the parent's send is in flight. */
  readonly sending = input(false);
  /** Blocks typing entirely (no session, engine offline). */
  readonly disabled = input(false);
  /** Why the last send failed, shown above the box. */
  readonly error = input<string | null>(null);
  readonly placeholder = input('Type a message');

  /** The text to send; the parent clears the draft with [`clear`]. */
  readonly send = output<string>();
  /** `true` on the first keystroke of a draft, `false` once it is sent/blurred. */
  readonly typing = output<boolean>();

  private readonly box = viewChild.required<ElementRef<HTMLTextAreaElement>>('box');
  readonly draft = signal('');
  private announcedTyping = false;

  readonly canSend = computed(() => !this.disabled() && !this.sending() && this.draft().trim().length > 0);

  protected onInput(event: Event): void {
    const el = event.target as HTMLTextAreaElement;
    this.draft.set(el.value);
    this.autoGrow(el);
    const typing = el.value.trim().length > 0;
    if (typing !== this.announcedTyping) {
      this.announcedTyping = typing;
      this.typing.emit(typing);
    }
  }

  protected onKeydown(event: KeyboardEvent): void {
    // Enter sends, Shift+Enter (and IME composition) inserts a newline.
    if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    this.emit();
  }

  protected submit(event: Event): void {
    event.preventDefault();
    this.emit();
  }

  private emit(): void {
    if (!this.canSend()) return;
    this.send.emit(this.draft().trim());
  }

  /** Called by the parent once the send succeeded. */
  clear(): void {
    this.draft.set('');
    const el = this.box().nativeElement;
    el.value = '';
    this.autoGrow(el);
    if (this.announcedTyping) {
      this.announcedTyping = false;
      this.typing.emit(false);
    }
  }

  focus(): void {
    this.box().nativeElement.focus();
  }

  /** Grow with the text up to a few lines, then scroll inside the box. */
  private autoGrow(el: HTMLTextAreaElement): void {
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, 140)}px`;
  }
}
