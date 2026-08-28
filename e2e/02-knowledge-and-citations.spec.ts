/**
 * Critical path 2: authorise a folder → index it → ask a question →
 * receive an answer that cites the file.
 */

import { expect, test } from '@playwright/test';

import { authoriseKnowledge, connectRuntime, freshMachine } from './support/steps';

const HANDBOOK = '# Leave\n\nEvery employee receives 25 days of annual leave each year.\n';

test.describe('knowledge and citations', () => {
  test.beforeEach(async () => {
    await freshMachine();
  });

  test('a folder can be authorised, indexed, searched and revoked', async ({ page }) => {
    await connectRuntime(page);
    await authoriseKnowledge(page, {
      'handbook.md': HANDBOOK,
      'unrelated.md': '# Bread\n\nA long cold fermentation develops flavour in sourdough.\n',
    });

    const card = page.locator('.card', { hasText: 'otwono-knowledge-' }).first();
    await card.getByRole('button', { name: 'Index now' }).click();

    await expect(page.getByText(/Indexed 2 file\(s\)/)).toBeVisible();
    await expect(card.getByText('2 file(s)')).toBeVisible();

    // Search finds the right file and shows where the passage came from.
    await page.getByLabel('Search your indexed files').fill('how many days of annual leave');
    await page.getByRole('button', { name: 'Search', exact: true }).click();
    await expect(page.getByText('handbook.md').first()).toBeVisible();
    await expect(page.getByText(/25 days of annual leave/)).toBeVisible();

    // Revoking deletes what was indexed, straight away, and the folder can no
    // longer be searched at all.
    await card.getByRole('button', { name: 'Revoke access' }).click();
    await expect(page.getByText(/deleted straight away/i)).toBeVisible();
    await expect(card.getByText('Revoked')).toBeVisible();
    await expect(card.getByText('0 passage(s)')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Search', exact: true })).toHaveCount(0);
  });

  test('an answer that used a file cites it by name and location', async ({ page }) => {
    await connectRuntime(page);
    await authoriseKnowledge(page, { 'handbook.md': HANDBOOK });

    const card = page.locator('.card', { hasText: 'otwono-knowledge-' }).first();
    await card.getByRole('button', { name: 'Index now' }).click();
    await expect(page.getByText(/Indexed 1 file\(s\)/)).toBeVisible();

    await page.goto('/chat');
    await page.getByRole('button', { name: 'New chat' }).first().click();

    // Select the source for this conversation.
    const chosen = page.getByRole('group', { name: 'Knowledge for this chat' }).getByRole('checkbox').first();
    await chosen.click();
    await expect(chosen).toBeChecked();

    await page.getByLabel('Your message').fill('How much annual leave do staff get?');
    await page.getByRole('button', { name: 'Send', exact: true }).click();

    const transcript = page.getByRole('log', { name: 'Conversation' });
    await expect(
      transcript.getByText(/According to the handbook, staff receive 25 days of annual leave/),
    ).toBeVisible();

    // The answer says which of the user's files it came from.
    await expect(transcript.getByText(/source(s)? from your files/).first()).toBeVisible();
    await expect(transcript.getByText('handbook.md').first()).toBeVisible();
  });

  test('a file that cannot be read is reported rather than silently skipped', async ({ page }) => {
    await connectRuntime(page);
    await authoriseKnowledge(page, { 'good.md': 'Readable text about payroll.', 'blank.md': '   ' });

    const card = page.locator('.card', { hasText: 'otwono-knowledge-' }).first();
    await card.getByRole('button', { name: 'Index now' }).click();
    await expect(page.getByText(/Indexed 1 file\(s\); 0 unchanged, 1 skipped, 0 failed/)).toBeVisible();

    await card.getByRole('button', { name: 'Show files' }).click();
    const row = card.getByRole('row', { name: /blank\.md/ });
    await expect(row).toContainText('skipped');
    await expect(row).toContainText('no readable text was found');
  });
});
