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
  let route;
  const ctx = new Context();
  ctx.provide('webServer', { register(value) { route = value; return () => {}; } });
  const fiber = await ctx.plugin(plugin, { socketPath, viewerTokenPath, pollMs: 100, approvalUrl: 'https://example.com/approve' });
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
    await page.goto(`${origin}/arca-canvas`);
    await page.waitForFunction(() => document.querySelector('[data-kind="machine"]')?.textContent.includes('example-node'));
    assert.equal(await page.locator('.arca-card').count(), 5);
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
    await page.close();
  }
  if (evidence) await writeFile(join(evidence, 'regions.json'), JSON.stringify(results, null, 2));

  const page = await browser.newPage();
  const malicious = '<img src=x onerror="window.injected=1"><script>window.injected=1</script>';
  await page.route('**/arca-canvas/snapshot', (route) => route.fulfill({ json: {
    at: malicious, error: malicious, approvalUrl: 'javascript:window.injected=1',
    machine: { node: malicious, mode: malicious }, instances: [{ name: malicious }],
    databases: [{ name: malicious }], alerts: { firing: [{ rule: malicious, message: malicious }] }, plans: [{}],
  } }));
  await page.goto(`${origin}/arca-canvas`);
  await page.waitForFunction(() => document.querySelector('.arca-card-title')?.textContent.includes('<img'));
  assert.ok((await page.locator('#arca-root').textContent()).includes(malicious));
  assert.equal(await page.locator('#arca-root img, #arca-root script, #arca-root a').count(), 0);
  assert.equal(await page.evaluate(() => window.injected), undefined);
  await page.close();
});
