import assert from 'node:assert/strict';
import { chmod, mkdtemp, readFile, rename, rm, symlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { inspect } from 'node:util';
import test from 'node:test';
import { createDevicesClient, validateDevicesUrl } from '../src/devices-client.js';
import { collectSnapshot, collectDevices, createSnapshotCache } from '../src/snapshot.js';

const fixtures = JSON.parse(await readFile(new URL('./devices-fixtures.json', import.meta.url)));
const arca = JSON.parse(await readFile(new URL('./fixtures.json', import.meta.url)));
const arcaClient = { call: async (tool) => arca[tool] };
async function snapshotWithDevices(arcaClient, config, devices) {
  const [snapshot, { setting, ...state }] = await Promise.all([collectSnapshot(arcaClient, config), collectDevices(devices, AbortSignal.timeout(1000))]);
  return { ...snapshot, ...state };
}
const sentinel = 'device_fixture_secret';
const reply = (body, value = fixtures[body.params.name]) => ({ jsonrpc: '2.0', id: body.id, result: {
  content: [{ type: 'text', text: 'Read-only result' }],
  structuredContent: body.params.name === 'droid_list' ? { droids: value }
    : body.params.name === 'desktop_list' ? { desktops: value } : value,
} });
async function fake(t, respond = reply, initialize = async () => {}) {
  const dir = await mkdtemp(join(tmpdir(), 'arca-devices-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const config = { devicesTokenPath: join(dir, 'reader') };
  await writeFile(config.devicesTokenPath, sentinel, { mode: 0o400 });
  const calls = [];
  const server = createServer(async (req, res) => {
    let data = '';
    for await (const chunk of req) data += chunk;
    const body = JSON.parse(data);
    calls.push({ body, headers: req.headers, method: req.method, url: req.url });
    if (body.method === 'initialize') await initialize(config);
    const value = body.method === 'initialize' ? { jsonrpc: '2.0', id: body.id, result: { protocolVersion: '2025-11-25' } } : await respond(body);
    res.writeHead(value.status ?? 200, { 'Content-Type': 'application/json' });
    res.end(typeof value === 'string' ? value : JSON.stringify(value));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  config.devicesUrl = `http://127.0.0.1:${server.address().port}/reader-mcp`;
  return { config, calls, dir, server };
}

test('host HTTP MCP initializes each read, decodes keyed lists, and uses only the four reads', async (t) => {
  const { config, calls } = await fake(t);
  const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(config));
  assert.equal(snapshot.error, null);
  assert.equal(snapshot.devices, 'ok');
  assert.deepEqual(snapshot.machine, arca.arca_status);
  assert.deepEqual(snapshot.phones, [{ name: 'example-phone', state: 'ready', network: 'wired', battery: 100, size: [720, 1280] }]);
  assert.deepEqual(snapshot.desktops, [{ name: 'example-desktop', status: 'Running', memoryBytes: 4294967296 }]);
  assert.deepEqual(calls.map(({ body }) => body.method), Array(4).fill(['initialize', 'tools/call']).flat());
  assert.deepEqual(calls.filter(({ body }) => body.method === 'tools/call').map(({ body }) => body.params), [
    { name: 'droid_list', arguments: {} }, { name: 'desktop_list', arguments: {} },
    { name: 'droidd_status', arguments: {} }, { name: 'phone_status', arguments: { droid: 'example-phone' } },
  ]);
  for (const call of calls) {
    assert.equal(call.method, 'POST');
    assert.equal(call.url, '/reader-mcp');
    assert.equal(call.headers.origin, undefined);
    assert.equal(call.headers.authorization, `Bearer ${sentinel}`);
    assert.equal(call.headers['mcp-protocol-version'], '2025-11-25');
    assert.match(call.headers.accept, /application\/json.*text\/event-stream/);
  }
  await assert.rejects(createDevicesClient(config).call('phone_tap'), /Device reader/);
  assert.equal(calls.length, 8);
});

test('per-request token rotation and upstream echoes never expose either bearer', async (t) => {
  const rotated = 'device_fixture_rotated';
  const { config, calls } = await fake(t, (body) => reply(body, { note: `${sentinel} ${rotated}`, [sentinel]: rotated }), async (config) => {
    await writeFile(`${config.devicesTokenPath}.new`, rotated, { mode: 0o400 });
    await rename(`${config.devicesTokenPath}.new`, config.devicesTokenPath);
  });
  const client = createDevicesClient(config);
  const value = await client.call('droidd_status');
  await client.call('droidd_status');
  assert.deepEqual(calls.map(({ headers }) => headers.authorization), [`Bearer ${sentinel}`, ...Array(3).fill(`Bearer ${rotated}`)]);
  assert.doesNotMatch(JSON.stringify(value), new RegExp(`${sentinel}|${rotated}`));
});

test('JSON text fallback skips untrusted notices and accepts array results', async (t) => {
  const { config } = await fake(t, (body) => ({ jsonrpc: '2.0', id: body.id, result: {
    content: [{ type: 'text', text: 'UNTRUSTED output' }, { type: 'text', text: JSON.stringify(fixtures[body.params.name]) }],
  } }));
  const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(config));
  assert.equal(snapshot.phones.length, 1);
  assert.equal(snapshot.desktops.length, 1);
});

test('empty config is off; either missing setting is unavailable without HTTP', async (t) => {
  const { config, calls } = await fake(t);
  for (const input of [{}, { ...config, devicesUrl: '' }, { ...config, devicesTokenPath: '' }]) {
    assert.equal(createDevicesClient(input) === null, !input.devicesUrl && !input.devicesTokenPath);
    const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(input));
    assert.equal(snapshot.error, null);
    assert.equal(snapshot.devices, !input.devicesUrl && !input.devicesTokenPath ? 'off' : 'unavailable');
    assert.deepEqual(snapshot.phones, []);
    assert.deepEqual(snapshot.desktops, []);
  }
  assert.equal(calls.length, 0);
});

for (const mode of [0o440, 0o600, 0o444, 0o401, 0o4000]) {
  test(`device token mode ${mode.toString(8)} is refused without HTTP or a banner`, async (t) => {
    const { config, calls } = await fake(t);
    await chmod(config.devicesTokenPath, mode);
    const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(config));
    assert.equal(snapshot.error, null);
    assert.deepEqual(snapshot.phones, []);
    assert.equal(calls.length, 0);
  });
}

test('missing, directory, symlink and malformed token files are refused', async (t) => {
  const { config, dir, calls } = await fake(t);
  await symlink(config.devicesTokenPath, join(dir, 'link'));
  for (const devicesTokenPath of [join(dir, 'missing'), dir, join(dir, 'link')]) {
    await assert.rejects(createDevicesClient({ ...config, devicesTokenPath }).call('droid_list'), /Device reader/);
  }
  await chmod(config.devicesTokenPath, 0o600);
  await writeFile(config.devicesTokenPath, 'invalid bearer');
  await chmod(config.devicesTokenPath, 0o400);
  await assert.rejects(createDevicesClient(config).call('droid_list'), /Device reader/);
  assert.equal(calls.length, 0);
});

for (const [label, respond] of [
  ['401', () => ({ status: 401, message: sentinel })],
  ['403', () => ({ status: 403, message: sentinel })],
  ['redirect', () => ({ status: 302 })],
  ['malformed JSON', () => `${sentinel}{`],
  ['RPC error', (body) => ({ jsonrpc: '2.0', id: body.id, error: { message: sentinel } })],
  ['tool error', (body) => ({ jsonrpc: '2.0', id: body.id, result: { isError: true, content: [{ type: 'text', text: sentinel }] } })],
  ['wrong id', (body) => ({ ...reply(body), id: -1 })],
  ['bad shape', (body) => reply(body, null)],
  ['malformed desktop row', (body) => reply(body, body.params.name === 'desktop_list' ? [null] : fixtures[body.params.name])],
  ...['desktop_list', 'droidd_status'].map((tool) => [`failed ${tool}`, (body) => body.params.name === tool ? { jsonrpc: '2.0', id: body.id, result: { isError: true } } : reply(body)]),
]) {
  test(`${label} leaves Arca only with unavailable state, no error banner or upstream logs`, async (t) => {
    const output = [];
    for (const method of ['log', 'warn', 'error', 'info', 'debug']) t.mock.method(console, method, (...args) => output.push(inspect(args)));
    const { config } = await fake(t, respond);
    const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(config));
    assert.equal(snapshot.error, null);
    assert.deepEqual(snapshot.machine, arca.arca_status);
    assert.equal(snapshot.devices, 'unavailable');
    assert.deepEqual(snapshot.phones, []);
    assert.deepEqual(snapshot.desktops, []);
    assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
    assert.deepEqual(output, []);
  });
}

test('unreachable or invalid configured URL degrades quietly', async (t) => {
  const { config, server } = await fake(t);
  await new Promise((resolve) => server.close(resolve));
  for (const devicesUrl of [config.devicesUrl, 'invalid', 'file:///mcp', 'http://user:pass@example.invalid/mcp']) {
    const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient({ ...config, devicesUrl }));
    assert.equal(snapshot.error, null);
    assert.deepEqual(snapshot.phones, []);
    assert.deepEqual(snapshot.desktops, []);
  }
});

test('status reads are not skipped from droid_list state alone', async (t) => {
  const { config, calls } = await fake(t, (body) => reply(body, body.params.name === 'droid_list' ? [{ name: 'stopped-phone', state: 'stopped' }] : fixtures[body.params.name]));
  const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(config));
  assert.equal(snapshot.phones[0].state, 'stopped');
  assert.equal(snapshot.phones[0].network, 'wired');
  assert.equal(calls.some(({ body }) => body.params.name === 'phone_status'), true);
});


test('bearer echoes in successful structured and JSON-text device data never enter the snapshot or logs', async (t) => {
  const output = [];
  for (const method of ['log', 'warn', 'error', 'info', 'debug']) t.mock.method(console, method, (...args) => output.push(inspect(args)));
  const { config } = await fake(t, (body) => {
    const tool = body.params.name;
    const value = tool === 'droid_list' ? [{ ...fixtures.droid_list[0], name: sentinel }]
      : tool === 'desktop_list' ? [{ ...fixtures.desktop_list[0], name: sentinel }]
      : { ...fixtures[tool], [sentinel]: sentinel };
    return tool === 'desktop_list' ? { jsonrpc: '2.0', id: body.id, result: {
      content: [{ type: 'text', text: 'UNTRUSTED output' }, { type: 'text', text: JSON.stringify(value) }],
    } } : reply(body, value);
  });
  const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(config));
  assert.equal(snapshot.error, null);
  assert.equal(snapshot.phones[0].name, '[redacted]');
  assert.equal(snapshot.desktops[0].name, '[redacted]');
  assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
  assert.deepEqual(output, []);
});

const allowedUrls = [
  'https://example.com/mcp', 'https://8.8.8.8/mcp', 'https://[2001:db8::1]/mcp',
  'https://localhost/mcp', 'http://127.0.0.1/mcp', 'http://127.255.255.255/mcp',
  'http://10.0.0.0/mcp', 'http://10.255.255.255/mcp',
  'http://172.16.0.0/mcp', 'http://172.31.255.255/mcp',
  'http://192.168.0.0/mcp', 'http://192.168.255.255/mcp',
  'http://100.64.0.0/mcp', 'http://100.127.255.255/mcp',
  'http://[::1]:8080/mcp', 'http://[0:0:0:0:0:0:0:1]/mcp',
  'http://[fd00::1]/mcp', 'http://[fdff:ffff::1]/mcp',
];
const refusedUrls = [
  '', 'invalid', '/mcp', '//example.com/mcp', 'https://', 'file:///mcp',
  'http://localhost/mcp', 'http://example.com/mcp', 'http://8.8.8.8/mcp',
  'http://126.255.255.255/mcp', 'http://128.0.0.0/mcp',
  'http://9.255.255.255/mcp', 'http://11.0.0.0/mcp',
  'http://172.15.255.255/mcp', 'http://172.32.0.0/mcp',
  'http://192.167.255.255/mcp', 'http://192.169.0.0/mcp',
  'http://100.63.255.255/mcp', 'http://100.128.0.0/mcp',
  'http://[fcff::1]/mcp', 'http://[fe00::1]/mcp', 'http://[2001:db8::1]/mcp',
  'http://[::]/mcp', 'http://[::ffff:127.0.0.1]/mcp',
  'http://user:pass@127.0.0.1/mcp', 'https://user:pass@example.com/mcp',
  'https://user@example.com/mcp', 'http://127.0.0.1/mcp#fragment', 'https://example.com/mcp#',
  'http://2130706433/mcp', 'http://0x7f000001/mcp', 'http://127.1/mcp',
  'https:example.com/mcp', 'http://127.0.0.1\\@example.com/mcp',
];
for (const url of allowedUrls) test(`URL permits ${url}`, () => assert.ok(validateDevicesUrl(url)));
for (const url of refusedUrls) test(`URL refuses ${url}`, () => assert.equal(validateDevicesUrl(url), null));

async function until(predicate, timeout = 1000) {
  const deadline = performance.now() + timeout;
  while (!predicate()) {
    assert.ok(performance.now() < deadline, 'timed out waiting for device phase');
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
}

test('refused settings preserve Arca cards, send zero requests and log only the setting once', async (t) => {
  const { config, calls } = await fake(t);
  for (const [setting, value] of [
    ...refusedUrls.map((url) => ['devicesUrl', url]),
    ['devicesTokenPath', 'relative-reader-secret'], ['devicesTokenPath', ''],
  ]) {
    const logs = [];
    const cache = createSnapshotCache(arcaClient, 5, { log: (line) => logs.push(line) }, createDevicesClient({ ...config, [setting]: value }));
    try {
      await until(() => logs.length === 1 && cache.get().machine);
      await new Promise((resolve) => setTimeout(resolve, 20));
      assert.equal(cache.get().devices, 'unavailable');
      assert.equal(cache.get().error, null);
      assert.deepEqual(cache.get().machine, arca.arca_status);
      assert.equal(cache.get().instances.length, 2);
      assert.deepEqual(cache.get().phones, []);
      assert.deepEqual(logs, [`Arca canvas devices: unavailable (${setting})`]);
      assert.equal(JSON.stringify(logs).includes(sentinel), false);
      if (value) assert.equal(JSON.stringify(logs).includes(value), false);
    } finally { cache.stop(); }
  }
  assert.equal(calls.length, 0);
});

test('one failed phone_status preserves every phone and desktop and marks that phone unreachable', async (t) => {
  const { config } = await fake(t, (body) => {
    if (body.params.name === 'droid_list') return reply(body, [{ name: 'failed-phone', state: 'ready' }, { name: 'other-phone', state: 'ready' }]);
    if (body.params.name === 'phone_status' && body.params.arguments.droid === 'failed-phone') return { jsonrpc: '2.0', id: body.id, result: { isError: true } };
    return reply(body);
  });
  const snapshot = await snapshotWithDevices(arcaClient, {}, createDevicesClient(config));
  assert.equal(snapshot.devices, 'unreachable');
  assert.equal(snapshot.error, null);
  assert.deepEqual(snapshot.phones[0], { name: 'failed-phone', state: 'unreachable', network: null, battery: null, size: null });
  assert.equal(snapshot.phones[1].name, 'other-phone');
  assert.equal(snapshot.phones[1].battery, 100);
  assert.equal(snapshot.desktops.length, 1);
});

test('background phase updates devices independently and logs only transitions, with retry', async (t) => {
  let fail = false;
  const { config, calls } = await fake(t, (body) => fail ? { status: 503 } : reply(body));
  const logs = [];
  const cache = createSnapshotCache(arcaClient, 10, { log: (line) => logs.push(line) }, createDevicesClient(config));
  t.after(() => cache.stop());
  await until(() => cache.get().devices === 'ok');
  const first = calls.length;
  await until(() => calls.length >= first + 16);
  assert.deepEqual(logs, ['Arca canvas devices: ok']);
  fail = true;
  await until(() => cache.get().devices === 'unreachable');
  assert.equal(cache.get().phones.length, 1, 'last good list stays visible');
  const failed = calls.length;
  await until(() => calls.length >= failed + 4);
  assert.deepEqual(logs, ['Arca canvas devices: ok', 'Arca canvas devices: unreachable']);
  fail = false;
  await until(() => cache.get().devices === 'ok');
  assert.deepEqual(logs, ['Arca canvas devices: ok', 'Arca canvas devices: unreachable', 'Arca canvas devices: ok']);
  assert.equal(cache.get().error, null);
});

test('one phase reads at most two phone_status calls and rotates', async (t) => {
  const phones = ['phone-1', 'phone-2', 'phone-3', 'phone-4'].map((name) => ({ name, state: 'ready' }));
  const { config, calls } = await fake(t, (body) => reply(body, body.params.name === 'droid_list' ? phones : fixtures[body.params.name]));
  const client = createDevicesClient(config);
  const state = { lastFailureAt: new Map(), cursor: 0 };
  const statusNames = () => calls.filter(({ body }) => body.params?.name === 'phone_status')
    .map(({ body }) => body.params.arguments.droid);
  const first = await collectDevices(client, AbortSignal.timeout(1000), state);
  assert.equal(first.devices, 'ok');
  assert.deepEqual(statusNames(), ['phone-1', 'phone-2']);
  const second = await collectDevices(client, AbortSignal.timeout(1000), state);
  assert.equal(second.devices, 'ok');
  assert.deepEqual(statusNames(), ['phone-1', 'phone-2', 'phone-3', 'phone-4']);
});

test('a timed-out phone_status holds that phone for at least sixty seconds', async (t) => {
  let clock = 1000;
  const { config, calls } = await fake(t, (body) => body.params.name === 'phone_status'
    ? new Promise((resolve) => setTimeout(() => resolve(reply(body)), 500))
    : reply(body));
  const client = createDevicesClient(config);
  const state = { lastFailureAt: new Map(), cursor: 0, now: () => clock };
  const statusCalls = () => calls.filter(({ body }) => body.params?.name === 'phone_status').length;
  const first = await collectDevices(client, AbortSignal.timeout(100), state);
  assert.equal(first.devices, 'unreachable');
  assert.equal(statusCalls(), 1);
  assert.equal(state.lastFailureAt.has('example-phone'), true);
  assert.equal(state.lastFailureAt.get('example-phone'), clock);

  const second = await collectDevices(client, AbortSignal.timeout(1000), state);
  assert.equal(second.devices, 'unreachable');
  assert.equal(statusCalls(), 1, 'the failed phone is skipped immediately');
  clock += 59_999;
  await collectDevices(client, AbortSignal.timeout(1000), state);
  assert.equal(statusCalls(), 1, 'the phone is skipped just before sixty seconds');
  clock += 1;
  const fourth = await collectDevices(client, AbortSignal.timeout(1000), state);
  assert.equal(statusCalls(), 2, 'the phone is retried at sixty seconds');
  assert.equal(fourth.devices, 'ok');
  assert.equal(state.lastFailureAt.has('example-phone'), false);
});

test('429 keeps the last good device list with its age; a first 429 stays unavailable', async (t) => {
  let clock = 1_000_000;
  let rateLimited = false;
  const { config } = await fake(t, (body) => rateLimited ? { status: 429 } : reply(body));
  const logs = [];
  const cache = createSnapshotCache(arcaClient, 10, { now: () => clock, log: (line) => logs.push(line) },
    createDevicesClient(config));
  t.after(() => cache.stop());
  await until(() => cache.get().devices === 'ok');
  const goodAt = cache.get().devicesAt;
  const goodPhones = cache.get().phones;
  assert.ok(goodAt);
  clock += 5000;
  rateLimited = true;
  await until(() => cache.get().devices === 'stale');
  assert.equal(cache.get().devicesAt, goodAt);
  assert.deepEqual(cache.get().phones, goodPhones);
  assert.equal(cache.get().error, null);
  assert.deepEqual(logs, ['Arca canvas devices: ok', 'Arca canvas devices: stale']);

  const firstLogs = [];
  const first = createSnapshotCache(arcaClient, 10, { now: () => clock, log: (line) => firstLogs.push(line) },
    createDevicesClient(config));
  t.after(() => first.stop());
  await until(() => firstLogs.length === 1);
  assert.equal(first.get().devices, 'unavailable');
  assert.equal(first.get().devicesAt, null);
  assert.deepEqual(first.get().phones, []);
  assert.deepEqual(first.get().desktops, []);
});

test('a device phase is single-flight while its first list read is pending', async (t) => {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const { config, calls } = await fake(t, (body) => body.params.name === 'droid_list'
    ? pending.then(() => reply(body)) : reply(body));
  const droidLists = () => calls.filter(({ body }) => body.params?.name === 'droid_list').length;
  const cache = createSnapshotCache(arcaClient, 10, { devicesTimeoutMs: 1000 }, createDevicesClient(config));
  t.after(() => cache.stop());
  await until(() => droidLists() === 1);
  await new Promise((resolve) => setTimeout(resolve, 40));
  assert.equal(droidLists(), 1, 'a pending device phase blocks another phase');
  release();
  await until(() => cache.get().devices === 'ok');
  await until(() => droidLists() >= 2);
});

test('never-answering device server cannot block first Arca snapshot or polls and is aborted', async (t) => {
  const { config, server } = await fake(t);
  server.removeAllListeners('request');
  let requests = 0, closed = 0;
  server.on('request', (req, res) => {
    requests++;
    req.resume();
    res.on('close', () => { closed++; });
    // Accept the complete request and never send headers or a body.
  });
  let arcaReads = 0;
  const logs = [];
  const start = performance.now();
  const cache = createSnapshotCache({ async call(tool) { arcaReads++; return arca[tool]; } }, 10,
    { devicesTimeoutMs: 150, log: (line) => logs.push(line) }, createDevicesClient(config));
  t.after(() => cache.stop());
  await until(() => cache.get().machine);
  const elapsed = performance.now() - start;
  assert.ok(elapsed < 100, `first snapshot took ${elapsed}ms`);
  await until(() => arcaReads >= 18 && requests === 1);
  assert.ok(performance.now() - start < 100, 'subsequent Arca polls must not wait for devices');
  assert.equal(cache.get().error, null);
  assert.equal(cache.get().devices, 'unavailable');
  assert.deepEqual(cache.get().phones, []);
  await until(() => closed === 1);
  assert.deepEqual(logs, ['Arca canvas devices: unavailable']);
  await until(() => requests >= 2);
  cache.stop();
  await until(() => closed === requests);
  assert.deepEqual(logs, ['Arca canvas devices: unavailable']);
  t.diagnostic(`first Arca snapshot ${elapsed.toFixed(1)}ms (<100ms); whole device deadline 150ms; all ${closed} requests closed`);
});

test('missing reader token logs its setting once, and recovery changes the state', async (t) => {
  const { config, calls } = await fake(t);
  await rm(config.devicesTokenPath);
  const logs = [];
  const cache = createSnapshotCache(arcaClient, 10, { log: (line) => logs.push(line) }, createDevicesClient(config));
  t.after(() => cache.stop());
  await until(() => logs.length === 1 && cache.get().machine);
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(cache.get().devices, 'unavailable');
  assert.equal(cache.get().error, null);
  assert.equal(calls.length, 0);
  assert.deepEqual(logs, ['Arca canvas devices: unavailable (devicesTokenPath)']);
  await writeFile(config.devicesTokenPath, sentinel, { mode: 0o400 });
  await until(() => cache.get().devices === 'ok');
  assert.deepEqual(logs, ['Arca canvas devices: unavailable (devicesTokenPath)', 'Arca canvas devices: ok']);
});

test('one overall deadline includes initialize and tools/call, aborting the unfinished request', async (t) => {
  const { config, server } = await fake(t);
  server.removeAllListeners('request');
  let requests = 0, closed = 0;
  server.on('request', async (req, res) => {
    requests++;
    let data = '';
    for await (const chunk of req) data += chunk;
    const body = JSON.parse(data);
    const timer = setTimeout(() => res.end(JSON.stringify(body.method === 'initialize'
      ? { jsonrpc: '2.0', id: body.id, result: { protocolVersion: '2025-11-25' } } : reply(body))), 60);
    res.on('close', () => { clearTimeout(timer); closed++; });
  });
  const logs = [];
  const start = performance.now();
  const cache = createSnapshotCache(arcaClient, 10000,
    { devicesTimeoutMs: 100, log: (line) => logs.push(line) }, createDevicesClient(config));
  t.after(() => cache.stop());
  await until(() => logs.length === 1 && closed === 2);
  assert.ok(performance.now() - start < 180, 'deadline must cover both requests together');
  assert.equal(requests, 2);
  assert.equal(cache.get().devices, 'unavailable');
  assert.equal(cache.get().error, null);
});

test('devices start before Arca finishes and their result survives an in-flight Arca poll', async (t) => {
  const { config, calls } = await fake(t);
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const cache = createSnapshotCache({ async call(tool) { await pending; return arca[tool]; } }, 10000,
    { log: () => {} }, createDevicesClient(config));
  t.after(() => cache.stop());
  await until(() => cache.get().devices === 'ok');
  assert.equal(calls.length, 8);
  assert.equal(cache.get().machine, null);
  release();
  await until(() => cache.get().machine);
  assert.equal(cache.get().devices, 'ok');
  assert.equal(cache.get().phones.length, 1);
});
