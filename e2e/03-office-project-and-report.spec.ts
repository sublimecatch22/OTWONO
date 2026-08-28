/**
 * Critical path 3: create an office → give it a team → start a project there →
 * approve the plan → run it → see it verified → export the completion report.
 */

import { expect, test } from '@playwright/test';

import { connectRuntime, freshMachine } from './support/steps';

type Page = import('@playwright/test').Page;

/** Add one of the shipped agents to the open workspace, by the name a user sees. */
async function addAgent(page: Page, name: string) {
  await page.getByLabel('Add an agent').selectOption({ label: `${name} — ${ROLES[name]}` });
}

const ROLES: Record<string, string> = {
  'Executive Orchestrator': 'Coordination',
  Writer: 'Writing',
  'Verification Agent': 'Verification',
};

async function createOffice(page: Page, name: string) {
  await page.goto('/workspaces');
  await page.getByLabel('Kind').selectOption({ label: 'Office' });
  await page.getByLabel('Name').fill(name);
  await page.getByRole('button', { name: 'Create', exact: true }).click();
  await expect(page.getByRole('heading', { name, level: 1 })).toBeVisible();
}

test.describe('office, project and report', () => {
  test.beforeEach(async () => {
    await freshMachine();
  });

  test('an office can be created and staffed with agents', async ({ page }) => {
    await createOffice(page, 'Q3 Operations');

    const team = page.locator('.card', { hasText: 'Team (' });
    await expect(team.getByText('No agents yet')).toBeVisible();

    await addAgent(page, 'Executive Orchestrator');
    await expect(team.getByText('Executive Orchestrator')).toBeVisible();

    await addAgent(page, 'Writer');
    await addAgent(page, 'Verification Agent');
    await expect(page.getByRole('heading', { name: 'Team (3)' })).toBeVisible();

    // Someone has to be in charge, and the interface says who.
    await team
      .getByRole('listitem')
      .filter({ hasText: 'Executive Orchestrator' })
      .getByRole('button', { name: 'Make coordinator' })
      .click();
    await expect(team.locator('.badge', { hasText: 'coordinator' })).toBeVisible();
  });

  test('a project is planned, approved, run, verified and reported', async ({ page }) => {
    await connectRuntime(page);
    await createOffice(page, 'Q3 Operations');
    await addAgent(page, 'Executive Orchestrator');

    await page.goto('/projects');
    await page.getByLabel('What are you trying to achieve?').fill('Quarterly report for Q3');
    await page.getByLabel('Say more about it').fill('Summarise the Q3 numbers for the board.');
    await page
      .getByLabel('How will you know it is done?')
      .fill('Includes revenue for all three months\nUnder 800 words');
    await page.getByLabel('Where this belongs').selectOption({ label: 'Q3 Operations' });
    await page.getByRole('button', { name: 'Create project' }).click();

    await expect(page.getByRole('heading', { name: 'Quarterly report for Q3' })).toBeVisible();
    // Nothing has been planned, so nothing can have run.
    await expect(page.getByText('No tasks yet')).toBeVisible();

    // Planning produces tasks to read before anything is done.
    await page.getByRole('button', { name: 'Plan the work' }).click();
    await expect(page.getByRole('heading', { name: 'Tasks (2)' })).toBeVisible();
    await expect(page.getByText('Gather the figures')).toBeVisible();
    await expect(page.getByText('Write the summary')).toBeVisible();

    // The plan runs only once the user approves it.
    await page.getByRole('button', { name: 'Approve and run' }).click();
    await expect(page.locator('.notice', { hasText: 'Last run' })).toBeVisible();
    await expect(page.getByText(/2 completed/)).toBeVisible();

    // Work was checked rather than assumed good.
    const tasks = page.locator('.card', { hasText: 'Tasks (' });
    await expect(tasks.locator('.badge', { hasText: 'Completed' })).toHaveCount(2);
    await tasks.locator('summary', { hasText: 'Verification' }).first().click();
    await expect(tasks.getByText(/Met — the output covers the criterion/).first()).toBeVisible();

    // The report can be read in the application and taken away as a file.
    await page.getByRole('button', { name: 'Completion report' }).click();
    const report = page.locator('.card', { hasText: 'Completion report' });
    await expect(report.getByText('Quarterly report for Q3')).toBeVisible();
    await expect(report.getByText(/Gather the figures/)).toBeVisible();

    const download = page.waitForEvent('download');
    await report.getByRole('button', { name: 'Download' }).click();
    const file = await download;
    expect(file.suggestedFilename()).toBe('quarterly-report-for-q3-report.md');
  });
});
