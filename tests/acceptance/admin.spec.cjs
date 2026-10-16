const { test, expect } = require('@playwright/test');

async function queue(request) { return (await (await request.get('/admin/admin')).json()).releases; }
async function publicVersion(request) {
  const catalog = await (await request.get('/public/apps/index.json')).json();
  return catalog.apps[0]?.versionNumber;
}
async function decide(page, card, button) {
  await card.getByRole('button', { name: button, exact: true }).click();
  if (button === 'Reject / request update') await page.getByLabel('Reason for the author').fill('Please fix the broken import.');
  await page.locator('#decision-submit').click();
  await expect(page.locator('#decision')).not.toBeVisible();
}
function release(page, version) {
  return page.locator('#app-detail .release').filter({ has: page.getByRole('heading', { name: `1.0 (build ${version})`, exact: true }) });
}
test.beforeEach(async ({ request }) => {
  expect((await request.post('/__test/reset')).ok()).toBeTruthy();
});
test('review, publish, history, rollback, rejection, and restoring latest', async ({ page, request }) => {
  await page.goto('/admin/');
  await expect(page.getByRole('table')).toBeVisible();
  await expect(page.locator('#review-queue .card')).toHaveCount(2);
  await page.getByRole('link', { name: 'Rust test app', exact: true }).click();
  await expect(page.locator('#app-detail .release')).toHaveCount(4);
  await page.reload();
  await expect(release(page, 3)).toBeVisible();
  await decide(page, release(page, 3), 'Approve');
  expect(await publicVersion(request)).toBe(3);
  await decide(page, release(page, 1), 'Roll back to this version');
  expect(await publicVersion(request)).toBe(1);
  await page.getByRole('button', { name: 'Use latest approved', exact: true }).click();
  await page.locator('#decision-submit').click();
  await expect(page.locator('#decision')).not.toBeVisible();
  expect(await publicVersion(request)).toBe(3);
  await decide(page, release(page, 3), 'Reject / request update');
  expect(await publicVersion(request)).toBe(2);
  expect((await queue(request)).find(row => row.app.versionNumber === 3).reason).toBe('Please fix the broken import.');
  await page.getByRole('link', { name: 'All apps' }).click();
  await expect(page.getByRole('table')).toBeVisible();
  await page.goBack();
  await expect(page.getByRole('heading', { name: 'Version history' })).toBeVisible();
});
test('reviewers download embargoed packages but cannot approve or expose them publicly', async ({ page, request }) => {
  const rows = await queue(request);
  const privateRelease = rows.find(row => row.app.versionNumber === 4);
  await page.goto(`/reviewer/#app/${privateRelease.app.appId}`);
  await expect(page.getByText('Read-only reviewer access.', { exact: false })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Approve', exact: true })).toHaveCount(0);
  const before = await (await request.get('/public/apps/index.json')).json();
  await release(page, 4).getByRole('link', { name: 'Preview listing' }).click();
  await expect(page.getByRole('status')).toContainText('Private listing preview · Build 4');
  await expect(page.getByRole('heading', { name: 'Description', exact: true })).toBeVisible();
  await expect(page.locator('.description-body')).toContainText('A signed integration-test package.');
  await expect(page.locator('.icon')).toHaveAttribute('src', /^data:image\/svg\+xml;base64,/);
  await expect(page.locator('.shots img')).toHaveAttribute('src', /^data:image\/png;base64,/);
  await expect.poll(() => page.locator('.shots img').evaluate(img => img.complete && img.naturalWidth > 0)).toBe(true);
  await expect.poll(() => page.locator('.icon').evaluate(img => img.complete && img.naturalWidth > 0)).toBe(true);
  await page.locator('.shot-button').click();
  await expect(page.locator('#lightbox')).toHaveAttribute('aria-hidden', 'false');
  await page.getByRole('button', { name: 'Close', exact: true }).click();
  expect(await (await request.get('/public/apps/index.json')).json()).toEqual(before);
  expect((await request.get(`/submitter/preview/${privateRelease.app.packageId}`)).ok()).toBeFalsy();
  expect((await request.get(`/public/preview/${privateRelease.app.packageId}`)).status()).toBe(404);
  const downloaded = page.waitForEvent('download');
  await page.getByRole('link', { name: 'Download SPK', exact: true }).click();
  const download = await downloaded;
  expect(download.suggestedFilename()).toBe(`${privateRelease.app.packageId}.spk`);
  expect(await download.failure()).toBeNull();
  expect((await request.get(`/public/packages/${privateRelease.app.packageId}`)).status()).toBe(404);
  expect((await request.get(`/submitter/download/${privateRelease.app.packageId}`)).ok()).toBeFalsy();
  expect((await request.post(`/reviewer/approve/${privateRelease.app.packageId}`)).ok()).toBeFalsy();
  await page.getByRole('button', { name: 'Install for testing' }).click();
  await expect(page.getByRole('dialog')).toContainText('This release is private.');
  await page.getByLabel('Your Sandstorm server URL').fill('http://127.0.0.1:9940');
  const appsPopup = page.waitForEvent('popup');
  await page.getByRole('button', { name: 'Open Apps page' }).click();
  const apps = await appsPopup;
  await expect(apps).toHaveURL('http://127.0.0.1:9940/apps');
  expect(await apps.evaluate(() => window.opener)).toBeNull();
  await apps.close();
  await page.getByRole('link', { name: 'Back to app history' }).click();
  const approved = rows.find(row => row.app.versionNumber === 2);
  await release(page, 2).getByRole('link', { name: 'Preview listing' }).click();
  await page.getByRole('status').filter({ hasText: 'Build 2' }).waitFor();
  await page.getByRole('button', { name: 'Install for testing' }).click();
  await page.getByLabel('Your Sandstorm server URL').fill('http://127.0.0.1:9940');
  const popup = page.waitForEvent('popup');
  await page.getByRole('button', { name: 'Open installer' }).click();
  const installer = await popup;
  await expect(installer).toHaveURL(new RegExp(`/install/${approved.app.packageId}\\?url=`));
  const url = new URL(installer.url());
  expect(url.searchParams.get('url')).toBe(`http://127.0.0.1:9940/public/packages/${approved.app.packageId}`);
  await installer.close();
});
test('idle refresh continues after navigation and discovers new submissions', async ({ page, request }) => {
  await page.clock.install();
  await page.goto('/admin/');
  await page.getByRole('link', { name: 'Rust test app', exact: true }).click();
  await page.getByRole('link', { name: 'All apps' }).click();
  await expect(page.locator('#catalog-heading h1')).toBeFocused();
  expect((await request.post('/__test/submit', { data: '5' })).ok()).toBeTruthy();
  await page.clock.fastForward(30001);
  await expect(page.locator('#review-queue .card')).toHaveCount(3);
});
test('refresh does not interrupt a rejection draft', async ({ page, request }) => {
  await page.clock.install();
  await page.goto('/admin/');
  await page.getByRole('link', { name: 'Rust test app', exact: true }).click();
  await release(page, 3).getByRole('button', { name: 'Reject / request update', exact: true }).click();
  await page.getByLabel('Reason for the author').fill('Draft review');
  await request.post('/__test/submit', { data: '5' });
  await page.clock.fastForward(30001);
  await expect(page.getByLabel('Reason for the author')).toHaveValue('Draft review');
  await expect(page.locator('#decision')).toBeVisible();
});
test('refresh failure after a saved decision is visible on the page', async ({ page, request }) => {
  await page.goto('/admin/');
  await page.getByRole('link', { name: 'Rust test app', exact: true }).click();
  await request.post('/__test/fail-next-queue');
  await decide(page, release(page, 3), 'Approve');
  await expect(page.locator('#error')).toContainText('The decision was saved, but the view could not refresh');
  await expect(page.locator('#message')).not.toBeVisible();
  expect(await publicVersion(request)).toBe(3);
});
test('legacy malformed packages can be rejected through the admin UI', async ({ page, request }) => {
  const badId = await (await request.post('/__test/legacy-invalid')).text();
  const row = (await queue(request)).find(row => row.app.packageId === badId);
  expect(row.validationError).toContain('utf-8');
  await page.goto(`/admin/#app/${row.app.appId}`);
  const card = page.locator('#app-detail .release').filter({ hasText: 'Unlabeled (build 99)' });
  await expect(card).toContainText('Invalid package metadata');
  await expect(card.getByRole('button', { name: 'Approve', exact: true })).toHaveCount(0);
  await decide(page, card, 'Reject / request update');
  expect(await publicVersion(request)).toBe(2);
  expect((await queue(request)).find(row => row.app.packageId === badId).review).toBe('needsUpdate');
});
test('mobile layout preserves table and review navigation', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/admin/');
  await expect(page.getByRole('table')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
  await page.locator('#review-queue').getByRole('link', { name: 'View app history' }).first().click();
  await expect(page.getByRole('heading', { name: 'Version history' })).toBeVisible();
});


test('malformed signed submissions are rejected before affecting the catalog', async ({ page, request }) => {
  const bytes = await (await request.get('/__test/malformed.spk')).body();
  const upload = await request.post('/submitter/upload', { data: bytes });
  expect(upload.ok()).toBeFalsy();
  expect(await upload.text()).toContain('invalid marketplace metadata');
  expect(await queue(request)).toHaveLength(4);
  expect(await publicVersion(request)).toBe(2);
  await page.goto('/admin/');
  await expect(page.getByRole('table')).toBeVisible();
});


test('experimental mode filters to pending updates and preserves mode through navigation', async ({ page, request }) => {
  const newId = await (await request.post('/__test/new-app')).text();
  const rows = await queue(request);
  const pending = rows.find(row => row.app.versionNumber === 3);
  const stable = rows.find(row => row.app.versionNumber === 2);
  const firstTime = rows.find(row => row.app.packageId === newId);
  expect((await request.get(`/packages/${newId}`)).status()).toBe(404);
  expect((await request.get(`/experimental/${firstTime.app.appId}.json`)).status()).toBe(404);
  const index = await (await request.get('/experimental/index.json')).json();
  expect(index.apps.map(app => app.packageId)).toEqual([pending.app.packageId]);

  await request.post(`/admin/approve/${newId}`);
  await page.goto('/');
  await expect(page.locator('[data-install]')).toHaveCount(2);
  await expect(page.locator(`[data-install][data-package-id="${stable.app.packageId}"]`)).toHaveCount(1);
  await page.goto('/?experimental=false');
  await expect(page.locator('[data-install]')).toHaveCount(2);
  await page.goto('/?experimental=true&host=https%3A%2F%2Fsandstorm.example');
  await expect(page.getByRole('status')).toContainText('Experimental mode');
  await expect(page.locator('[data-install]')).toHaveAttribute('data-package-id', pending.app.packageId);
  await expect(page.getByText('Unreviewed app')).toHaveCount(0);
  await page.locator(`[data-tag="${pending.app.categories[0]}"]`).click();
  await expect(page).toHaveURL(/experimental=true/);
  await expect(page.locator('[data-install]')).toHaveAttribute('data-package-id', pending.app.packageId);
  await page.getByRole('searchbox', { name: 'Search apps' }).fill('Rust');
  await page.getByRole('searchbox', { name: 'Search apps' }).press('Enter');
  await expect(page).toHaveURL(/experimental=true/);
  await page.locator('.card-link').click();
  await expect(page.getByRole('status')).toHaveText('Experimental release · Pending approval');
  await expect(page.locator('.description-body')).toHaveText(/Description for build 3/);
  await expect(page.locator('.description-body')).not.toHaveText(/Description for build 2/);
  await expect(page.locator('.icon')).toHaveAttribute('src', `/images/${pending.app.imageId}`);
  const install = new URL(await page.locator('[data-install]').getAttribute('href'));
  expect(install.pathname).toBe(`/install/${pending.app.packageId}`);
  expect(install.searchParams.get('url')).toBe(`http://127.0.0.1:9940/packages/${pending.app.packageId}`);
  await expect(page.getByRole('link', { name: 'Download SPK' })).toHaveAttribute('href', `/packages/${pending.app.packageId}`);
  await page.locator('.shot-button').click();
  await expect(page.locator('#lightbox')).toHaveAttribute('aria-hidden', 'false');
  await page.getByRole('button', { name: 'Close', exact: true }).click();
  await page.getByRole('link', { name: 'Back to all apps' }).click();
  await expect(page).toHaveURL(/experimental=true/);
  await expect(page).toHaveURL(/host=/);
  await request.post(`/admin/reject/${pending.app.packageId}`, { data: 'Not ready' });
  await page.reload();
  await expect(page.locator('[data-install]')).toHaveCount(0);
  await expect(page.getByText('No matching apps.')).toBeVisible();
  await page.goto(`/app/${pending.app.appId}/?experimental=true`);
  await expect(page.getByRole('heading', { name: 'No pending update' })).toBeVisible();
  await expect(page.locator('[data-install]')).toHaveCount(0);
  expect((await request.get(`/packages/${pending.app.packageId}`)).status()).toBe(404);
});
