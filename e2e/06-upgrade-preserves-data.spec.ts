/**
 * Critical path 7: restart the service over a data directory it has already
 * written — what an upgrade does — and find every kind of work still there.
 */

import { expect, test } from '@playwright/test';

import { authoriseKnowledge, connectRuntime, freshMachine, restartService } from './support/steps';

test.describe('upgrade over existing data', () => {
  test('settings, chats, projects, agents and knowledge all survive a restart', async ({
    page,
  }) => {
    await freshMachine();
    await connectRuntime(page);

    // A preference the user changed.
    await page.goto('/settings');
    await page.getByLabel('Theme').selectOption('dark');
    await expect(page.getByText(/Saved/i).first()).toBeVisible();

    // A conversation with a real reply in it.
    await page.goto('/chat');
    await page.getByRole('button', { name: 'New chat' }).first().click();
    await page.getByLabel('Your message').fill('Remember this across the upgrade');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    const transcript = page.getByRole('log', { name: 'Conversation' });
    await expect(transcript.getByText(/Hello from the test runtime/i)).toBeVisible();
    const chatUrl = page.url();

    // An indexed folder.
    await authoriseKnowledge(page, { 'handbook.md': 'Staff receive 25 days of annual leave.\n' });
    const source = page.locator('.card', { hasText: 'otwono-knowledge-' }).first();
    await source.getByRole('button', { name: 'Index now' }).click();
    await expect(page.getByText(/Indexed 1 file\(s\)/)).toBeVisible();

    // A project with a plan.
    await page.goto('/projects');
    await page.getByLabel('What are you trying to achieve?').fill('Survive the upgrade');
    await page.getByLabel('Say more about it').fill('Prove nothing is lost on restart.');
    await page.getByRole('button', { name: 'Create project' }).click();
    await page.getByRole('button', { name: 'Plan the work' }).click();
    await expect(page.getByRole('heading', { name: 'Tasks (2)' })).toBeVisible();
    const projectUrl = page.url();

    // An agent the user has edited.
    await page.goto('/agents');
    await page.getByRole('button', { name: /^Writer/ }).click();
    await page.getByLabel('Name').fill('Upgrade Survivor');
    await page.getByRole('button', { name: 'Save changes' }).click();
    await expect(page.getByRole('button', { name: /^Upgrade Survivor/ })).toBeVisible();

    // The upgrade: the same data directory, a newly started service.
    await restartService();
    await page.reload();

    await page.goto('/settings');
    await expect(page.getByLabel('Theme')).toHaveValue('dark');

    await page.goto(chatUrl);
    await expect(transcript.getByText('Remember this across the upgrade')).toBeVisible();
    await expect(transcript.getByText(/Hello from the test runtime/i)).toBeVisible();

    await page.goto(projectUrl);
    await expect(page.getByRole('heading', { name: 'Tasks (2)' })).toBeVisible();
    await expect(page.getByText('Gather the figures')).toBeVisible();

    await page.goto('/agents');
    await expect(page.getByRole('button', { name: /^Upgrade Survivor/ })).toBeVisible();

    await page.goto('/connections');
    await expect(page.locator('.card', { hasText: 'Test runtime' })).toBeVisible();

    // The knowledge index is still searchable, not just still listed.
    await page.goto('/knowledge');
    await expect(page.locator('.card', { hasText: 'otwono-knowledge-' }).first()).toContainText(
      '1 file(s)',
    );
    await page.getByLabel('Search your indexed files').fill('annual leave');
    await page.getByRole('button', { name: 'Search', exact: true }).click();
    await expect(page.getByText('handbook.md').first()).toBeVisible();
  });
});
