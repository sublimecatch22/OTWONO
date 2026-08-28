import { defineConfig, devices } from '@playwright/test';

/**
 * End-to-end tests run against the real application: the real local service,
 * the real web build, and a fake AI runtime speaking Ollama's wire protocol.
 * Nothing in the application is stubbed.
 */
export default defineConfig({
  testDir: './e2e',
  testMatch: '**/*.spec.ts',
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: process.env.CI ? [['github'], ['list']] : [['list']],
  retries: process.env.CI ? 1 : 0,

  use: {
    baseURL: 'http://127.0.0.1:1420',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },

  projects: [
    {
      name: 'chromium',
      use: {
        ...devices['Desktop Chrome'],
        launchOptions: {
          // The container ships Chromium at a fixed path.
          executablePath: process.env.PLAYWRIGHT_CHROMIUM_PATH || undefined,
        },
      },
    },
  ],

  webServer: {
    command: 'node e2e/support/server.mjs',
    url: 'http://127.0.0.1:1420/health',
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
});
