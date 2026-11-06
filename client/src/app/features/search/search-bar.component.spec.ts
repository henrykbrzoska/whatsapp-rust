import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';

import { SearchResponse } from '../../core/models';
import { PLATFORM_DEPS } from '../../core/services/tauri.service';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { fakeDeps } from '../../testing/fakes';
import { SearchBarComponent } from './search-bar.component';

const RESPONSE: SearchResponse = {
  mode: 'hybrid',
  vector: false,
  results: [
    {
      score: 0.0328,
      message: { id: 'a', chat_jid: '1@g.us', sender: '1@s.whatsapp.net', timestamp: 10, type: 'text', text: 'Dinner tonight at 7?' },
    },
  ],
};

describe('SearchBarComponent', () => {
  let fixture: ComponentFixture<SearchBarComponent>;
  let search: jasmine.Spy;

  beforeEach(() => {
    TestBed.configureTestingModule({
      imports: [SearchBarComponent],
      providers: [provideRouter([]), { provide: PLATFORM_DEPS, useValue: fakeDeps() }],
    });
    search = spyOn(TestBed.inject(WhatsappService), 'search').and.resolveTo(RESPONSE);
    fixture = TestBed.createComponent(SearchBarComponent);
    fixture.detectChanges();
  });

  const el = () => fixture.nativeElement as HTMLElement;
  const modeButton = (m: string) =>
    Array.from(el().querySelectorAll<HTMLButtonElement>('.mode-toggle button')).find((b) => b.textContent?.trim() === m)!;

  async function type(q: string): Promise<void> {
    const input = el().querySelector('input')!;
    input.value = q;
    input.dispatchEvent(new Event('input'));
    el().querySelector('form')!.dispatchEvent(new Event('submit'));
    await fixture.whenStable();
    fixture.detectChanges();
  }

  it('offers keyword / vector / hybrid with hybrid as default', () => {
    expect(Array.from(el().querySelectorAll('.mode-toggle button')).map((b) => b.textContent?.trim())).toEqual([
      'keyword',
      'vector',
      'hybrid',
    ]);
    expect(modeButton('hybrid').getAttribute('aria-pressed')).toBe('true');
  });

  it('searches on submit and renders hits with a chat link', async () => {
    await type('dinner');
    expect(search).toHaveBeenCalledWith('dinner', { mode: 'hybrid', chat: null });
    expect(el().querySelector('.search-results .text')?.textContent).toContain('Dinner tonight at 7?');
    expect(el().querySelector('.search-results a')?.getAttribute('href')).toBe('/chat/1@g.us');
    expect(el().querySelector('.hint')?.textContent).toContain('fastembed');
  });

  it('re-runs the search when the mode changes', async () => {
    await type('dinner');
    modeButton('keyword').click();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(search).toHaveBeenCalledWith('dinner', { mode: 'keyword', chat: null });
    expect(el().querySelector('.hint')).toBeNull();
  });

  it('scopes the search to the chat input', async () => {
    fixture.componentRef.setInput('chat', '2@s.whatsapp.net');
    await type('ok');
    expect(search).toHaveBeenCalledWith('ok', { mode: 'hybrid', chat: '2@s.whatsapp.net' });
  });

  it('does not search for an empty query', async () => {
    await type('   ');
    expect(search).not.toHaveBeenCalled();
  });
});
