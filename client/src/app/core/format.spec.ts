import { formatClock, formatDay, initial, isGroupJid, linkify, mediaLabel, mediaRender, sameDay, shortJid } from './format';

/** Unix seconds for a local date/time, so the tests never depend on the zone. */
function ts(y: number, m: number, d: number, h = 12, min = 0): number {
  return Math.floor(new Date(y, m - 1, d, h, min).getTime() / 1000);
}

describe('linkify', () => {
  it('leaves plain text in one piece', () => {
    expect(linkify('no links here')).toEqual([{ text: 'no links here', href: null }]);
    expect(linkify('')).toEqual([]);
  });

  it('turns http(s) and www into links, keeping the text around them', () => {
    expect(linkify('see https://angular.dev/guide now')).toEqual([
      { text: 'see ', href: null },
      { text: 'https://angular.dev/guide', href: 'https://angular.dev/guide' },
      { text: ' now', href: null },
    ]);
    expect(linkify('www.example.com')).toEqual([{ text: 'www.example.com', href: 'https://www.example.com' }]);
  });

  it('finds several links in one message', () => {
    const parts = linkify('a http://a.test b https://b.test');
    expect(parts.filter((p) => p.href).map((p) => p.href)).toEqual(['http://a.test', 'https://b.test']);
  });

  it('keeps the engine handshake URL (query string and all) in the link', () => {
    const url = 'http://localhost:4200/?engine=BEBOK_READY%20http%3A%2F%2F127.0.0.1%3A8787%2F%3Ftoken%3Dabc';
    expect(linkify(url)).toEqual([{ text: url, href: url }]);
  });

  it('leaves sentence punctuation outside the link', () => {
    expect(linkify('read https://x.dev/a.')).toEqual([
      { text: 'read ', href: null },
      { text: 'https://x.dev/a', href: 'https://x.dev/a' },
      { text: '.', href: null },
    ]);
    expect(linkify('(see https://x.dev/a)')).toEqual([
      { text: '(see ', href: null },
      { text: 'https://x.dev/a', href: 'https://x.dev/a' },
      { text: ')', href: null },
    ]);
    // A bracket the URL opened itself stays in the link (wiki-style paths).
    const wiki = 'https://x.dev/a_(b)';
    expect(linkify(wiki)).toEqual([{ text: wiki, href: wiki }]);
  });

  it('never links a scheme that could execute', () => {
    for (const body of ['javascript:alert(1)', 'data:text/html,<script>', 'file:///etc/passwd']) {
      expect(linkify(body)).toEqual([{ text: body, href: null }], body);
    }
  });
});

describe('formatDay', () => {
  const now = new Date(2026, 10, 6, 15, 0); // 6 Nov 2026, local

  it('names today and yesterday', () => {
    expect(formatDay(ts(2026, 11, 6, 9), now)).toBe('Today');
    expect(formatDay(ts(2026, 11, 5, 23), now)).toBe('Yesterday');
  });

  it('falls back to a date for anything older', () => {
    const label = formatDay(ts(2026, 10, 2), now);
    expect(label).not.toBe('Today');
    expect(label).not.toBe('Yesterday');
    expect(label.length).toBeGreaterThan(0);
  });
});

describe('sameDay', () => {
  it('compares local calendar days, not 24-hour windows', () => {
    expect(sameDay(ts(2026, 11, 6, 0, 1), ts(2026, 11, 6, 23, 59))).toBeTrue();
    expect(sameDay(ts(2026, 11, 6, 23, 59), ts(2026, 11, 7, 0, 1))).toBeFalse();
  });
});

describe('labels', () => {
  it('formats a clock from unix seconds', () => {
    expect(formatClock(ts(2026, 11, 6, 14, 5))).toMatch(/\d{1,2}[:.]\d{2}/);
    expect(formatClock(null)).toBe('');
  });

  it('shortens JIDs and spots group chats', () => {
    expect(shortJid('48123456789@s.whatsapp.net')).toBe('48123456789');
    expect(shortJid('1203@g.us')).toBe('Group 1203');
    expect(isGroupJid('1203@g.us')).toBeTrue();
    expect(isGroupJid('48123@s.whatsapp.net')).toBeFalse();
  });

  it('labels media kinds and takes an avatar initial', () => {
    expect(mediaLabel('image')).toContain('Photo');
    expect(mediaLabel('text')).toBe('(no text)');
    expect(mediaLabel('')).toContain('Attachment');
    expect(initial('ann')).toBe('A');
    expect(initial('')).toBe('?');
  });
});

describe('mediaRender', () => {
  it('plays images and stickers in an <img>', () => {
    expect(mediaRender('image')).toBe('image');
    expect(mediaRender('sticker')).toBe('image');
  });

  it('plays videos and gifs in a <video>', () => {
    expect(mediaRender('video')).toBe('video');
    expect(mediaRender('gif')).toBe('video');
  });

  it('plays audio and voice notes in an <audio>', () => {
    expect(mediaRender('audio')).toBe('audio');
    expect(mediaRender('ptt')).toBe('audio');
  });

  it('offers a document as a download', () => {
    expect(mediaRender('document')).toBe('document');
  });

  it('has no element for what is not an attachment', () => {
    for (const kind of ['text', 'location', 'contact', 'vcard', 'revoked', '', 'poll']) {
      expect(mediaRender(kind)).withContext(kind).toBeNull();
    }
  });
});
