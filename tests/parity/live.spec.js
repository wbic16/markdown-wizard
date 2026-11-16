// Read-only smoke against the live https://md.phext.io/.
// Loads the editor the way a first-time visitor meets it and checks
// the shell is whole. Writes nothing, publishes nothing.

const { test, expect } = require('@playwright/test');

test('live md.phext.io → the editor shell loads with a seeded bundle', async ({ page }) => {
  const res = await page.goto('/');
  expect(res.status()).toBe(200);
  await expect(page.locator('#editor')).toBeVisible();
  await expect(page.locator('#sidebar .entry')).toHaveCount(3);
  await expect(page.locator('#releaseBtn')).toBeVisible();
});
