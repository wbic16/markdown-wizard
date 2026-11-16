// Parity suite against a LOCAL markdown-wizard instance (write
// workflows). scripts/parity.sh boots the real binary on a scratch
// port with a fresh data dir and exports PARITY_BASE_URL.
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  testMatch: 'parity.spec.js',
  timeout: 90_000,
  expect: { timeout: 10_000 },
  workers: 1,
  reporter: 'list',
  use: {
    baseURL: process.env.PARITY_BASE_URL || 'http://127.0.0.1:18131',
    viewport: { width: 1600, height: 1000 },
  },
});
