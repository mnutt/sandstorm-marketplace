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
test('request errors hide HTML pages and retain concise application errors', () => {
  const html = '<!DOCTYPE html><title>Error 502</title><p>It appears Sandstorm is not running</p>';
  assert.equal(admin.requestErrorMessage(502, html), 'The marketplace is temporarily unavailable (HTTP 502). Please try again shortly.');
  assert.equal(admin.requestErrorMessage(400, 'Contact email required'), 'Contact email required');
  assert.equal(admin.requestErrorMessage(403, '<html>Access denied</html>'), 'You do not have permission to perform this action.');
  for (const body of [html, '<html>Proxy error</html>', 'x'.repeat(501), '']) {
    assert.equal(admin.requestErrorMessage(400, body), 'The request could not be completed (HTTP 400). Please try again.');
  }
});
function release(version, overrides = {}) {
  return { app: {appId: 'app-a', packageId: `package-${version}`, name: 'Test app', versionNumber: version}, review: 'pending', requestState: 'publish', contactEmail: 'author@example.com', ...overrides };
}
test('review inbox excludes withdrawn submissions and orders by receipt', () => {
  const old = release(1, {receivedAt: '2026-01-01T00:00:00Z'});
  const newest = release(2, {receivedAt: '2026-02-01T00:00:00Z', requestState: 'review'});
  const withdrawn = release(3, {requestState: 'ignore'});
  const rejected = release(4, {review: 'needsUpdate'});
  assert.deepEqual(admin.pendingSubmissions([old, withdrawn, rejected, newest]), [newest, old]);
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

test('audit attribution uses signing keys for submissions and user identities for admin actions', () => {
  const signer = {kind: 'appSigningKey', identityId: '09'.repeat(32), displayName: 'Alice', signingKey: 'ab'.repeat(32)};
  assert.equal(admin.auditActorName(signer), 'Signing key');
  assert.deepEqual(admin.auditActorForDisplay(signer), {kind: 'appSigningKey', signingKey: signer.signingKey});
  const reviewer = {...signer, kind: 'sandstormIdentity', signingKey: null};
  assert.equal(admin.auditActorName(reviewer), 'Alice');
  assert.deepEqual(admin.auditActorForDisplay(reviewer, 'submitted'), {kind: 'submissionApi', displayName: 'Submission API'});
  assert.equal(admin.auditActorName({...signer, identityId: null}), 'Signing key');
  assert.equal(admin.auditActorName({...reviewer, displayName: ''}), `Sandstorm user ${signer.identityId}`);
  assert.equal(admin.auditActorName({kind: 'system', displayName: 'Marketplace system'}), 'Marketplace system');
});

test('audit table and JSON share attribution filtering without changing stored events', () => {
  const tokenUser = {kind: 'appSigningKey', identityId: 'token-owner-id', displayName: 'Token owner', signingKey: 'verified-key'};
  const stored = {action: 'publicationChanged', actor: {kind: 'system', displayName: 'Marketplace system'}, details: {latestDecision: 12, decisionActor: tokenUser}};
  const displayed = admin.auditEventForDisplay(stored);
  assert.equal(JSON.stringify(displayed).includes('Token owner'), false);
  assert.equal(JSON.stringify(displayed).includes('token-owner-id'), false);
  assert.equal(displayed.details.decisionActor.signingKey, 'verified-key');
  assert.equal(stored.details.decisionActor.displayName, 'Token owner');
  const submission = admin.auditEventForDisplay({action: 'submitted', actor: {...tokenUser, signingKey: null}, details: {}});
  assert.equal(admin.auditActorName(submission.actor), 'Submission API');
  assert.equal(JSON.stringify(submission).includes('Token owner'), false);
  const approval = {action: 'approved', actor: {kind: 'sandstormIdentity', identityId: 'reviewer-id', displayName: 'Reviewer'}, details: {}};
  assert.deepEqual(admin.auditEventForDisplay(approval), approval);
});

test('admin app routes use only the app ID', () => {
  const id = '0'.repeat(52);
  assert.equal(admin.appIdFromPath(`/${id}/`), id);
  assert.equal(admin.appIdFromPath(`/${id}`), id);
  for (const path of ['/', '/admin', `/${id}/extra`, `/${id}-name/`, '/%invalid/', '/<script>/']) {
    assert.equal(admin.appIdFromPath(path), null);
  }
});
