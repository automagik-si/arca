import assert from 'node:assert/strict';
import { mkdir, readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';

const here = fileURLToPath(new URL('.', import.meta.url));
const pageJs = await readFile(join(here, '../page.js'));
const pageCss = await readFile(join(here, '../page.css'));
const html = '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="arca-canvas/page.css"></head><body><main id="arca-root" data-arca-base="/arca-canvas" data-poll-ms="50"></main><script src="arca-canvas/page.js" defer></script></body></html>';

const MIN = 60_000;
const DAY = 24 * 60 * MIN;
const EMBEDDED = 'A1:B2:C3:D4:E5:F6:07:18:29:3A:4B:5C:6D:7E:8F:90';
const PRESENTED = '0F:1E:2D:3C:4B:5A:69:78:87:96:A5:B4:C3:D2:E1:F0';

// Four synthetic nodes: a current trusted one, one behind on the same channel, one whose update key is not trusted, and
// one plain Incus that is not Arca OS.
function osUpdate(now, overrides = {}) {
  return {
    known: true, asOf: now - MIN, version: '202611100000', channel: 'stable', checkFrequency: '6h', automatic: true,
    lastCheckAt: now - 4 * MIN, lastSuccessAt: now - 4 * MIN,
    lastUpdatedVersion: '202611100000', lastUpdatedAt: now - 2 * DAY,
    needsReboot: false, pendingVersion: null, previousVersion: '202611010000', goodVersion: '202611100000',
    lastResult: null, lastReason: null, lastErrorKind: null, trust: null,
    ...overrides,
  };
}

function host(name, update, now) {
  return {
    name, address: '', error: null, nodeName: name,
    machine: { mode: 'kernel', incusReachable: true },
    instances: name === 'alpha' ? [{ name: 'web', project: 'default', status: 'Running', type: 'container' }] : [],
    instancesKnown: true, projects: ['default'], databases: [], services: [], servicesKnown: true,
    metrics: null, alerts: [], plans: [], journal: [], journalKnown: true, journalPending: false,
    devices: 'off', devicesAt: null, phones: [], desktops: [],
    osUpdate: update, osUpdateRead: update ? 'ok' : 'missing',
    at: new Date(now).toISOString(),
  };
}

function envelope() {
  const now = Date.now();
  return {
    at: new Date(now).toISOString(), approvalUrl: '', pageHost: 'alpha', projects: [], defaultProject: '',
    hosts: [
      host('alpha', osUpdate(now), now),
      host('beta', osUpdate(now, {
        version: '202611010000', lastUpdatedVersion: '202611010000', lastUpdatedAt: now - 13 * DAY, asOf: now - 2 * MIN,
      }), now),
      host('gamma', osUpdate(now, {
        version: '202611010000', lastErrorKind: 'trust', lastSuccessAt: now - 3 * 60 * MIN, lastUpdatedAt: now - 13 * DAY,
        lastUpdatedVersion: '202611010000',
        trust: { embeddedCaFingerprint: EMBEDDED, presentedCaFingerprint: PRESENTED, failures: 4, escalated: true },
      }), now),
      host('delta', osUpdate(now, {
        known: false, version: null, channel: null, checkFrequency: null, automatic: null, lastCheckAt: null,
        lastSuccessAt: null, lastUpdatedVersion: null, lastUpdatedAt: null, needsReboot: null,
        previousVersion: null, goodVersion: null,
      }), now),
    ],
  };
}

test('the Fleet view is one click from the top bar, round-trips #fleet, and stays read only', async (t) => {
  const methods = [];
  const server = createServer((req, res) => {
    methods.push(req.method);
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/arca-canvas') { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(html); return; }
    if (path === '/arca-canvas/page.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end(pageJs); return; }
    if (path === '/arca-canvas/page.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end(pageCss); return; }
    if (path === '/arca-canvas/snapshot') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(envelope()));
      return;
    }
    if (path === '/arca-canvas/chat/history') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ session: { id: 'session-fleet', name: 'Doctor' }, messages: [] }));
      return;
    }
    res.writeHead(404).end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections?.(); server.close(resolve); }));
  const browser = await chromium.launch({ headless: true });
  t.after(() => browser.close());
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.addInitScript(() => {
    const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, 'innerHTML');
    Object.defineProperty(Element.prototype, 'innerHTML', {
      configurable: true,
      get: descriptor.get,
      set() { throw new Error('innerHTML is forbidden'); },
    });
  });
  const base = `http://127.0.0.1:${server.address().port}/arca-canvas`;
  await page.goto(base);
  await page.waitForFunction(() => location.hash.startsWith('#project/'));
  const projectHash = await page.evaluate(() => location.hash);
  assert.equal(await page.locator('.arca-fleet').isVisible(), false, 'the project canvas shows first');

  // One click from the top bar, beside the project picker.
  const link = page.locator('.arca-brand .arca-fleet-link');
  assert.equal(await link.innerText(), 'Fleet');
  await link.click();
  await page.waitForFunction(() => location.hash === '#fleet');
  await page.locator('.arca-fleet-row').first().waitFor();
  assert.equal(await link.getAttribute('aria-current'), 'page');
  assert.equal(await page.locator('.arca-scroller').isVisible(), false);
  assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Fleet updates');
  assert.match(await page.locator('.arca-canvas-chips').innerText(), /as of \d\d:\d\d[\s\S]*3 of 4 nodes report update state/);

  const rows = page.locator('.arca-fleet-row');
  assert.deepEqual(await rows.evaluateAll((nodes) => nodes.map((node) => [node.dataset.node, node.dataset.warning])), [
    ['alpha', ''], ['beta', 'behind'], ['gamma', 'trust'], ['delta', 'unknown'],
  ]);
  const cells = (name) => page.locator(`.arca-fleet-row[data-node="${name}"] > *`).evaluateAll((nodes) => nodes.map((node) => node.textContent));
  assert.deepEqual(await cells('alpha'), ['alpha', '202611100000', 'stable · every 6h', 'On', '4 min ago', '202611100000 · 2 days ago', '—']);
  assert.deepEqual((await cells('delta')).slice(0, 2), ['delta', 'not an Arca OS node']);
  const notes = await page.locator('.arca-fleet-note').evaluateAll((nodes) => nodes.map((node) => node.textContent));
  assert.equal(notes.length, 3, 'one warning for each node that has one, none for the current node');
  assert.match(notes[0], /Behind the fleet's newest version \(202611100000\) on stable\.[\s\S]*newest version on your fleet/);
  assert.match(notes[1], /Its update key is not trusted, so it will not update until this is fixed\./);
  assert.match(notes[1], new RegExp(EMBEDDED));
  assert.match(notes[1], new RegExp(PRESENTED));
  assert.match(notes[1], /4 checks in a row/);
  assert.match(await page.locator('.arca-fleet-note[data-kind="trust"] td').getAttribute('title'), new RegExp(PRESENTED));
  assert.match(notes[2], /This node does not report update state\./);
  assert.equal(await page.locator('.arca-fleet button, .arca-fleet input, .arca-fleet select, .arca-fleet form').count(), 0, 'no control changes anything');

  // A poll keeps the Fleet view, and a reload at #fleet opens it again.
  await page.waitForTimeout(200);
  assert.equal(await page.evaluate(() => location.hash), '#fleet');
  await page.reload();
  await page.locator('.arca-fleet-row').first().waitFor();
  assert.equal(await page.evaluate(() => location.hash), '#fleet');

  if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
    await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: theme });
      await page.waitForTimeout(250);
      await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `fleet-updates-${theme}.png`) });
    }
    await page.emulateMedia({ colorScheme: 'light' });
  }

  // Back returns to the project shown before; Forward reopens the Fleet view.
  await page.goBack();
  await page.waitForFunction((hash) => location.hash === hash, projectHash);
  assert.equal(await page.locator('.arca-fleet').isVisible(), false);
  assert.equal(await page.locator('.arca-scroller').isVisible(), true);
  assert.equal(await link.getAttribute('aria-current'), null);
  await page.goForward();
  await page.waitForFunction(() => location.hash === '#fleet');
  await page.locator('.arca-fleet-row').first().waitFor();

  // In the Fleet view the picker shows "Fleet", so choosing the project shown before is a real change that leaves the view.
  const picker = page.locator('.arca-project-select');
  const shownBefore = decodeURIComponent(projectHash.slice('#project/'.length));
  assert.equal(await picker.inputValue(), '');
  assert.equal(await picker.evaluate((node) => node.selectedOptions[0]?.textContent), 'Fleet');
  await picker.selectOption({ label: shownBefore });
  await page.waitForFunction((hash) => location.hash === hash, projectHash);
  assert.equal(await page.locator('.arca-fleet').isVisible(), false);
  assert.equal(await page.locator('.arca-scroller').isVisible(), true);
  assert.equal(await picker.inputValue(), shownBefore);
  assert.equal(await picker.locator('option', { hasText: 'Fleet' }).count(), 0, 'the placeholder leaves with the view');

  // A fresh tab opened straight at #fleet on a one-project canvas has no history; the picker is the way to the project.
  const fresh = await context.newPage();
  fresh.on('pageerror', (error) => errors.push(error.message));
  await fresh.goto(`${base}#fleet`);
  await fresh.locator('.arca-fleet-row').first().waitFor();
  assert.equal(await fresh.locator('.arca-project-select option:not([hidden])').count(), 1, 'one project');
  await fresh.locator('.arca-project-select').selectOption({ label: shownBefore });
  await fresh.waitForFunction((hash) => location.hash === hash, projectHash);
  assert.equal(await fresh.locator('.arca-scroller').isVisible(), true);

  // Dark mode renders the same table on the dark tokens.
  await fresh.goto(`${base}#fleet`);
  await fresh.locator('.arca-fleet-row').first().waitFor();
  // The page polls every 50 ms and each snapshot rebuilds the card, so the card is found and read in one call: a node
  // found in one call can be detached by the next, and a detached node has no computed colour.
  const background = () => fresh.evaluate(() => {
    const card = document.querySelector('.arca-fleet-card');
    return card ? getComputedStyle(card).backgroundColor : '';
  });
  await fresh.emulateMedia({ colorScheme: 'light' });
  const light = await background();
  await fresh.emulateMedia({ colorScheme: 'dark' });
  const dark = await background();
  assert.ok(light, 'the light card has a colour');
  assert.ok(dark, 'the dark card has a colour');
  assert.notEqual(light, dark, 'the card follows the colour scheme');
  assert.equal(await fresh.locator('.arca-fleet-row').count(), 4);

  assert.ok(methods.every((method) => method === 'GET'), 'the canvas sends only GET requests');
  assert.deepEqual(errors, []);
  assert.doesNotMatch(pageJs.toString(), /\.innerHTML\b/);
});
