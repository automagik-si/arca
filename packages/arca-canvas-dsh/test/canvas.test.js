import assert from 'node:assert/strict';
import { chmod, mkdtemp, readFile, rename, rm, symlink, writeFile } from 'node:fs/promises';
import { createServer, request } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { inspect } from 'node:util';
import { runInNewContext } from 'node:vm';
import test from 'node:test';
import { Context } from '@deepseek-ai/cordis';
import yaml from 'js-yaml';
import * as plugin from '../index.js';
import { createArcaClient, PROTOCOL_VERSION } from '../src/arca-client.js';
import { collectSnapshot, createSnapshotCache } from '../src/snapshot.js';

const fixtures = JSON.parse(await readFile(new URL('./fixtures.json', import.meta.url), 'utf8'));
const sentinel = 'arca_test_bearer_do_not_publish';

async function sandbox(t) {
  const dir = await mkdtemp(join(tmpdir(), 'arca-canvas-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const config = { socketPath: join(dir, 'mcp.sock'), viewerTokenPath: join(dir, 'viewer.token'), pollMs: 100 };
  await writeFile(config.viewerTokenPath, `${sentinel}\n`, { mode: 0o400 });
  return { dir, config };
}

function toolResult(body, value) {
  return { jsonrpc: '2.0', id: body.id, result: {
    content: [{ type: 'text', text: JSON.stringify(value) }],
    structuredContent: value,
    isError: false,
  } };
}

async function fakeArca(t, config, respond = (body) => toolResult(body, fixtures[body.params.name]), onInitialize = async () => {}) {
  const calls = [];
  const server = createServer(async (req, res) => {
    try {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      calls.push({ body, method: req.method, url: req.url, headers: req.headers });
      if (body.method === 'initialize') await onInitialize();
      const reply = body.method === 'initialize'
        ? { jsonrpc: '2.0', id: body.id, result: { protocolVersion: PROTOCOL_VERSION, capabilities: {}, serverInfo: { name: 'fixture', version: '1' } } }
        : await respond(body, req);
      res.writeHead(reply.status ?? 200, { 'Content-Type': 'application/json' });
      res.end(typeof reply === 'string' ? reply : JSON.stringify(reply));
    } catch {
      res.writeHead(500).end();
    }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(config.socketPath, resolve); });
  t.after(() => new Promise((resolve) => server.close(resolve)));
  return calls;
}

async function until(predicate) {
  const deadline = Date.now() + 3000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error('Timed out waiting for fixture');
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

test('unix HTTP POST initializes then calls a tool with per-request bearer and no Origin', async (t) => {
  const { config } = await sandbox(t);
  const calls = await fakeArca(t, config);
  const client = createArcaClient(config);
  assert.deepEqual(await client.call('arca_status'), fixtures.arca_status);
  assert.deepEqual(calls.map(({ body }) => body.method), ['initialize', 'tools/call']);
  for (const call of calls) {
    assert.equal(call.method, 'POST');
    assert.equal(call.url, '/mcp');
    assert.equal(call.headers.authorization, `Bearer ${sentinel}`);
    assert.equal(call.headers.origin, undefined);
    assert.equal(call.headers['mcp-protocol-version'], PROTOCOL_VERSION);
  }
  assert.equal(calls[0].body.params.protocolVersion, PROTOCOL_VERSION);
  assert.deepEqual(calls[1].body.params, { name: 'arca_status', arguments: {} });
});

test('token is read again between initialize and tools/call, and on later calls', async (t) => {
  const { config } = await sandbox(t);
  const nextToken = 'arca_rotated_test_bearer';
  const calls = await fakeArca(t, config, undefined, async () => {
    await writeFile(`${config.viewerTokenPath}.new`, nextToken, { mode: 0o400 });
    await rename(`${config.viewerTokenPath}.new`, config.viewerTokenPath);
  });
  const client = createArcaClient(config);
  await client.call('arca_status');
  await client.call('arca_status');
  assert.deepEqual(calls.map((call) => call.headers.authorization), [
    `Bearer ${sentinel}`, `Bearer ${nextToken}`, `Bearer ${nextToken}`, `Bearer ${nextToken}`,
  ]);
  await rm(config.viewerTokenPath);
  await assert.rejects(client.call('arca_status'), /regular 0400/);
  assert.equal(calls.length, 4);
});

test('all six fixtures become one snapshot with project-qualified instances', async (t) => {
  const { config } = await sandbox(t);
  const calls = await fakeArca(t, config);
  const snapshot = await collectSnapshot(createArcaClient(config));
  assert.deepEqual(Object.keys(snapshot), ['at', 'error', 'approvalUrl', 'machine', 'instances', 'databases', 'metrics', 'alerts', 'plans', 'phones', 'desktops']);
  assert.ok(Number.isFinite(Date.parse(snapshot.at)));
  assert.equal(snapshot.error, null);
  assert.equal(snapshot.approvalUrl, '');
  assert.deepEqual(snapshot.machine, fixtures.arca_status);
  assert.deepEqual(snapshot.instances, fixtures.watch_report.projects.flatMap(({ project, instances }) => instances.map((instance) => ({ ...instance, project }))));
  assert.deepEqual(snapshot.databases, fixtures.postgres_list.databases);
  assert.deepEqual(snapshot.metrics, fixtures.node_metrics);
  assert.deepEqual(snapshot.alerts, fixtures.alerts_list);
  assert.deepEqual(snapshot.plans, fixtures.plan_list.plans);
  assert.equal(calls.length, 12);
  assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
});

test('partial failures keep successful data and discard upstream error details', async (t) => {
  const { config } = await sandbox(t);
  await fakeArca(t, config, (body) => body.params.name === 'node_metrics'
    ? { jsonrpc: '2.0', id: body.id, error: { code: -32000, message: sentinel } }
    : toolResult(body, fixtures[body.params.name]));
  const snapshot = await collectSnapshot(createArcaClient(config));
  assert.match(snapshot.error, /node_metrics: Arca could not complete/);
  assert.equal(snapshot.metrics, null);
  assert.deepEqual(snapshot.machine, fixtures.arca_status);
  assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
});

for (const mode of [0o440, 0o600, 0o4000, 0o444, 0o401]) {
  test(`refuses token mode ${mode.toString(8)} before any HTTP request`, async (t) => {
    const { config } = await sandbox(t);
    const calls = await fakeArca(t, config);
    await chmod(config.viewerTokenPath, mode);
    const snapshot = await collectSnapshot(createArcaClient(config));
    assert.match(snapshot.error, /regular 0400/);
    assert.equal(calls.length, 0);
    assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
  });
}

test('missing file, directory and symlink token paths are refused', async (t) => {
  const { dir, config } = await sandbox(t);
  const link = join(dir, 'viewer-link');
  await symlink(config.viewerTokenPath, link);
  for (const viewerTokenPath of [join(dir, 'missing'), dir, link]) {
    await assert.rejects(createArcaClient({ ...config, viewerTokenPath }).call('arca_status'), /regular 0400/);
  }
});

test('missing socket becomes an empty snapshot with an error', async (t) => {
  const { config } = await sandbox(t);
  const snapshot = await collectSnapshot(createArcaClient(config));
  assert.match(snapshot.error, /socket is unavailable/);
  assert.equal(snapshot.machine, null);
  assert.equal(snapshot.metrics, null);
  for (const key of ['instances', 'databases', 'alerts', 'plans']) assert.deepEqual(snapshot[key], []);
});

for (const [label, respond, message] of [
  ['401', () => ({ status: 401, message: sentinel }), /\(401\)/],
  ['403', () => ({ status: 403, message: sentinel }), /\(403\)/],
  ['malformed JSON', () => `${sentinel}{`, /malformed JSON/],
  ['wrong JSON-RPC id', (body) => ({ ...toolResult(body, {}), id: -1 }), /invalid MCP/],
  ['MCP tool error', (body) => ({ jsonrpc: '2.0', id: body.id, result: { isError: true, content: [{ type: 'text', text: sentinel }] } }), /could not complete/],
]) {
  test(`${label} becomes a safe snapshot error without logs`, async (t) => {
    const { config } = await sandbox(t);
    const output = [];
    for (const method of ['log', 'warn', 'error', 'info', 'debug']) {
      t.mock.method(console, method, (...args) => output.push(inspect(args)));
    }
    await fakeArca(t, config, respond);
    const client = createArcaClient(config);
    await assert.rejects(client.call('arca_status'), (error) => {
      assert.match(error.message, message);
      assert.equal(inspect(error).includes(sentinel), false);
      return true;
    });
    const snapshot = await collectSnapshot(client);
    assert.match(snapshot.error, message);
    assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
    assert.deepEqual(output, []);
  });
}

test('redacts bearer echoes in structured content, text content and object keys', async (t) => {
  const { config } = await sandbox(t);
  await fakeArca(t, config, (body) => {
    const value = { ...fixtures[body.params.name], [sentinel]: sentinel, note: `prefix ${sentinel} suffix` };
    const reply = toolResult(body, value);
    if (body.params.name === 'arca_status') delete reply.result.structuredContent;
    return reply;
  });
  const snapshot = await collectSnapshot(createArcaClient(config));
  assert.equal(snapshot.error, null);
  assert.match(snapshot.machine.note, /\[redacted\]/);
  assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
});

test('cache polls repeatedly, retains successes, and stops cleanly', async () => {
  let count = 0;
  const cache = createSnapshotCache({ async call(tool) { count++; return fixtures[tool]; } }, 10);
  try {
    await until(() => count >= 12 && cache.get().error === null);
    assert.deepEqual(cache.get().machine, fixtures.arca_status);
    cache.stop();
    const stoppedAt = count;
    await new Promise((resolve) => setTimeout(resolve, 35));
    assert.equal(count, stoppedAt);
  } finally {
    cache.stop();
  }
});

function exchange(socketPath, path, method = 'GET') {
  return new Promise((resolve, reject) => {
    const req = request({ socketPath, path, method }, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body }));
    });
    req.on('error', reject);
    req.end();
  });
}

test('Cordis receives row config, serves cached GET routes without a sidebar, and disposes', async (t) => {
  const { dir, config } = await sandbox(t);
  config.approvalUrl = 'https://example.com/approve';
  const calls = await fakeArca(t, config, (body) => toolResult(body,
    body.params.name === 'arca_status' ? { ...fixtures.arca_status, node: '<script>untrusted</script>' } : fixtures[body.params.name]));
  const routes = [];
  const ctx = new Context();
  ctx.provide('webServer', { register(route) {
    routes.push(route);
    return () => routes.splice(routes.indexOf(route), 1);
  } });
  const fiber = await ctx.plugin(plugin, config);
  t.after(() => fiber.dispose());
  assert.equal(routes.length, 1);
  assert.equal(routes[0].path, '/arca-canvas');
  assert.equal(routes[0].kind, 'prefix');
  const socketPath = join(dir, 'web.sock');
  const web = createServer((req, res) => routes[0].handler(req, res));
  await new Promise((resolve) => web.listen(socketPath, resolve));
  t.after(() => new Promise((resolve) => web.close(resolve)));
  await until(() => calls.length >= 12);
  let snapshot;
  for (let attempts = 0; attempts < 100; attempts++) {
    snapshot = await exchange(socketPath, '/arca-canvas/snapshot?cached=1');
    if (JSON.parse(snapshot.body).machine) break;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.equal(snapshot.status, 200);
  assert.equal(JSON.parse(snapshot.body).approvalUrl, config.approvalUrl);
  assert.equal(JSON.parse(snapshot.body).machine.node, '<script>untrusted</script>');
  assert.equal(snapshot.headers['cache-control'], 'no-store');
  const page = await exchange(socketPath, '/arca-canvas');
  assert.equal(page.status, 200);
  assert.match(page.body, /Arca canvas/);
  assert.match(page.body, /src="\/arca-canvas\/page.js"/);
  assert.match(page.headers['content-security-policy'], /script-src 'self'/);
  for (const [asset, type] of [['page.js', 'text/javascript'], ['page.css', 'text/css']]) {
    const result = await exchange(socketPath, `/arca-canvas/${asset}`);
    assert.equal(result.status, 200);
    assert.match(result.headers['content-type'], new RegExp(type));
    assert.equal(result.body, await readFile(new URL(`../${asset}`, import.meta.url), 'utf8'));
    assert.equal((await exchange(socketPath, `/arca-canvas/${asset}`, 'POST')).status, 405);
  }
  assert.equal(page.body.includes('<script>untrusted'), false);
  assert.equal(page.body.includes(sentinel), false);
  assert.equal((await exchange(socketPath, '/arca-canvas/snapshot', 'POST')).status, 405);
  assert.equal((await exchange(socketPath, '/arca-canvas/other')).status, 404);
  await fiber.dispose();
  assert.equal(routes.length, 0);
});

test('bundle patch has one configured row and schema rejects invalid settings', async () => {
  const patch = yaml.load(await readFile(new URL('../cordis.patch.yml', import.meta.url), 'utf8'));
  assert.equal(patch.length, 1);
  assert.equal(patch[0].insert.length, 1);
  const row = patch[0].insert[0];
  assert.equal(row.id, plugin.name);
  assert.equal(row.name, 'arca-canvas-dsh');
  assert.equal(row.failOnStartupError, false);
  assert.deepEqual(plugin.inject, ['webServer']);
  const validate = (config) => plugin.Config['~standard'].validate(config);
  assert.deepEqual(validate(row.config).value, row.config);
  assert.ok(validate({ ...row.config, pollMs: 0 }).issues);
  assert.ok(validate({ ...row.config, socketPath: 'relative.sock' }).issues);
  for (const approvalUrl of ['/approve', 'javascript:alert(1)', 'https://', 'https://user:pass@example.com/approve']) {
    assert.ok(validate({ ...row.config, approvalUrl }).issues);
  }
  assert.equal(validate({ ...row.config, approvalUrl: 'https://example.com/approve' }).value.approvalUrl, 'https://example.com/approve');
  const { pollMs, ...minimal } = row.config;
  assert.equal(validate(minimal).value.pollMs, 5000);
});

test('built client registers a browser-only module without host imports', async () => {
  let registered;
  const source = await readFile(new URL('../client.js', import.meta.url), 'utf8');
  runInNewContext(source, { window: { __ModuleLoader__: { load(value) { registered = value; } } } });
  assert.equal(registered.id, 'arca-canvas-dsh');
  const module = registered.factory((name) => {
    assert.equal(name, 'react');
    return { createElement: (tag, props) => ({ tag, props }) };
  });
  assert.equal(module.name, 'arca-canvas');
  assert.deepEqual(Array.from(module.inject), ['betterSidebar']);
  module.apply({});
  let registrations = 0;
  let disposed = false;
  const ctx = new Context();
  const fiber = await ctx.plugin(module);
  assert.equal(registrations, 0);
  ctx.provide('betterSidebar', { registerTab(tab) {
    registrations++;
    assert.equal(tab.id, 'arca-canvas');
    assert.equal(tab.title, 'Arca canvas');
    const view = tab.component({ visible: true });
    assert.equal(view.tag, 'iframe');
    assert.equal(view.props.src, '/arca-canvas');
    assert.equal(view.props.title, 'Arca canvas');
    return () => { disposed = true; };
  } });
  await until(() => registrations === 1);
  await fiber.dispose();
  assert.equal(disposed, true);
  assert.equal(source.includes('viewerTokenPath'), false);
});
