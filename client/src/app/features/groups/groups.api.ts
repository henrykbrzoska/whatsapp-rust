// Group API contract for Phase 5 Task F, backed by the engine's `/group/*`
// routes (see `API-WHATSAPP.md` §3.3). The wire shapes are the generated
// `specta` types; `GROUPS_API` is the seam the components use so tests can
// substitute a spy.

import { InjectionToken, inject } from '@angular/core';

import { Jid } from '../../core/models';
import { WhatsappService } from '../../core/services/whatsapp.service';
import type { GroupInfoData, GroupParticipantData } from '../../types/specta';

export type { GroupInfoData, GroupParticipantData };
/** `GET /group/{jid}/info` — name, topic, participants, settings. */
export type GroupInfo = GroupInfoData;
/** One member of a group. */
export type GroupParticipant = GroupParticipantData;

export interface GroupsApi {
  getGroupInfo(jid: Jid): Promise<GroupInfo>;
  getGroupInviteLink(jid: Jid, reset?: boolean): Promise<string>;
  leaveGroup(jid: Jid): Promise<void>;
  createGroup(name: string, participants: readonly Jid[]): Promise<GroupInfo>;
}

/** Default implementation: the real `WhatsappService` group commands. */
export function groupsApi(): GroupsApi {
  const wa = inject(WhatsappService);
  return {
    getGroupInfo: (jid) => wa.groupInfo(jid),
    getGroupInviteLink: (jid, reset = false) => wa.groupInviteLink(jid, reset),
    leaveGroup: (jid) => wa.leaveGroup(jid),
    createGroup: (name, participants) => wa.createGroup(name, participants),
  };
}

export const GROUPS_API = new InjectionToken<GroupsApi>('GROUPS_API', {
  providedIn: 'root',
  factory: groupsApi,
});

/** Splits a free-form participant list (commas, whitespace, newlines) into JIDs. */
export function parseParticipants(input: string): Jid[] {
  const out = input
    .split(/[\s,;]+/)
    .map((s) => s.trim())
    .filter(Boolean)
    .map((s) => (s.includes('@') ? s : `${s.replace(/^\+/, '')}@s.whatsapp.net`));
  return [...new Set(out)];
}

/** Display order: owner first, then admins, then everyone else by name. */
export function sortParticipants(
  participants: readonly GroupParticipant[],
): GroupParticipant[] {
  const rank = (p: GroupParticipant) => (p.isSuperAdmin ? 0 : p.isAdmin ? 1 : 2);
  const name = (p: GroupParticipant) => p.displayName || p.phoneNumber || p.jid;
  return [...participants].sort((a, b) => rank(a) - rank(b) || name(a).localeCompare(name(b)));
}
