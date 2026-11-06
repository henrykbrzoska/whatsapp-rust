// Wire types shared with the engine (`API-WHATSAPP.md`).
//
// The Rust types are the source of truth: `client/src/app/types/specta.ts` is
// GENERATED from them by `npm run specta` (see
// `engine/crates/bebok-server/src/bin/export-types.rs`). Client-only types
// (query options, UI helpers) stay hand-written below.

import type { ChatStats, ChatSummary, SearchHit, SearchMode, StoredMessage, WaEvent } from '../types/specta';

export type { ChatStats, ChatSummary, SearchHit, SearchMode, StoredMessage, WaEvent };

export type Jid = string;

export const SEARCH_MODES: readonly SearchMode[] = ['keyword', 'vector', 'hybrid'];

export interface SearchResponse {
  mode: SearchMode;
  /** `false` when the engine runs without an embedder (no `fastembed`). */
  vector: boolean;
  results: SearchHit[];
}

export interface SearchOptions {
  mode?: SearchMode;
  chat?: Jid | null;
  limit?: number;
}

export type ExportFormat = 'json' | 'jsonl' | 'csv';
export const EXPORT_FORMATS: readonly ExportFormat[] = ['json', 'jsonl', 'csv'];

export interface VersionInfo {
  version: string;
  protocolRef: string;
}

export interface ExportProgress {
  exported: number;
  total: number;
}
