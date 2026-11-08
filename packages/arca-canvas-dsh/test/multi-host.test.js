import assert from 'node:assert/strict';
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
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

test('two Unix hosts keep projects, machines, journal and failures isolated', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-multi-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const state = { alpha: { unknown: 1 }, beta: { unknown: 2 } };
  const hosts = [];
  const servers = [];
  const calls = { alpha: [], beta: [], devices: [] };
  let releaseBeta;
  const delayBeta = new Promise((resolve) => { releaseBeta = resolve; });
  t.after(() => releaseBeta());
  const values = (name) => ({
    arca_status: { node: 'synthetic-node', mode: 'kernel', incusReachable: true },
    watch_report: { projects: [
      { project: 'shared', instances: [{ name: 'worker', status: name === 'alpha' ? 'Running' : 'Stopped', kind: 'container' }] },
      { project: `${name}-only`, instances: [{ name: `${name}-worker`, status: 'Running', kind: 'container' }] },
      ...(name === 'alpha' ? [{ project: 'plumbing', instances: [{ name: 'nats', status: 'Running', kind: 'container' }] }] : []),
    ] },
    postgres_list: { databases: [{ name: `${name}-database`, status: 'ready' }] },
    node_metrics: {
      latest: {
        at: name === 'alpha' ? 2 : 1,
        cpu: { cores: name === 'alpha' ? 8 : 4, usedPct: 25 },
        memory: { totalBytes: 16_000_000_000, availableBytes: 12_000_000_000 },
      },
    },
    alerts_list: { firing: [], unknown: Array.from({ length: state[name].unknown }, () => ({ rule: 'synthetic-rule' })) },
    plan_list: { plans: name === 'alpha' ? [] : [{ digest: 'synthetic-plan' }] },
    journal_list: { records: [{
      id: 1,
      entry: `deploy-${name}`,
      target: { kind: 'instance', name: `${name}-worker`, project: `${name}-only` },
      state: 'Applied',
      at_ms: name === 'alpha' ? 2 : 1,
    }] },
  });
  for (const name of ['alpha', 'beta']) {
    const config = { name, socketPath: join(dir, `${name}.sock`), viewerTokenPath: join(dir, `${name}.token`) };
    await writeFile(config.viewerTokenPath, 'synthetic_viewer_bearer', { mode: 0o400 });
    const server = createServer(async (req, res) => {
      let data = '';
      for await (const chunk of req) data += chunk;
      const body = JSON.parse(data);
      calls[name].push(body);
      if (name === 'beta' && body.method !== 'initialize') await delayBeta;
      let result;
      if (body.method === 'initialize') result = { protocolVersion: PROTOCOL_VERSION };
      else if (state[name].toolError === body.params.name) {
        res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result: { isError: true } }));
        return;
      } else result = { structuredContent: values(name)[body.params.name] };
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
    const deviceValues = {
      droid_list: { droids: [{ name: 'worker', state: 'ready' }, { name: 'loose-phone', state: 'ready' }] },
      desktop_list: { desktops: [{ name: 'alpha-worker', status: 'Running' }] },
      droidd_status: {}, phone_status: { network: { type: 'wired' } },
    };
    const result = body.method === 'initialize' ? { protocolVersion: PROTOCOL_VERSION }
      : { structuredContent: deviceValues[body.params.name] };
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }));
  });
  await listen(t, devices, 0, '127.0.0.1');
  const ctx = new Context();
  let route;
  ctx.provide('webServer', { register(value) { route = value; return () => {}; } });
  const fiber = await ctx.plugin(plugin, {
    hosts, pollMs: 100, approvalUrl: 'https://example.com/approve',
    devicesUrl: `http://127.0.0.1:${devices.address().port}/mcp`, devicesTokenPath: hosts[0].viewerTokenPath,
    projects: [{
      name: 'Shared project',
      domain: 'shared.example',
      members: [
        { host: 'alpha', incusProject: 'shared', instance: 'worker' },
        { host: 'alpha', incusProject: 'plumbing', instance: 'nats' },
      ],
    }],
    defaultProject: 'Shared project',
  });
  t.after(() => fiber.dispose());
  const methods = [];
  const web = createServer((req, res) => { methods.push(req.method); route.handler(req, res); });
  await listen(t, web, 0, '127.0.0.1');
  const url = `http://127.0.0.1:${web.address().port}/arca-canvas`;
  const read = async () => (await fetch(`${url}/snapshot`)).json();
  const early = await until(read, (snapshot) => snapshot.hosts[0].instancesKnown);
  assert.equal(early.hosts[1].instancesKnown, false, 'a pending host cannot delay the healthy host');
  assert.equal(early.projects[0].name, 'Shared project');
  assert.equal(early.defaultProject, 'Shared project');
  assert.ok(calls.beta.length > 0, 'hosts start polling concurrently');
  releaseBeta();
  const ready = await until(read, (snapshot) => snapshot.hosts.every((host) => host.instancesKnown)
    && snapshot.hosts[0].devices === 'ok' && snapshot.hosts.every((host) => host.journalKnown));
  assert.deepEqual(ready.hosts.map((host) => host.name), ['alpha', 'beta']);
  assert.equal(ready.hosts[1].devices, 'off');
  assert.deepEqual(ready.hosts[1].phones, []);
  assert.ok(ready.hosts.every((host) => Number.isFinite(Date.parse(host.at)) && host.error === null));
  assert.deepEqual(ready.hosts[1].journal, [{ id: 1, entry: 'deploy-beta', target: { kind: 'instance', name: 'beta-worker', project: 'beta-only' }, state: 'Applied', at_ms: 1 }]);

  const browser = await chromium.launch({ headless: true });
  t.after(() => browser.close());
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(url);
  await page.waitForSelector('.arca-node-project');
  await page.waitForFunction(() => location.hash === '#project/Shared%20project');

  assert.equal(await page.locator('.arca-machine:not(.arca-placeholder)').count(), 2);
  assert.deepEqual(await page.locator('.arca-machine:not(.arca-placeholder) .arca-machine-name').allTextContents(), ['alpha', 'beta']);
  assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Shared project');
  assert.deepEqual(await page.locator('.arca-node-instance').locator('.arca-nn').allTextContents(), ['worker', 'nats']);
  const canvasText = await page.locator('.arca-stage').innerText();
  assert.match(canvasText, /worker[\s\S]*runs on alpha/);
  assert.match(canvasText, /nats[\s\S]*runs on alpha/);
  assert.doesNotMatch(canvasText, /beta-worker/);
  assert.doesNotMatch(canvasText, /plumbing/i);
  assert.equal(await page.locator('.arca-node-domain').count(), 1);
  assert.match(await page.locator('.arca-node-domain').innerText(), /shared\.example/);

  const options = await page.locator('.arca-project-select option').allTextContents();
  assert.deepEqual(options, ['Shared project', 'Alpha Only', 'Beta Only', 'Shared']);
  assert.equal(await page.locator('.arca-project-select').inputValue(), 'Shared project');

  await page.locator('.arca-project-select').selectOption('Beta Only');
  await page.waitForFunction(() => location.hash === '#project/Beta%20Only');
  assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Beta Only');
  assert.match(await page.locator('.arca-stage').innerText(), /beta-worker[\s\S]*runs on beta/);
  assert.doesNotMatch(await page.locator('.arca-stage').innerText(), /alpha-worker|nats/);

  await page.locator('.arca-project-select').selectOption('Shared');
  await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Shared');
  assert.match(await page.locator('.arca-stage').innerText(), /worker[\s\S]*Stopped/);
  assert.doesNotMatch(await page.locator('.arca-stage').innerText(), /beta-worker/);
  assert.match(await page.locator('.arca-canvas-chips').innerText(), /beta · shared/);
  assert.match(await page.locator('.arca-node-project .arca-nk').innerText(), /Incus project shared/);

  assert.equal(await page.locator('.arca-tool').count(), 2);
  assert.match(await page.locator('.arca-tool').first().innerText(), /deploy-alpha[\s\S]*alpha[\s\S]*instance alpha-worker - Applied/);
  assert.equal(await page.locator('.arca-chat-view .arca-approval a').getAttribute('href'), 'https://example.com/approve');

  state.beta.toolError = 'watch_report';
  await until(read, (snapshot) => !snapshot.hosts[1].instancesKnown);
  await page.locator('.arca-project-select').selectOption('Shared project');
  await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Shared project');
  await page.waitForFunction(() => [...document.querySelectorAll('.arca-machine-instances')].some((node) => node.textContent === 'Instances unknown'));
  assert.match(await page.locator('.arca-machine').nth(1).innerText(), /Instances unknown/);
  assert.equal(await page.locator('.arca-machine').first().innerText().then((value) => value.includes('Instances unknown')), false);
  assert.deepEqual(await page.locator('.arca-node-instance').locator('.arca-nn').allTextContents(), ['worker', 'nats']);
  state.beta.toolError = null;
  await until(read, (snapshot) => snapshot.hosts[1].instancesKnown);

  await chmod(hosts[1].viewerTokenPath, 0o600);
  const tokenFailure = await until(read, (snapshot) => snapshot.hosts[1].error?.includes('regular 0400'));
  assert.equal(tokenFailure.hosts[0].instancesKnown, true);
  await page.waitForFunction(() => [...document.querySelectorAll('.arca-machine[data-online="false"] .arca-machine-name')]
    .some((node) => node.textContent === 'beta'));
  await chmod(hosts[1].viewerTokenPath, 0o400);
  await until(read, (snapshot) => snapshot.hosts[1].error === null);

  await new Promise((resolve) => servers[1].close(resolve));
  const socketFailure = await until(read, (snapshot) => snapshot.hosts[1].error?.includes('socket is unavailable'));
  assert.equal(socketFailure.hosts[0].error, null);
  assert.equal(socketFailure.hosts[0].instancesKnown, true);
  assert.match(await page.locator('.arca-machine').first().innerText(), /Online/);
  await page.waitForFunction(() => [...document.querySelectorAll('.arca-machine[data-online="false"] .arca-machine-name')]
    .some((node) => node.textContent === 'beta'));

  assert.ok(methods.every((method) => method === 'GET'), 'all canvas requests stay GET');
  const reads = ['arca_status', 'watch_report', 'postgres_list', 'node_metrics', 'alerts_list', 'plan_list', 'journal_list',
    'droid_list', 'desktop_list', 'droidd_status', 'phone_status'];
  for (const entries of Object.values(calls)) {
    assert.ok(entries.every((body) => body.method === 'initialize' || (body.method === 'tools/call' && reads.includes(body.params.name))));
  }
  assert.deepEqual(errors, []);
});
