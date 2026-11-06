import { Component, OnDestroy, computed, inject, signal } from '@angular/core';
import { RouterLink } from '@angular/router';
import { WhatsappService } from '../../core/services/whatsapp.service';

/** How long one QR code stays valid (wa-core `QR_CHANNEL_TIMEOUT`). */
const QR_LIFETIME_S = 20;

@Component({
  selector: 'app-login',
  imports: [RouterLink],
  template: `
    <section class="page login-page">
      <h1>Connect WhatsApp</h1>
      <p class="muted">Link this app as a companion device. Scan the QR code with WhatsApp on your phone.</p>
      @if (!wa.paired() || wa.connectionState() === 'logged_out') {
        <ol class="muted" style="margin:8px 0 16px 20px">
          <li>Open WhatsApp on your phone</li>
          <li>Tap <b>Settings → Linked devices → Link a device</b></li>
          <li>Point your phone at the QR code below</li>
        </ol>
      }

      <div class="login-actions">
        @if (wa.connectionState() === 'connected') {
          <p class="ok" data-testid="paired">✓ Connected as a linked device.</p>
          <a routerLink="/" class="button primary">Go to chats</a>
          <button type="button" data-testid="disconnect" (click)="doDisconnect()">Disconnect</button>
          <button type="button" class="danger" data-testid="logout" (click)="doLogout()">Logout (unlink)</button>
        } @else if (wa.connectionState() === 'logged_out') {
          <p class="error" data-testid="logged-out">Logged out — link again.</p>
          <button type="button" class="primary" data-testid="connect" (click)="doConnect()" [disabled]="busy()">Connect</button>
        } @else if (wa.connectionState() === 'connecting' || (wa.paired() && wa.qrCodes().length === 0)) {
          <!-- Paired already: the engine is dialling WhatsApp, no QR needed. -->
          <p class="muted" data-testid="reconnecting">
            This device is linked — {{ wa.connectionState() === 'connecting' ? 'connecting to WhatsApp…' : 'waiting for the engine…' }}
          </p>
          <a routerLink="/" class="button">Go to chats</a>
          <button type="button" data-testid="disconnect" (click)="doDisconnect()">Disconnect</button>
          <button type="button" class="danger" data-testid="logout" (click)="doLogout()">Logout (unlink)</button>
        } @else {
          <button type="button" class="primary" data-testid="connect" (click)="doConnect()" [disabled]="busy()">
            {{ busy() ? 'Connecting…' : (wa.qrCodes().length ? 'Refresh QR' : 'Show QR code') }}
          </button>
          <span class="muted" data-testid="pair-status">Status: {{ wa.connectionState() }} · SSE: {{ wa['tauri'].sseState() }} · Engine: {{ wa.engineStatus() }}</span>
          <button type="button" data-testid="disconnect" (click)="doDisconnect()" [disabled]="wa.connectionState()==='disconnected'">Disconnect</button>
        }
      </div>

      @if (statusError()) {
        <p class="error" role="alert">Engine: {{ statusError() }}</p>
      }
      @if (error()) {
        <p class="error" role="alert">{{ error() }}</p>
      }
      @if (lastStatus()) {
        <pre class="muted" style="font-size:0.75rem;white-space:pre-wrap">{{ lastStatus() }}</pre>
      }

      @if (activeCode(); as code) {
        <div class="qr-wrap" data-testid="qr-wrap">
          <div class="qr-card" data-testid="qr-card">
            <img
              [src]="qrImageUrl(code)"
              alt="WhatsApp QR code"
              data-testid="qr-code"
              width="280"
              height="280"
              loading="eager"
              (error)="onImgError($event)"
            />
            <p class="hint" role="status" data-testid="qr-status">
              Code {{ activeIndex() + 1 }} of {{ wa.qrCodes().length }} · rotates in {{ secondsLeft() }}s
            </p>
            <details>
              <summary>Raw QR data</summary>
              <code class="qr-raw" data-testid="qr-raw">{{ code }}</code>
              <button type="button" class="link" (click)="copy(code)">Copy</button>
              @if (copied()) { <span class="ok"> copied</span> }
            </details>
          </div>
          <p class="hint">Each code stays valid for ~20 seconds; the engine sends the whole rotation up front and the screen advances on its own.</p>
        </div>
      } @else if (wa.connectionState()==='qr' || busy()) {
        <p class="muted">Waiting for QR… (check engine log in Settings → Diagnostic log if it takes &gt;10s)</p>
      }

      @if (wa.engineStatus() !== 'online') {
        <p class="hint">Engine {{ wa.engineStatus() }} — check <a routerLink="/settings">Settings</a> and paste the BEBOK_READY URL.</p>
      }
    </section>
  `,
  styles: `
    .login-page { max-width: 640px; }
    .login-actions { display:flex; gap:8px; align-items:center; flex-wrap:wrap; margin:12px 0; }
    .qr-wrap { display:flex; flex-direction:column; gap:16px; margin-top:16px; }
    .qr-card { background: var(--panel); border:1px solid var(--border); border-radius:12px; padding:16px; display:flex; flex-direction:column; align-items:center; gap:12px; }
    .qr-card img { border:1px solid var(--border); border-radius:8px; background:#fff; }
    .qr-raw { word-break: break-all; font-size:0.75rem; display:block; max-width:100%; white-space:pre-wrap; }
    button.danger { border-color: var(--error); color: var(--error); }
  `,
})
export class LoginComponent implements OnDestroy {
  protected readonly wa = inject(WhatsappService);
  readonly error = signal<string | null>(null);
  readonly statusError = signal<string | null>(null);
  readonly lastStatus = signal<string | null>(null);
  readonly busy = signal(false);
  readonly copied = signal(false);
  /** Index into `wa.qrCodes()` of the code that is valid right now. */
  readonly activeIndex = signal(0);
  /** Seconds until the current code is rotated out. */
  readonly secondsLeft = signal(QR_LIFETIME_S);
  /** The one code on screen (the engine sends a whole rotation as one event). */
  readonly activeCode = computed(() => this.wa.qrCodes()[this.activeIndex()] ?? null);
  private lastCodes: readonly string[] | null = null;
  private copyTimer: ReturnType<typeof setTimeout> | null = null;
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private qrTimer: ReturnType<typeof setInterval> | null = null;
  private unlisten: (() => void) | null = null;

  constructor() {
    // Ensure SSE is subscribed — without it QR never arrives
    void this.wa.start();
    // Poll /status while connecting/qr so UI recovers even if SSE drops
    this.pollTimer = setInterval(() => void this.pollStatus(), 2000);
    // Advance the QR rotation (each code is valid ~20 s, see wa-core `pair.rs`).
    this.qrTimer = setInterval(() => this.tickQrRotation(), 1000);
    void this.pollStatus();
  }

  ngOnDestroy(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
    if (this.qrTimer) clearInterval(this.qrTimer);
    this.unlisten?.();
  }

  /** Restart the rotation when a new `Qr` event lands, otherwise count down. */
  private tickQrRotation(): void {
    const codes = this.wa.qrCodes();
    if (codes !== this.lastCodes) {
      this.lastCodes = codes;
      this.activeIndex.set(0);
      this.secondsLeft.set(QR_LIFETIME_S);
      return;
    }
    if (!codes.length) return;
    const left = this.secondsLeft() - 1;
    if (left > 0) {
      this.secondsLeft.set(left);
      return;
    }
    this.activeIndex.update((i) => (i + 1) % codes.length);
    this.secondsLeft.set(QR_LIFETIME_S);
  }

  qrImageUrl(code: string): string {
    return `https://api.qrserver.com/v1/create-qr-code/?size=280x280&data=${encodeURIComponent(code)}`;
  }

  onImgError(ev: Event): void {
    (ev.target as HTMLImageElement).style.display = 'none';
  }

  async pollStatus(): Promise<void> {
    try {
      const s = await this.wa.status();
      this.lastStatus.set(JSON.stringify(s, null, 2));
      if (s.error) this.statusError.set(s.error);
      else this.statusError.set(null);
      // Sync local state if SSE missed an event. `connected` on the engine means
      // "socket up", `loggedIn` means "paired" — until the phone scans the QR the
      // socket is connected but the session is not usable yet.
      if (s.connected && s.loggedIn) {
        this.wa.connectionState.set('connected');
        this.busy.set(false);
      } else if (s.connected && !s.loggedIn) {
        // Pairing in progress: keep the QR view (never claim "connected as a linked device").
        if (this.wa.connectionState() !== 'logged_out') this.wa.connectionState.set('qr');
      } else if (s.state === 'disconnected' && s.error) {
        // connect_failure without SSE
        if (this.wa.connectionState() !== 'disconnected' && this.wa.connectionState() !== 'logged_out') {
          this.wa.connectionState.set('disconnected');
        }
        this.busy.set(false);
      }
    } catch {
      // engine offline — ignore
    }
  }

  async doConnect(): Promise<void> {
    this.busy.set(true);
    this.error.set(null);
    this.statusError.set(null);
    try {
      await this.wa.connect();
      // Keep busy until QR or connected arrives (via SSE or poll)
      setTimeout(() => {
        if (this.wa.qrCodes().length === 0 && this.wa.connectionState() !== 'connected') {
          void this.pollStatus();
        }
        // Don't stay busy forever — poll will clear it on failure
        setTimeout(() => {
          if (this.wa.qrCodes().length === 0 && this.wa.connectionState() !== 'connected') {
            this.busy.set(false);
          }
        }, 25000);
      }, 1000);
    } catch (e) {
      this.error.set(e instanceof Error ? e.message : String(e));
      this.busy.set(false);
    }
  }

  async doDisconnect(): Promise<void> {
    try {
      await this.wa.disconnect();
      this.busy.set(false);
    } catch (e) {
      this.error.set(e instanceof Error ? e.message : String(e));
    }
  }

  async doLogout(): Promise<void> {
    try {
      await this.wa.logout();
      this.busy.set(false);
    } catch (e) {
      this.error.set(e instanceof Error ? e.message : String(e));
    }
  }

  async copy(text: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      this.copied.set(true);
      if (this.copyTimer) clearTimeout(this.copyTimer);
      this.copyTimer = setTimeout(() => this.copied.set(false), 1500);
    } catch {
      this.error.set('Clipboard not available');
    }
  }
}
