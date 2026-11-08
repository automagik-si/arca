import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';

const here = fileURLToPath(new URL('.', import.meta.url));
const pageJs = await readFile(join(here, '../page.js'));
const pageCss = await readFile(join(here, '../page.css'));
const fixture = JSON.parse(await readFile(join(here, 'tree-fixtures.json'), 'utf8'));
const html = '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="arca-canvas/page.css"></head><body><main id="arca-root" data-arca-base="/arca-canvas" data-poll-ms="50"></main><script src="arca-canvas/page.js" defer></script></body></html>';

test('hosts, projects, selection, URL and retained state survive live polls', async (t) => {
  let data = fixture;
  const server = createServer((req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/arca-canvas') { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(html); return; }
    if (path === '/arca-canvas/page.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end(pageJs); return; }
    if (path === '/arca-canvas/page.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end(pageCss); return; }
    if (path === '/arca-canvas/snapshot') { res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(data)); return; }
    res.writeHead(404).end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const browser = await chromium.launch({ headless: true });
  t.after(() => browser.close());
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  await page.addInitScript(() => {
    const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, 'innerHTML');
    Object.defineProperty(Element.prototype, 'innerHTML', {
      configurable: true,
      get: descriptor.get,
      set() { throw new Error('innerHTML is forbidden'); },
    });
  });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const origin = `http://127.0.0.1:${server.address().port}`;
  await page.goto(`${origin}/arca-canvas`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-project-tile');

  assert.equal(await page.locator('.arca-section-head').count(), 2);
  assert.equal(await page.locator('.arca-rail-row[data-kind="host"]').count(), 1);
  assert.equal(await page.locator('.arca-project-tile').count(), 12);
  assert.match(await page.locator('.arca-rail-row[data-kind="host"]').innerText(), /12 projects · 32 instances/);
  assert.match(await page.locator('.arca-project-tile').first().innerText(), /\d+ running/);
  assert.doesNotMatch(await page.locator('.arca-project-tile').first().innerText(), /\d+ instances/);
  assert.equal(await page.locator('.arca-host-devices .arca-device-row').count(), 6);
  assert.match(await page.locator('.arca-host-devices').innerText(), /alpha-node-01/);
  assert.equal(await page.locator('.arca-rail-row[data-kind="not-placed"]').count(), 1);
  assert.equal(await page.locator('.arca-rail-row[data-kind="databases"]').count(), 1);
  assert.match(await page.locator('.arca-rail-row[data-kind="project"]').first().innerText(), /\d+ running/);
  assert.match(await page.locator('.arca-host-alerts').innerText(), /1 firing alert/);
  assert.match(await page.locator('.arca-host-alerts .arca-no-data').innerText(), /1 check has no data/);
  assert.equal(await page.locator('.arca-plans').getAttribute('role'), 'link');

  await page.evaluate(() => {
    document.querySelector('.arca-rail-row[data-kind="host"]').dataset.marker = 'keep';
    document.querySelector('.arca-project-tile').dataset.marker = 'keep';
  });
  await page.waitForTimeout(250);
  assert.equal(await page.evaluate(() => document.querySelector('.arca-rail-row[data-kind="host"]').dataset.marker), 'keep');
  assert.equal(await page.evaluate(() => document.querySelector('.arca-project-tile').dataset.marker), 'keep');

  await page.locator('.arca-project-tile').first().click();
  await page.waitForSelector('.arca-canvas-node');
  assert.equal(new URL(page.url()).hash, '#project/alpha');
  assert.equal(await page.locator('.arca-canvas-node').count(), 5);
  assert.match(await page.locator('.arca-canvas-node').first().innerText(), /Container/);
  assert.doesNotMatch(await page.locator('.arca-canvas-node').first().innerText(), /No image or IP reported/);
  assert.equal(await page.locator('.arca-canvas-node .arca-canvas-node-short:not([hidden])').count(), 0);
  assert.match(await page.locator('.arca-devices-frame').innerText(), /alpha-node-01/);

  await page.locator('.arca-canvas-node').first().click();
  await page.waitForFunction(() => location.hash.includes('/instance/'));
  await page.waitForFunction(() => document.querySelector('.arca-detail-title')?.textContent === 'alpha-node-01');
  assert.equal(new URL(page.url()).hash, '#project/alpha/instance/alpha-node-01');
  assert.equal(await page.locator('.arca-canvas-node.is-selected').count(), 1);

  await page.goBack();
  await page.waitForFunction(() => location.hash === '#project/alpha');
  await page.goBack();
  await page.waitForFunction(() => location.hash.startsWith('#host/'));
  assert.equal(await page.locator('.arca-project-tile').count(), 12);

  await page.locator('.arca-section-head').nth(1).click();
  assert.equal(await page.locator('.arca-section-body').nth(1).evaluate((node) => node.hidden), true);
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-project-tile');
  assert.equal(await page.locator('.arca-section-body').nth(1).evaluate((node) => node.hidden), true);

  await page.locator('.arca-rail-toggle').click();
  assert.equal(await page.locator('.arca-canvas').evaluate((node) => node.classList.contains('tree-collapsed')), true);
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-rail-toggle');
  assert.equal(await page.locator('.arca-canvas').evaluate((node) => node.classList.contains('tree-collapsed')), true);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-rail-toggle');
  if (await page.locator('.arca-canvas').evaluate((node) => node.classList.contains('tree-collapsed'))) {
    await page.locator('.arca-rail-toggle').click();
  }
  const phone = await page.evaluate(() => {
    const rail = document.querySelector('.arca-rail').getBoundingClientRect();
    const stage = document.querySelector('.arca-stage').getBoundingClientRect();
    const host = document.querySelector('.arca-rail-row[data-kind="host"] .arca-rail-label').getBoundingClientRect();
    return { rail: { top: rail.top, bottom: rail.bottom, height: rail.height }, stageTop: stage.top, hostTop: host.top, hostBottom: host.bottom };
  });
  assert.ok(phone.hostTop >= phone.rail.top && phone.hostBottom <= phone.rail.bottom + 1);
  assert.ok(phone.stageTop >= phone.rail.bottom - 1);
  assert.ok(pageJs.length < 2 * 1024 * 1024);

  const hostileProject = '<a href="javascript:window.__arcaXss=1">x</a>';
  data = {
    ...fixture,
    projects: [...fixture.projects, hostileProject],
    instances: [...fixture.instances, { name: 'hostile-node', project: hostileProject, kind: 'container', status: 'Running', owner: { class: 'foreign' }, privileged: false }],
  };
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-project-tile');
  assert.equal(await page.locator('.arca-project-tile').filter({ hasText: hostileProject }).count(), 1);
  assert.equal(await page.evaluate(() => window.__arcaXss), undefined);
  assert.equal(await page.locator('.arca-project-tile a').count(), 0);
  assert.equal(await page.locator('.arca-project-tile img').count(), 0);
  assert.doesNotMatch(pageJs.toString(), /\.innerHTML\b/);
  assert.deepEqual(errors, []);
});
