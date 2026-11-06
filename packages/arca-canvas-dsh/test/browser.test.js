import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { chromium } from 'playwright';
import { Context } from '@deepseek-ai/cordis';
import * as plugin from '../index.js';

const fixture = JSON.parse(await readFile(new URL('./fixtures.json', import.meta.url), 'utf8'));
fixture.node_metrics.latest = { cpu: { usedPct: 24.5, cores: 8 }, load: { one: 1.2 } };
fixture.alerts_list.firing = [{ rule: 'cpu-high', message: 'CPU exceeded its threshold', target: 'example-node' }];
const devices = JSON.parse(await readFile(new URL('./devices-fixtures.json', import.meta.url), 'utf8'));
const reference = { topbar: [0, 0, 1440, 56], canvas: [0, 56, 1044, 844], steps: [1045, 124, 395, 616] };
const evidence = process.env.ARCA_CANVAS_EVIDENCE_DIR;

test('real fallback renders fixture snapshots safely in light and dark without a sidebar', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-browser-'));
  t.after(() => rm(dir, { force: true, recursive: true }));
  const socketPath = join(dir, 'arca.sock');
  const viewerTokenPath = join(dir, 'viewer');
  await writeFile(viewerTokenPath, 'browser_fixture_bearer', { mode: 0o400 });
  const upstream = createServer(async (req, res) => {
    let data = '';
    for await (const chunk of req) data += chunk;
    const body = JSON.parse(data);
    const result = body.method === 'initialize'
      ? { protocolVersion: '2025-11-25', capabilities: {}, serverInfo: { name: 'fixture', version: '1' } }
      : { structuredContent: fixture[body.params.name], content: [] };
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }));
  });
  await new Promise((resolve) => upstream.listen(socketPath, resolve));
  t.after(() => new Promise((resolve) => upstream.close(resolve)));
  const devicesTokenPath = join(dir, 'reader');
  await writeFile(devicesTokenPath, 'browser_reader_fixture_bearer', { mode: 0o400 });
  let deviceAvailable = true;
  const deviceServer = createServer(async (req, res) => {
    let data = '';
    for await (const chunk of req) data += chunk;
    const body = JSON.parse(data);
    if (!deviceAvailable) { res.writeHead(503).end(); return; }
    const value = devices[body.params.name];
    const result = body.method === 'initialize' ? { protocolVersion: '2025-11-25' }
      : { content: [], structuredContent: body.params.name === 'droid_list' ? { droids: value }
        : body.params.name === 'desktop_list' ? { desktops: value } : value };
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }));
  });
  await new Promise((resolve) => deviceServer.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => deviceServer.close(resolve)));
  const devicesUrl = `http://127.0.0.1:${deviceServer.address().port}/mcp`;
  let route;
  const ctx = new Context();
  ctx.provide('webServer', { register(value) { route = value; return () => {}; } });
  const fiber = await ctx.plugin(plugin, { socketPath, viewerTokenPath, devicesUrl, devicesTokenPath, pollMs: 100, approvalUrl: 'https://example.com/approve' });
  t.after(() => fiber.dispose());
  const web = createServer((req, res) => route.handler(req, res));
  await new Promise((resolve) => web.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => web.close(resolve)));
  const origin = `http://127.0.0.1:${web.address().port}`;
  const browser = await chromium.launch({ headless: true });
  t.after(() => browser.close());
  const results = [];
  for (const scheme of ['light', 'dark']) {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    let snapshotRequests = 0;
    page.on('request', (request) => { if (request.url().endsWith('/snapshot')) snapshotRequests++; });
    await page.goto(`${origin}/arca-canvas`);
    await page.waitForFunction(() => document.querySelector('[data-kind="machine"]')?.textContent.includes('example-node') && document.querySelector('[data-kind="desktop"]'));
    assert.equal(await page.locator('.arca-card').count(), 7);
    assert.equal(await page.locator('.arca-devices').count(), 0);
    assert.match(await page.locator('[data-kind="phone"]').textContent(), /Screen 720 × 1280/);
    const instanceText = await page.locator('[data-kind="instance"]').allTextContents();
    assert.equal(instanceText.length, 2);
    assert.ok(instanceText.every((value) => value.includes('Owner foreign') && !value.includes('[object Object]')));
    assert.equal(await page.locator('.arca-plans').getAttribute('href'), 'https://example.com/approve');
    assert.match(await page.locator('.arca-steps-placeholder').textContent(), /hidden/);
    const measured = await page.evaluate(() => {
      const shell = document.querySelector('.arca-canvas');
      const style = getComputedStyle(shell);
      const regions = Object.fromEntries([...document.querySelectorAll('[data-region]')].map((node) => {
        const { x, y, width, height } = node.getBoundingClientRect();
        return [node.dataset.region, [x, y, width, height]];
      }));
      const stage = document.querySelector('.arca-stage');
      const stageStyle = getComputedStyle(stage);
      const contentLeft = stage.getBoundingClientRect().left + parseFloat(stageStyle.paddingLeft);
      const contentWidth = stage.clientWidth - parseFloat(stageStyle.paddingLeft) - parseFloat(stageStyle.paddingRight);
      const cards = [...document.querySelectorAll('.arca-card')].map((card) => {
        const { x, y, width, height } = card.getBoundingClientRect();
        return { kind: card.dataset.kind, x, y, width, height };
      });
      return { regions, contentLeft, contentWidth, worldWidth: document.querySelector('.arca-world').clientWidth, cards,
        bg: style.backgroundColor, ink: style.color, surface: getComputedStyle(document.querySelector('.arca-card')).backgroundColor };
    });
    assert.equal(measured.contentWidth, 1012);
    assert.equal(measured.worldWidth, measured.contentWidth);
    for (const card of measured.cards) {
      assert.ok(card.x >= measured.contentLeft && card.x + card.width <= measured.contentLeft + measured.contentWidth, `${scheme} ${card.kind} exceeds content box`);
      assert.ok(card.y + card.height <= 900, `${scheme} fixture card must be visible`);
    }
    assert.equal(measured.bg, scheme === 'light' ? 'rgb(244, 244, 247)' : 'rgb(11, 11, 15)');
    assert.equal(measured.ink, scheme === 'light' ? 'rgb(12, 13, 17)' : 'rgb(241, 242, 246)');
    assert.equal(measured.surface, scheme === 'light' ? 'rgb(255, 255, 255)' : 'rgb(21, 22, 28)');
    for (const [region, box] of Object.entries(reference)) {
      assert.ok(measured.regions[region].every((value, i) => Math.abs(value - box[i]) <= 12), `${scheme} ${region}: ${measured.regions[region]}`);
    }
    assert.deepEqual(errors, []);
    if (evidence) await page.screenshot({ path: join(evidence, `${scheme}-1440x900.png`) });
    results.push({ scheme, ...measured });
    // Resize the embedding root (sidebar) and then the window, before a poll.
    const requestsBeforeResize = snapshotRequests;
    for (const width of [1200, 980, 1440]) {
      await page.evaluate((width) => { document.getElementById('arca-root').style.width = `${width}px`; }, width);
      await page.waitForFunction(() => {
        const stage = document.querySelector('.arca-stage');
        const style = getComputedStyle(stage);
        const width = stage.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
        return document.querySelector('.arca-world').clientWidth === width;
      });
      const fit = await page.evaluate(() => {
        const world = document.querySelector('.arca-world');
        return [...world.children].every((card) => parseFloat(card.style.left) + card.offsetWidth <= world.clientWidth);
      });
      assert.equal(fit, true, `cards fit resized root ${width}`);
    }
    await page.evaluate(() => { document.getElementById('arca-root').style.width = ''; });
    await page.setViewportSize({ width: 1100, height: 900 });
    await page.waitForFunction(() => document.querySelector('.arca-world').clientWidth === 1100 - 396 - 32);
    assert.equal(await page.evaluate(() => {
      const world = document.querySelector('.arca-world');
      return [...world.children].every((card) => parseFloat(card.style.left) + card.offsetWidth <= world.clientWidth);
    }), true);
    assert.equal(snapshotRequests, requestsBeforeResize, 'resize reflows without a new snapshot poll');
    assert.deepEqual(errors, []);
    await page.close();
  }
  if (evidence) await writeFile(join(evidence, 'regions.json'), JSON.stringify(results, null, 2));

  deviceAvailable = false;
  const unavailableResults = [];
  for (const scheme of ['light', 'dark']) {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.goto(`${origin}/arca-canvas`);
    await page.waitForFunction(() => document.querySelector('.arca-devices')?.textContent === 'Phones unavailable');
    assert.equal(await page.locator('.arca-card').count(), 5);
    assert.equal(await page.locator('[data-kind="phone"], [data-kind="desktop"]').count(), 0);
    assert.match(await page.locator('.arca-status').textContent(), /^Snapshot /);
    assert.match(await page.locator('.arca-steps-placeholder').textContent(), /hidden/);
    const measured = await page.evaluate(() => {
      const regions = Object.fromEntries([...document.querySelectorAll('[data-region]')].map((node) => {
        const { x, y, width, height } = node.getBoundingClientRect();
        return [node.dataset.region, [x, y, width, height]];
      }));
      const cards = [...document.querySelectorAll('.arca-card')].map((node) => {
        const { x, y, width, height } = node.getBoundingClientRect();
        return { kind: node.dataset.kind, x, y, width, height };
      });
      const chip = document.querySelector('.arca-devices');
      return { regions, cards, chip: chip.textContent, chipColor: getComputedStyle(chip).color,
        mutedColor: getComputedStyle(document.querySelector('.arca-crumb')).color };
    });
    assert.equal(measured.chipColor, measured.mutedColor);
    for (const [region, box] of Object.entries(reference)) {
      assert.ok(measured.regions[region].every((value, i) => Math.abs(value - box[i]) <= 12));
    }
    for (const card of measured.cards) assert.ok(card.x >= 16 && card.x + card.width <= 1028 && card.y + card.height <= 900);
    assert.deepEqual(errors, []);
    if (evidence) await page.screenshot({ path: join(evidence, `${scheme}-unavailable-1440x900.png`) });
    unavailableResults.push({ scheme, ...measured });
    await page.close();
  }
  if (evidence) await writeFile(join(evidence, 'unavailable-regions.json'), JSON.stringify(unavailableResults, null, 2));

  const page = await browser.newPage();
  const malicious = '<img src=x onerror="window.injected=1"><script>window.injected=1</script>';
  await page.route('**/arca-canvas/snapshot', (route) => route.fulfill({ json: {
    at: malicious, error: malicious, approvalUrl: 'javascript:window.injected=1',
    machine: { node: malicious, mode: malicious }, instances: [{ name: malicious }],
    databases: [{ name: malicious }], phones: [{ name: malicious, network: malicious }], desktops: [{ name: malicious, status: malicious }], alerts: { firing: [{ rule: malicious, message: malicious }] }, plans: [{}],
  } }));
  await page.goto(`${origin}/arca-canvas`);
  await page.waitForFunction(() => document.querySelector('.arca-card-title')?.textContent.includes('<img'));
  assert.ok((await page.locator('#arca-root').textContent()).includes(malicious));
  assert.equal(await page.locator('#arca-root img, #arca-root script, #arca-root a').count(), 0);
  assert.equal(await page.evaluate(() => window.injected), undefined);
  await page.close();
});
