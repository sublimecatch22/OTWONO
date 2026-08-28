/**
 * Critical path 4: a boardroom takes a question, each member gives a position,
 * and the chair's synthesis records the disagreement rather than hiding it.
 */

import { expect, test } from '@playwright/test';

import { connectRuntime, freshMachine } from './support/steps';

type Page = import('@playwright/test').Page;

const ROLES: Record<string, string> = {
  'Executive Orchestrator': 'Coordination',
  'Security Reviewer': 'Security',
  'Budget Reviewer': 'Finance',
};

async function addAgent(page: Page, name: string) {
  await page.getByLabel('Add an agent').selectOption({ label: `${name} — ${ROLES[name]}` });
}

test.describe('boardroom sessions', () => {
  test.beforeEach(async () => {
    await freshMachine();
  });

  test('a session produces a synthesis, the dissent and what is still open', async ({ page }) => {
    await connectRuntime(page);

    await page.goto('/workspaces');
    await page.getByLabel('Kind').selectOption({ label: 'Boardroom' });
    await page.getByLabel('Name').fill('Release Board');
    await page.getByRole('button', { name: 'Create', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Release Board', level: 1 })).toBeVisible();

    await addAgent(page, 'Executive Orchestrator');
    await addAgent(page, 'Security Reviewer');
    await addAgent(page, 'Budget Reviewer');
    await expect(page.getByRole('heading', { name: 'Team (3)' })).toBeVisible();

    // Someone has to chair, or there is nobody to write the synthesis.
    await page
      .locator('.card', { hasText: 'Team (' })
      .getByRole('listitem')
      .filter({ hasText: 'Executive Orchestrator' })
      .getByRole('button', { name: 'Make coordinator' })
      .click();

    const sessions = page.locator('.card', { hasText: 'Sessions' });
    await page.getByLabel('The question for this session').fill('Should we ship on Friday?');
    await sessions.getByRole('button', { name: 'Start a session' }).click();

    await sessions.getByRole('button', { name: 'Run', exact: true }).click();
    await expect(page.getByText('The session finished')).toBeVisible();

    // The chair's own contribution repeats these headings in the transcript
    // below, so aim at the synthesis panel rather than at the words.
    const synthesis = page.locator('.session > .card').first();
    await expect(synthesis.getByText(/wait for the audit to close/)).toBeVisible();

    // Disagreement is reported, not smoothed over.
    await expect(synthesis.getByRole('heading', { name: 'Dissent' })).toBeVisible();
    await expect(synthesis.getByText(/preferred shipping on Friday/)).toBeVisible();
    await expect(synthesis.getByRole('heading', { name: 'Unresolved' })).toBeVisible();
    await expect(synthesis.getByText(/Who signs off the audit\?/)).toBeVisible();
    await expect(synthesis.getByText('Delay the release to Monday.')).toBeVisible();

    // Every member spoke, and each contribution says which stage it came from
    // and whether it was sourced or speculation.
    const transcript = page.locator('.session > .card').nth(1);
    await expect(transcript.getByText('Security Reviewer').first()).toBeVisible();
    await expect(transcript.getByText('Budget Reviewer').first()).toBeVisible();
    await expect(transcript.locator('.badge', { hasText: 'position' }).first()).toBeVisible();
  });
});
