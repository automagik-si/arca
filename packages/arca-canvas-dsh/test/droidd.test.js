import assert from 'node:assert/strict';
import { chmod, mkdtemp, readFile, rename, rm, symlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { inspect } from 'node:util';
import test from 'node:test';
import { createDroiddClient } from '../src/droidd-client.js';
import { collectSnapshot } from '../src/snapshot.js';

const fixtures = JSON.parse(await readFile(new URL('./droidd-fixtures.json', import.meta.url)));
const arca = JSON.parse(await readFile(new URL('./fixtures.json', import.meta.url)));
const arcaClient = { call: async (tool) => arca[tool] };
const sentinel = 'device_fixture_secret';
const reply = (body, value = fixtures[body.params.name]) => ({ jsonrpc: '2.0', id: body.id, result: {
  content: [{ type: 'text', text: 'Read-only result' }],
  structuredContent: body.params.name === 'droid_list' ? { droids: value }
    : body.params.name === 'desktop_list' ? { desktops: value } : value,
} });
async function fake(t, respond = reply, initialize = async () => {}) {
  const dir = await mkdtemp(join(tmpdir(), 'arca-devices-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const config = { droiddTokenPath: join(dir, 'reader') };
  await writeFile(config.droiddTokenPath, sentinel, { mode: 0o400 });
  const calls = [];
  const server = createServer(async (req, res) => {
    let data = '';
    for await (const chunk of req) data += chunk;
    const body = JSON.parse(data);
    calls.push({ body, headers: req.headers, method: req.method, url: req.url });
    if (body.method === 'initialize') await initialize(config);
    const value = body.method === 'initialize' ? { jsonrpc: '2.0', id: body.id, result: { protocolVersion: '2025-11-25' } } : respond(body);
    res.writeHead(value.status ?? 200, { 'Content-Type': 'application/json' });
    res.end(typeof value === 'string' ? value : JSON.stringify(value));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  config.droiddUrl = `http://127.0.0.1:${server.address().port}/reader-mcp`;
  return { config, calls, dir, server };
}

test('host HTTP MCP initializes each read, decodes keyed lists, and uses only the four reads', async (t) => {
  const { config, calls } = await fake(t);
  const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient(config));
  assert.equal(snapshot.error, null);
  assert.deepEqual(snapshot.machine, arca.arca_status);
  assert.deepEqual(snapshot.phones, [{ name: 'example-phone', state: 'ready', network: 'wired', battery: 100, size: { width: 720, height: 1280 } }]);
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
  await assert.rejects(createDroiddClient(config).call('phone_tap'), /Device reader/);
  assert.equal(calls.length, 8);
});

test('per-request token rotation and upstream echoes never expose either bearer', async (t) => {
  const rotated = 'device_fixture_rotated';
  const { config, calls } = await fake(t, (body) => reply(body, { note: `${sentinel} ${rotated}`, [sentinel]: rotated }), async (config) => {
    await writeFile(`${config.droiddTokenPath}.new`, rotated, { mode: 0o400 });
    await rename(`${config.droiddTokenPath}.new`, config.droiddTokenPath);
  });
  const client = createDroiddClient(config);
  const value = await client.call('droidd_status');
  await client.call('droidd_status');
  assert.deepEqual(calls.map(({ headers }) => headers.authorization), [`Bearer ${sentinel}`, ...Array(3).fill(`Bearer ${rotated}`)]);
  assert.doesNotMatch(JSON.stringify(value), new RegExp(`${sentinel}|${rotated}`));
});

test('JSON text fallback skips untrusted notices and accepts array results', async (t) => {
  const { config } = await fake(t, (body) => ({ jsonrpc: '2.0', id: body.id, result: {
    content: [{ type: 'text', text: 'UNTRUSTED output' }, { type: 'text', text: JSON.stringify(fixtures[body.params.name]) }],
  } }));
  const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient(config));
  assert.equal(snapshot.phones.length, 1);
  assert.equal(snapshot.desktops.length, 1);
});

test('either missing config value skips all device HTTP requests', async (t) => {
  const { config, calls } = await fake(t);
  for (const input of [{}, { ...config, droiddUrl: '' }, { ...config, droiddTokenPath: '' }]) {
    assert.equal(createDroiddClient(input), null);
    const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient(input));
    assert.equal(snapshot.error, null);
    assert.deepEqual(snapshot.phones, []);
    assert.deepEqual(snapshot.desktops, []);
  }
  assert.equal(calls.length, 0);
});

for (const mode of [0o440, 0o600, 0o444, 0o401, 0o4000]) {
  test(`device token mode ${mode.toString(8)} is refused without HTTP or a banner`, async (t) => {
    const { config, calls } = await fake(t);
    await chmod(config.droiddTokenPath, mode);
    const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient(config));
    assert.equal(snapshot.error, null);
    assert.deepEqual(snapshot.phones, []);
    assert.equal(calls.length, 0);
  });
}

test('missing, directory, symlink and malformed token files are refused', async (t) => {
  const { config, dir, calls } = await fake(t);
  await symlink(config.droiddTokenPath, join(dir, 'link'));
  for (const droiddTokenPath of [join(dir, 'missing'), dir, join(dir, 'link')]) {
    await assert.rejects(createDroiddClient({ ...config, droiddTokenPath }).call('droid_list'), /Device reader/);
  }
  await chmod(config.droiddTokenPath, 0o600);
  await writeFile(config.droiddTokenPath, 'invalid bearer');
  await chmod(config.droiddTokenPath, 0o400);
  await assert.rejects(createDroiddClient(config).call('droid_list'), /Device reader/);
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
  ...['desktop_list', 'droidd_status', 'phone_status'].map((tool) => [`failed ${tool}`, (body) => body.params.name === tool ? { jsonrpc: '2.0', id: body.id, result: { isError: true } } : reply(body)]),
]) {
  test(`${label} leaves Arca only, no device error or logs`, async (t) => {
    const output = [];
    for (const method of ['log', 'warn', 'error', 'info', 'debug']) t.mock.method(console, method, (...args) => output.push(inspect(args)));
    const { config } = await fake(t, respond);
    const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient(config));
    assert.equal(snapshot.error, null);
    assert.deepEqual(snapshot.machine, arca.arca_status);
    assert.deepEqual(snapshot.phones, []);
    assert.deepEqual(snapshot.desktops, []);
    assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
    assert.deepEqual(output, []);
  });
}

test('unreachable or invalid configured URL degrades quietly', async (t) => {
  const { config, server } = await fake(t);
  await new Promise((resolve) => server.close(resolve));
  for (const droiddUrl of [config.droiddUrl, 'invalid', 'file:///mcp', 'http://user:pass@example.invalid/mcp']) {
    const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient({ ...config, droiddUrl }));
    assert.equal(snapshot.error, null);
    assert.deepEqual(snapshot.phones, []);
    assert.deepEqual(snapshot.desktops, []);
  }
});

test('stopped phones stay visible without a status read that cannot succeed', async (t) => {
  const { config, calls } = await fake(t, (body) => reply(body, body.params.name === 'droid_list' ? [{ name: 'stopped-phone', state: 'stopped' }] : fixtures[body.params.name]));
  const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient(config));
  assert.equal(snapshot.phones[0].state, 'stopped');
  assert.equal(snapshot.phones[0].network, null);
  assert.equal(calls.some(({ body }) => body.params.name === 'phone_status'), false);
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
  const snapshot = await collectSnapshot(arcaClient, {}, createDroiddClient(config));
  assert.equal(snapshot.error, null);
  assert.equal(snapshot.phones[0].name, '[redacted]');
  assert.equal(snapshot.desktops[0].name, '[redacted]');
  assert.equal(JSON.stringify(snapshot).includes(sentinel), false);
  assert.deepEqual(output, []);
});
