// Read-only smoke against the live https://md.phext.io/ — loads the
// editor shell and nothing more. Never writes, never publishes.
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  testMatch: 'live.spec.js',
  timeout: 60_000,
  expect: { timeout: 10_000 },
  workers: 1,
  reporter: 'list',
  use: {
    baseURL: 'https://md.phext.io',
    viewport: { width: 1600, height: 1000 },
  },
});
