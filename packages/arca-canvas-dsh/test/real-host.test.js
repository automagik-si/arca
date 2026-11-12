import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { cp, mkdir, mkdtemp, readFile, rm } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { delimiter, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

// The canvas on a real DSH. Every other test of this package stands in for the host (its agents, its tool registry,
// its web server, its module loader), so a host that renames what the canvas calls, or a version the canvas's
// engines.dsh range refuses, passes them all and fails in front of a user. This one starts a real harness.
//
// It runs only when ARCA_REAL_DSH_PROOF is the path of a `dsh` program, with `pnpm` beside it or on PATH: the harness
// installs a plugin with pnpm, which fetches the canvas's one dependency from the registry. The harness gets a home of
// its own and an environment of three names, so nothing of the person who runs it, a credential least of all, reaches
// it, and no model is asked.
const dsh = process.env.ARCA_REAL_DSH_PROOF;
const packageDir = fileURLToPath(new URL('..', import.meta.url));
const secret = (text) => text.replace(/token=[A-Za-z0-9_-]+/g, 'token=[secret]');

function run(file, args, options) {
  return new Promise((resolve, reject) => {
    execFile(file, args, { ...options, maxBuffer: 16 << 20 }, (error, stdout, stderr) => {
      if (error) reject(new Error(`dsh ${args.join(' ')} failed (${error.code ?? error.signal}):\n${secret(stdout + stderr)}`));
      else resolve(stdout + stderr);
    });
  });
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

test('a real DSH loads the canvas and serves its page, its snapshot and its chat history', {
  skip: dsh ? false : 'ARCA_REAL_DSH_PROOF is not set to the path of a dsh program',
  timeout: 300_000,
}, async (t) => {
  const scratch = await mkdtemp(join(tmpdir(), 'arca-canvas-real-dsh-'));
  t.after(() => rm(scratch, { recursive: true, force: true }));
  const home = join(scratch, 'home');
  const plugin = join(scratch, 'plugin');
  await mkdir(home);

  // What is installed is what the package ships: its manifest and the files the manifest names, nothing else of
  // this directory.
  const manifest = JSON.parse(await readFile(join(packageDir, 'package.json'), 'utf8'));
  for (const file of ['package.json', ...manifest.files]) {
    await mkdir(dirname(join(plugin, file)), { recursive: true });
    await cp(join(packageDir, file), join(plugin, file));
  }

  const env = {
    HOME: home,
    PATH: [dirname(dsh), dirname(process.execPath), process.env.PATH].join(delimiter),
    DSH_TELEMETRY_DISABLED: '1',
  };
  await run(dsh, ['plugin', '--profile', 'web', 'add', '-w', '--ignore-scripts', `file:${plugin}`], {
    cwd: home, env, timeout: 180_000,
  });

  const port = await freePort();
  const host = spawn(dsh, ['--profile', 'web', '--no-open', '--host', '127.0.0.1', '--port', String(port)], {
    cwd: home, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let said = '';
  let ended = null;
  host.stdout.on('data', (chunk) => { said += chunk; });
  host.stderr.on('data', (chunk) => { said += chunk; });
  host.on('close', (code, signal) => { ended = { code, signal }; });
  t.after(async () => {
    // The harness and whatever it started: its whole process group is told to end, is waited for until none of it
    // is left, and is killed after ten seconds. Nothing of it outlives the test.
    const signal = (name) => { try { process.kill(-host.pid, name); return true; } catch { return false; } };
    signal('SIGTERM');
    const patience = Date.now() + 10_000;
    while (signal(0)) {
      if (Date.now() >= patience) signal('SIGKILL');
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  });

  // The harness says where its web is once it serves.
  const deadline = Date.now() + 120_000;
  let url;
  while (!url) {
    url = /^dsh web: (http\S+)/m.exec(said)?.[1];
    if (url) break;
    assert.equal(ended, null, `the harness ended before it served:\n${secret(said)}`);
    assert.ok(Date.now() < deadline, `the harness did not say where its web is within 120 seconds:\n${secret(said)}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }

  // A plugin the harness will not run is left out with a line, and the web starts all the same: a range that refuses
  // this harness, or a requirement the harness no longer meets, is read here.
  const leftOut = () => assert.doesNotMatch(
    said,
    /skipping profile bundle|did not activate|incompatible with dsh/,
    `the harness left a plugin out:\n${secret(said)}`,
  );
  leftOut();

  // The launch address carries a token and answers with the cookie every later request needs.
  const launch = await fetch(url, { redirect: 'manual' });
  const cookie = launch.headers.getSetCookie().map((line) => line.split(';')[0]).join('; ');
  assert.ok(cookie, `the launch address set no cookie (status ${launch.status})`);
  const get = async (path) => {
    const response = await fetch(`http://127.0.0.1:${port}${path}`, { headers: { Cookie: cookie } });
    return { status: response.status, type: response.headers.get('content-type') ?? '', body: await response.text() };
  };

  const page = await get('/arca-canvas');
  assert.equal(page.status, 200, `the canvas page: ${page.status}\n${secret(said)}`);
  assert.match(page.type, /^text\/html/);

  const snapshot = await get('/arca-canvas/snapshot');
  assert.equal(snapshot.status, 200, `the snapshot: ${snapshot.status}\n${secret(said)}`);
  assert.match(snapshot.type, /^application\/json/);
  assert.equal(typeof JSON.parse(snapshot.body), 'object');

  const history = await get('/arca-canvas/chat/history');
  assert.equal(history.status, 200, `the chat history: ${history.status}\n${secret(said)}`);
  assert.ok(Array.isArray(JSON.parse(history.body).messages), 'the chat history lists its messages');

  leftOut();
});
