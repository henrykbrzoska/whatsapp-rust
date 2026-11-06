import { ComponentFixture, TestBed } from '@angular/core/testing';

import { PLATFORM_DEPS, TauriService } from '../../core/services/tauri.service';
import { WhatsappService } from '../../core/services/whatsapp.service';
import { fakeDeps, jsonResponse } from '../../testing/fakes';
import type { WaEvent } from '../../types/specta';
import { GroupCreateComponent } from './group-create.component';
import { GroupInfoComponent } from './group-info.component';
import { GROUPS_API, GroupInfo, GroupsApi, parseParticipants, sortParticipants } from './groups.api';

/**
 * Fixture covering the fields the UI reads (`name`, `topic`, `participants`,
 * `ownerJid`); the engine type carries every Go `types.GroupInfo` field.
 */
const GROUP: GroupInfo = {
  jid: '123@g.us',
  name: 'Book club',
  topic: 'Monthly reads',
  ownerJid: '1@s.whatsapp.net',
  groupCreated: 1_700_000_000,
  participants: [
    { jid: '3@s.whatsapp.net', isAdmin: false, isSuperAdmin: false, displayName: 'Carol' },
    { jid: '2@s.whatsapp.net', isAdmin: true, isSuperAdmin: false, displayName: 'Bob' },
    { jid: '1@s.whatsapp.net', isAdmin: true, isSuperAdmin: true, displayName: 'Alice' },
  ],
} as unknown as GroupInfo;

function mockApi(): jasmine.SpyObj<GroupsApi> {
  const api = jasmine.createSpyObj<GroupsApi>('GroupsApi', ['getGroupInfo', 'getGroupInviteLink', 'leaveGroup', 'createGroup']);
  api.getGroupInfo.and.resolveTo(GROUP);
  api.getGroupInviteLink.and.resolveTo('https://chat.whatsapp.com/ABC123');
  api.leaveGroup.and.resolveTo();
  api.createGroup.and.resolveTo(GROUP);
  return api;
}

async function settle(fixture: ComponentFixture<unknown>): Promise<void> {
  fixture.detectChanges();
  await fixture.whenStable();
  fixture.detectChanges();
}

describe('GroupInfoComponent', () => {
  let fixture: ComponentFixture<GroupInfoComponent>;
  let api: jasmine.SpyObj<GroupsApi>;
  const el = () => fixture.nativeElement as HTMLElement;
  const q = (id: string) => el().querySelector<HTMLElement>(`[data-testid="${id}"]`);

  beforeEach(async () => {
    api = mockApi();
    TestBed.configureTestingModule({ imports: [GroupInfoComponent], providers: [{ provide: GROUPS_API, useValue: api }] });
    fixture = TestBed.createComponent(GroupInfoComponent);
    fixture.componentRef.setInput('jid', '123@g.us');
    await settle(fixture);
  });

  it('renders name, topic and participants (owner, admins, then members)', () => {
    expect(api.getGroupInfo).toHaveBeenCalledWith('123@g.us');
    expect(q('group-name')?.textContent).toContain('Book club');
    expect(q('group-topic')?.textContent).toContain('Monthly reads');
    const names = Array.from(el().querySelectorAll('[data-testid="participants"] .name')).map((n) => n.textContent?.trim());
    expect(names).toEqual(['Alice', 'Bob', 'Carol']);
    expect(el().querySelector('li[data-jid="1@s.whatsapp.net"] .role')?.textContent).toContain('Owner');
    expect(el().querySelector('li[data-jid="2@s.whatsapp.net"] .role')?.textContent).toContain('Admin');
  });

  it('loads the invite link and copies it to the clipboard', async () => {
    const write = spyOn(navigator.clipboard, 'writeText').and.resolveTo();
    q('load-invite')!.click();
    await settle(fixture);
    expect(q('invite-link')?.textContent).toContain('https://chat.whatsapp.com/ABC123');

    q('copy-invite')!.click();
    await settle(fixture);
    expect(write).toHaveBeenCalledWith('https://chat.whatsapp.com/ABC123');
    expect(q('copy-invite')?.textContent).toContain('Copied');
  });

  it('leaves the group and emits the jid', async () => {
    const left: string[] = [];
    fixture.componentInstance.left.subscribe((j) => left.push(j));
    q('leave')!.click();
    await settle(fixture);
    expect(api.leaveGroup).toHaveBeenCalledWith('123@g.us');
    expect(left).toEqual(['123@g.us']);
  });

  it('shows an error and emits nothing when leaving fails', async () => {
    api.leaveGroup.and.rejectWith(new Error('not a participant'));
    const left: string[] = [];
    fixture.componentInstance.left.subscribe((j) => left.push(j));
    q('leave')!.click();
    await settle(fixture);
    expect(left).toEqual([]);
    expect(el().querySelector('[role="alert"]')?.textContent).toContain('not a participant');
  });

  it('re-reads the group when a group_info event for it arrives', async () => {
    const wa = TestBed.inject(WhatsappService);
    api.getGroupInfo.calls.reset();
    wa.handleEvent({ type: 'group_info', info: { jid: '123@g.us' } } as unknown as WaEvent);
    await settle(fixture);
    expect(api.getGroupInfo).toHaveBeenCalledWith('123@g.us');
  });
});

describe('GROUPS_API default provider', () => {
  it('calls the engine /group routes through WhatsappService', async () => {
    const deps = fakeDeps({ stored: { url: 'http://e', token: 'tok' } });
    TestBed.configureTestingModule({ providers: [{ provide: PLATFORM_DEPS, useValue: deps }] });
    const request = spyOn(TestBed.inject(TauriService), 'request');
    const api = TestBed.inject(GROUPS_API);

    request.and.resolveTo(jsonResponse(GROUP));
    expect(await api.getGroupInfo('123@g.us')).toEqual(GROUP);
    expect(request.calls.mostRecent().args).toEqual(['GET', '/group/123%40g.us/info', {}]);

    request.and.resolveTo(jsonResponse({ link: 'https://chat.whatsapp.com/XYZ' }));
    expect(await api.getGroupInviteLink('123@g.us', true)).toBe('https://chat.whatsapp.com/XYZ');
    expect(request.calls.mostRecent().args).toEqual(['GET', '/group/123%40g.us/invite-link', { query: { reset: 'true' } }]);

    request.and.resolveTo(jsonResponse({ ok: true }));
    await api.leaveGroup('123@g.us');
    expect(request.calls.mostRecent().args.slice(0, 2)).toEqual(['POST', '/group/123%40g.us/leave']);

    request.and.resolveTo(jsonResponse(GROUP));
    expect(await api.createGroup('Book club', ['1@s.whatsapp.net'])).toEqual(GROUP);
    expect(request.calls.mostRecent().args.slice(0, 2)).toEqual(['POST', '/group/create']);
    expect(request.calls.mostRecent().args[2]).toEqual({ body: { name: 'Book club', participants: ['1@s.whatsapp.net'] } });
  });
});

describe('GroupCreateComponent', () => {
  it('creates a group from the name and parsed participants', async () => {
    const api = mockApi();
    TestBed.configureTestingModule({ imports: [GroupCreateComponent], providers: [{ provide: GROUPS_API, useValue: api }] });
    const fixture = TestBed.createComponent(GroupCreateComponent);
    const created: GroupInfo[] = [];
    fixture.componentInstance.created.subscribe((g) => created.push(g));
    fixture.componentInstance.name.set(' Book club ');
    fixture.componentInstance.participants.set('+48123, 456@s.whatsapp.net\n48123');
    await fixture.componentInstance.submit();
    expect(api.createGroup).toHaveBeenCalledWith('Book club', ['48123@s.whatsapp.net', '456@s.whatsapp.net']);
    expect(created).toEqual([GROUP]);
  });

  it('parses participants without duplicates', () => {
    expect(parseParticipants('  1, 2;3 \n 1 ')).toEqual(['1@s.whatsapp.net', '2@s.whatsapp.net', '3@s.whatsapp.net']);
  });

  it('sorts participants owner, admins, then members', () => {
    expect(sortParticipants(GROUP.participants).map((p) => p.jid)).toEqual([
      '1@s.whatsapp.net',
      '2@s.whatsapp.net',
      '3@s.whatsapp.net',
    ]);
  });
});
