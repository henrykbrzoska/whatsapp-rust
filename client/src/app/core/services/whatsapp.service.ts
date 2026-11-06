import { Injectable, inject, signal } from '@angular/core';

import {
  ChatStats,
  ExportFormat,
  ExportProgress,
  Jid,
  SearchOptions,
  SearchResponse,
  StoredMessage,
  VersionInfo,
  WaEvent,
} from '../models';
import type {
  ChatListEntry,
  GroupInfoData,
  GroupParticipantData,
  PrivacySetting,
  PrivacySettingType,
  PrivacySettings,
} from '../../types/specta';
import { EngineError, TauriService, WA_EVENT, exportHistoryPath } from './tauri.service';

export type ConnectionState = 'disconnected' | 'connecting' | 'qr' | 'connected' | 'logged_out';

/** `GET /status` — what the engine says about its single session. */
export interface EngineSessionStatus {
  /** `disconnected`, `connecting`, `connected` or `logged_out`. */
  state: string;
  /** `true` while the encrypted socket is usable. */
  connected: boolean;
  /** `true` when a device is paired — also before the first connect. */
  loggedIn: boolean;
  error: string | null;
  attempts: number;
  /**
   * WhatsApp closed the socket because another login took the device over
   * (`conflict type=replaced`). Never auto-reconnect then: two engines sharing
   * one device would kick each other forever — only an explicit Connect may
   * take the session back.
   */
  replaced?: boolean;
}

/** `POST /presence/send` — Go `types.Presence`. */
export type UserPresenceState = 'available' | 'unavailable';
/** `POST /presence/chat` — Go `types.ChatPresence`. */
export type ChatPresenceState = 'composing' | 'paused';
/** `POST /presence/chat` — Go `types.ChatPresenceMedia` (`text` is `""` on the wire). */
export type ChatPresenceMedia = 'text' | 'audio';
/** `POST /group/{jid}/participants` — Go `types.ParticipantChange`. */
export type GroupParticipantAction = 'add' | 'remove' | 'promote' | 'demote';

/** `GET /group/{jid}/invite-link` */
interface InviteLinkReply {
  link: string;
}
/** `POST /group/join` */
interface JoinReply {
  jid: Jid;
}
/** `POST /group/{jid}/participants` */
interface ParticipantsReply {
  ok: boolean;
  participants: GroupParticipantData[];
}

/**
 * `GET /contact`, `GET /contacts` — wa-core `ContactInfo` plus the resolved
 * display name (`bebok-server/src/routes/presence.rs:ContactResponse`).
 */
export interface Contact {
  jid: Jid;
  /** `false` when the store has no row for the JID (all names empty). */
  found: boolean;
  firstName: string;
  fullName: string;
  pushName: string;
  businessName: string;
  redactedPhone: string;
  /** Address-book name, then business name, then push name. */
  displayName: string | null;
}

/** `GET /picture` — `types.ProfilePictureInfo` (`pictureType` is `image` or `preview`). */
export interface ProfilePicture {
  id: string;
  url: string;
  pictureType: string;
  directPath: string;
  /** Base64 hash the server sent, empty when none. */
  hash: string;
}

/** `GET /blocklist` — `types.Blocklist`. */
export interface BlocklistItem {
  jid: Jid;
  /** The phone-number JID, when the server sent one. */
  pn: Jid | null;
  active: boolean;
}

export interface Blocklist {
  dhash: string;
  addressingMode: string;
  items: BlocklistItem[];
}

/** `GET /debug/log` — one ring-buffer entry (`bebok-server/src/debug_log.rs`). */
export interface DebugLogEntry {
  seq: number;
  /** Unix milliseconds. */
  ts: number;
  /** `event` (a published `WaEvent`) or `engine` (an engine note). */
  kind: 'event' | 'engine' | string;
  message: string;
  data?: unknown;
}

export interface DebugLog {
  capacity: number;
  entries: DebugLogEntry[];
}

/** Fields of `GET /config` the client reads (`bebok-server/src/config.rs`). */
export interface EngineConfig {
  addr: string;
  auth: boolean;
  /** Always `<redacted>` (or `null` when auth is off). */
  token: string | null;
  cors: string[];
  diagnostic: boolean;
  dataDir: string;
  waUrl: string | null;
  handshakeTimeoutSecs: number;
  /** `true` when the engine ships an embedder (`fastembed` feature). */
  vectorSearch: boolean;
  [key: string]: unknown;
}

/** `GET|POST /group/{jid}/…` — the JID stays one (percent-encoded) segment. */
const groupPath = (jid: Jid) => `/group/${encodeURIComponent(jid)}`;
export type EngineStatus = 'unknown' | 'unconfigured' | 'online' | 'offline' | 'unauthorized';

/** `GET /chats/:jid/messages` — one page of a thread, oldest message first. */
export interface MessagePage {
  messages: StoredMessage[];
  /** `true` when older messages exist before `messages[0]`. */
  hasMore: boolean;
}

/** One chat `POST /history/fetch` asked the phone about. */
export interface FetchedChat {
  chat: Jid;
  /** The message the phone was asked to go back from. */
  oldestId: string;
  oldestTimestamp: number;
  /** The ID of the peer request, for matching it in `GET /debug/log`. */
  requestId: string;
}

/**
 * `POST /history/fetch` answer — what was *asked for*, not what arrived.
 *
 * The phone answers minutes later with an encrypted blob, which the engine
 * imports and announces as a `history_imported` event.
 */
export interface FetchReport {
  /** Messages asked for per chat. */
  count: number;
  requested: FetchedChat[];
  /** Chats that were skipped, with the reason. */
  skipped: string[];
}

/** `GET /media/{id}/info` — what the engine knows without downloading. */
export interface MediaInfo {
  id: string;
  /** The stored message kind (`image`, `video`, …). */
  kind: string;
  /** `true` when the message carries a downloadable attachment. */
  available: boolean;
  /** `true` when the bytes are already in the engine's blob cache. */
  cached: boolean;
  mimeType: string | null;
  /** Declared plaintext size in bytes, when the sender gave one. */
  size: number | null;
}

/** The keyset cursor of a page: the oldest message the view already holds. */
export interface MessageCursor {
  timestamp: number;
  id: string;
}

/** `POST /appstate/sync` — what one app-state sync applied. */
export interface AppStateSyncReport {
  collections: { name: string; applied: number; hasMore: boolean }[];
  /** Contact names that were written — the names the UI shows. */
  contacts: number;
  /** Mute / pin / archive changes that were applied. */
  chatSettings: number;
  /** Collections that could not be synced (`not connected`, a missing key…). */
  errors: string[];
  /**
   * What was fetched but not applied — above all the app-state sync keys this
   * device does not have. Without it a sync that applied nothing would read
   * as "0 contacts, no errors".
   */
  skipped: string[];
  /** App-state key IDs the engine asked the phone for during this sync. */
  requestedKeys: string[];
}

/** `POST /send/message` — the acknowledged message, as it was stored. */
export interface SendResult {
  id: string;
  timestamp: number;
  chat: Jid;
  sender: Jid;
  message: StoredMessage;
}

export interface ExportResult {
  filename: string;
  size: number;
  /** Absolute path of the saved file (Tauri only — browsers don't expose it). */
  path?: string;
}

/** `attachment; filename="chat-1@g.us.json"` → `chat-1@g.us.json`. */
export function filenameFromDisposition(header: string | null): string | null {
  if (!header) return null;
  const star = /filename\*\s*=\s*(?:UTF-8'')?([^;]+)/i.exec(header);
  if (star) {
    try {
      return decodeURIComponent(star[1].trim().replace(/^"|"$/g, ''));
    } catch {
      /* fall through to the plain form */
    }
  }
  const plain = /filename\s*=\s*"([^"]*)"|filename\s*=\s*([^;]+)/i.exec(header);
  const name = (plain?.[1] ?? plain?.[2])?.trim();
  return name || null;
}

@Injectable({ providedIn: 'root' })
export class WhatsappService {
  private readonly tauri = inject(TauriService);
  private unlisten: (() => void) | null = null;
  private starting: Promise<void> | null = null;
  private readonly eventHandlers = new Set<(e: WaEvent) => void>();

  readonly connectionState = signal<ConnectionState>('disconnected');
  /** `true` when the engine has a paired device (`GET /status.loggedIn`). */
  readonly paired = signal(false);
  readonly engineStatus = signal<EngineStatus>('unknown');
  readonly engineError = signal<string | null>(null);
  readonly version = signal<VersionInfo | null>(null);
  readonly qrCodes = signal<string[]>([]);
  /** Running exports keyed by chat JID (from `history_export_progress`). */
  readonly exportProgress = signal<Readonly<Record<Jid, ExportProgress>>>({});
  /** The last `history_sync` notification (conversation/message backfill). */
  readonly lastHistorySync = signal<
    Extract<WaEvent, { type: 'history_sync' }> | null
  >(null);
  /** `true` while the engine is importing a history-sync batch. */
  readonly historySyncing = signal(false);

  /** Ping the engine and subscribe to `wa-event`. Safe to call repeatedly. */
  start(): Promise<void> {
    this.starting ??= this.doStart().finally(() => (this.starting = null));
    return this.starting;
  }

  private async doStart(): Promise<void> {
    if (!(await this.ping())) return;
    if (!this.unlisten) {
      try {
        this.unlisten = await this.tauri.listen<WaEvent>(WA_EVENT, (e) => this.handleEvent(e));
      } catch (e) {
        console.warn('wa-event subscription failed', e);
      }
    }
    await this.resumeSession();
  }

  /**
   * Adopt the engine's own session state, then bring a paired device back
   * online.
   *
   * The engine keeps its pairing in the store but loses the socket when it
   * restarts, and events only describe *changes* — so a client that just
   * loaded would sit in `disconnected` forever and refuse to send. Reading
   * `GET /status` fixes the display; calling `POST /connect` (a no-op when a
   * session is already live) brings the messages back.
   */
  async resumeSession(): Promise<void> {
    let status: EngineSessionStatus;
    try {
      status = await this.status();
    } catch {
      return;
    }
    this.applyStatus(status);
    if (status.loggedIn && !status.connected && !status.replaced && status.state !== 'connecting') {
      try {
        await this.connect();
      } catch (e) {
        this.engineError.set(e instanceof Error ? e.message : String(e));
      }
    }
  }

  /** Map `GET /status` onto `connectionState` / `paired`. */
  applyStatus(status: EngineSessionStatus): void {
    this.paired.set(status.loggedIn);
    // A takeover is a terminal state for this window: the engine reports why,
    // and `resumeSession` refuses to grab the session back on its own.
    if (status.replaced) {
      this.connectionState.set('disconnected');
      this.engineError.set(status.error ?? 'another session took over this device');
      return;
    }
    if (status.error) {
      this.engineError.set(status.error);
    }
    if (status.connected) {
      this.connectionState.set('connected');
      return;
    }
    switch (status.state) {
      case 'logged_out':
        this.connectionState.set('logged_out');
        break;
      case 'connecting':
        // Keep a QR already on screen; otherwise show "connecting".
        if (this.connectionState() !== 'qr') this.connectionState.set('connecting');
        break;
      default:
        this.connectionState.set('disconnected');
    }
  }

  /** Re-run after the endpoint changed (Settings → Connect). */
  async restart(): Promise<void> {
    this.stop();
    await this.start();
  }

  stop(): void {
    this.unlisten?.();
    this.unlisten = null;
  }

  /** `GET /version`; updates `engineStatus`. Returns whether the engine answered. */
  async ping(): Promise<boolean> {
    if (!this.tauri.isTauri && !this.tauri.configured()) {
      this.engineStatus.set('unconfigured');
      this.engineError.set(null);
      return false;
    }
    try {
      this.version.set(await this.tauri.invoke<VersionInfo>('version'));
      this.engineStatus.set('online');
      this.engineError.set(null);
      return true;
    } catch (e) {
      this.engineStatus.set(e instanceof EngineError && e.status === 401 ? 'unauthorized' : 'offline');
      this.engineError.set(e instanceof Error ? e.message : String(e));
      return false;
    }
  }

  /**
   * Subscribe to every `wa-event`. Stores that derive UI state from the stream
   * (presence, group changes) register here instead of polling the engine.
   * Returns an unsubscribe function.
   */
  onEvent(handler: (e: WaEvent) => void): () => void {
    this.eventHandlers.add(handler);
    return () => {
      this.eventHandlers.delete(handler);
    };
  }

  handleEvent(e: WaEvent): void {
    switch (e.type) {
      case 'qr':
        this.qrCodes.set(e.codes);
        this.connectionState.set('qr');
        break;
      case 'pair_success':
      case 'connected':
        this.qrCodes.set([]);
        this.paired.set(true);
        this.connectionState.set('connected');
        this.historySyncing.set(false);
        break;
      case 'keep_alive_restored':
        this.qrCodes.set([]);
        this.connectionState.set('connected');
        break;
      case 'disconnected':
      case 'connect_failure':
        this.connectionState.set('disconnected');
        this.historySyncing.set(false);
        break;
      case 'stream_replaced':
        // Another login took the device over (`conflict type=replaced`). Show
        // it, and leave reconnecting to an explicit Connect — the engine will
        // not retry either (see `ConnectionStatus.replaced`).
        this.connectionState.set('disconnected');
        this.historySyncing.set(false);
        this.engineError.set('another session took over this device — connect manually to take it back');
        break;
      case 'logged_out':
        this.qrCodes.set([]);
        this.paired.set(false);
        this.connectionState.set('logged_out');
        this.historySyncing.set(false);
        break;
      case 'history_export_progress':
        this.exportProgress.update((p) => ({ ...p, [e.chat]: { exported: e.exported, total: e.total } }));
        break;
      case 'history_sync':
        // One notification per conversation/message backfill batch; the engine
        // imports them into `wa-store`, the UI only shows "syncing history…".
        this.lastHistorySync.set(e);
        this.historySyncing.set(true);
        break;
      case 'presence':
      case 'chat_presence':
        // Presence is per-store state (`PresenceStore`), not a global signal.
        break;
      case 'group_info':
      case 'joined_group':
      case 'picture':
        // The group cache lives in the engine; `GroupInfoComponent` re-reads
        // `GET /group/{jid}/info` when a change for its JID arrives.
        break;
      case 'app_state_sync_progress':
      case 'privacy_settings':
      case 'newsletter_live_update':
        break;
      default:
        break;
    }
    this.emitEvent(e);
  }

  private emitEvent(e: WaEvent): void {
    for (const handler of this.eventHandlers) {
      try {
        handler(e);
      } catch (err) {
        console.warn('wa-event handler failed', err);
      }
    }
  }

  async status(): Promise<EngineSessionStatus> {
    try {
      return await this.tauri.invoke<EngineSessionStatus>('status');
    } catch {
      const res = await this.tauri.request('GET', '/status');
      return (await res.json()) as EngineSessionStatus;
    }
  }

  async connect(): Promise<void> {
    await this.tauri.invoke<void>('connect');
    // Optimistically enter connecting — the real state arrives over SSE
    // (`qr` / `connected`) or from the polling fallback in LoginComponent.
    if (this.connectionState() === 'disconnected' || this.connectionState() === 'logged_out') {
      this.connectionState.set(this.paired() ? 'connecting' : 'qr');
    }
  }
  async disconnect(): Promise<void> {
    await this.tauri.invoke<void>('disconnect');
    this.connectionState.set('disconnected');
    this.qrCodes.set([]);
  }
  async logout(): Promise<void> {
    await this.tauri.invoke<void>('logout');
    this.connectionState.set('logged_out');
    this.qrCodes.set([]);
  }

  /**
   * `POST /appstate/sync` — pull the address book and the chat flags from
   * WhatsApp. This is what turns raw JIDs into names: the engine applies the
   * contact mutations into its `ContactStore`, which `GET /chats` and
   * `GET /contacts` then serve.
   */
  async syncAppState(full = false): Promise<AppStateSyncReport> {
    const res = await this.tauri.request('POST', '/appstate/sync', { query: { full: full ? 'true' : undefined } });
    return (await res.json()) as AppStateSyncReport;
  }

  /** `GET /chats` — summary + mute/pin/archive flags for the chat list. */
  listChats(): Promise<ChatListEntry[]> {
    return this.tauri.invoke<ChatListEntry[]>('list_chats');
  }

  /**
   * `GET /chats/:jid/messages` — a backward page of one thread. Without a
   * cursor the newest `limit` messages come back; pass the oldest message the
   * view holds to walk further back. Reads the local database, so it works
   * offline and never waits for the socket.
   */
  listMessages(chat: Jid, opts: { before?: MessageCursor | null; limit?: number } = {}): Promise<MessagePage> {
    return this.tauri.invoke<MessagePage>('list_messages', {
      chat,
      before: opts.before?.timestamp,
      beforeId: opts.before?.id,
      limit: opts.limit,
    });
  }

  /** `POST /send/message` — plain text to one chat (needs a live session). */
  sendMessage(to: Jid, text: string): Promise<SendResult> {
    return this.tauri.invoke<SendResult>('send_message', { to, text });
  }

  /**
   * `POST /receipt/read` — acknowledge incoming messages of one chat.
   * `sender` is the participant in a group chat; 1:1 chats can omit it.
   */
  async markRead(chat: Jid, ids: readonly string[], sender?: Jid): Promise<void> {
    if (ids.length === 0) return;
    await this.tauri.invoke<unknown>('mark_read', { chat, ids, sender });
  }

  chatStats(chat: Jid): Promise<ChatStats> {
    return this.tauri.invoke<ChatStats>('get_chat_stats', { chat });
  }

  /**
   * `POST /history/fetch` — ask our own phone for the messages before the
   * oldest one the engine holds, for one chat or (without `chat`) for every
   * known chat.
   *
   * Asynchronous by nature: the answer is a history-sync blob that arrives
   * minutes later, so the report only says what went out. Watch for the
   * `history_imported` event to learn what landed.
   */
  fetchHistory(chat?: Jid, count?: number): Promise<FetchReport> {
    return this.tauri.invoke<FetchReport>('fetch_history', { chat, count });
  }

  /**
   * `GET /media/{id}` — the attachment's bytes, as a URL a media element can
   * load (the token rides in the query; see `TauriService.mediaUrl`).
   */
  mediaUrl(messageId: string): string {
    return this.tauri.mediaUrl(messageId);
  }

  /** `GET /media/{id}/info` — kind, MIME type and size without downloading. */
  mediaInfo(messageId: string): Promise<MediaInfo> {
    return this.tauri.invoke<MediaInfo>('media_info', { id: messageId });
  }

  config(): Promise<EngineConfig> {
    return this.tauri.invoke<EngineConfig>('get_config');
  }

  /**
   * `GET /debug/log` — the engine's ring buffer of published events and engine
   * notes (always on).
   */
  async debugLog(limit = 200, since?: number): Promise<DebugLog> {
    const res = await this.tauri.request('GET', '/debug/log', {
      query: { limit, since: since ?? undefined },
    });
    return (await res.json()) as DebugLog;
  }

  /** `DELETE /debug/log` — empties the ring buffer, returns how many went. */
  async clearDebugLog(): Promise<number> {
    const res = await this.tauri.request('DELETE', '/debug/log');
    const body = (await res.json()) as { cleared?: number };
    return body.cleared ?? 0;
  }

  // ---------------------------------------------------------------------
  // Contacts, pictures and the blocklist (§3.4)
  // ---------------------------------------------------------------------

  /** `GET /contacts` — every known contact, named ones first. */
  async contacts(): Promise<Contact[]> {
    const res = await this.tauri.request('GET', '/contacts');
    return (await res.json()) as Contact[];
  }

  /** `GET /contact?jid=` — one contact (`found: false` when unknown). */
  async contact(jid: Jid): Promise<Contact> {
    const res = await this.tauri.request('GET', '/contact', { query: { jid } });
    return (await res.json()) as Contact;
  }

  /** `GET /picture?jid=` — `null` when the JID has no picture (or it is unchanged). */
  async picture(jid: Jid, opts: { preview?: boolean; existingId?: string } = {}): Promise<ProfilePicture | null> {
    const res = await this.tauri.request('GET', '/picture', {
      query: { jid, preview: opts.preview ?? undefined, existingId: opts.existingId ?? undefined },
    });
    return (await res.json()) as ProfilePicture | null;
  }

  /** `GET /blocklist`. */
  async blocklist(): Promise<Blocklist> {
    const res = await this.tauri.request('GET', '/blocklist');
    return (await res.json()) as Blocklist;
  }

  /** `POST /block` — returns the updated blocklist. */
  async block(jid: Jid): Promise<Blocklist> {
    return this.json<Blocklist>('POST', '/block', { body: { jid } });
  }

  /** `POST /unblock` — returns the updated blocklist. */
  async unblock(jid: Jid): Promise<Blocklist> {
    return this.json<Blocklist>('POST', '/unblock', { body: { jid } });
  }

  /** Local search (`GET /search`). An empty query never hits the engine. */
  async search(query: string, opts: SearchOptions = {}): Promise<SearchResponse> {
    const mode = opts.mode ?? 'hybrid';
    const q = query.trim();
    if (!q) return { mode, vector: false, results: [] };
    return this.tauri.invoke<SearchResponse>('search_messages', {
      query: q,
      mode,
      chat: opts.chat ?? undefined,
      limit: opts.limit,
    });
  }

  /**
   * "Download full chat history". Browser: `GET /chats/:jid/history/export`
   * → blob → file download under the `Content-Disposition` name. Tauri: the
   * native `export_chat_history` command streams the same endpoint into the
   * Downloads folder. Progress arrives via `history_export_progress` events.
   */
  async exportHistory(chat: Jid, format: ExportFormat = 'json'): Promise<ExportResult> {
    this.exportProgress.update((p) => ({ ...p, [chat]: { exported: 0, total: 0 } }));
    try {
      if (this.tauri.isTauri) {
        return await this.tauri.invoke<ExportResult>('export_chat_history', { chat, format });
      }
      const res = await this.tauri.request('GET', exportHistoryPath(chat), { query: { format } });
      const blob = await res.blob();
      const filename = filenameFromDisposition(res.headers.get('Content-Disposition')) ?? `chat-${chat}.${format}`;
      this.saveBlob(blob, filename);
      return { filename, size: blob.size };
    } finally {
      this.exportProgress.update((p) => {
        const { [chat]: _done, ...rest } = p;
        return rest;
      });
    }
  }

  // ---------------------------------------------------------------------
  // Groups (§3.3), newsletters (§3.5), presence + privacy (§3.4)
  // ---------------------------------------------------------------------

  /** `POST /group/create` — create a group and return its metadata. */
  createGroup(name: string, participants: readonly Jid[]): Promise<GroupInfoData> {
    return this.json<GroupInfoData>('POST', '/group/create', { body: { name, participants } });
  }

  /** `GET /group/{jid}/info`. */
  groupInfo(jid: Jid): Promise<GroupInfoData> {
    return this.json<GroupInfoData>('GET', `${groupPath(jid)}/info`);
  }

  /** `POST /group/{jid}/leave`. */
  async leaveGroup(jid: Jid): Promise<void> {
    await this.json('POST', `${groupPath(jid)}/leave`, { body: {} });
  }

  /** `POST /group/{jid}/participants` — add/remove/promote/demote. */
  async updateGroupParticipants(
    jid: Jid,
    action: GroupParticipantAction,
    participants: readonly Jid[],
  ): Promise<GroupParticipantData[]> {
    const reply = await this.json<ParticipantsReply>('POST', `${groupPath(jid)}/participants`, {
      body: { action, participants },
    });
    return reply.participants;
  }

  /** `POST /group/{jid}/name`. */
  async setGroupName(jid: Jid, name: string): Promise<void> {
    await this.json('POST', `${groupPath(jid)}/name`, { body: { name } });
  }

  /** `POST /group/{jid}/topic` — an empty topic clears the description. */
  async setGroupTopic(jid: Jid, topic: string): Promise<void> {
    await this.json('POST', `${groupPath(jid)}/topic`, { body: { topic } });
  }

  /** `GET /group/{jid}/invite-link` — `reset` rotates the link. */
  async groupInviteLink(jid: Jid, reset = false): Promise<string> {
    const reply = await this.json<InviteLinkReply>('GET', `${groupPath(jid)}/invite-link`, {
      query: reset ? { reset: 'true' } : undefined,
    });
    return reply.link;
  }

  /** `POST /group/join` — an invite code or a full `chat.whatsapp.com` link. */
  async joinGroup(code: string): Promise<Jid> {
    const reply = await this.json<JoinReply>('POST', '/group/join', { body: { code } });
    return reply.jid;
  }

  /** `POST /newsletter/subscribe` — a newsletter JID or a bare id. */
  async newsletterSubscribe(id: string): Promise<void> {
    await this.json('POST', '/newsletter/subscribe', { body: { id } });
  }

  /** `POST /newsletter/unsubscribe`. */
  async newsletterUnsubscribe(id: string): Promise<void> {
    await this.json('POST', '/newsletter/unsubscribe', { body: { id } });
  }

  /** `POST /presence/subscribe` — updates arrive as `presence` events. */
  async subscribePresence(jid: Jid): Promise<void> {
    await this.json('POST', '/presence/subscribe', { body: { jid } });
  }

  /** `POST /presence/send` — announce ourselves as available/unavailable. */
  async sendPresence(state: UserPresenceState): Promise<void> {
    await this.json('POST', '/presence/send', { body: { type: state } });
  }

  /** `POST /presence/chat` — our typing/recording state in `chat`. */
  async sendChatPresence(
    chat: Jid,
    state: ChatPresenceState,
    media: ChatPresenceMedia = 'text',
  ): Promise<void> {
    await this.json('POST', '/presence/chat', { body: { chat, state, media } });
  }

  /** `GET /privacy` — the settings as the server reports them. */
  getPrivacy(): Promise<PrivacySettings> {
    return this.json<PrivacySettings>('GET', '/privacy');
  }

  /** `POST /privacy` — change one setting; returns the settings afterwards. */
  setPrivacy(name: PrivacySettingType, value: PrivacySetting): Promise<PrivacySettings> {
    return this.json<PrivacySettings>('POST', '/privacy', { body: { name, value } });
  }

  /** `request` + JSON decode; a `{error}` body already became an `EngineError`. */
  private async json<T>(
    method: 'GET' | 'POST',
    path: string,
    opts: { query?: Record<string, string>; body?: unknown } = {},
  ): Promise<T> {
    const res = await this.tauri.request(method, path, opts);
    const text = await res.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }

  /** Triggers a browser download. Separate method so tests can spy on it. */
  saveBlob(blob: Blob, filename: string): void {
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.rel = 'noopener';
    a.style.display = 'none';
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }
}
