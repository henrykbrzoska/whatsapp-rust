import { Component, OnInit, inject, signal } from '@angular/core';
import { takeUntilDestroyed } from '@angular/core/rxjs-interop';
import { ActivatedRouteSnapshot, NavigationEnd, Router, RouterLink, RouterLinkActive, RouterOutlet } from '@angular/router';
import { filter } from 'rxjs';

import { TauriService } from './core/services/tauri.service';
import { WhatsappService } from './core/services/whatsapp.service';

function deepest(route: ActivatedRouteSnapshot): ActivatedRouteSnapshot {
  let r = route;
  while (r.firstChild) r = r.firstChild;
  return r;
}

@Component({
  selector: 'app-root',
  imports: [RouterOutlet, RouterLink, RouterLinkActive],
  template: `
    @if (!bare()) {
      <header class="app-header">
        <a class="brand" routerLink="/">ng<span>Whatsapp</span></a>
        <nav class="app-nav">
          <a routerLink="/" routerLinkActive="active" [routerLinkActiveOptions]="{ exact: true }">Chats</a>
          <a routerLink="/login" routerLinkActive="active">Login</a>
          <a routerLink="/settings" routerLinkActive="active">Settings</a>
          <a routerLink="/about" routerLinkActive="active">About</a>
        </nav>
        <span class="engine-status" [attr.data-status]="wa.engineStatus()" [title]="wa.engineError() ?? ''">
          <span class="dot"></span>{{ statusLabel() }}
        </span>
      </header>
      @if (wa.engineStatus() === 'unconfigured' || wa.engineStatus() === 'unauthorized' || wa.engineStatus() === 'offline') {
        <div class="banner" role="alert">
          @switch (wa.engineStatus()) {
            @case ('unconfigured') {
              Not connected to an engine. Start it with <code>npm run engine</code> and paste the
              <code>BEBOK_READY</code> URL in <a routerLink="/settings">Settings</a>.
            }
            @case ('unauthorized') {
              The engine rejected the token — paste the current <code>BEBOK_READY</code> URL in
              <a routerLink="/settings">Settings</a>.
            }
            @default {
              Engine unreachable: {{ wa.engineError() }}. <button type="button" class="link" (click)="wa.restart()">Retry</button>
            }
          }
        </div>
      }
    }
    <main [class.bare]="bare()">
      <router-outlet />
    </main>
  `,
})
export class App implements OnInit {
  protected readonly wa = inject(WhatsappService);
  protected readonly tauri = inject(TauriService);
  private readonly router = inject(Router);
  protected readonly bare = signal(false);

  constructor() {
    this.router.events
      .pipe(
        filter((e) => e instanceof NavigationEnd),
        takeUntilDestroyed(),
      )
      .subscribe(() => this.bare.set(deepest(this.router.routerState.snapshot.root).data['bare'] === true));
  }

  ngOnInit(): void {
    void this.wa.start();
  }

  protected statusLabel(): string {
    switch (this.wa.engineStatus()) {
      case 'online':
        return `Engine ${this.wa.version()?.version ?? ''}`.trim();
      case 'offline':
        return 'Engine offline';
      case 'unauthorized':
        return 'Bad token';
      case 'unconfigured':
        return 'No engine';
      default:
        return 'Connecting…';
    }
  }
}
