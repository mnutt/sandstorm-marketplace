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
