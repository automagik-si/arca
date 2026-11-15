import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { existsSync, readdirSync } from 'node:fs';
import { zstdDecompressSync } from 'node:zlib';
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { delimiter, dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import test from 'node:test';

// The terminal Doctor on a real DSH: the host's own agent factory, preset registry and tool registry, with the
// shipped doctor.patch.yml. Every other Doctor test stands in for the host. A session reaches the arca-doctor preset
// by being created on it (DSH_TUI_PRESET, a resume) or, while still blank, by a preset switch (/preset arca-doctor),
// which rebinds a live agent without announcing it again. Both must come out fenced exactly like the canvas Doctor.
//
// It needs a `dsh` program: ARCA_REAL_DSH_PROOF, else `dsh` on PATH. No package is installed, no model is asked and
// the harness gets a home of its own and an environment of three names.
const DOCTOR_TOOLS = [
  'alerts_list', 'arca_propose', 'arca_status', 'function_list', 'function_runs', 'journal_list', 'node_metrics',
  'plan_list', 'postgres_list', 'service_list', 'snapshot_list', 'storage_list', 'tenant_list', 'tenant_status',
  'tenant_usage', 'watch_report',
];
const DENIAL = 'The Doctor may only read or propose: that tool is not available.';
const packageDir = fileURLToPath(new URL('..', import.meta.url));
const secret = (text) => text.replace(/token=[A-Za-z0-9_-]+/g, 'token=[secret]');

function findDsh() {
  if (process.env.ARCA_REAL_DSH_PROOF) return process.env.ARCA_REAL_DSH_PROOF;
  for (const dir of (process.env.PATH ?? '').split(delimiter)) {
    if (dir && existsSync(join(dir, 'dsh'))) return join(dir, 'dsh');
  }
  return '';
}
const dsh = findDsh();

// The turn/end events in a session's saved file, once the file has them.
async function savedTurnEnds(home, sessionId) {
  const deadline = Date.now() + 20_000;
  for (;;) {
    const file = findFile(home, sessionId, 'session.v4.jsonl.zstd');
    if (file) {
      const ends = zstdFrames(await readFile(file)).toString('utf8').split('\n').filter(Boolean)
        .map((line) => JSON.parse(line)).map((record) => record.event ?? record)
        .filter((event) => event.type === 'turn/end').map((event) => event.data?.reason ?? null);
      if (ends.length) return ends;
    }
    assert.ok(Date.now() < deadline, `no saved turn/end for session ${sessionId}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

// A DSH session file is one zstd frame per append. Node decodes one frame per call, so walk the frames: the frame
// header gives its own length, then each block header its size, until the last block (RFC 8878, section 3.1.1).
function zstdFrames(buffer) {
  const out = [];
  let at = 0;
  while (at < buffer.length) {
    const magic = buffer.readUInt32LE(at);
    if ((magic & 0xfffffff0) === 0x184d2a50) {
      at += 8 + buffer.readUInt32LE(at + 4);
      continue;
    }
    assert.equal(magic, 0xfd2fb528, 'a zstd frame');
    const start = at;
    const descriptor = buffer[at + 4];
    const single = (descriptor >> 5) & 1;
    const sizeField = descriptor >> 6;
    at += 5 + (single ? 0 : 1) + [0, 1, 2, 4][descriptor & 3] + (sizeField === 0 ? single : [1, 2, 4, 8][sizeField]);
    for (let last = 0; !last;) {
      const block = buffer.readUIntLE(at, 3);
      last = block & 1;
      at += 3 + (((block >> 1) & 3) === 1 ? 1 : block >> 3);
    }
    if ((descriptor >> 2) & 1) at += 4;
    out.push(zstdDecompressSync(buffer.subarray(start, at)));
  }
  return Buffer.concat(out);
}

function findFile(dir, sessionId, name) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (!entry.isDirectory() || entry.name === 'node_modules') continue;
    const path = join(dir, entry.name);
    if (entry.name === sessionId && existsSync(join(path, name))) return join(path, name);
    const found = findFile(path, sessionId, name);
    if (found) return found;
  }
  return '';
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

test('on a real DSH, a Doctor session is fenced whether it is created on the preset or switched to it', {
  skip: dsh ? false : 'no dsh program: set ARCA_REAL_DSH_PROOF or put dsh on PATH',
  timeout: 300_000,
}, async (t) => {
  const scratch = await mkdtemp(join(tmpdir(), 'arca-doctor-real-dsh-'));
  t.after(() => rm(scratch, { recursive: true, force: true }));
  const home = join(scratch, 'home');
  await mkdir(home);
  const env = {
    HOME: home,
    PATH: [dirname(dsh), dirname(process.execPath), process.env.PATH].join(delimiter),
    DSH_TELEMETRY_DISABLED: '1',
  };
  // The shipped web profile carries the real preset registry. It is created on first use; the package is linked
  // into it so the preset's child row `arca-canvas-dsh/doctor` resolves as it does in an installed profile.
  execFileSync(dsh, ['--profile', 'web', '--dump-config'], { cwd: home, env, stdio: 'ignore', timeout: 120_000 });
  const modules = join(home, '.dsh', 'profiles', 'web', 'node_modules');
  await mkdir(modules, { recursive: true });
  await symlink(packageDir, join(modules, 'arca-canvas-dsh'));
  const out = join(scratch, 'probe.json');
  const probePatch = join(scratch, 'probe.patch.yml');
  await writeFile(probePatch, [
    '- insert:',
    '    - id: arca-doctor-preset-probe',
    `      name: '${pathToFileURL(join(packageDir, 'test', 'fixtures', 'doctor-preset-probe.mjs')).href}'`,
    '      config:',
    `        out: '${out}'`,
    '',
  ].join('\n'));

  const port = await freePort();
  const host = spawn(dsh, [
    '--profile', 'web', '--patch', join(packageDir, 'doctor.patch.yml'), '--patch', probePatch,
    '--no-open', '--host', '127.0.0.1', '--port', String(port),
  ], { cwd: home, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  let said = '';
  let ended = null;
  host.stdout.on('data', (chunk) => { said += chunk; });
  host.stderr.on('data', (chunk) => { said += chunk; });
  host.on('close', (code, signal) => { ended = { code, signal }; });
  t.after(async () => {
    const signal = (name) => { try { process.kill(-host.pid, name); return true; } catch { return false; } };
    signal('SIGTERM');
    const patience = Date.now() + 10_000;
    while (signal(0)) {
      if (Date.now() >= patience) signal('SIGKILL');
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  });

  const deadline = Date.now() + 180_000;
  while (!existsSync(out)) {
    assert.equal(ended, null, `the harness ended before the probe wrote:\n${secret(said)}`);
    assert.ok(Date.now() < deadline, `the probe wrote nothing within 180 seconds:\n${secret(said)}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  const result = JSON.parse(await readFile(out, 'utf8'));
  t.diagnostic(JSON.stringify(result));
  assert.equal(result.error, undefined, `the probe failed:\n${result.error}\n${secret(said)}`);
  const [created, blank, switched] = result.agents;

  assert.equal(blank.preset, 'standard');
  assert.ok(blank.tools.includes('bash'), 'a standard session has bash before the switch');
  assert.equal(blank.step.includes('arca_propose'), false, 'and no Doctor tool');
  assert.equal(blank.guard.bash, null, 'and may call it');

  for (const agent of [created, switched]) {
    assert.equal(agent.preset, 'arca-doctor', `${agent.how}: runs the Doctor preset`);
    assert.deepEqual(agent.tools, DOCTOR_TOOLS, `${agent.how}: sees exactly the Doctor's tools`);
    assert.deepEqual(agent.step, DOCTOR_TOOLS, `${agent.how}: a model step admits exactly the Doctor's tools`);
    for (const name of ['bash', 'str_replace_editor', 'run_code', 'action_propose']) {
      assert.equal(agent.guard[name], DENIAL, `${agent.how}: ${name} is refused`);
    }
    assert.equal(agent.guard.arca_status, null, `${agent.how}: a read is allowed`);
    assert.equal(agent.guard.arca_propose, null, `${agent.how}: a proposal is allowed`);
    assert.deepEqual(agent.model, result.route, `${agent.how}: runs on the canvas Doctor's model`);
  }
  // A turn a limit stops ends as an aborted turn with the Doctor's cause, live and in the saved session, which is
  // what dsh-tui replays a session's history and its tree entry from. A bare-string cause is written as null there.
  const stopped = result.stopped;
  const aborted = { kind: 'aborted', reason: { kind: 'hook', reason: 'tool-calls' } };
  assert.equal(stopped.counted, 41, 'the turn reached its first step');
  assert.equal(stopped.last, 'This answer stopped at the 40-tool-call limit for one turn.');
  assert.deepEqual(stopped.ends, [aborted], 'the live turn/end');
  const saved = await savedTurnEnds(home, stopped.sessionId);
  assert.deepEqual(saved, [aborted], 'the saved turn/end');

  // The launcher a bot starts it with reads the same profile: with the preset it passes, without it refuses.
  const check = (...patch) => spawnSync(process.execPath, [
    join(packageDir, 'doctor-launch.js'), '--profile', 'web', ...patch, '--check',
  ], { cwd: home, env: { ...env, ARCA_DOCTOR_DSH: dsh }, encoding: 'utf8', timeout: 120_000 });
  const withPreset = check('--patch', join(packageDir, 'doctor.patch.yml'));
  assert.equal(withPreset.status, 0, `the launcher finds the shipped preset:\n${secret(withPreset.stderr)}`);
  const disabledChild = join(scratch, 'disabled-child.patch.yml');
  const shipped = await readFile(join(packageDir, 'doctor.patch.yml'), 'utf8');
  await writeFile(disabledChild, shipped.replace('          - name: arca-canvas-dsh/doctor\n',
    '          - name: arca-canvas-dsh/doctor\n            disabled: true\n'));
  const off = check('--patch', disabledChild);
  assert.equal(off.status, 2, 'the launcher refuses a preset whose Doctor plugin is switched off');
  assert.match(off.stderr, /plugin is switched off, so the Doctor was not started/);
  const without = check();
  assert.equal(without.status, 2, 'the launcher refuses a profile without the preset');
  assert.match(without.stderr, /has no arca-doctor preset switched on, so the Doctor was not started/);
});
