/** Unix seconds → short local date/time. */
export function formatTimestamp(ts: number | null | undefined): string {
  if (ts === null || ts === undefined) return '—';
  const d = new Date(ts * 1000);
  return Number.isNaN(d.getTime()) ? String(ts) : d.toLocaleString();
}

/** Unix seconds → `14:05` (the clock on a message bubble). */
export function formatClock(ts: number | null | undefined): string {
  if (ts === null || ts === undefined) return '';
  const d = new Date(ts * 1000);
  if (Number.isNaN(d.getTime())) return '';
  return d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

/**
 * Unix seconds → the day separator inside a thread: `Today`, `Yesterday` or a
 * locale date. `now` is injectable so tests do not depend on the clock.
 */
export function formatDay(ts: number, now: Date = new Date()): string {
  const d = new Date(ts * 1000);
  if (Number.isNaN(d.getTime())) return String(ts);
  const days = daysBetween(d, now);
  if (days === 0) return 'Today';
  if (days === 1) return 'Yesterday';
  const sameYear = d.getFullYear() === now.getFullYear();
  return d.toLocaleDateString(undefined, {
    weekday: days < 7 ? 'long' : undefined,
    day: 'numeric',
    month: 'short',
    year: sameYear ? undefined : 'numeric',
  });
}

/** Calendar days from `d` to `now` (local midnight to local midnight). */
function daysBetween(d: Date, now: Date): number {
  const midnight = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  return Math.round((midnight(now) - midnight(d)) / 86_400_000);
}

/** `true` when both timestamps fall on the same local calendar day. */
export function sameDay(a: number, b: number): boolean {
  const x = new Date(a * 1000);
  const y = new Date(b * 1000);
  return x.getFullYear() === y.getFullYear() && x.getMonth() === y.getMonth() && x.getDate() === y.getDate();
}

/** `48123456789@s.whatsapp.net` → `48123456789`, groups keep a marker. */
export function shortJid(jid: string): string {
  const [user, server] = jid.split('@');
  if (server === 'g.us') return `Group ${user}`;
  return user || jid;
}

/** The first letter (or digit) of a chat name, for the round avatar. */
export function initial(name: string): string {
  return (name.trim().charAt(0) || '?').toUpperCase();
}

/** A group JID (`…@g.us`) — a thread that needs sender labels. */
export function isGroupJid(jid: string): boolean {
  return jid.endsWith('@g.us');
}

/**
 * A non-text message has no body to render, so the bubble shows a label
 * instead: `image` → `🖼 Photo`, unknown kinds keep their own name.
 */
export function mediaLabel(kind: string): string {
  switch (kind) {
    case 'image':
      return '🖼 Photo';
    case 'video':
      return '🎞 Video';
    case 'gif':
      return '🎞 GIF';
    case 'audio':
    case 'ptt':
      return '🎙 Voice message';
    case 'document':
      return '📄 Document';
    case 'sticker':
      return '🪄 Sticker';
    case 'location':
      return '📍 Location';
    case 'contact':
    case 'vcard':
      return '👤 Contact';
    case 'revoked':
      return '🚫 Message deleted';
    case 'text':
      // A text message the engine could not turn into a body (an unsupported
      // payload, or one that needs a media download).
      return '(no text)';
    default:
      return `📎 ${kind || 'Attachment'}`;
  }
}

/**
 * How a bubble should render an attachment. `null` means "no attachment" —
 * a location, a contact card or a plain text message, which stay a label.
 */
export type MediaRender = 'image' | 'video' | 'audio' | 'document';

/**
 * Message kind → the element that plays it.
 *
 * A GIF arrives as `video/mp4` with `gifPlayback`, so it renders as a looping
 * `<video>`, and a sticker (a WebP or an animated WebP) as an `<img>`.
 */
export function mediaRender(kind: string): MediaRender | null {
  switch (kind) {
    case 'image':
    case 'sticker':
      return 'image';
    case 'video':
    case 'gif':
      return 'video';
    case 'audio':
    case 'ptt':
      return 'audio';
    case 'document':
      return 'document';
    default:
      return null;
  }
}

/** One piece of a message body: plain text, or a link to render as `<a>`. */
export interface TextSegment {
  text: string;
  /** The URL to open, or `null` for plain text. */
  href: string | null;
}

/**
 * Split a message body into text and links so the template can render real
 * anchors without ever touching `innerHTML`.
 *
 * Matches `http(s)://…` and bare `www.…`; only those two become links, so a
 * `javascript:` or `data:` body stays plain text. Trailing punctuation (and a
 * closing bracket with no opener) is left outside the link, the way chat
 * clients do it, so "see https://x.dev/a." does not link the full stop.
 */
export function linkify(text: string): TextSegment[] {
  const pattern = /\b(?:https?:\/\/|www\.)[^\s<>"']+/gi;
  const segments: TextSegment[] = [];
  let last = 0;
  for (const match of text.matchAll(pattern)) {
    const start = match.index ?? 0;
    const raw = trimUrlTail(match[0]);
    if (!raw) continue;
    if (start > last) segments.push({ text: text.slice(last, start), href: null });
    segments.push({ text: raw, href: /^www\./i.test(raw) ? `https://${raw}` : raw });
    last = start + raw.length;
  }
  if (last < text.length) segments.push({ text: text.slice(last), href: null });
  return segments;
}

/** Drop punctuation a sentence left on the end of a URL. */
function trimUrlTail(url: string): string {
  let out = url;
  while (out.length > 0) {
    const last = out[out.length - 1];
    if ('.,;:!?\'"'.includes(last)) {
      out = out.slice(0, -1);
      continue;
    }
    // A closing bracket counts only when the URL opened one.
    const pairs: Record<string, string> = { ')': '(', ']': '[', '}': '{' };
    const opener = pairs[last];
    if (opener && countOf(out, last) > countOf(out, opener)) {
      out = out.slice(0, -1);
      continue;
    }
    break;
  }
  return out;
}

function countOf(text: string, char: string): number {
  let n = 0;
  for (const c of text) if (c === char) n++;
  return n;
}
