import { Component, signal } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';

import { StoredMessage } from '../../core/models';
import { PLATFORM_DEPS } from '../../core/services/tauri.service';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { fakeDeps } from '../../testing/fakes';
import { MessageListComponent, buildRows } from './message-list.component';

/**
 * A host so the inputs can change the way the real parent changes them, and
 * the outputs land somewhere observable.
 */
@Component({
  imports: [MessageListComponent],
  template: `
    <app-message-list
      [messages]="messages()"
      [hasMore]="hasMore()"
      [loadingOlder]="loadingOlder()"
      [isGroup]="isGroup()"
      [names]="names()"
      [canFetch]="canFetch()"
      [fetchState]="fetchState()"
      [fetchNote]="fetchNote()"
      (loadOlder)="older = older + 1"
      (fetchOlder)="fetched = fetched + 1"
    />
  `,
})
class HostComponent {
  readonly messages = signal<StoredMessage[]>([]);
  readonly hasMore = signal(false);
  readonly loadingOlder = signal(false);
  readonly isGroup = signal(false);
  readonly names = signal<Record<string, string>>({});
  readonly canFetch = signal(false);
  readonly fetchState = signal<'idle' | 'waiting'>('idle');
  readonly fetchNote = signal<string | null>(null);
  older = 0;
  fetched = 0;
}

/** Noon on three consecutive days, so day separators are predictable. */
const DAY = 86_400;
const T0 = Math.floor(new Date('2026-03-01T12:00:00Z').getTime() / 1000);

function msg(over: Partial<StoredMessage> & Pick<StoredMessage, 'id'>): StoredMessage {
  return {
    chat_jid: '1@g.us',
    sender: 'a@s.whatsapp.net',
    timestamp: T0,
    type: 'text',
    text: 'hello',
    from_me: false,
    ...over,
  };
}

/** A message the engine kept the payload for — the only kind media renders. */
function media(id: string, type: string, text: string | null = null): StoredMessage {
  return msg({ id, type, text, raw_json: 'deadbeef' });
}

describe('MessageListComponent', () => {
  let fixture: ComponentFixture<HostComponent>;
  let host: HostComponent;

  const el = () => fixture.nativeElement as HTMLElement;
  const q = (selector: string) => el().querySelector<HTMLElement>(selector);
  const testid = (id: string) => q(`[data-testid="${id}"]`);
  const render = () => {
    fixture.detectChanges();
    fixture.detectChanges(); // the measured-height pass re-renders the window
  };

  beforeEach(() => {
    TestBed.configureTestingModule({
      imports: [HostComponent],
      providers: [{ provide: PLATFORM_DEPS, useValue: fakeDeps({ stored: { url: 'http://e', token: 'tok' } }) }],
    });
    // The thread builds media URLs through the service, which needs an endpoint.
    TestBed.inject(WhatsappService);
    fixture = TestBed.createComponent(HostComponent);
    host = fixture.componentInstance;
  });

  describe('media bubbles', () => {
    it('renders an image with a link to the full size', () => {
      host.messages.set([media('m1', 'image')]);
      render();

      const img = testid('media-image') as HTMLImageElement | null;
      expect(img).withContext('an <img> for an image message').toBeTruthy();
      expect(img!.getAttribute('src')).toBe('http://e/media/m1?token=tok');
      expect(img!.getAttribute('loading')).toBe('lazy');
      expect(q('a.media-link')?.getAttribute('href')).toBe('http://e/media/m1?token=tok');
      expect(testid('media-caption')).withContext('no caption without text').toBeNull();
    });

    it('renders a sticker as an image too', () => {
      host.messages.set([media('m1', 'sticker')]);
      render();
      expect(testid('media-image')).toBeTruthy();
    });

    it('renders a video with controls and without autoplay', () => {
      host.messages.set([media('m1', 'video')]);
      render();

      const video = testid('media-video') as HTMLVideoElement | null;
      expect(video).toBeTruthy();
      expect(video!.controls).toBeTrue();
      expect(video!.autoplay).withContext('play is the user’s decision').toBeFalse();
      expect(video!.getAttribute('preload')).toBe('metadata');
      expect(video!.loop).toBeFalse();
    });

    it('loops and mutes a gif, which arrives as an mp4', () => {
      host.messages.set([media('m1', 'gif')]);
      render();

      const video = testid('media-video') as HTMLVideoElement;
      expect(video.loop).toBeTrue();
      expect(video.muted).toBeTrue();
      expect(video.autoplay).toBeFalse();
    });

    it('renders audio and voice messages as an <audio> element', () => {
      for (const kind of ['audio', 'ptt']) {
        host.messages.set([media('m1', kind)]);
        render();
        const audio = testid('media-audio') as HTMLAudioElement | null;
        expect(audio).withContext(kind).toBeTruthy();
        expect(audio!.controls).toBeTrue();
        expect(audio!.getAttribute('preload')).toBe('none');
      }
    });

    it('renders a document as a download link', () => {
      host.messages.set([media('m1', 'document')]);
      render();

      const link = testid('media-doc') as HTMLAnchorElement | null;
      expect(link).toBeTruthy();
      expect(link!.getAttribute('href')).toBe('http://e/media/m1?token=tok');
      expect(link!.hasAttribute('download')).toBeTrue();
      expect(link!.textContent).toContain('Document');
    });

    it('keeps the caption under the media, with its links', () => {
      host.messages.set([media('m1', 'image', 'look at https://example.com')]);
      render();

      const caption = testid('media-caption');
      expect(caption?.textContent).toContain('look at');
      expect(caption?.querySelector('a')?.getAttribute('href')).toBe('https://example.com');
    });

    it('falls back to the label with a retry when the media cannot be loaded', () => {
      host.messages.set([media('m1', 'image')]);
      render();

      testid('media-image')!.dispatchEvent(new Event('error'));
      render();

      expect(testid('media-image')).withContext('the broken <img> is gone').toBeNull();
      expect(testid('media-failed')?.textContent).toContain('Photo');
      const retry = testid('media-retry') as HTMLButtonElement;
      expect(retry.textContent).toContain('retry');

      retry.click();
      render();

      const img = testid('media-image') as HTMLImageElement;
      expect(img).withContext('the retry asks for the bytes again').toBeTruthy();
      expect(img.getAttribute('src')).toBe('http://e/media/m1?token=tok&retry=1');
    });

    it('shows the label for a media row whose payload the engine never kept', () => {
      host.messages.set([msg({ id: 'm1', type: 'image', text: null })]);
      render();

      expect(testid('media-image')).withContext('no payload = nothing to download').toBeNull();
      expect(q('.text.media')?.textContent).toContain('Photo');
    });

    it('leaves a location or a contact card as a label', () => {
      host.messages.set([media('m1', 'location'), media('m2', 'vcard')]);
      render();

      expect(testid('media-image')).toBeNull();
      expect(el().textContent).toContain('Location');
      expect(el().textContent).toContain('Contact');
    });

    it('renders a text message as text, never as media', () => {
      host.messages.set([msg({ id: 'm1', text: 'plain https://example.com' })]);
      render();

      expect(testid('media-image')).toBeNull();
      expect(q('.bubble .text')?.textContent).toContain('plain');
      expect(q('.bubble .text a')?.getAttribute('href')).toBe('https://example.com');
    });
  });

  describe('older history', () => {
    it('offers the local page button while the engine has older messages', () => {
      host.messages.set([msg({ id: 'm1' })]);
      host.hasMore.set(true);
      render();

      expect(testid('load-older')).toBeTruthy();
      expect(testid('fetch-older')).toBeNull();

      testid('load-older')!.click();
      expect(host.older).toBe(1);
    });

    it('escalates to WhatsApp once the local history is exhausted', () => {
      host.messages.set([msg({ id: 'm1' })]);
      host.hasMore.set(false);
      host.canFetch.set(true);
      render();

      expect(testid('load-older')).toBeNull();
      const fetch = testid('fetch-older')!;
      expect(fetch.textContent).toContain('Fetch older messages from WhatsApp');

      fetch.click();
      expect(host.fetched).toBe(1);
    });

    it('shows a quiet waiting state while the phone answers', () => {
      host.messages.set([msg({ id: 'm1' })]);
      host.canFetch.set(true);
      host.fetchState.set('waiting');
      render();

      expect(testid('fetch-older')).toBeNull();
      expect(testid('fetch-waiting')?.textContent).toContain('Waiting for the phone');
    });

    it('reports what the last fetch did', () => {
      host.messages.set([msg({ id: 'm1' })]);
      host.canFetch.set(true);
      host.fetchNote.set('12 older message(s) arrived.');
      render();

      expect(testid('fetch-note')?.textContent).toContain('12 older message(s) arrived.');
    });

    it('says so when there is nothing more to ask for', () => {
      host.messages.set([msg({ id: 'm1' })]);
      render();

      expect(testid('load-older')).toBeNull();
      expect(testid('fetch-older')).withContext('no session = no fetch button').toBeNull();
      expect(testid('older')?.textContent).toContain('Beginning of this conversation');
    });

    it('shows "loading" instead of either button while a page is on its way', () => {
      host.messages.set([msg({ id: 'm1' })]);
      host.hasMore.set(true);
      host.loadingOlder.set(true);
      render();

      expect(testid('load-older')).toBeNull();
      expect(testid('older')?.textContent).toContain('Loading earlier messages');
    });
  });

  describe('the thread itself', () => {
    it('separates the days and labels the authors of a group', () => {
      host.isGroup.set(true);
      host.names.set({ 'a@s.whatsapp.net': 'Ada' });
      host.messages.set([
        msg({ id: 'm1', timestamp: T0 }),
        msg({ id: 'm2', timestamp: T0 + DAY, sender: 'b@s.whatsapp.net', push_name: 'Bo' }),
      ]);
      render();

      expect(el().querySelectorAll('.day').length).toBe(2);
      const authors = Array.from(el().querySelectorAll('.bubble .author')).map((a) => a.textContent?.trim());
      expect(authors).toEqual(['Ada', 'Bo']);
    });

    it('puts our own messages on the other side', () => {
      host.messages.set([msg({ id: 'm1', from_me: true })]);
      render();
      expect(q('.bubble-row')?.classList).toContain('mine');
    });
  });

  describe('buildRows', () => {
    it('opens a day and marks the head and tail of an author run', () => {
      const rows = buildRows(
        [
          msg({ id: 'm1', timestamp: T0 }),
          msg({ id: 'm2', timestamp: T0 + 60 }),
          msg({ id: 'm3', timestamp: T0 + 120, from_me: true }),
        ],
        false,
      );

      expect(rows.map((r) => r.kind)).toEqual(['day', 'msg', 'msg', 'msg']);
      const flags = rows.filter((r) => r.kind === 'msg').map((r) => (r.kind === 'msg' ? [r.head, r.tail] : null));
      expect(flags).toEqual([
        [true, false],
        [false, true],
        [true, true],
      ]);
    });

    it('starts a new run for every sender in a group', () => {
      const rows = buildRows(
        [msg({ id: 'm1', sender: 'a@s.whatsapp.net' }), msg({ id: 'm2', sender: 'b@s.whatsapp.net' })],
        true,
      );
      const heads = rows.filter((r) => r.kind === 'msg').map((r) => (r.kind === 'msg' ? r.head : null));
      expect(heads).toEqual([true, true]);
    });
  });
});
