/**
 * Critical path 1: first launch → connect a runtime → choose a model →
 * hold a chat that survives a reload.
 */

import { expect, test } from '@playwright/test';

import { connectRuntime, freshMachine } from './support/steps';

test.describe('first run and chat', () => {
  // Each test starts on what the application sees as a new machine.
  test.beforeEach(async () => {
    await freshMachine();
  });

  test('the application loads with chat as its home screen', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveURL(/\/chat$/);
    await expect(page.getByRole('navigation', { name: 'Main' })).toBeVisible();
    await expect(page.getByRole('link', { name: /Chat/ })).toBeVisible();
  });

  test('with no connection, the chat screen says what to do about it', async ({ page }) => {
    await page.goto('/chat');
    await page.getByRole('button', { name: 'New chat' }).first().click();
    await expect(page.getByText(/No model is connected/i)).toBeVisible();
    await expect(page.getByText(/organise projects and index knowledge/i)).toBeVisible();
  });

  test('a runtime can be connected and its models listed', async ({ page }) => {
    await connectRuntime(page);

    const card = page.locator('.card', { hasText: 'Test runtime' });
    await card.getByRole('button', { name: 'Test' }).click();

    // Capabilities the runtime reported are labelled as reported, not guessed.
    await expect(card.getByRole('rowheader', { name: 'llama3.1:8b' })).toBeVisible();
    await expect(card.getByText('reported').first()).toBeVisible();
    await expect(card.getByText('131,072')).toBeVisible();
  });

  test('a chat streams a reply and the conversation survives a reload', async ({ page }) => {
    await connectRuntime(page);

    await page.goto('/chat');
    await page.getByRole('button', { name: 'New chat' }).first().click();

    await page.getByLabel('Your message').fill('Say hello to the tester');
    await page.getByRole('button', { name: 'Send', exact: true }).click();

    const transcript = page.getByRole('log', { name: 'Conversation' });
    await expect(transcript.getByText('Say hello to the tester')).toBeVisible();
    await expect(transcript.getByText(/Hello from the test runtime/i)).toBeVisible();

    // The conversation titled itself from the first message.
    await expect(page.getByRole('heading', { name: 'Say hello to the tester' })).toBeVisible();

    const url = page.url();
    await page.reload();
    await expect(page).toHaveURL(url);
    await expect(transcript.getByText(/Hello from the test runtime/i)).toBeVisible();
  });

  test('the run drawer shows what happened during a reply', async ({ page }) => {
    await connectRuntime(page);
    await page.goto('/chat');
    await page.getByRole('button', { name: 'New chat' }).first().click();

    await page.getByLabel('Your message').fill('Tell me something');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await expect(
      page.getByRole('log', { name: 'Conversation' }).getByText(/test runtime/i),
    ).toBeVisible();

    await page.getByRole('button', { name: /Run details/ }).click();
    const drawer = page.getByRole('complementary', { name: 'Run details' });
    await expect(drawer.getByText('Started')).toBeVisible();
    await expect(drawer.getByText('Finished')).toBeVisible();
  });
});
