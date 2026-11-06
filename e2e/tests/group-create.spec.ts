import { engineUrl, expect, instrumentSse, test } from '../fixtures';

/**
 * Flow 3 — creating a group. This one is a complete UI flow: the client has a
 * real component (`features/groups/group-create.component.ts`, route
 * `/group/new`) wired to `GROUPS_API.createGroup` → `POST /group/create`
 * (`API-WHATSAPP.md` §3.3). The mock answers with a `GroupInfoData` payload
 * (specta shape) and records the request, so the spec asserts the full round
 * trip: typed input → parsed JIDs → HTTP command → reply.
 */
test.describe('group create', () => {
  test('creates a group from the form', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl());
    await expect(page.locator('.engine-status')).toHaveAttribute('data-status', 'online');

    // Reachable from the chat list, exactly like a user would.
    await page.getByTestId('new-group').click();
    await expect(page).toHaveURL(/\/group\/new$/);
    await expect(page.getByRole('heading', { name: 'New group' })).toBeVisible();

    const createButton = page.getByTestId('create');
    // No name yet → the form refuses to submit.
    await expect(createButton).toBeDisabled();

    await page.getByTestId('group-name').fill('E2E mock group');
    await page.getByTestId('group-participants').fill('48123000001, +48222000002\n48222000002@s.whatsapp.net');
    await expect(createButton).toBeEnabled();
    await createButton.click();

    // `POST /group/create` with the name and de-duplicated, normalised JIDs.
    await expect
      .poll(async () => (await mock.requests()).filter((r) => r.path === '/group/create').length)
      .toBe(1);
    const call = (await mock.requests()).find((r) => r.path === '/group/create');
    expect(call?.method).toBe('POST');
    expect(call?.authorization).toBe('Bearer e2e-token');
    expect(call?.body).toEqual({
      name: 'E2E mock group',
      participants: ['48123000001@s.whatsapp.net', '48222000002@s.whatsapp.net'],
    });
    expect(call?.status).toBe(200);

    // The engine stored the group; the client did not error out.
    await expect.poll(async () => (await mock.state()).groups.length).toBe(1);
    await expect(page.getByRole('alert')).toHaveCount(0);
    await expect(createButton).toHaveText('Create group');
    await expect(createButton).toBeEnabled();
  });

  test('a whitespace-only name never reaches the engine', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl());
    await expect(page.locator('.engine-status')).toHaveAttribute('data-status', 'online');
    await page.goto(engineUrl('/group/new'));

    // `GroupCreateComponent.submit` trims the name and bails out when it is empty.
    await page.getByTestId('group-name').fill('   ');
    await expect(page.getByTestId('create')).toBeDisabled();
    expect((await mock.requests()).some((r) => r.path === '/group/create')).toBe(false);
  });

  test('an engine error is surfaced in the form', async ({ page, mock }) => {
    await instrumentSse(page);
    await page.goto(engineUrl());
    await expect(page.locator('.engine-status')).toHaveAttribute('data-status', 'online');
    await page.goto(engineUrl('/group/new'));

    // The mock answers 400 `{ error }` for names containing "reject".
    await page.getByTestId('group-name').fill('reject me');
    await page.getByTestId('create').click();

    await expect(page.locator('.group-create [role="alert"]')).toContainText('group name rejected by the mock engine');
    await expect(page.getByTestId('create')).toHaveText('Create group');
    expect((await mock.requests()).some((r) => r.path === '/group/create' && r.status === 400)).toBe(true);
  });
});
