const { test } = require('node:test');
const assert = require('node:assert/strict');
const { serverOrigin, installUrl, matchesApp, categoryFromPath } = require('../crates/marketplace-web/static/marketplace.js');
test('category paths support direct links, trailing slashes, and encoded names', () => {
  assert.equal(categoryFromPath('/genre/Games'), 'Games');
  assert.equal(categoryFromPath('/genre/Games/'), 'Games');
  assert.equal(categoryFromPath('/genre/Office%20Tools'), 'Office Tools');
  for (const path of ['/', '/app/Games/', '/genre/Games/other', '/genre/%invalid']) {
    assert.equal(categoryFromPath(path), '');
  }
});
test('install links target the chosen Sandstorm server with the public SPK URL', () => {
  assert.equal(installUrl('https://cloud.example/path', 'abc', 'https://market.example/packages/abc'),
    'https://cloud.example/install/abc?url=https%3A%2F%2Fmarket.example%2Fpackages%2Fabc');
  for (const value of ['javascript:alert(1)', '//evil.example', 'https://user:pass@example.com', null]) {
    assert.equal(serverOrigin(value), null);
  }
});
test('static search combines category and case-insensitive text', () => {
  assert.equal(matchesApp('Etherpad collaborative editor', ['Productivity'], 'ETHER', 'Productivity'), true);
  assert.equal(matchesApp('Etherpad collaborative editor', ['Productivity'], 'wekan', ''), false);
  assert.equal(matchesApp('Etherpad collaborative editor', ['Productivity'], '', 'Games'), false);
});

const admin = require('../crates/marketplace-grain/src/review.js');
function release(version, overrides = {}) {
  return { app: {appId: 'app-a', packageId: `package-${version}`, name: 'Test app', versionNumber: version}, review: 'pending', requestState: 'publish', contactEmail: 'author@example.com', ...overrides };
}
test('review inbox excludes withdrawn submissions and orders by receipt', () => {
  const old = release(1, {receivedAt: '2026-01-01T00:00:00Z'});
  const newest = release(2, {receivedAt: '2026-02-01T00:00:00Z', requestState: 'review'});
  const withdrawn = release(3, {requestState: 'ignore'});
  const rejected = release(4, {review: 'needsUpdate'});
  assert.deepEqual(admin.reviewItems([old, withdrawn, rejected, newest], 'pending', ''), [newest, old]);
  assert.deepEqual(admin.reviewItems([old, rejected], 'needsUpdate', 'AUTHOR@EXAMPLE'), [rejected]);
  assert.equal(admin.reviewItems([old, withdrawn], 'all', '').length, 2);
  assert.equal(admin.reviewItems([old], 'all', 'missing').length, 0);
});
test('app history groups by identity, orders numeric versions, and restricts rollback', () => {
  const older = release(2, {review: 'approved'});
  const current = release(10, {review: 'approved', current: true});
  const pending = release(11);
  const other = release(1, {app: {appId: 'app-b', name: 'Another app', versionNumber: 1}});
  const groups = admin.groupApps([older, other, pending, current]);
  assert.equal(groups.length, 2);
  assert.deepEqual(groups[1], [pending, current, older]);
  assert.equal(admin.canRollback(older, groups[1]), true);
  assert.equal(admin.canRollback(current, groups[1]), false);
  assert.equal(admin.canRollback(pending, groups[1]), false);
  assert.equal(admin.canRollback({...older, requestState: 'ignore'}, groups[1]), false);
  assert.equal(admin.canRollback(older, [older]), false);
});


test('app table sorts by latest publication across history, with unpublished apps last', () => {
  const app = (id, version, publishedAt, current = false) => release(version, {
    app: {appId: id, name: id, versionNumber: version}, publishedAt, current
  });
  const groups = admin.publishedApps([
    app('A', 1, 100, true), app('A', 2, 300), // A rollback does not erase publication history.
    app('B', 1, 200, true), app('C', 9, 0), app('D', 1, 0)
  ]);
  assert.deepEqual(groups.map(group => group[0].app.appId), ['A', 'B', 'C', 'D']);
  assert.equal(admin.lastPublished(groups[0]), 300);
  assert.equal(admin.lastPublished(groups[2]), 0);
});
