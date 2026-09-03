const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const { spawnSync } = require('node:child_process');
const test = require('node:test');

const extractor = require('../extension/x-extractor.js');

test('recognizes canonical and legacy X status URLs', () => {
  assert.deepEqual(
    extractor.parseXStatusUrl('https://x.com/lennysan/status/2094821687590375802?s=20'),
    {
      username: 'lennysan',
      statusId: '2094821687590375802',
      canonicalUrl: 'https://x.com/lennysan/status/2094821687590375802',
    }
  );
  assert.equal(
    extractor.parseXStatusUrl('https://twitter.com/lennysan/status/2094821687590375802').statusId,
    '2094821687590375802'
  );
  assert.equal(extractor.parseXStatusUrl('https://example.com/lennysan/status/1'), null);
});

test('renders bounded, escaped post HTML with media and a source link', () => {
  const result = extractor.renderXPost({
    displayName: '<Lenny>',
    handle: '@lennysan',
    title: 'Lenny on X: Design',
    postText: 'Design post',
    postHtml: 'Hello 🤯 <a href="https://x.com/anshuc">@anshuc</a>',
    timestampLabel: 'Sep 2',
    timestampIso: '2026-09-02T00:16:00.000Z',
    media: {
      images: [{ src: 'https://pbs.twimg.com/media/example.jpg', alt: 'Example' }],
      videoPoster: null,
      hasVideo: true,
    },
    externalLinks: [{ href: 'https://example.com/article', label: 'Article' }],
    sourceUrl: 'https://x.com/lennysan/status/2094821687590375802',
  });

  assert.equal(result.contentKind, 'x-post');
  assert.equal(result.author, '@lennysan');
  assert.match(result.content, /&lt;Lenny&gt;/);
  assert.match(result.content, /Hello 🤯/);
  assert.match(result.content, /Watch it on X/);
  assert.match(result.content, /https:\/\/example\.com\/article/);
  assert.doesNotMatch(result.content, /<Lenny>/);
});

test('extracts only the target X post in a real browser DOM', (t) => {
  const chromeCandidates = [
    process.env.CHROME_BIN,
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/google-chrome',
    '/usr/bin/chromium',
  ].filter(Boolean);
  const chrome = chromeCandidates.find((candidate) => fs.existsSync(candidate));
  if (!chrome) {
    t.skip('Chrome is not installed');
    return;
  }

  const fixture = path.join(__dirname, 'fixtures', 'x-status.html');
  const run = spawnSync(chrome, [
    '--headless=new',
    '--disable-gpu',
    '--disable-extensions',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-background-networking',
    '--blink-settings=imagesEnabled=false',
    '--dump-dom',
    pathToFileURL(fixture).href,
  ], { encoding: 'utf8', timeout: 30000 });

  assert.equal(run.status, 0, run.stderr);
  const titleMatch = run.stdout.match(/<title>([^<]+)<\/title>/i);
  assert.ok(titleMatch, 'fixture did not return an extraction result');
  const result = JSON.parse(decodeURIComponent(titleMatch[1]));

  assert.equal(result.contentKind, 'x-post');
  assert.equal(result.author, '@lennysan');
  assert.match(result.title, /^Lenny Rachitsky on X:/);
  assert.match(result.content, /@anshuc/);
  assert.match(result.content, /🤯/);
  assert.match(result.content, /name=large/);
  assert.match(result.content, /Video attached/);
  assert.doesNotMatch(result.content, /abs\.twimg\.com\/emoji/);
  assert.doesNotMatch(result.content, /This reply must not be included/);
});
