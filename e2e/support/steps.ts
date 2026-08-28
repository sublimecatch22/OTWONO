/**
 * Shared steps for the end-to-end tests.
 *
 * They drive the interface the way a person would — clicking what is visible
 * and reading what is on screen — rather than calling the API behind it.
 */

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { expect, type Page } from '@playwright/test';

/** Where the harness recorded its own address and the fake runtime's. */
export function harnessInfo(): { url: string; ollamaUrl: string } {
  const file = path.join(os.tmpdir(), 'otwono-e2e.json');
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

async function control(route: string): Promise<{ dataDir: string }> {
  const response = await fetch(`${harnessInfo().url}/__harness/${route}`, { method: 'POST' });
  if (!response.ok) throw new Error(`harness ${route} failed: ${response.status}`);
  return (await response.json()) as { dataDir: string };
}

/**
 * Start the service over a data directory it has never seen, so a test begins
 * on what is, as far as the application can tell, a new machine.
 */
export async function freshMachine(): Promise<string> {
  const { dataDir } = await control('reset');
  return dataDir;
}

/** Restart the service over the data it already has, as an upgrade would. */
export async function restartService(): Promise<string> {
  const { dataDir } = await control('restart');
  return dataDir;
}

/** Add the fake runtime as a connection and choose a model. */
export async function connectRuntime(page: Page): Promise<void> {
  const { ollamaUrl } = harnessInfo();

  await page.goto('/connections');
  await expect(page.getByRole('heading', { name: 'Connections', level: 1 })).toBeVisible();

  const existing = page.locator('.card', { hasText: 'Test runtime' });
  if (await existing.count()) return;

  await page.getByRole('button', { name: 'Show' }).click();
  await page.getByLabel('Runtime').selectOption('ollama');
  await page.getByLabel('Name').fill('Test runtime');
  await page.getByLabel('Address').fill(ollamaUrl);
  await page.getByRole('button', { name: 'Add connection' }).click();

  const card = page.locator('.card', { hasText: 'Test runtime' });
  await expect(card).toBeVisible();

  await card.getByRole('button', { name: 'Test' }).click();
  await expect(card.getByText(/is running with 2 models available/i)).toBeVisible();

  await card.getByLabel('Default model').selectOption('llama3.1:8b');
  await card.getByLabel('Embedding model').selectOption('nomic-embed-text:latest');

  // A click rather than `check()`: the switch is drawn from the query cache,
  // so its new state arrives on the next render rather than inside the click.
  const use = card.getByLabel('Use this connection');
  await use.click();
  await expect(use).toBeChecked();

  await page.reload();
  await expect(page.locator('.card', { hasText: 'Test runtime' })).toBeVisible();
}

/** Write a folder of files and authorise it as a knowledge source. */
export async function authoriseKnowledge(
  page: Page,
  files: Record<string, string>,
): Promise<string> {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'otwono-knowledge-'));
  for (const [name, contents] of Object.entries(files)) {
    fs.writeFileSync(path.join(directory, name), contents);
  }

  await page.goto('/knowledge');
  // The empty state offers the same button, so aim at the one in the header.
  await page.locator('.screen__head').getByRole('button', { name: 'Authorise a folder' }).click();
  await page.getByLabel('Or type a path').fill(directory);
  await page.getByLabel('Or type a path').press('Enter');

  await expect(page.getByText(directory)).toBeVisible();
  return directory;
}

export async function dismissToasts(page: Page): Promise<void> {
  const dismiss = page.getByRole('button', { name: 'Dismiss' });
  while (await dismiss.count()) {
    await dismiss.first().click();
  }
}
