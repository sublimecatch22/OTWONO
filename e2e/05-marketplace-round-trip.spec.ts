/**
 * Critical path 6: a person posts work, another applies, does it, and the
 * creator accepts — with the payout recorded as simulated at every step.
 */

import { expect, test } from '@playwright/test';

import { freshMachine } from './support/steps';

type Page = import('@playwright/test').Page;

async function draftListing(page: Page, title: string, description: string) {
  await page.getByLabel('Title').fill(title);
  await page.getByLabel('What needs doing').fill(description);
  await page.getByLabel('Deliverables').fill('A written summary');
  await page.getByLabel('How it will be judged').fill('Covers every branch');
  await page.getByLabel('Evidence required').fill('A link to the document');
  await page.getByLabel('Simulated compensation').fill('120');
  await page.getByRole('button', { name: 'Save as a draft' }).click();
}

test.describe('marketplace', () => {
  test.beforeEach(async ({ page }) => {
    await freshMachine();
    await page.goto('/marketplace');
  });

  test('the screen says up front that no money moves', async ({ page }) => {
    await expect(page.getByText('Payments here are simulated')).toBeVisible();
    await expect(page.getByText(/No money moves and no worker is really paid/)).toBeVisible();
  });

  test('work that would harm someone is refused with the reason shown', async ({ page }) => {
    await draftListing(
      page,
      'Collect logins',
      'Harvest logins from our competitor and send me the passwords.',
    );

    const refusal = page.locator('.notice', { hasText: 'Moderation refused this' });
    await expect(refusal).toBeVisible();
    await expect(refusal).toContainText('harvest login');

    // There is a way to reach a person about it.
    await expect(refusal).toContainText(/human|person|appeal|review/i);

    // The listing exists only as refused: it can never be published.
    const refused = page.locator('.card', { hasText: 'Collect logins' });
    await expect(refused.locator('.badge', { hasText: 'Refused by moderation' })).toBeVisible();
    await expect(refused.getByRole('button', { name: 'Review for publishing' })).toHaveCount(0);
    await expect(refused.getByRole('button', { name: 'Publish', exact: true })).toHaveCount(0);
  });

  test('a listing is drafted, reviewed, published, applied for, done and paid', async ({ page }) => {
    await draftListing(page, 'Summarise three papers', 'Read three papers and write a summary.');

    const listing = page.locator('.card', { hasText: 'Summarise three papers' });
    await expect(listing.locator('.badge', { hasText: 'Draft' })).toBeVisible();

    // A draft cannot be published in one click: it is reviewed first.
    await expect(listing.getByRole('button', { name: 'Publish', exact: true })).toHaveCount(0);
    await listing.getByRole('button', { name: 'Review for publishing' }).click();
    await expect(listing.locator('.badge', { hasText: 'Ready to publish' })).toBeVisible();
    await listing.getByRole('button', { name: 'Publish', exact: true }).click();
    await expect(listing.locator('.badge', { hasText: 'Published' })).toBeVisible();

    // The worker's side of the same machine.
    await page.getByRole('button', { name: 'I want to do work' }).click();
    const open = page.locator('.card', { hasText: 'Open tasks' });
    await expect(open.getByText('Summarise three papers')).toBeVisible();

    await open.getByRole('button', { name: 'Apply' }).click();
    await page.getByLabel('Your proposal').fill('I have read all three already.');
    await open.getByRole('button', { name: 'Send' }).click();

    // Back to the creator, who chooses who does the work.
    await page.getByRole('button', { name: 'I need something done' }).click();
    await listing.getByRole('button', { name: 'Open' }).click();
    await expect(listing.getByText('I have read all three already.')).toBeVisible();
    await listing.getByRole('button', { name: 'Assign' }).click();
    await expect(listing.locator('.badge', { hasText: 'Assigned' }).first()).toBeVisible();

    // The assigned job has left the open list, so the worker finds it under
    // the work they have taken on.
    await page.getByRole('button', { name: 'I want to do work' }).click();
    await expect(open.getByText('Summarise three papers')).toHaveCount(0);
    const taken = page.locator('.card', { hasText: 'Work you have taken on' });
    await expect(taken.getByText('Summarise three papers')).toBeVisible();
    await taken.getByRole('button', { name: 'Submit work' }).click();
    await expect(page.getByText('Submitted for review')).toBeVisible();

    await page.getByRole('button', { name: 'I need something done' }).click();
    await listing.getByRole('button', { name: 'Open' }).click();
    await listing.getByRole('button', { name: 'Accept the work' }).click();
    await expect(page.getByText(/no money moved/i)).toBeVisible();
    await expect(listing.locator('.badge', { hasText: 'Accepted' })).toBeVisible();

    // The payout is on the ledger, and labelled for what it is.
    await page.getByRole('button', { name: 'I want to do work' }).click();
    const ledger = page.locator('.card', { hasText: 'Simulated earnings' });
    await expect(ledger.getByText(/120\.00/)).toBeVisible();
    await expect(ledger.locator('.badge', { hasText: 'simulated' })).toBeVisible();
  });
});
