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
  await expect(page.locator('#queue').getByRole('table')).toBeVisible();
  await expect(page.locator('#review-queue .card')).toHaveCount(2);
  await page.locator('#queue').getByRole('link', { name: 'Rust test app', exact: true }).click();
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
  await page.getByRole('link', { name: 'Sandstorm App Market' }).click();
  await expect(page.locator('#queue').getByRole('table')).toBeVisible();
  await page.goBack();
  await expect(page.getByRole('heading', { name: 'Version history' })).toBeVisible();
});
test('reviewers download embargoed packages but cannot approve or expose them publicly', async ({ page, request }) => {
  const rows = await queue(request);
  const privateRelease = rows.find(row => row.app.versionNumber === 4);
  await page.goto(`/reviewer/${privateRelease.app.appId}/`);
  await expect(page.getByText('Read-only reviewer access.', { exact: false })).toBeVisible();
  await expect(page.locator('.app-icon')).toBeVisible();
  expect(await (await request.get(`/reviewer/icon/${privateRelease.app.packageId}`)).json()).toMatch(/^data:image\/svg\+xml;base64,/);
  expect((await request.get(`/submitter/icon/${privateRelease.app.packageId}`)).ok()).toBeFalsy();
  expect((await request.get(`/public/icon/${privateRelease.app.packageId}`)).status()).toBe(404);
  await expect(page.getByRole('button', { name: 'Approve', exact: true })).toHaveCount(0);
  const before = await (await request.get('/public/apps/index.json')).json();
  await release(page, 4).getByRole('link', { name: 'Preview listing' }).click();
  await expect(page.getByRole('status')).toContainText('Private listing preview · Build 4');
  await expect(page.getByRole('heading', { name: 'Description', exact: true })).toBeVisible();
  await expect(page.locator('.description-body')).toContainText('A signed integration-test package.');
  await expect(page.locator('.description-body li strong')).toHaveText('Features');
  await expect(page.locator('.description-body code')).toHaveText('code');
  await expect(page.locator('.description-body a')).toHaveAttribute('href', 'https://example.org');
  await expect(page.locator('.description-body')).toContainText('<script>');
  await expect(page.locator('.description-body script, .description-body img')).toHaveCount(0);
  expect(await page.evaluate(() => window.descriptionInjected)).toBeUndefined();
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
  await page.locator('#queue').getByRole('link', { name: 'Rust test app', exact: true }).click();
  await page.getByRole('link', { name: 'Sandstorm App Market' }).click();
  await expect(page.locator('#catalog-heading h1')).toBeFocused();
  expect((await request.post('/__test/submit', { data: '5' })).ok()).toBeTruthy();
  await page.clock.fastForward(30001);
  await expect(page.locator('#review-queue .card')).toHaveCount(3);
});
test('refresh does not interrupt a rejection draft', async ({ page, request }) => {
  await page.clock.install();
  await page.goto('/admin/');
  await page.locator('#queue').getByRole('link', { name: 'Rust test app', exact: true }).click();
  await release(page, 3).getByRole('button', { name: 'Reject / request update', exact: true }).click();
  await page.getByLabel('Reason for the author').fill('Draft review');
  await request.post('/__test/submit', { data: '5' });
  await page.clock.fastForward(30001);
  await expect(page.getByLabel('Reason for the author')).toHaveValue('Draft review');
  await expect(page.locator('#decision')).toBeVisible();
});
test('refresh failures show readable messages instead of gateway HTML', async ({ page }) => {
  await page.clock.install();
  await page.goto('/admin/');
  await expect(page.locator('#queue')).toContainText('Rust test app');
  const html = '<!DOCTYPE html><title>Error 502</title><p>It appears Sandstorm is not running</p>';
  for (const [status, expected] of [
    [502, 'The marketplace is temporarily unavailable (HTTP 502). Please try again shortly.'],
    [200, 'The marketplace returned an unexpected response. Please try again shortly.'],
  ]) {
    await page.route('**/admin/admin', route => route.fulfill({ status, contentType: 'text/html', body: html }));
    await page.clock.fastForward(30001);
    await expect(page.locator('#error')).toHaveText(`Could not refresh: ${expected}`);
    await expect(page.locator('#queue')).toContainText('Rust test app');
    await page.unroute('**/admin/admin');
  }
  await page.route('**/admin/admin', route => route.abort());
  await page.clock.fastForward(30001);
  await expect(page.locator('#error')).toHaveText('Could not refresh: Could not connect to the marketplace. Please try again shortly.');
});
test('refresh failure after a saved decision is visible on the page', async ({ page, request }) => {
  await page.goto('/admin/');
  await page.locator('#queue').getByRole('link', { name: 'Rust test app', exact: true }).click();
  await request.post('/__test/fail-next-queue');
  await decide(page, release(page, 3), 'Approve');
  await expect(page.locator('#error')).toContainText('The decision was saved, but the view could not refresh');
  await expect(page.locator('#message')).not.toBeVisible();
  expect(await publicVersion(request)).toBe(3);
});
test('malformed stored metadata can be rejected through the admin UI', async ({ page, request }) => {
  const badId = await (await request.post('/__test/invalid-metadata')).text();
  const row = (await queue(request)).find(row => row.app.packageId === badId);
  expect(row.validationError).toContain('utf-8');
  await page.goto(`/admin/${row.app.appId}/`);
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
  await expect(page.locator('#queue').getByRole('table')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
  await page.locator('#review-queue h3').getByRole('link').first().click();
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
  await expect(page.locator('#queue').getByRole('table')).toBeVisible();
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

test('deprecation hides discovery but preserves listings and downloads and can be undone', async ({ page, request }) => {
  const rows = await queue(request);
  const current = rows.find(row => row.current);
  const appId = current.app.appId;
  for (const role of ['reviewer', 'submitter']) {
    expect((await request.post(`/${role}/deprecation/${appId}`, { data: '{}' })).ok()).toBeFalsy();
  }
  await page.goto(`/admin/${appId}/`);
  await page.getByRole('button', { name: 'Deprecate app', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Restore app to indexes' })).toBeVisible();
  expect(await publicVersion(request)).toBeUndefined();
  expect((await request.get(`/public/packages/${current.app.packageId}`)).ok()).toBeTruthy();
  expect((await (await request.get('/public/experimental/index.json')).json()).apps).toHaveLength(0);
  await page.goto(`/public/app/${appId}/`);
  await expect(page.locator('.deprecation-notice')).toContainText('This app is deprecated.');
  await expect(page.getByRole('link', { name: 'Download SPK', exact: true })).toBeVisible();
  await page.goto(`/admin/${appId}/`);
  await page.getByRole('button', { name: 'Restore app to indexes' }).click();
  await expect(page.getByRole('button', { name: 'Deprecate app', exact: true })).toBeVisible();
  expect(await publicVersion(request)).toBe(current.app.versionNumber);
});

test('private activity history records approvers, publication, and paginates safely', async ({ page, request }) => {
  const rows = await queue(request);
  const item = rows.find(row => row.app.versionNumber === 4);
  // Build enough history to cross a storage chunk boundary without exposing the embargoed release.
  for (let n = 0; n < 52; n++) {
    expect((await request.post(`/admin/approve/${item.app.packageId}`, { data: '' })).ok()).toBeTruthy();
    expect((await request.post(`/admin/unapprove/${item.app.packageId}`, { data: '' })).ok()).toBeTruthy();
  }
  await page.goto(`/reviewer/${item.app.appId}/`);
  const history = page.locator('.audit-history');
  await expect(history.getByRole('table', { name: 'Activity history' })).toBeVisible();
  await expect(history.getByRole('columnheader')).toHaveText(['Date', 'Event', 'Actor', 'Build', 'Details']);
  await expect(history.locator('tbody tr')).toHaveCount(50);
  await expect(history.locator('tbody tr').first()).toContainText('Test approver');
  await expect(history.locator('tbody tr').first()).toContainText('Approval revoked');
  await history.getByRole('button', { name: 'Load older events' }).click();
  await expect(history.locator('tbody tr')).toHaveCount(100);
  await history.getByRole('button', { name: 'Load older events' }).click();
  await expect(history.getByRole('button', { name: 'Load older events' })).not.toBeVisible();
  await expect(history).toContainText('Package submitted');
  await expect(history).toContainText('Public visibility changed');
  expect((await request.get(`/submitter/audit/${item.app.appId}`)).ok()).toBeFalsy();
  expect((await request.get(`/public/marketplace.sqlite`)).status()).toBe(404);
  // Text is rendered as text, including rejection reasons that resemble HTML.
  await request.post(`/admin/reject/${item.app.packageId}`, { data: '<img src=x onerror=alert(1)> fix this' });
  await history.getByRole('button', { name: 'Refresh', exact: true }).click();
  await expect(history.locator('tbody tr')).toHaveCount(50);
  const detailsButton = history.locator('tbody tr').first().getByRole('cell').first().getByRole('button');
  await detailsButton.click();
  const dialog = page.getByRole('dialog', { name: /Event #\d+ details/ });
  await expect(dialog).toBeVisible();
  await expect(dialog.locator('pre')).toContainText('<img src=x onerror=alert(1)>');
  await expect(dialog.locator('img')).toHaveCount(0);
  expect(JSON.parse(await dialog.locator('pre').textContent()).action).toBe('rejected');
  const bounds = await dialog.boundingBox();
  expect(bounds.width).toBeGreaterThan(page.viewportSize().width * 0.85);
  expect(bounds.width).toBeLessThan(page.viewportSize().width * 0.92);
  expect(bounds.height).toBeGreaterThan(page.viewportSize().height * 0.9);
  await page.keyboard.press('Escape');
  await expect(dialog).not.toBeVisible();
  await expect(detailsButton).toBeFocused();
  await detailsButton.click();
  await dialog.getByRole('button', { name: 'Close', exact: true }).click();
  await expect(dialog).not.toBeVisible();
});

test('app paths survive direct loads, reloads, and browser history', async ({ page, request }) => {
  const { app } = (await queue(request))[0];
  await page.goto('/admin/');
  await page.locator('#queue').getByRole('link', { name: 'Rust test app', exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/admin/${app.appId}/$`));
  await page.reload();
  await expect(page.locator('#app-detail h1')).toHaveText(app.name);
  await expect(page.locator('#error')).toBeHidden();
  await expect(page.locator('.app-icon')).toBeVisible();
  const publicPage = page.getByRole('link', { name: 'Public Page', exact: true });
  await expect(publicPage).toHaveAttribute('href', `http://127.0.0.1:9940/public/app/${app.appId}/`);
  await expect(publicPage).toHaveAttribute('target', '_blank');
  expect((await request.get(await publicPage.getAttribute('href'))).ok()).toBeTruthy();
  const preview = page.getByRole('link', { name: 'Preview listing' }).first();
  expect(await preview.evaluate(link => new URL(link.href).pathname)).toMatch(/^\/admin\/preview\//);
  await page.getByRole('link', { name: 'Sandstorm App Market' }).click();
  await expect(page).toHaveURL(/\/admin\/$/);
  await page.goBack();
  await expect(page.locator('#app-detail h1')).toHaveText(app.name);
  await page.goForward();
  await expect(page.locator('#catalog-heading')).toBeVisible();
  await page.goto(`/admin/${app.appId}/`);
  await expect(page.locator('#app-detail h1')).toHaveText(app.name);
  await page.goto(`/admin/${'0'.repeat(52)}/`);
  await expect(page.locator('#app-detail h1')).toHaveText('App not found');
  expect((await request.get(`/submitter/${app.appId}/`)).ok()).toBeFalsy();
  expect((await request.get(`/admin/${app.appId}/extra`)).ok()).toBeFalsy();
});

test('Sandstorm shell URL follows app navigation and restores the iframe on reload', async ({ page, request }) => {
  const { app } = (await queue(request))[0];
  // Model the documented shell contract: forward the outer path on load and
  // accept setPath messages from the grain iframe to update the address bar.
  await page.route('**/shell/**', route => route.fulfill({
    contentType: 'text/html',
    body: `<!doctype html><iframe id="grain" style="width:100%;height:900px"></iframe><script>
      const grain = document.getElementById('grain');
      grain.src = '/reviewer' + location.pathname.slice('/shell'.length);
      window.addEventListener('message', event => {
        if (event.source === grain.contentWindow && typeof event.data?.setPath === 'string') {
          history.replaceState(null, '', '/shell' + event.data.setPath);
        }
      });
    </script>`,
  }));
  await page.goto('/shell/');
  const grain = page.frameLocator('#grain');
  await grain.locator('#queue').getByRole('link', { name: app.name, exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/shell/${app.appId}/$`));
  await page.reload();
  await expect(grain.locator('#app-detail h1')).toHaveText(app.name);
  await expect(grain.locator('#error')).toBeHidden();
  await grain.getByRole('link', { name: 'Sandstorm App Market' }).click();
  await expect(page).toHaveURL(/\/shell\/$/);
  await page.goBack();
  await expect(page).toHaveURL(new RegExp(`/shell/${app.appId}/$`));
  await expect(grain.locator('#app-detail h1')).toHaveText(app.name);
  await page.goForward();
  await expect(page).toHaveURL(/\/shell\/$/);
  await page.reload();
  await expect(grain.locator('#catalog-heading')).toBeVisible();
});


test('Public Page is hidden when only experimental releases remain', async ({ page, request }) => {
  const rows = await queue(request);
  const appId = rows[0].app.appId;
  await page.goto(`/admin/${appId}/`);
  await expect(page.getByRole('link', { name: 'Public Page', exact: true })).toBeVisible();
  await expect(page.locator('.deprecation-controls')).toContainText('Not deprecated.');
  for (const row of rows.filter(row => row.review === 'approved')) {
    expect((await request.post(`/admin/unapprove/${row.app.packageId}`, { data: '' })).ok()).toBeTruthy();
  }
  // The experimental version still produces a public HTML page, but no approved version.
  expect((await request.get(`/public/app/${appId}/`)).ok()).toBeTruthy();
  await page.reload();
  await expect(page.locator('.app-info')).toContainText('No approved public version.');
  await expect(page.getByRole('link', { name: 'Public Page', exact: true })).toHaveCount(0);
});

test('submission notification recipient defaults to unset and is approver-only', async ({ page, request }) => {
  await page.goto('/admin/notifications');
  const settings = page.getByRole('region', { name: 'Submission notifications' });
  await expect(settings).toBeVisible();
  await expect(page.getByLabel('Notification email')).toHaveValue('');
  await expect(page.locator('#notification-status')).toHaveText('Email notifications are disabled.');
  await page.getByLabel('Notification email').fill('review-list@example.org');
  await settings.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.locator('#message')).toHaveText('Notification settings saved.');
  await page.reload();
  await expect(page.getByLabel('Notification email')).toHaveValue('review-list@example.org');
  expect((await (await request.get('/reviewer/admin')).json()).notifications).toBeUndefined();
  expect((await request.post('/reviewer/notifications', { data: 'other@example.org' })).ok()).toBeFalsy();
  expect((await request.post('/reviewer/notifications/retry', { data: '' })).ok()).toBeFalsy();
  await page.getByLabel('Notification email').fill('');
  await settings.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.locator('#notification-status')).toHaveText('Email notifications are disabled.');
  await page.goto('/reviewer/');
  await expect(page.locator('#notifications')).toBeHidden();
});

test('listing changelogs show the selected build and expand without crowding reviews', async ({ page, request }) => {
  const rows = await queue(request);
  const appId = rows[0].app.appId;
  const preview = version => `/admin/preview/${rows.find(row => row.app.versionNumber === version).app.packageId}`;
  await page.goto(`/app/${appId}`);
  await expect(page.getByRole('heading', { name: 'Changelog', exact: true })).toBeVisible();
  const headingStyles = await page.locator('.description > h2, .changelog > h2').evaluateAll(headings =>
    headings.map(heading => {
      const style = getComputedStyle(heading);
      return [style.fontSize, style.fontWeight, style.lineHeight, style.marginTop, style.marginBottom];
    }));
  expect(headingStyles).toHaveLength(2);
  expect(headingStyles[1]).toEqual(headingStyles[0]);
  await expect(page.locator('.changelog-body')).toContainText('Improved sync for build 2.');
  await expect(page.locator('.changelog-body strong')).toHaveText('Improved');
  await expect(page.locator('.changelog-full')).toHaveCount(0);
  const details = await (await request.get(`/public/apps/${appId}.json`)).json();
  expect(details.changeLog).toContain('## Build 2');
  expect(JSON.stringify(details)).not.toContain('## Build 4');

  for (const [url, build] of [[`/app/${appId}?experimental=true`, 3], [preview(4), 4]]) {
    await page.goto(url);
    await expect(page.locator('.changelog-excerpt')).toContainText(`Improved sync for build ${build}.`);
    await expect(page.locator('.changelog-full .changelog-body')).toBeHidden();
    await page.getByText('Show full changelog', { exact: true }).click();
    await expect(page.locator('.changelog-excerpt')).toBeHidden();
    await expect(page.locator('.changelog-full .changelog-body')).toContainText('Historical change 15.');
    expect(await page.evaluate(() => window.changelogInjected)).toBeUndefined();
    const safe = await page.locator('.changelog-full .changelog-body').evaluate(body =>
      [...body.querySelectorAll('*')].every(node =>
        ['P', 'H3', 'H4', 'H5', 'H6', 'UL', 'OL', 'LI', 'EM', 'STRONG', 'BLOCKQUOTE', 'PRE', 'CODE', 'A', 'BR', 'HR'].includes(node.tagName) &&
        [...node.attributes].every(attr => node.tagName === 'A' && ['href', 'rel'].includes(attr.name)) &&
        (node.tagName !== 'A' || ['http:', 'https:'].includes(new URL(node.href).protocol))));
    expect(safe).toBeTruthy();
    await expect(page.locator('.changelog img, .changelog script, .changelog svg')).toHaveCount(0);
    await page.getByText('Show less', { exact: true }).click();
    await expect(page.locator('.changelog-excerpt')).toBeVisible();
    await expect(page.locator('.changelog-full .changelog-body')).toBeHidden();
  }
  await page.goto(preview(1));
  await expect(page.locator('.changelog')).toHaveCount(0);
});

test('approvers manage signing rules and reviewers cannot change them', async ({ page, request }) => {
  await page.goto('/admin/replacements');
  await expect(page.locator('#signing-keys')).toBeVisible();
  const initial = await (await request.get('/admin/admin')).json();
  expect(initial.signingKeys.rules.length).toBeGreaterThan(0);
  await expect(page.locator('#signing-key-rows tr')).toHaveCount(initial.signingKeys.rules.length);
  for (const name of ['Paperwork', 'draw.io', 'Wekan', 'WordPress', 'MediaWiki']) {
    await expect(page.locator('#signing-key-rows').getByRole('cell', { name, exact: true })).toBeVisible();
  }
  await expect(page.locator('#signing-keys')).toContainText('Sandstorm servers and packaging tools');
  const original = initial.releases[0].app.appId, replacement = '2'.repeat(51) + '0';
  await expect(page.locator('#signing-key-form')).toBeHidden();
  await page.getByRole('button', { name: 'New Replacement', exact: true }).click();
  await expect(page.getByRole('dialog', { name: 'New Replacement', exact: true })).toBeVisible();
  await expect(page.locator('#signing-key-original')).toBeEmpty();
  await expect(page.locator('#signing-key-replacement')).toBeEmpty();
  await page.locator('#signing-key-original').fill(original);
  await page.locator('#signing-key-replacement').fill(replacement);
  await page.getByRole('button', { name: 'Save signing-key rule' }).click();
  await expect(page.locator('#message')).toContainText('Signing-key rule saved');
  await expect(page.locator('#signing-key-dialog')).toBeHidden();
  const row = page.locator('#signing-key-rows tr').filter({ has: page.getByText(original, { exact: true }) });
  await expect(row).toContainText('Retained');
  await expect(row.getByRole('button', { name: 'Rust test app', exact: true })).toBeVisible();
  await row.getByRole('button', { name: 'Edit', exact: true }).click();
  await expect(page.getByRole('dialog', { name: 'Edit Replacement', exact: true })).toBeVisible();
  await expect(page.locator('#signing-key-original')).toHaveValue(original);
  await expect(page.locator('#signing-key-original')).not.toBeEditable();
  await expect(page.locator('#signing-key-replacement')).toHaveValue(replacement);
  await expect(page.locator('#signing-key-revoke')).not.toBeChecked();
  await page.locator('#signing-key-revoke').check();
  await expect(page.locator('#signing-key-allowed')).toBeEnabled();
  await page.getByRole('button', { name: 'Save signing-key rule' }).click();
  await expect(row).toContainText('0 allowed');
  await expect(page.locator('#signing-key-dialog')).toBeHidden();
  const updated = await (await request.get('/admin/admin')).json();
  expect(updated.signingKeys.rules.find(rule => rule.original === original)).toEqual({ original, replacement, allowedPackages: [] });
  await expect(row.getByRole('cell').last().getByRole('button')).toHaveText(['Edit']);
  await row.getByRole('button', { name: 'Rust test app', exact: true }).click();
  await expect(page.locator('#audit-details')).toBeVisible();
  await expect(page.locator('#audit-details-json')).toContainText('signingKeyRuleChanged');
  await page.goto('/reviewer/');
  await expect(page.locator('#signing-keys')).toBeHidden();
  expect((await request.post('/reviewer/signing-keys', { data: { revision: 0, rule: { original, replacement, allowedPackages: null } } })).ok()).toBeFalsy();
});

test('signing rule errors keep the draft and are not masked by refresh failures', async ({ page }) => {
  await page.clock.install();
  await page.goto('/admin/replacements');
  await expect(page.locator('#signing-keys')).toBeVisible();
  await page.getByRole('button', { name: 'New Replacement', exact: true }).click();
  await page.locator('#signing-key-original').fill('0'.repeat(52));
  await page.route('**/admin/signing-keys', route => route.fulfill({
    status: 400, contentType: 'text/plain', body: 'signing-key rules changed; refresh and try again',
  }));
  await page.route('**/admin/admin', route => route.abort());
  await page.getByRole('button', { name: 'Save signing-key rule' }).click();
  await expect(page.locator('#signing-key-error')).toHaveText('signing-key rules changed; refresh and try again');
  await expect(page.getByRole('dialog', { name: 'New Replacement', exact: true })).toBeVisible();
  await expect(page.locator('#signing-key-original')).toHaveValue('0'.repeat(52));
  await expect(page.locator('#signing-key-status')).toBeEmpty();
  await expect(page.getByRole('button', { name: 'Save signing-key rule' })).toBeEnabled();
});

test('replacement modal cancels without saving and new forms discard previous drafts', async ({ page, request }) => {
  await page.clock.install();
  await page.goto('/admin/replacements');
  await expect(page.locator('#signing-keys')).toBeVisible();
  const before = (await (await request.get('/admin/admin')).json()).signingKeys;
  const rule = before.rules.find(rule => rule.allowedPackages?.length);
  const row = page.locator('#signing-key-rows tr').filter({ has: page.getByText(rule.original, { exact: true }) });
  await row.getByRole('button', { name: 'Edit', exact: true }).click();
  await expect(page.locator('#signing-key-revoke')).toBeChecked();
  await expect(page.locator('#signing-key-allowed')).toHaveValue(rule.allowedPackages.join('\n'));
  await page.locator('#signing-key-allowed').fill('a'.repeat(32));
  await page.clock.fastForward(30001);
  await expect(page.locator('#signing-key-allowed')).toHaveValue('a'.repeat(32));
  await page.getByRole('button', { name: 'Cancel', exact: true }).click();
  await expect(page.locator('#signing-key-dialog')).toBeHidden();
  await expect(row.getByRole('button', { name: 'Edit', exact: true })).toBeFocused();
  await page.getByRole('button', { name: 'New Replacement', exact: true }).click();
  await expect(page.locator('#signing-key-original')).toBeEmpty();
  await expect(page.locator('#signing-key-original')).toBeEditable();
  await expect(page.locator('#signing-key-replacement')).toBeEmpty();
  await expect(page.locator('#signing-key-replacement')).toBeEditable();
  await expect(page.locator('#signing-key-revoke')).not.toBeChecked();
  await expect(page.locator('#signing-key-allowed')).toBeEmpty();
  await expect(page.locator('#signing-key-allowed')).toBeDisabled();
  await page.locator('#signing-key-original').fill('0'.repeat(52));
  await page.keyboard.press('Escape');
  await expect(page.locator('#signing-key-dialog')).toBeHidden();
  await expect(page.getByRole('button', { name: 'New Replacement', exact: true })).toBeFocused();
  await row.getByRole('button', { name: 'Edit', exact: true }).click();
  await expect(page.locator('#signing-key-allowed')).toHaveValue(rule.allowedPackages.join('\n'));
  expect((await (await request.get('/admin/admin')).json()).signingKeys).toEqual(before);
});

test('replacement modal prevents duplicate saves and dismissal during revalidation', async ({ page }) => {
  await page.goto('/admin/replacements');
  await page.getByRole('button', { name: 'New Replacement', exact: true }).click();
  await page.locator('#signing-key-original').fill('0'.repeat(52));
  let finish;
  const waiting = new Promise(resolve => { finish = resolve; });
  await page.route('**/admin/signing-keys', async route => {
    await waiting;
    await route.fulfill({ status: 400, contentType: 'text/plain', body: 'Please retry verification' });
  });
  await page.getByRole('button', { name: 'Save signing-key rule' }).click();
  try {
    await expect(page.locator('#signing-key-status')).toContainText('Revalidating');
    await expect(page.getByRole('button', { name: 'Save signing-key rule' })).toBeDisabled();
    await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeDisabled();
    await expect(page.locator('#signing-key-original')).toBeDisabled();
    await page.keyboard.press('Escape');
    await expect(page.locator('#signing-key-dialog')).toBeVisible();
  } finally {
    finish();
  }
  await expect(page.locator('#signing-key-error')).toHaveText('Please retry verification');
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeEnabled();
});

test('new replacements cannot overwrite an existing revocation', async ({ page, request }) => {
  await page.goto('/admin/replacements');
  const before = (await (await request.get('/admin/admin')).json()).signingKeys;
  const rule = before.rules.find(rule => rule.allowedPackages?.length);
  await page.getByRole('button', { name: 'New Replacement', exact: true }).click();
  await page.locator('#signing-key-original').fill(rule.original);
  await page.locator('#signing-key-replacement').fill(rule.replacement);
  await page.getByRole('button', { name: 'Save signing-key rule' }).click();
  await expect(page.locator('#signing-key-error')).toContainText('already exists');
  await expect(page.locator('#signing-key-dialog')).toBeVisible();
  expect((await (await request.get('/admin/admin')).json()).signingKeys).toEqual(before);
});

test('a committed rule can be retried immediately after a failed save response', async ({ page, request }) => {
  await page.goto('/admin/replacements');
  await expect(page.locator('#signing-keys')).toBeVisible();
  const before = (await (await request.get('/admin/admin')).json()).signingKeys;
  const attempts = [];
  await page.route('**/admin/signing-keys', async route => {
    attempts.push(route.request().postDataJSON());
    if (attempts.length === 1) {
      // Persist the real rule, then simulate a failure after commit. Backend
      // integration tests separately exercise an actual publication failure.
      const response = await route.fetch();
      expect(response.status()).toBe(204);
      await route.fulfill({ status: 400, contentType: 'text/plain', body: 'Signing-key rule saved, but public site rebuild failed' });
    } else {
      await route.continue();
    }
  });
  await page.getByRole('button', { name: 'New Replacement', exact: true }).click();
  await page.locator('#signing-key-original').fill('0'.repeat(52));
  await page.locator('#signing-key-replacement').fill('2'.repeat(51) + '0');
  await page.getByRole('button', { name: 'Save signing-key rule' }).click();
  await expect(page.locator('#signing-key-error')).toContainText('rule saved');
  await expect(page.locator('#signing-key-original')).toHaveValue('0'.repeat(52));
  await page.getByRole('button', { name: 'Save signing-key rule' }).click();
  await expect(page.locator('#signing-key-dialog')).toBeHidden();
  expect(attempts.map(({ mode, revision }) => ({ mode, revision }))).toEqual([
    { mode: 'create', revision: before.revision },
    { mode: 'edit', revision: before.revision + 1 },
  ]);
  expect((await (await request.get('/admin/admin')).json()).signingKeys.revision).toBe(before.revision + 1);
});

test('header tabs route independently and preserve navigation on reload', async ({ page, request }) => {
  await page.goto('/admin/');
  const tabs = page.getByRole('navigation', { name: 'Administration' });
  const apps = tabs.locator('[data-tab="apps"]');
  await expect(apps).toHaveAttribute('aria-current', 'page');
  await expect(page.locator('#pending-count')).toHaveText('1');
  await expect(page.locator('#notifications')).toBeHidden();
  await expect(page.locator('#signing-keys')).toBeHidden();
  await tabs.getByRole('link', { name: 'Notifications', exact: true }).click();
  await expect(page).toHaveURL(/\/admin\/notifications$/);
  await expect(page.locator('#notifications')).toBeVisible();
  await expect(page.locator('#catalog-view')).toBeHidden();
  await expect(apps).not.toHaveAttribute('aria-current', 'page');
  await page.reload();
  await expect(tabs.getByRole('link', { name: 'Notifications', exact: true })).toHaveAttribute('aria-current', 'page');
  await expect(page.locator('#notifications')).toBeVisible();
  await tabs.getByRole('link', { name: 'Key replacements', exact: true }).click();
  await expect(page.locator('#signing-keys')).toBeVisible();
  await expect(page.locator('#notifications')).toBeHidden();
  await page.reload();
  await expect(page.locator('#signing-keys')).toBeVisible();
  await page.goBack();
  await expect(page.locator('#notifications')).toBeVisible();
  await apps.click();
  await expect(page).toHaveURL(/\/admin\/apps\/$/);
  await page.reload();
  await expect(page.locator('#queue table')).toBeVisible();
  const { app } = (await queue(request))[0];
  await page.goto(`/admin/${app.appId}/`);
  await expect(apps).toHaveAttribute('aria-current', 'page');
  await expect(tabs.getByRole('link', { name: 'Public site' })).toHaveAttribute('href', 'http://127.0.0.1:9940/public/');
  for (const route of ['notifications', 'replacements']) {
    await page.goto(`/reviewer/${route}`);
    await expect(page.locator('#access-note')).toBeVisible();
    await expect(tabs.locator(`[data-tab="${route}"]`)).toBeHidden();
    await expect(page.locator('#notifications')).toBeHidden();
    await expect(page.locator('#signing-keys')).toBeHidden();
  }
});
