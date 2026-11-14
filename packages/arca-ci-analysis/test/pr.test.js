import assert from 'node:assert/strict';
import { test } from 'node:test';
import { applyChange, pickFinding, prText, verifyChange } from '../src/pr.js';
import { parse } from '../src/yaml.js';

const FILE = `name: ci
on:
  push:
  pull_request:

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: stable
      # build it
      - run: cargo test
  web:
    runs-on:
      - ubuntu-latest
    steps:
      - uses: actions/setup-node@v4
      - run: npm ci
`;

function check(change) {
  const after = applyChange(FILE, change);
  assert.equal(verifyChange(FILE, after, change), null);
  return after;
}

test('add-timeout writes timeout-minutes under runs-on, even a block runs-on', () => {
  const after = check({ kind: 'add-timeout', path: 'ci.yml', jobs: [{ id: 'build', minutes: 20 }, { id: 'web', minutes: 10 }] });
  assert.match(after, /runs-on: ubuntu-latest\n {4}timeout-minutes: 20\n/);
  assert.match(after, / {6}- ubuntu-latest\n {4}timeout-minutes: 10\n/);
  assert.equal(after.split('\n').length, FILE.split('\n').length + 2);
});

test('add-concurrency writes a top-level group before jobs', () => {
  const after = check({ kind: 'add-concurrency', path: 'ci.yml', group: '${{ github.workflow }}-${{ github.ref }}', cancel: true });
  const doc = parse(after);
  assert.equal(doc.concurrency['cancel-in-progress'], true);
  assert.equal(Object.keys(doc.jobs).length, 2);
});

test('restrict-push limits a bare push, and turns a flow on: into a block', () => {
  const after = check({ kind: 'restrict-push', path: 'ci.yml', branches: ['main'] });
  assert.match(after, /on:\n {2}push:\n {4}branches: \[main\]\n {2}pull_request:/);
  const flow = 'on: [push, pull_request]\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: y\n';
  const change = { kind: 'restrict-push', branches: ['main', 'dev'] };
  const out = applyChange(flow, change);
  assert.equal(verifyChange(flow, out, change), null);
  assert.deepEqual(parse(out).on, { push: { branches: ['main', 'dev'] }, pull_request: null });
});

test('add-step pins the action and lands after the toolchain step, before a comment of the next', () => {
  const after = check({ kind: 'add-step', path: 'ci.yml', job: 'build', after: 1, action: 'Swatinem/rust-cache', pin: { sha: 'a'.repeat(40), tag: 'v2.8.0' } });
  assert.match(after, /toolchain: stable\n {6}- uses: Swatinem\/rust-cache@a{40} # v2\.8\.0\n {6}# build it\n/);
});

test('add-input writes a with: block when the step has none', () => {
  const after = check({ kind: 'add-input', path: 'ci.yml', job: 'web', step: 0, input: 'cache', value: 'npm' });
  assert.match(after, /- uses: actions\/setup-node@v4\n {8}with:\n {10}cache: npm\n/);
});

test('set-runner changes one plain runs-on value and refuses any other shape', () => {
  const file = FILE.replace('runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout', 'runs-on: blacksmith-4vcpu-ubuntu-2404 # fast\n    steps:\n      - uses: actions/checkout');
  const change = { kind: 'set-runner', path: 'ci.yml', jobs: [{ id: 'build', from: 'blacksmith-4vcpu-ubuntu-2404', to: 'ubuntu-latest' }] };
  const after = applyChange(file, change);
  assert.equal(verifyChange(file, after, change), null);
  assert.match(after, /runs-on: ubuntu-latest # fast\n/);
  assert.throws(() => applyChange(FILE, { ...change, jobs: [{ id: 'web', from: 'ubuntu-latest', to: 'x' }] }), /does not run on/);
});

test('set-runner writes the fork-safe expression plain or quoted, and it reads back as written', () => {
  const to = "${{ github.event.pull_request && github.event.pull_request.head.repo.full_name != github.repository && 'ubuntu-latest' || 'acme-pool' }}";
  const change = { kind: 'set-runner', path: 'ci.yml', jobs: [{ id: 'build', from: 'blacksmith-4vcpu-ubuntu-2404', to }], pool: 'acme-pool', public: true, forks: 'ubuntu-latest' };
  for (const [written, expected] of [
    ['blacksmith-4vcpu-ubuntu-2404', `runs-on: ${to}\n`],
    ['"blacksmith-4vcpu-ubuntu-2404"', `runs-on: "${to}"\n`],
    ["'blacksmith-4vcpu-ubuntu-2404'", `runs-on: "${to}"\n`],
  ]) {
    const file = FILE.replace('runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout', `runs-on: ${written}\n    steps:\n      - uses: actions/checkout`);
    const after = applyChange(file, change);
    assert.equal(verifyChange(file, after, change), null, written);
    assert.ok(after.includes(expected), written);
    assert.equal(parse(after).jobs.build['runs-on'], to);
  }
});

test('the top writable finding is picked, and the ones above it are named', () => {
  const report = {
    repo: 'acme/app',
    findings: [
      { id: 'a', title: 'Queue', mechanical: false, change: { kind: 'capacity' } },
      { id: 'b', title: 'Timeouts', mechanical: true, change: { kind: 'add-timeout', path: 'ci.yml', jobs: [] } },
    ],
  };
  const { finding, skipped } = pickFinding(report);
  assert.equal(finding.id, 'b');
  assert.deepEqual(skipped.map((f) => f.id), ['a']);
  assert.throws(() => pickFinding(report, 'zzz'), /no finding zzz/);
});

test('the pull request text carries the evidence and the window', () => {
  const report = { repo: 'acme/app', window: { since: 's', until: 'u', days: 7 }, totals: { runs: 3, jobs: 9 } };
  const finding = {
    id: 'missing-timeout-1', title: 'Set timeout-minutes on 1 job(s) of ci', suggestion: 'Add timeout-minutes.', evidence: ['build: p95 2 min'],
    weekly: { runnerMinutes: 0, wallMinutes: 0 }, confidence: 'measured', change: { kind: 'add-timeout', path: 'ci.yml', jobs: [{ id: 'build', minutes: 10 }] },
  };
  const { title, body } = prText(report, finding, { skipped: [{ title: 'Queue' }] });
  assert.equal(title, 'ci: set timeout-minutes on 1 job(s) of ci');
  assert.match(body, /- build: p95 2 min/);
  assert.match(body, /acme\/app/);
  assert.match(body, /never merges/);
  assert.match(body, /- Queue/);
});
