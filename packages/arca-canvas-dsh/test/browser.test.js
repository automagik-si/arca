import assert from 'node:assert/strict';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';

const here = fileURLToPath(new URL('.', import.meta.url));
const pageJs = await readFile(join(here, '../page.js'));
const pageCss = await readFile(join(here, '../page.css'));
const fixture = JSON.parse(await readFile(join(here, 'dashboard-fixtures.json'), 'utf8'));
const html = '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="arca-canvas/page.css"></head><body><main id="arca-root" data-arca-base="/arca-canvas" data-poll-ms="50"></main><script src="arca-canvas/page.js" defer></script></body></html>';

function luminance(rgb) {
  const channels = rgb.match(/\d+/g).slice(0, 3).map((value) => {
    const channel = Number(value) / 255;
    return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  });
  return channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
}

test('v28 layout binds both hosts, the selected project and the Doctor feed', async (t) => {
  let data = fixture;
  const methods = [];
  const server = createServer((req, res) => {
    methods.push(req.method);
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
  await page.waitForSelector('.arca-node-project');
  await page.waitForFunction(() => location.hash === '#project/Studio');

  await t.test('all four regions exist and live data binds into each', async () => {
    assert.equal(await page.locator('.arca-topbar').count(), 1);
    assert.equal(await page.locator('.arca-machines').count(), 1);
    assert.equal(await page.locator('.arca-stage').count(), 1);
    assert.equal(await page.locator('.arca-panel').count(), 1);

    assert.equal(await page.locator('.arca-project-select').inputValue(), 'Studio');
    const options = await page.locator('.arca-project-select option').allTextContents();
    assert.deepEqual(options, ['Studio', 'Automation', 'Sandbox', 'Tools']);
    assert.deepEqual(await page.locator('.arca-project-select optgroup').evaluateAll((nodes) => nodes.map((node) => node.label)),
      ['Your projects', 'Machine projects']);
    assert.equal(await page.locator('.arca-mode').first().getAttribute('aria-pressed'), 'true');
    assert.equal(await page.locator('.arca-mode').nth(1).isDisabled(), true);
    assert.match(await page.locator('.arca-mode').nth(1).innerText(), /Arca Cloud[\s\S]*coming soon/i);
    assert.match(await page.locator('.arca-clock-chip').innerText(), /^\d{2}:\d{2}$/);
    assert.equal(await page.locator('.arca-summary').innerText(), '2 machines · 12 cores · 24 GB');

    const machines = page.locator('.arca-machine:not(.arca-placeholder)');
    assert.equal(await machines.count(), 2);
    assert.deepEqual(await machines.locator('.arca-machine-name').allTextContents(), ['alpha', 'beta']);
    for (const [index, expected] of [['0', ['2 / 8 cores', '4 / 16 GB']], ['1', ['2 / 4 cores', '6 / 8 GB']]].entries()) {
      const card = machines.nth(index);
      assert.equal(await card.locator('.arca-status').innerText(), 'Online');
      assert.deepEqual(await card.locator('.arca-meter-value').allTextContents(), expected[1]);
      assert.equal(await card.locator('.arca-meter').count(), 2);
      assert.equal(await card.locator('.arca-meter-fill').count(), 2);
      const widths = await card.locator('.arca-meter-fill').evaluateAll((nodes) => nodes.map((node) => Number(node.getAttribute('width'))));
      assert.ok(widths.every((width) => width > 0 && width <= 100), `bar widths ${widths}`);
      assert.equal(await card.locator('.arca-machine-instances').innerText(), '3 instances');
    }
    assert.equal(await page.locator('.arca-placeholder').count(), 2);
    assert.match(await page.locator('.arca-placeholder').first().innerText(), /Storage[\s\S]*soon/i);
    assert.match(await page.locator('.arca-placeholder').nth(1).innerText(), /Phones[\s\S]*soon/i);

    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Studio');
    assert.deepEqual(await page.locator('.arca-canvas-chips .arca-chip').allTextContents(),
      ['4 things', '2 hosts', 'studio.example']);
    assert.equal(await page.locator('.arca-node-project').count(), 1);
    assert.equal(await page.locator('.arca-node-domain').count(), 1);
    assert.equal(await page.locator('.arca-node-instance').count(), 3);
    assert.equal(await page.locator('.arca-node-database').count(), 1);
    assert.equal(await page.locator('.arca-wire').count(), 5);
    assert.ok(await page.locator('.arca-wire-base').first().getAttribute('d'));
    assert.match(await page.locator('.arca-wire-base').first().getAttribute('d'), /^M [\d.]+ [\d.]+ C /);

    const app = page.locator('.arca-node-instance').filter({ hasText: 'app' });
    assert.match(await app.innerText(), /runs on alpha/);
    assert.equal(await app.locator('a.arca-card-link').getAttribute('href'), 'https://app.example.test');
    const database = page.locator('.arca-node-database');
    assert.match(await database.innerText(), /PostgreSQL/);
    assert.match(await database.innerText(), /ready/);
    assert.match(await database.innerText(), /10\.0\.0\.5:5432/);
    assert.match(await database.innerText(), /1 link/);
    assert.equal(await database.locator('a.arca-card-link').count(), 0);
    assert.doesNotMatch(await page.locator('.arca-stage').innerText(), /orphan|orders|customers/);
    assert.doesNotMatch(await page.locator('.arca-project-select').innerText(), /Plumbing/);

    assert.equal(await page.locator('.arca-panel [role="tab"]').count(), 2);
    assert.equal(await page.locator('[role="tab"]').first().innerText(), 'Chat');
    assert.equal(await page.locator('[role="tab"]').nth(1).innerText(), 'Steps');
    assert.equal(await page.locator('[role="tab"]').first().getAttribute('aria-selected'), 'true');
    assert.equal(await page.locator('.arca-tool').count(), 4);
    assert.match(await page.locator('.arca-tool').first().innerText(), /rotate-token[\s\S]*beta[\s\S]*token viewer - Applied/);
    for (const record of data.journal ?? []) {
      assert.match(await page.locator('.arca-tool').filter({ hasText: record.entry }).innerText(), new RegExp(record.host));
    }
    const approval = page.locator('.arca-chat-view .arca-approval a');
    assert.equal(await approval.getAttribute('href'), fixture.approvalUrl);
    assert.match(await approval.innerText(), /Review approvals/);
    const composer = page.locator('.arca-composer-input');
    assert.equal(await composer.getAttribute('readonly'), '');
    assert.equal(await composer.getAttribute('placeholder'), 'Chat is coming in phase 2');
    assert.equal(await page.locator('.arca-send').getAttribute('aria-disabled'), 'true');
    await composer.click();
    await page.keyboard.type('hello');
    assert.equal(await composer.inputValue(), '');
    await page.locator('[role="tab"]').nth(1).click();
    assert.equal(await page.locator('.arca-steps-view').isVisible(), true);
    assert.equal(await page.locator('.arca-chat-view').isVisible(), false);
    assert.equal(await page.locator('.arca-step').count(), 4);
    assert.match(await page.locator('.arca-step').first().innerText(), /rotate-token[\s\S]*beta[\s\S]*Applied/);
    await page.locator('[role="tab"]').first().click();
    assert.equal(await page.locator('.arca-chat-view').isVisible(), true);
  });

  await t.test('project picker, hash and reload selection follow the configured default', async () => {
    assert.equal(new URL(page.url()).hash, '#project/Studio');
    await page.locator('.arca-project-select').selectOption('Automation');
    await page.waitForFunction(() => location.hash === '#project/Automation');
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Automation');
    assert.equal(await page.locator('.arca-node-domain').count(), 0);
    assert.equal(await page.locator('.arca-node-database').count(), 0);
    assert.equal(await page.locator('.arca-node-instance').count(), 1);
    assert.match(await page.locator('.arca-node-instance').innerText(), /queue[\s\S]*runs on beta/);
    assert.equal(await page.locator('.arca-canvas-chips .arca-chip').first().innerText(), '1 thing');

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector('.arca-node-project');
    assert.equal(await page.locator('.arca-project-select').inputValue(), 'Automation');
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Automation');

    await page.evaluate(() => { location.hash = '#project/Tools'; });
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Tools');
    assert.equal(await page.locator('.arca-node-instance').count(), 1);
    assert.match(await page.locator('.arca-node-instance').innerText(), /rogue[\s\S]*runs on beta/);

    await page.evaluate(() => { location.hash = '#project/does-not-exist'; });
    await page.waitForFunction(() => location.hash === '#project/Studio');
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Studio');
    assert.equal(await page.locator('.arca-project-select').inputValue(), 'Studio');
  });

  await t.test('missing journal and metrics degrade without invented rows', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host) => ({
        ...host, metrics: null, journal: [], journalKnown: false, plans: [],
      })),
    };
    await page.waitForFunction(() => document.querySelectorAll('.arca-tool').length === 0);
    assert.equal(await page.locator('.arca-chat-view .arca-approval').count(), 0);
    assert.match(await page.locator('.arca-chat-view').innerText(), /Activity journal unavailable/);
    assert.equal(await page.locator('.arca-machine').first().innerText().then((value) => value.includes('No metrics')), true);
    assert.equal(await page.locator('.arca-summary').innerText(), '2 machines');
    data = fixture;
    await page.waitForSelector('.arca-tool');
  });

  await t.test('hostile snapshot strings render as text and never as markup', async () => {
    const hostile = '<a href="javascript:window.__arcaXss=1">x</a>';
    data = {
      ...fixture,
      projects: [{ name: 'Hostile', members: [{ host: 'alpha', incusProject: hostile, instance: 'hostile-node' }] }],
      defaultProject: 'Hostile',
      hosts: fixture.hosts.map((host) => host.name === 'alpha' ? {
        ...host,
        projects: [hostile],
        instances: [...host.instances, { name: 'hostile-node', project: hostile, kind: 'container', status: 'Running' }],
        journal: [{ id: 9, entry: hostile, target: { kind: hostile, name: hostile }, state: 'Applied', at_ms: 1794160200000 }],
      } : host),
    };
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Hostile');
    assert.equal(await page.evaluate(() => window.__arcaXss), undefined);
    assert.equal(await page.locator('.arca-stage a[href^="javascript:"]').count(), 0);
    assert.equal(await page.locator('.arca-stage img').count(), 0);
    assert.equal(await page.locator('.arca-tool').filter({ hasText: hostile }).count(), 1);
    data = fixture;
    await page.evaluate(() => { location.hash = '#project/Studio'; });
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Studio');
  });

  await t.test('light and dark theme tokens, rectangles and screenshots', async () => {
    const measurements = {};
    for (const [theme, expected] of [
      ['light', { bg: 'rgb(244, 244, 247)', surface: 'rgb(255, 255, 255)', ink: 'rgb(12, 13, 17)', muted: 'rgb(93, 97, 112)' }],
      ['dark', { bg: 'rgb(11, 11, 15)', surface: 'rgb(21, 22, 28)', ink: 'rgb(241, 242, 246)', muted: 'rgb(164, 169, 183)' }],
    ]) {
      await page.emulateMedia({ colorScheme: theme });
      await page.waitForTimeout(300);
      const actual = await page.evaluate(() => {
        const style = (selector) => getComputedStyle(document.querySelector(selector));
        return {
          bg: style('.arca-canvas').backgroundColor,
          surface: style('.arca-topbar').backgroundColor,
          ink: style('.arca-canvas-title').color,
          muted: style('.arca-panel-sub').color,
          panel: style('.arca-panel').backgroundColor,
          composer: style('.arca-composer-box').borderTopColor,
          colorScheme: style('.arca-canvas').colorScheme,
          rectangles: Object.fromEntries([
            '.arca-topbar', '.arca-machines', '.arca-stage', '.arca-panel',
            '.arca-world', '.arca-node-project', '.arca-node-instance', '.arca-wire', '.arca-composer',
          ].map((selector) => [selector, document.querySelector(selector).getBoundingClientRect().toJSON()])),
          nodes: {
            project: document.querySelectorAll('.arca-node-project').length,
            instances: document.querySelectorAll('.arca-node-instance').length,
            databases: document.querySelectorAll('.arca-node-database').length,
            domain: document.querySelectorAll('.arca-node-domain').length,
            wires: document.querySelectorAll('.arca-wire').length,
          },
        };
      });
      assert.equal(actual.bg, expected.bg, `${theme} background`);
      assert.equal(actual.surface, expected.surface, `${theme} surface`);
      assert.equal(actual.ink, expected.ink, `${theme} ink`);
      assert.equal(actual.muted, expected.muted, `${theme} muted`);
      assert.equal(actual.panel, expected.surface, `${theme} panel surface`);
      assert.equal(actual.colorScheme, 'light dark');
      assert.deepEqual(actual.nodes, { project: 1, instances: 3, databases: 1, domain: 1, wires: 5 });
      const topbar = actual.rectangles['.arca-topbar'];
      const machines = actual.rectangles['.arca-machines'];
      const stage = actual.rectangles['.arca-stage'];
      const panel = actual.rectangles['.arca-panel'];
      assert.deepEqual([topbar.width, topbar.height], [1440, 56]);
      assert.ok(machines.width >= 1000 && machines.height >= 140);
      assert.ok(stage.width >= 1000 && stage.height >= 500);
      assert.equal(panel.width, 396);
      assert.equal(Math.round(topbar.bottom), Math.round(panel.top));
      assert.ok(actual.rectangles['.arca-node-instance'].width >= 300);
      assert.equal(Math.round(actual.rectangles['.arca-composer'].bottom), 900);
      const contrast = (Math.max(luminance(actual.muted), luminance(actual.panel)) + 0.05)
        / (Math.min(luminance(actual.muted), luminance(actual.panel)) + 0.05);
      assert.ok(contrast >= 4.5, `${theme} muted contrast ${contrast}`);
      measurements[theme] = { viewport: { width: 1440, height: 900 }, ...actual, contrast };
      if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
        await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
        await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `canvas-v28-${theme}.png`) });
      }
    }
    if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
      await writeFile(join(process.env.ARCA_CANVAS_EVIDENCE_DIR, 'rectangles.json'), `${JSON.stringify(measurements, null, 2)}\n`);
    }
    await page.emulateMedia({ colorScheme: 'light' });
  });

  await t.test('polling, keyboard and errors stay read-only', async () => {
    assert.ok(methods.length > 0);
    assert.ok(methods.every((method) => method === 'GET'), `all requests GET: ${[...new Set(methods)]}`);
    assert.doesNotMatch(pageJs.toString(), /\.innerHTML\b/);
    assert.ok(pageJs.length < 2 * 1024 * 1024);
    await page.locator('.arca-project-select').focus();
    assert.equal(await page.locator('.arca-project-select').evaluate((node) => node === document.activeElement), true);
    await page.keyboard.press('ArrowDown');
    assert.deepEqual(errors, []);
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-panel');
  const stacked = await page.evaluate(() => ({
    machines: document.querySelector('.arca-machines').getBoundingClientRect().toJSON(),
    stage: document.querySelector('.arca-stage').getBoundingClientRect().toJSON(),
    panel: document.querySelector('.arca-panel').getBoundingClientRect().toJSON(),
  }));
  assert.ok(stacked.panel.top >= stacked.stage.bottom - 1, 'panel stacks below the canvas');
  assert.ok(stacked.stage.top >= stacked.machines.bottom - 1, 'canvas follows the machines strip');
});
