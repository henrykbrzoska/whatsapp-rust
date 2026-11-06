import { TestBed } from '@angular/core/testing';
import { Router, provideRouter, withComponentInputBinding } from '@angular/router';

import { App } from './app';
import { routes } from './app.routes';
import { PLATFORM_DEPS } from './core/services/tauri.service';
import { fakeDeps, jsonResponse } from './testing/fakes';

describe('App (bare route)', () => {
  beforeEach(() => {
    const deps = fakeDeps({ stored: { url: 'http://e', token: 'tok' } });
    deps.fetchSpy.and.callFake(async (url: string) =>
      url.endsWith('/version') ? jsonResponse({ version: '0.1.0', protocolRef: 'abc' }) : jsonResponse([]),
    );
    TestBed.configureTestingModule({
      imports: [App],
      providers: [provideRouter(routes, withComponentInputBinding()), { provide: PLATFORM_DEPS, useValue: deps }],
    });
  });

  async function render(url: string): Promise<HTMLElement> {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await TestBed.inject(Router).navigateByUrl(url);
    fixture.detectChanges();
    await fixture.whenStable();
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  it('renders the shell (header + nav) on regular routes', async () => {
    const el = await render('/about');
    expect(el.querySelector('.app-header')).not.toBeNull();
    expect(el.querySelectorAll('.app-nav a').length).toBe(4);
    expect(el.querySelector('main')?.classList).not.toContain('bare');
  });

  it('hides the shell on the bare browser-view route', async () => {
    const el = await render('/browser-view');
    expect(el.querySelector('.app-header')).toBeNull();
    expect(el.querySelector('.app-nav')).toBeNull();
    expect(el.querySelector('main')?.classList).toContain('bare');
    expect(el.querySelector('app-browser-view')).not.toBeNull();
  });

  it('reaches every top-level page (an empty-path shell must not swallow them)', async () => {
    const router = TestBed.inject(Router);
    // The messenger layout lives on an empty-path route with children. If it
    // is declared before the full-page routes, the router never falls through
    // to them and each of these lands on `/` through the `**` wildcard.
    for (const [url, selector] of [
      ['/login', 'app-login'],
      ['/settings', 'app-settings'],
      ['/about', 'app-about'],
      ['/browser-view', 'app-browser-view'],
    ]) {
      const el = await render(url);
      expect(router.url).withContext(`navigating to ${url}`).toBe(url);
      expect(el.querySelector(selector)).withContext(`${selector} for ${url}`).not.toBeNull();
    }
  });

  it('renders the chat shell on / and on a conversation', async () => {
    const router = TestBed.inject(Router);
    let el = await render('/');
    expect(router.url).toBe('/');
    expect(el.querySelector('app-chats-shell')).not.toBeNull();
    expect(el.querySelector('app-no-chat')).not.toBeNull();

    el = await render('/chat/1@g.us');
    expect(router.url).toBe('/chat/1@g.us');
    expect(el.querySelector('app-chats-shell')).not.toBeNull();
    expect(el.querySelector('app-chat')).not.toBeNull();
  });

  it('sends an unknown path to the chat list', async () => {
    const el = await render('/nope/whatever');
    expect(TestBed.inject(Router).url).toBe('/');
    expect(el.querySelector('app-chats-shell')).not.toBeNull();
  });

  it('shows the engine version once online', async () => {
    const el = await render('/');
    expect(el.querySelector('.engine-status')?.textContent).toContain('Engine 0.1.0');
  });
});
