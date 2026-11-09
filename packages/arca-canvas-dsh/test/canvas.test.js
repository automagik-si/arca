import assert from 'node:assert/strict';
import { chmod, mkdtemp, readFile, rename, rm, symlink, writeFile } from 'node:fs/promises';
import { createServer, request } from 'node:http';
import { hostname, tmpdir } from 'node:os';
import { join } from 'node:path';
import { inspect } from 'node:util';
import { runInNewContext } from 'node:vm';
import test from 'node:test';
import { Context } from '@deepseek-ai/cordis';
import yaml from 'js-yaml';
import * as plugin from '../index.js';
import { createArcaClient, PROTOCOL_VERSION } from '../src/arca-client.js';
import { buildDashboard } from '../src/model.js';
import { collectSnapshot, createSnapshotCache, emptySnapshot } from '../src/snapshot.js';

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

test('all eight fixtures become one snapshot with project-qualified instances', async (t) => {
  const { config } = await sandbox(t);
  const calls = await fakeArca(t, config);
  const snapshot = await collectSnapshot(createArcaClient(config));
  assert.deepEqual(Object.keys(snapshot), ['at', 'error', 'approvalUrl', 'nodeName', 'machine', 'instances', 'instancesKnown', 'projects', 'databases', 'services', 'servicesKnown', 'metrics', 'alerts', 'plans', 'journal', 'journalKnown', 'journalPending', 'devices', 'devicesAt', 'phones', 'desktops']);
  assert.equal(snapshot.instancesKnown, true);
  assert.deepEqual(snapshot.projects, fixtures.watch_report.projects.map(({ project }) => project));
  assert.ok(Number.isFinite(Date.parse(snapshot.at)));
  assert.equal(snapshot.error, null);
  assert.equal(snapshot.approvalUrl, '');
  assert.deepEqual(snapshot.machine, fixtures.arca_status);
  assert.deepEqual(snapshot.instances, fixtures.watch_report.projects.flatMap(({ project, instances }) => instances.map((instance) => ({ ...instance, project }))));
  assert.deepEqual(snapshot.databases, fixtures.postgres_list.databases);
  assert.deepEqual(snapshot.services, fixtures.service_list.services);
  assert.equal(snapshot.servicesKnown, true);
  assert.deepEqual(snapshot.metrics, fixtures.node_metrics);
  assert.deepEqual(snapshot.alerts, fixtures.alerts_list);
  assert.deepEqual(snapshot.plans, fixtures.plan_list.plans);
  assert.deepEqual(snapshot.journal, fixtures.journal_list.records);
  assert.equal(snapshot.journalKnown, true);
  assert.equal(calls.length, 16);
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
  assert.equal(snapshot.servicesKnown, false);
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

test('journal_list failure leaves no invented records and keeps the host useful', async (t) => {
  const { config } = await sandbox(t);
  await fakeArca(t, config, (body) => body.params.name === 'journal_list'
    ? { jsonrpc: '2.0', id: body.id, error: { code: -32000, message: sentinel } }
    : toolResult(body, fixtures[body.params.name]));
  const snapshot = await collectSnapshot(createArcaClient(config));
  assert.match(snapshot.error, /journal_list: Arca could not complete/);
  assert.deepEqual(snapshot.journal, []);
  assert.equal(snapshot.journalKnown, false);
  assert.deepEqual(snapshot.machine, fixtures.arca_status);
  assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
});

test('journal paging shows the newest records and later polls re-read a trailing window', async () => {
  const records = Array.from({ length: 620 }, (_, index) => ({
    id: index + 1,
    entry: `event-${index + 1}`,
    target: { kind: 'instance', name: `synthetic-${index + 1}` },
    state: 'Applied',
    at_ms: 1794160000000 + index,
  }));
  const calls = [];
  const client = {
    async call(tool, args = {}) {
      assert.equal(tool, 'journal_list');
      calls.push({ ...args });
      const after = args.after ?? 0;
      const limit = args.limit ?? 100;
      return { records: records.filter((record) => record.id > after).slice(0, limit) };
    },
  };
  const state = {};
  const snapshot = await collectSnapshot(client, {}, state);
  assert.equal(snapshot.journalKnown, true);
  assert.equal(snapshot.journalPending, false);
  assert.equal(snapshot.journal.length, 200);
  assert.deepEqual(snapshot.journal.map((record) => record.id), records.slice(-200).map((record) => record.id));
  assert.deepEqual(calls.map((args) => args.after), [0, 500]);

  calls.length = 0;
  records.push({ id: 621, entry: 'event-621', state: 'Applied', at_ms: 1794160000620 });
  const next = await collectSnapshot(client, {}, state);
  // The trailing window starts 200 ids below the cursor, so later states are re-read, not skipped.
  assert.deepEqual(calls.map((args) => args.after), [420]);
  assert.equal(next.journal.length, 200);
  assert.equal(next.journal.at(-1).id, 621);
  assert.deepEqual(next.journal.map((record) => record.id), records.slice(-200).map((record) => record.id));
});

async function latestJournalClient(initial) {
  const journal = [...initial];
  return {
    journal,
    client: {
      async call(tool, args = {}) {
        if (tool !== 'journal_list') {
          if (tool === 'watch_report') return { projects: [] };
          if (tool === 'postgres_list') return { databases: [] };
          if (tool === 'plan_list') return { plans: [] };
          return {};
        }
        assert.equal(tool, 'journal_list');
        // Mirrors handlers.rs: the newest record per id, ids above `after`, ascending, limited.
        const latest = new Map();
        for (const record of journal) latest.set(record.id, record);
        return {
          records: [...latest.values()]
            .sort((a, b) => a.id - b.id)
            .filter((record) => record.id > (args.after ?? 0))
            .slice(0, args.limit ?? 100),
        };
      },
    },
  };
}

test('a state change under an id already seen reaches the panel', async () => {
  const host = await latestJournalClient([{
    id: 7,
    entry: 'create-app',
    target: { kind: 'instance', name: 'app' },
    state: 'Intent',
    at_ms: 1000,
  }]);
  const state = {};
  const first = await collectSnapshot(host.client, {}, state);
  assert.equal(first.journal[0].state, 'Intent');
  assert.equal(buildDashboard({ hosts: [{ name: 'alpha', ...first }] }).journal[0].state, 'Intent');

  host.journal[0] = { ...host.journal[0], state: 'Failed', at_ms: 2000 };
  const failed = await collectSnapshot(host.client, {}, state);
  assert.equal(failed.journal.length, 1, 'the same effect is replaced, not duplicated');
  assert.equal(failed.journal[0].state, 'Failed');
  assert.equal(failed.journal[0].at_ms, 2000);
  assert.deepEqual(
    buildDashboard({ hosts: [{ name: 'alpha', ...failed }] }).journal.map((record) => [record.entry, record.state, record.tone]),
    [['create-app', 'Failed', 'bad']],
  );

  host.journal[0] = { ...host.journal[0], state: 'Confirmed', at_ms: 3000 };
  const confirmed = await collectSnapshot(host.client, {}, state);
  assert.equal(confirmed.journal[0].state, 'Confirmed');
  assert.deepEqual(buildDashboard({ hosts: [{ name: 'alpha', ...confirmed }] }).journal.map((record) => [record.state, record.tone]),
    [['Confirmed', 'ok']]);

  host.journal[0] = { ...host.journal[0], state: 'Reverted', at_ms: 4000 };
  const reverted = await collectSnapshot(host.client, {}, state);
  assert.equal(reverted.journal[0].state, 'Reverted');
  assert.deepEqual(buildDashboard({ hosts: [{ name: 'alpha', ...reverted }] }).journal.map((record) => [record.state, record.tone]),
    [['Reverted', 'neutral']]);
});

test('a journal that restarts from zero is re-read and the stale cache dropped', async () => {
  const host = await latestJournalClient(Array.from({ length: 400 }, (_, index) => ({
    id: index + 1,
    entry: `old-${index + 1}`,
    target: { kind: 'instance', name: `synthetic-${index + 1}` },
    state: 'Applied',
    at_ms: 1000 + index,
  })));
  const state = {};
  const first = await collectSnapshot(host.client, {}, state);
  assert.equal(first.journal.at(-1).id, 400);
  assert.equal(state.journal.cursor, 400);
  assert.equal(first.journal.length, 200);

  // The state directory was wiped and the journal restarted with fewer ids than the cursor: lower ids.
  host.journal.splice(0, host.journal.length, ...Array.from({ length: 250 }, (_, index) => ({
    id: index + 1,
    entry: `second-${index + 1}`,
    target: { kind: 'instance', name: 'fresh' },
    state: 'Applied',
    at_ms: 5000 + index,
  })));
  const lower = await collectSnapshot(host.client, {}, state);
  assert.equal(state.journal.cursor, 250);
  assert.equal(lower.journal.length, 200);
  assert.equal(lower.journal.at(-1).entry, 'second-250');
  assert.equal(JSON.stringify(lower.journal).includes('old-'), false);

  // A journal with nothing above the window is the same reset: empty comes back, so rescan from zero.
  host.journal.splice(0, host.journal.length, {
    id: 1,
    entry: 'fresh-effect',
    target: { kind: 'instance', name: 'app' },
    state: 'Intent',
    at_ms: 9000,
  });
  const reset = await collectSnapshot(host.client, {}, state);
  assert.equal(state.journal.cursor, 1);
  assert.deepEqual(reset.journal.map((record) => [record.id, record.entry, record.state]), [[1, 'fresh-effect', 'Intent']]);
  assert.equal(JSON.stringify(reset).includes('second-'), false);
});

test('the first deep journal read returns bounded and catches up in the background', async () => {
  const records = Array.from({ length: 6000 }, (_, index) => ({
    id: index + 1,
    entry: `event-${index + 1}`,
    state: 'Applied',
    at_ms: 1794160000000 + index,
  }));
  const journalCalls = [];
  const client = {
    async call(tool, args = {}) {
      if (tool !== 'journal_list') {
        if (tool === 'watch_report') return { projects: [] };
        if (tool === 'postgres_list') return { databases: [] };
        if (tool === 'plan_list') return { plans: [] };
        return {};
      }
      journalCalls.push(args.after ?? 0);
      await new Promise((resolve) => setTimeout(resolve, 1));
      return { records: records.filter((record) => record.id > (args.after ?? 0)).slice(0, args.limit ?? 100) };
    },
  };
  const state = {};
  const first = await collectSnapshot(client, {}, state);
  assert.equal(first.journalKnown, true);
  assert.equal(first.journalPending, true, 'the deep read is still running');
  assert.ok(journalCalls.length <= 3, `snapshot waited for ${journalCalls.length} journal pages`);
  assert.equal(first.journal.at(-1).id, 1000, 'only the bounded prefix is visible so far');

  await until(() => state.journal && state.journal.pending === false);
  assert.equal(state.journal.cursor, 6000);
  const next = await collectSnapshot(client, {}, state);
  assert.equal(next.journalPending, false);
  assert.equal(next.journal.length, 200);
  assert.deepEqual(next.journal.map((record) => record.id), records.slice(-200).map((record) => record.id));
});

test('cache polls repeatedly, retains successes, and stops cleanly', async () => {
  let count = 0;
  const cache = createSnapshotCache({ async call(tool) { count++; return fixtures[tool]; } }, 10);
  try {
    await until(() => count >= 16 && cache.get().error === null);
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
  config.nodeName = 'Example machine';
  config.projects = [{ name: 'Example project', domain: 'example.test', members: [{ host: 'Example machine', incusProject: 'default', instance: 'worker' }] }];
  config.defaultProject = 'Example project';
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
  await until(() => calls.length >= 16);
  let snapshot;
  for (let attempts = 0; attempts < 100; attempts++) {
    snapshot = await exchange(socketPath, '/arca-canvas/snapshot?cached=1');
    if (JSON.parse(snapshot.body).hosts[0].machine) break;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.equal(snapshot.status, 200);
  assert.equal(JSON.parse(snapshot.body).approvalUrl, config.approvalUrl);
  assert.equal(JSON.parse(snapshot.body).hosts[0].nodeName, 'Example machine');
  assert.deepEqual(JSON.parse(snapshot.body).projects, config.projects);
  assert.equal(JSON.parse(snapshot.body).defaultProject, 'Example project');
  assert.equal(JSON.parse(snapshot.body).hosts[0].machine.node, '<script>untrusted</script>');
  assert.equal(snapshot.headers['cache-control'], 'no-store');
  const page = await exchange(socketPath, '/arca-canvas');
  assert.equal(page.status, 200);
  assert.match(page.body, /Arca canvas/);
  assert.match(page.body, /src="arca-canvas\/page.js"/);
  assert.match(page.body, /data-arca-base="\/arca-canvas"/);
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
  assert.deepEqual(validate(row.config).value, { ...row.config, address: '', defaultProject: '', chatEnabled: true, chatStatePath: '', dailyTokenBudget: 1000000 });
  assert.ok(validate({ ...row.config, pollMs: 0 }).issues);
  assert.ok(validate({ ...row.config, socketPath: 'relative.sock' }).issues);
  assert.ok(validate({ ...row.config, nodeName: 42 }).issues);
  assert.equal(validate({ ...row.config, nodeName: 'Example machine' }).value.nodeName, 'Example machine');
  assert.equal(validate({ ...row.config, nodeName: '' }).value.nodeName, '');
  assert.equal(validate({ ...row.config, address: '192.0.2.10' }).value.address, '192.0.2.10');
  assert.equal(validate({ ...row.config, address: 'example-host' }).value.address, 'example-host');
  for (const address of ['http://192.0.2.10', '192.0.2.10:8080', '-edge', 'edge-', 'host/path', 'user@host']) {
    assert.ok(validate({ ...row.config, address }).issues, address);
  }
  assert.ok(validate({ ...row.config, chatStatePath: 'relative.json' }).issues);
  assert.ok(validate({ ...row.config, dailyTokenBudget: 0 }).issues);
  assert.ok(validate({ ...row.config, chatEnabled: 'yes' }).issues);
  for (const approvalUrl of ['/approve', 'javascript:alert(1)', 'https://', 'https://user:pass@example.com/approve']) {
    assert.ok(validate({ ...row.config, approvalUrl }).issues);
  }
  assert.equal(validate({ ...row.config, approvalUrl: 'https://example.com/approve' }).value.approvalUrl, 'https://example.com/approve');
  for (const devicesUrl of ['invalid', 'http://public.example/mcp', 'https://user:pass@example.com/mcp']) {
    assert.equal(validate({ ...row.config, devicesUrl, devicesTokenPath: 'relative' }).value.devicesUrl, devicesUrl);
  }
  const { devicesUrl, devicesTokenPath, ...withoutDevices } = row.config;
  assert.equal(validate(withoutDevices).value.devicesUrl, '');
  assert.equal(validate(withoutDevices).value.devicesTokenPath, '');
  const { pollMs, ...minimal } = row.config;
  assert.equal(validate(minimal).value.pollMs, 5000);
});

test('project config validates owner groups, members and the default project', () => {
  const validate = (config) => plugin.Config['~standard'].validate(config);
  const base = {
    hosts: [
      { name: 'alpha', socketPath: '/example/alpha.sock', viewerTokenPath: '/example/alpha.token' },
      { name: 'beta', socketPath: '/example/beta.sock', viewerTokenPath: '/example/beta.token' },
    ],
    projects: [
      { name: 'Studio', domain: 'studio.example', members: [{ host: 'alpha', incusProject: 'plumbing', instance: 'app' }] },
      { name: 'Automation', members: [{ host: 'beta', incusProject: 'sandbox', instance: 'queue' }] },
    ],
    defaultProject: 'Studio',
  };
  const valid = validate(base);
  assert.equal(valid.issues, undefined);
  assert.equal(valid.value.defaultProject, 'Studio');
  assert.equal(valid.value.projects[0].domain, 'studio.example');
  const invalid = [
    { ...base, projects: [] },
    { ...base, projects: [{ ...base.projects[0], name: '  ' }] },
    { ...base, projects: [base.projects[0], { ...base.projects[1], name: 'Studio' }] },
    { ...base, projects: [{ ...base.projects[0], members: [] }] },
    { ...base, projects: [{ ...base.projects[0], members: [{ host: 'missing', incusProject: 'p', instance: 'i' }] }] },
    { ...base, projects: [{ ...base.projects[0], members: [{ host: 'alpha', incusProject: '', instance: 'i' }] }] },
    { ...base, projects: [base.projects[0], { ...base.projects[1], members: [{ ...base.projects[0].members[0] }] }] },
    { ...base, defaultProject: 'Missing' },
    { ...base, projects: undefined },
  ];
  for (const config of invalid) assert.ok(validate(config).issues, 'invalid project config must fail');
  const hostOnly = validate({ ...base, projects: undefined, defaultProject: '' });
  assert.equal(hostOnly.issues, undefined);
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
  const page = await readFile(new URL('../page.js', import.meta.url), 'utf8');
  assert.doesNotMatch(page, /node:os|hostname\s*\(/);
});

test('host snapshot route returns Arca cards within 100ms while device HTTP never answers', async (t) => {
  const { config } = await sandbox(t);
  await fakeArca(t, config);
  let deviceRequests = 0, closed = 0;
  const devices = createServer((req, res) => {
    deviceRequests++;
    req.resume();
    res.on('close', () => { closed++; });
  });
  await new Promise((resolve) => devices.listen(0, '127.0.0.1', resolve));
  t.after(() => { devices.closeAllConnections(); return new Promise((resolve) => devices.close(resolve)); });
  const ctx = new Context();
  let route;
  ctx.provide('webServer', { register(value) { route = value; return () => {}; } });
  const web = createServer((req, res) => route.handler(req, res));
  await new Promise((resolve) => web.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => web.close(resolve)));
  const fiber = await ctx.plugin(plugin, { ...config,
    devicesUrl: `http://127.0.0.1:${devices.address().port}/mcp`, devicesTokenPath: config.viewerTokenPath });
  t.after(() => fiber.dispose());
  const url = `http://127.0.0.1:${web.address().port}/arca-canvas/snapshot`;
  // Measure the route phase alone: the cached snapshot must answer without waiting for Arca or devices.
  const routeStart = performance.now();
  let snapshot = (await (await fetch(url)).json()).hosts[0];
  const routeElapsed = performance.now() - routeStart;
  assert.ok(routeElapsed < 100, `host snapshot route took ${routeElapsed}ms`);
  const start = performance.now();
  for (let attempt = 0; attempt < 20 && !snapshot.machine; attempt++) {
    snapshot = (await (await fetch(url)).json()).hosts[0];
    if (!snapshot.machine) await new Promise((resolve) => setTimeout(resolve, 10));
  }
  const readyElapsed = performance.now() - start;
  assert.deepEqual(snapshot.machine, fixtures.arca_status);
  assert.equal(snapshot.instances.length, 2);
  assert.equal(snapshot.error, null);
  assert.equal(snapshot.devices, 'unavailable');
  await until(() => deviceRequests === 1);
  await new Promise((resolve) => setTimeout(resolve, 130));
  const pollStart = performance.now();
  let next = (await (await fetch(url)).json()).hosts[0];
  assert.ok(performance.now() - pollStart < 100, 'host route must serve cached data promptly');
  for (let attempt = 0; attempt < 20 && next.at === snapshot.at; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 10));
    next = (await (await fetch(url)).json()).hosts[0];
  }
  assert.notEqual(next.at, snapshot.at, 'Arca keeps polling during the pending device phase');
  assert.equal(next.error, null);
  await fiber.dispose();
  await until(() => closed === deviceRequests);
  t.diagnostic(`host route ${routeElapsed.toFixed(1)}ms (<100ms); Arca fixture ready in ${readyElapsed.toFixed(1)}ms; pending device request aborted on disposal`);
});

test('host snapshots resolve a human node name without using the machine id', async () => {
  const client = { async call(tool) { return fixtures[tool]; } };
  for (const [nodeName, expected] of [[undefined, hostname().trim() || 'Machine'], ['', 'Machine'], ['  \t ', 'Machine'], ['  Example machine  ', 'Example machine']]) {
    const snapshot = await collectSnapshot(client, { nodeName });
    assert.equal(snapshot.nodeName, expected);
    assert.equal(emptySnapshot({ nodeName }).nodeName, expected);
    assert.match(snapshot.machine.node, /^[a-f0-9]{64}$/);
  }
});

test('stop aborts a journal catch-up before it reads another page', async () => {
  const records = Array.from({ length: 6000 }, (_, index) => ({
    id: index + 1,
    entry: `synthetic-${index + 1}`,
    state: 'Applied',
    at_ms: 1794160000000 + index,
  }));
  let journalCalls = 0;
  let aborted = false;
  const client = {
    async call(tool, args = {}, signal) {
      if (tool !== 'journal_list') {
        if (tool === 'watch_report') return { projects: [] };
        if (tool === 'postgres_list') return { databases: [] };
        if (tool === 'plan_list') return { plans: [] };
        if (tool === 'service_list') return { services: [] };
        return {};
      }
      journalCalls++;
      if (journalCalls <= 2) {
        return { records: records.filter((record) => record.id > (args.after ?? 0)).slice(0, args.limit ?? 100) };
      }
      await new Promise((resolve, reject) => {
        if (signal?.aborted) {
          aborted = true;
          reject(new Error('aborted'));
          return;
        }
        signal?.addEventListener('abort', () => {
          aborted = true;
          reject(new Error('aborted'));
        }, { once: true });
      });
      return { records: [] };
    },
  };
  const cache = createSnapshotCache(client, 10);
  try {
    await until(() => journalCalls >= 3);
    cache.stop();
    await until(() => aborted);
    const stoppedAt = journalCalls;
    await new Promise((resolve) => setTimeout(resolve, 40));
    assert.equal(journalCalls, stoppedAt, 'no page read starts after dispose');
  } finally {
    cache.stop();
  }
});
