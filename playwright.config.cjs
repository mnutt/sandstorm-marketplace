const { defineConfig } = require('@playwright/test');
module.exports = defineConfig({
  testDir: './tests/acceptance',
  // Tests mutate the same disposable registry; each test resets it.
  workers: 1,
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  timeout: 30000,
  reporter: [['list'], ['html', { open: 'never' }]],
  use: {
    baseURL: 'http://127.0.0.1:9940',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    launchOptions: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH
      ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH } : {},
  },
  webServer: {
    command: 'cargo test --locked -p marketplace-grain --test acceptance_server -- --ignored --nocapture',
    url: 'http://127.0.0.1:9940/health',
    timeout: 180000,
    reuseExistingServer: false,
    env: { MARKETPLACE_TEST_PORT: '9940' },
  },
});
