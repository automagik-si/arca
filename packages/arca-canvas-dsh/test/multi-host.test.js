import assert from 'node:assert/strict';
import { chmod, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { Context } from '@deepseek-ai/cordis';
import { chromium } from 'playwright';
import * as plugin from '../index.js';
import { PROTOCOL_VERSION } from '../src/arca-client.js';

const hostSettings = (name) => ({ name, socketPath: `/example/${name}.sock`, viewerTokenPath: `/example/${name}.token` });

test('hosts config requires unique non-empty names and absolute reader paths', () => {
  const validate = (config) => plugin.Config['~standard'].validate(config);
  assert.equal(validate({ hosts: [hostSettings('alpha'), hostSettings('beta')] }).issues, undefined);
  assert.ok(validate({}).issues);
  for (const hosts of [[], [hostSettings('')], [hostSettings('  ')], [hostSettings('alpha'), hostSettings('alpha')],
    [{ ...hostSettings('alpha'), socketPath: 'relative' }], [{ ...hostSettings('alpha'), viewerTokenPath: 'relative' }],
    [{ socketPath: '/example/socket', viewerTokenPath: '/example/token' }]]) {
    assert.ok(validate({ hosts }).issues, 'invalid hosts config must fail');
  }
});

async function listen(t, server, address, host) {
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(address, host, resolve);
  });
  t.after(() => new Promise((resolve) => server.close(resolve)));
}

async function until(read, predicate) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    const value = await read();
    if (predicate(value)) return value;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error('Timed out waiting for host snapshot');
}

test('two Unix hosts isolate projects, devices, routes and failures in the live canvas', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-multi-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const state = { alpha: { unknown: 1 }, beta: { unknown: 2 } };
  const hosts = [];
  const servers = [];
  const calls = { alpha: [], beta: [], devices: [] };
  let delayBeta;
  let releaseBeta;
  // Hold one host's first reads to prove that the other host is published independently.
  delayBeta = new Promise((resolve) => { releaseBeta = resolve; });
  t.after(() => releaseBeta());
  for (const name of ['alpha', 'beta']) {
    const config = { name, socketPath: join(dir, `${name}.sock`), viewerTokenPath: join(dir, `${name}.token`) };
    await writeFile(config.viewerTokenPath, 'synthetic_viewer_bearer', { mode: 0o400 });
    const server = createServer(async (req, res) => {
      let data = '';
      for await (const chunk of req) data += chunk;
      const body = JSON.parse(data);
      calls[name].push(body);
      if (name === 'beta') await delayBeta;
      let result;
      if (body.method === 'initialize') result = { protocolVersion: PROTOCOL_VERSION };
      else {
        const values = {
          // Identical raw node ids and instance names must still stay distinct across hosts.
          arca_status: { node: 'synthetic-node', mode: 'watch-only', incusReachable: true },
          watch_report: { projects: [
            { project: 'shared', instances: [{ name: 'worker', status: name === 'alpha' ? 'Running' : 'Stopped', kind: 'container' }] },
            { project: `${name}-only`, instances: [{ name: `${name}-worker`, status: 'Running', kind: 'container' }] },
          ] },
          postgres_list: { databases: [{ name: `${name}-database`, status: 'ready' }] },
          node_metrics: { minutes: 5 },
          alerts_list: { firing: [], unknown: Array.from({ length: state[name].unknown }, () => ({ rule: 'synthetic-rule' })) },
          plan_list: { plans: [{ digest: 'synthetic-plan' }] },
        };
        if (state[name].toolError === body.params.name) {
          res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result: { isError: true } }));
          return;
        }
        result = { structuredContent: values[body.params.name] };
      }
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }));
    });
    await listen(t, server, config.socketPath);
    hosts.push(config);
    servers.push(server);
  }
  const devices = createServer(async (req, res) => {
    let data = '';
    for await (const chunk of req) data += chunk;
    const body = JSON.parse(data);
    calls.devices.push(body);
    const values = {
      droid_list: { droids: [{ name: 'worker', state: 'ready' }, { name: 'loose-phone', state: 'ready' }] },
      desktop_list: { desktops: [{ name: 'alpha-worker', status: 'Running' }] },
      droidd_status: {}, phone_status: { network: { type: 'wired' } },
    };
    const result = body.method === 'initialize' ? { protocolVersion: PROTOCOL_VERSION }
      : { structuredContent: values[body.params.name] };
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }));
  });
  await listen(t, devices, 0, '127.0.0.1');
  const ctx = new Context();
  let route;
  ctx.provide('webServer', { register(value) { route = value; return () => {}; } });
  const fiber = await ctx.plugin(plugin, {
    hosts, pollMs: 100, approvalUrl: 'https://example.com/approve',
    // Top-level settings continue to belong to the primary host only.
    devicesUrl: `http://127.0.0.1:${devices.address().port}/mcp`, devicesTokenPath: hosts[0].viewerTokenPath,
  });
  t.after(() => fiber.dispose());
  const methods = [];
  const web = createServer((req, res) => { methods.push(req.method); route.handler(req, res); });
  await listen(t, web, 0, '127.0.0.1');
  const url = `http://127.0.0.1:${web.address().port}/arca-canvas`;
  const read = async () => (await fetch(`${url}/snapshot`)).json();
  const early = await until(read, (data) => data.hosts[0].instancesKnown);
  assert.equal(early.hosts[1].instancesKnown, false, 'a pending host cannot delay the healthy host');
  assert.ok(calls.beta.length > 0, 'hosts start polling concurrently');
  releaseBeta();
  delayBeta = null;
  const ready = await until(read, (data) => data.hosts.every((host) => host.instancesKnown) && data.hosts[0].devices === 'ok');
  assert.deepEqual(ready.hosts.map((host) => host.name), ['alpha', 'beta']);
  assert.equal(ready.hosts[1].devices, 'off');
  assert.deepEqual(ready.hosts[1].phones, []);
  assert.ok(ready.hosts.every((host) => Number.isFinite(Date.parse(host.at)) && host.error === null));

  const browser = await chromium.launch({ headless: true });
  t.after(() => browser.close());
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(url);
  await page.waitForSelector('.arca-project-tile');
  const hostRows = page.locator('.arca-rail-row[data-kind="host"]');
  const projects = page.locator('.arca-rail-row[data-kind="project"]');
  const stage = page.locator('.arca-stage');
  const detail = page.locator('.arca-detail');
  const navigate = async (hash, title) => {
    await page.evaluate((value) => new Promise((resolve) => {
      if (location.hash === value) { resolve(); return; }
      window.addEventListener('hashchange', () => resolve(), { once: true });
      location.hash = value;
    }), hash);
    await page.waitForFunction((value) => document.querySelector('.arca-stage-title').textContent === value, title);
  };
  assert.equal(await hostRows.count(), 2);
  assert.equal(await hostRows.first().getAttribute('tabindex'), '0');
  assert.deepEqual(await hostRows.locator('.arca-rail-label').allTextContents(), ['alpha', 'beta']);
  assert.equal(await page.locator('[data-section="hosts"] .arca-section-count').textContent(), '2');
  assert.match(await stage.innerText(), /alpha-only/);
  assert.doesNotMatch(await stage.innerText(), /beta-only/);
  assert.deepEqual(await projects.locator('.arca-rail-label').allTextContents(), ['Alpha Only', 'Shared']);
  assert.match(await detail.innerText(), /1 check with no data/);
  assert.equal(await page.locator('.arca-host-devices .arca-device-row').count(), 3);
  assert.match(await stage.innerText(), /worker.*shared\/worker/s);
  assert.equal(await page.locator('a.arca-plans').getAttribute('href'), 'https://example.com/approve');

  const alphaSharedKey = await page.locator('.arca-project-tile').filter({ hasText: 'Shared' }).getAttribute('data-key');
  await page.locator('.arca-project-tile').filter({ hasText: 'Shared' }).click();
  await page.waitForSelector('.arca-canvas-node');
  assert.equal(new URL(page.url()).hash, '#project/alpha/shared');
  assert.match(await stage.innerText(), /Running/);
  assert.doesNotMatch(await stage.innerText(), /beta-worker/);
  await page.locator('.arca-canvas-node').click();
  await page.waitForFunction(() => location.hash === '#project/alpha/shared/instance/worker' && document.querySelector('.arca-detail-title').textContent === 'worker');
  assert.match(await detail.innerText(), /Running/);
  assert.match(await detail.innerText(), /Phone worker/);

  await hostRows.nth(1).click();
  await page.waitForFunction(() => document.querySelector('.arca-stage-title').textContent === 'beta');
  assert.equal(new URL(page.url()).hash, '#host/beta');
  assert.match(await stage.innerText(), /beta-only/);
  assert.doesNotMatch(await stage.innerText(), /alpha-only|loose-phone/);
  assert.deepEqual(await projects.locator('.arca-rail-label').allTextContents(), ['Beta Only', 'Shared']);
  assert.notEqual(await page.locator('.arca-project-tile').filter({ hasText: 'Shared' }).getAttribute('data-key'), alphaSharedKey);
  assert.equal(await page.locator('.arca-host-devices .arca-frame-empty').textContent(), 'Device data unavailable.');
  assert.match(await detail.innerText(), /2 checks with no data/);
  assert.doesNotMatch(await page.locator('.arca-canvas').innerText(), /watch-only/);
  await navigate('#project/beta/shared/instance/worker', 'worker');
  assert.match(await detail.innerText(), /Stopped/);
  assert.doesNotMatch(await detail.innerText(), /Matched by name/);
  await navigate('#project/beta/shared', 'Shared');
  assert.match(await stage.innerText(), /Stopped/);
  await navigate('#databases/beta', 'Databases');
  assert.match(await stage.innerText(), /beta-database/);
  assert.doesNotMatch(await stage.innerText(), /alpha-database/);
  await navigate('#not-placed/beta', 'Not placed');
  assert.match(await stage.innerText(), /Device data unavailable/);
  await navigate('#host/alpha', 'alpha');
  await page.getByRole('button', { name: /Devices not placed/ }).click();
  await page.waitForFunction(() => location.hash === '#not-placed/alpha' && document.querySelector('.arca-stage-title').textContent === 'Not placed');
  assert.match(await stage.innerText(), /loose-phone/);
  await navigate('#host/alpha', 'alpha');
  await page.getByRole('button', { name: /Databases not placed ·/ }).click();
  await page.waitForFunction(() => location.hash === '#databases/alpha' && document.querySelector('.arca-stage-title').textContent === 'Databases');
  assert.match(await stage.innerText(), /alpha-database/);

  for (const [hash, title, content] of [
    ['#host/synthetic-node', 'alpha', 'alpha-only'],
    ['#project/shared', 'Shared', 'Running'],
    ['#project/shared/instance/worker', 'worker', 'Running'],
    ['#not-placed', 'Not placed', 'loose-phone'],
    ['#databases', 'Databases', 'alpha-database'],
  ]) {
    await navigate(hash, title);
    assert.match(await stage.innerText(), new RegExp(content));
  }
  await navigate('#host/alpha', 'alpha');
  await hostRows.first().focus();
  await hostRows.first().press('ArrowDown');
  assert.equal(await hostRows.nth(1).evaluate((node) => node === document.activeElement), true);
  await hostRows.nth(1).press('Enter');
  await page.waitForFunction(() => location.hash === '#host/beta' && document.querySelector('.arca-stage-title').textContent === 'beta');
  await page.waitForTimeout(150);
  assert.equal(await hostRows.nth(1).evaluate((node) => node === document.activeElement), true);

  await t.test('two-host light and dark screenshots at 1440x900', async () => {
    await navigate('#host/alpha', 'alpha');
    const rectangles = {};
    for (const theme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: theme });
      rectangles[theme] = await page.evaluate(() => ({
        viewport: { width: innerWidth, height: innerHeight },
        rectangles: Object.fromEntries(['.arca-rail', '.arca-stage', '.arca-detail', '.arca-host-frame', '.arca-host-devices']
          .map((selector) => [selector, document.querySelector(selector).getBoundingClientRect().toJSON()])),
        hosts: [...document.querySelectorAll('.arca-rail-row[data-kind="host"]')].map((node) => node.getBoundingClientRect().toJSON()),
      }));
      if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
        await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
        await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `two-host-${theme}.png`) });
      }
    }
    if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
      await writeFile(join(process.env.ARCA_CANVAS_EVIDENCE_DIR, 'rectangles.json'), `${JSON.stringify(rectangles, null, 2)}\n`);
    }
  });

  // A tool error degrades just its field, even when watch data is unavailable.
  state.beta.toolError = 'watch_report';
  await until(read, (data) => !data.hosts[1].instancesKnown);
  await navigate('#host/beta', 'beta');
  await page.waitForFunction(() => document.querySelector('.arca-host-frame').textContent.includes('Watch report unavailable'));
  assert.match(await hostRows.nth(1).innerText(), /Watch report unavailable/);
  assert.equal(await hostRows.nth(1).locator('.arca-dot.bad').count(), 1);
  assert.equal(await page.locator('.arca-host-frame .arca-dot.bad').count(), 1);
  assert.equal(await projects.count(), 0);
  let data = await read();
  assert.equal(data.hosts[1].databases[0].name, 'beta-database');
  assert.equal(data.hosts[0].error, null);
  state.beta.toolError = null;
  await until(read, (data) => data.hosts[1].instancesKnown);
  await chmod(hosts[1].viewerTokenPath, 0o600);
  data = await until(read, (data) => data.hosts[1].error?.includes('regular 0400'));
  assert.equal(data.hosts[0].instancesKnown, true);
  await chmod(hosts[1].viewerTokenPath, 0o400);
  await until(read, (data) => data.hosts[1].error === null);
  await new Promise((resolve) => servers[1].close(resolve));
  data = await until(read, (data) => data.hosts[1].error?.includes('socket is unavailable'));
  assert.equal(data.hosts[1].instancesKnown, false);
  assert.equal(data.hosts[0].error, null);
  await page.reload();
  await page.waitForFunction(() => document.querySelector('.arca-host-frame')?.textContent.includes('Watch report unavailable'));
  await hostRows.first().click();
  await page.waitForSelector('.arca-project-tile');
  assert.match(await stage.innerText(), /alpha-only/);
  assert.doesNotMatch(await stage.innerText(), /beta-only|Watch report unavailable/);
  assert.equal(await page.locator('.arca-host-devices .arca-device-row').count(), 3);
  assert.ok(methods.every((method) => method === 'GET'), 'all canvas requests stay GET');
  const reads = ['arca_status', 'watch_report', 'postgres_list', 'node_metrics', 'alerts_list', 'plan_list',
    'droid_list', 'desktop_list', 'droidd_status', 'phone_status'];
  for (const entries of Object.values(calls)) {
    assert.ok(entries.every((body) => body.method === 'initialize' || (body.method === 'tools/call' && reads.includes(body.params.name))));
  }
  assert.deepEqual(errors, []);
});
