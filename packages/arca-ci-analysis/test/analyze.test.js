import assert from 'node:assert/strict';
import { test } from 'node:test';
import { analyze, percentile } from '../src/analyze.js';
import { pickLogs } from '../src/collect.js';
import { prText } from '../src/pr.js';
import { markdown, resultLine } from '../src/report.js';

const WORKFLOW = `name: ci
on:
  push:
  pull_request:
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: make test
`;

const t = (minute) => new Date(Date.UTC(2026, 10, 10, 10, 0) + minute * 60000).toISOString().replace('.000Z', 'Z');

let nextId = 1;
function run({ sha, event = 'pull_request', branch = 'feature', at = 0, until = 10, conclusion = 'success', attempt = 1 }) {
  return { id: nextId++, workflowId: 1, workflow: 'ci', path: '.github/workflows/ci.yml', event, headSha: sha, headBranch: branch, attempt, conclusion, createdAt: t(at), startedAt: t(at), updatedAt: t(until), prs: [] };
}
function job(r, { attempt = 1, conclusion = 'success', start = 0, end = 10, queue = 0, name = 'build' } = {}) {
  return {
    id: nextId++, runId: r.id, attempt, name, workflow: 'ci', path: r.path, headSha: r.headSha, conclusion,
    createdAt: t(start - queue), startedAt: t(start), completedAt: t(end), labels: ['ubuntu-latest'], runnerName: 'r', runnerGroup: 'GitHub Actions',
    steps: [{ name: 'Run make test', number: 2, conclusion, startedAt: t(start + 1), completedAt: t(end) }],
  };
}

function dataset(runs, jobs, logs = {}) {
  return {
    repo: 'acme/app', defaultBranch: 'main', visibility: 'private', generatedAt: t(1000),
    window: { since: t(-10), until: t(1000), days: 7 }, runs, jobs, workflows: { '.github/workflows/ci.yml': WORKFLOW },
    caches: { activeBytes: 0, activeCount: 0, largest: [] }, logs,
  };
}

test('percentile is nearest-rank', () => {
  assert.equal(percentile([5, 1, 3, 2, 4], 50), 3);
  assert.equal(percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 95), 10);
  assert.equal(percentile([], 50), null);
});

test('a job that failed and then passed on a rerun of the same commit is flaky, with its failing test', () => {
  const r = run({ sha: 'aaa' });
  const failed = job(r, { attempt: 1, conclusion: 'failure', start: 0, end: 4 });
  const passed = job(r, { attempt: 2, conclusion: 'success', start: 5, end: 15 });
  const other = run({ sha: 'bbb', at: 100, until: 110 });
  const report = analyze(
    dataset([r, other], [failed, passed, job(other, { start: 100, end: 110 })], {
      [failed.id]: { why: 'flake', cache: [], tests: { count: 3, ms: 0, failed: ['suite > races'], slow: [] }, suites: [] },
    }),
  );
  assert.equal(report.flaky.events, 1);
  assert.equal(report.flaky.jobs[0].name, 'build');
  assert.equal(report.flaky.jobs[0].examples[0].how, 'rerun');
  assert.deepEqual(report.flaky.tests, [{ name: 'suite > races', flakes: 1, jobs: ['build'] }]);
  assert.equal(report.flaky.jobs[0].wastedMinutes, 4);
});

test('p50, p95, queue and cost per job', () => {
  const runs = [];
  const jobs = [];
  for (let k = 0; k < 10; k += 1) {
    const r = run({ sha: `s${k}`, at: k * 100, until: k * 100 + k + 1 });
    runs.push(r);
    jobs.push(job(r, { start: k * 100, end: k * 100 + k + 1, queue: k }));
  }
  const report = analyze(dataset(runs, jobs));
  const row = report.jobs[0];
  assert.equal(row.p50, 5 * 60);
  assert.equal(row.p95, 10 * 60);
  assert.equal(row.queueP95, 9 * 60);
  assert.equal(report.totals.minutes, 55);
  assert.equal(report.totals.billedUsd, 0.33);
});

test('a run that kept going after a newer push to the same branch is waste a concurrency group cancels', () => {
  const old = run({ sha: 'old', at: 0, until: 20 });
  const newer = run({ sha: 'new', at: 5, until: 25 });
  const report = analyze(dataset([old, newer], [job(old, { start: 0, end: 20 }), job(newer, { start: 5, end: 25 })]));
  const finding = report.findings.find((f) => f.kind === 'cancel-superseded');
  assert.ok(finding);
  assert.equal(finding.mechanical, true);
  assert.equal(finding.change.kind, 'add-concurrency');
  assert.match(finding.evidence[0], /^15 runner-min/);
});

test('the same commit run for its push and its pull request is a duplicate, unless the push branch is named on purpose', () => {
  const push = run({ sha: 'x1', event: 'push', at: 0, until: 10 });
  const pr = run({ sha: 'x1', event: 'pull_request', at: 0, until: 10 });
  const report = analyze(dataset([push, pr], [job(push), job(pr)]));
  const finding = report.findings.find((f) => f.kind === 'duplicate-trigger');
  assert.ok(finding);
  assert.deepEqual(finding.change.branches, ['main']);

  const named = dataset([push, pr], [job(push), job(pr)]);
  named.workflows['.github/workflows/ci.yml'] = WORKFLOW.replace('  push:\n', '  push:\n    branches: [feature]\n');
  assert.equal(analyze(named).findings.find((f) => f.kind === 'duplicate-trigger'), undefined);
});

test('a job without timeout-minutes gets one of three times its p95', () => {
  const runs = [];
  const jobs = [];
  for (let k = 0; k < 5; k += 1) {
    const r = run({ sha: `t${k}`, at: k * 100, until: k * 100 + 8 });
    runs.push(r);
    jobs.push(job(r, { start: k * 100, end: k * 100 + 8 }));
  }
  const finding = analyze(dataset(runs, jobs)).findings.find((f) => f.kind === 'missing-timeout');
  assert.deepEqual(finding.change.jobs, [{ id: 'build', minutes: 25 }]);
});

test('short jobs on a billed runner move to the free runner of a public repository, or to the owner\'s pool', () => {
  const runs = [];
  const jobs = [];
  for (let k = 0; k < 3; k += 1) {
    const r = run({ sha: `b${k}`, at: k * 100, until: k * 100 + 1 });
    runs.push(r);
    jobs.push({ ...job(r, { start: k * 100, end: k * 100 + 1 }), labels: ['blacksmith-4vcpu-ubuntu-2404'] });
  }
  const data = dataset(runs, jobs);
  data.workflows['.github/workflows/ci.yml'] = WORKFLOW.replace('runs-on: ubuntu-latest', 'runs-on: blacksmith-4vcpu-ubuntu-2404');
  assert.equal(analyze(data).findings.find((f) => f.kind === 'paid-runner'), undefined, 'a private repository has no free runner');
  data.visibility = 'public';
  const free = analyze(data).findings.find((f) => f.kind === 'paid-runner');
  assert.deepEqual(free.change.jobs, [{ id: 'build', from: 'blacksmith-4vcpu-ubuntu-2404', to: 'ubuntu-latest' }]);
  assert.equal(free.weekly.usd, 0.02);
  data.visibility = 'private';
  const pool = analyze(data, { moveTo: 'acme-4vcpu-ubuntu-2404' }).findings.find((f) => f.kind === 'paid-runner');
  assert.equal(pool.change.jobs[0].to, 'acme-4vcpu-ubuntu-2404');
  assert.match(pool.evidence[0], /^acme-4vcpu-ubuntu-2404 is the owner's own runner pool/);
  assert.equal(pool.evidence.some((line) => /fork/.test(line)), false, 'a private repository has no pull requests from strangers');
});

function publicPoolData(on) {
  const runs = [];
  const jobs = [];
  for (let k = 0; k < 3; k += 1) {
    const r = run({ sha: `f${k}`, at: k * 100, until: k * 100 + 1 });
    runs.push(r);
    jobs.push({ ...job(r, { start: k * 100, end: k * 100 + 1 }), labels: ['blacksmith-4vcpu-ubuntu-2404'] });
  }
  const data = { ...dataset(runs, jobs), visibility: 'public' };
  data.workflows['.github/workflows/ci.yml'] = WORKFLOW
    .replace('runs-on: ubuntu-latest', 'runs-on: blacksmith-4vcpu-ubuntu-2404')
    .replace('  push:\n  pull_request:\n', on.map((event) => `  ${event}:\n`).join(''));
  return data;
}
const FORK_SAFE = "${{ github.event.pull_request && github.event.pull_request.head.repo.full_name != github.repository && 'ubuntu-latest' || 'acme-pool' }}";

test('a move to the owner\'s pool in a public repository warns, whatever starts it, and steers ordinary fork pull requests', () => {
  for (const event of ['pull_request', 'pull_request_target', 'pull_request_review', 'pull_request_review_comment']) {
    const pool = analyze(publicPoolData(['push', event]), { moveTo: 'acme-pool' }).findings.find((f) => f.kind === 'paid-runner');
    assert.deepEqual(pool.change, {
      kind: 'set-runner', path: '.github/workflows/ci.yml', jobs: [{ id: 'build', from: 'blacksmith-4vcpu-ubuntu-2404', to: FORK_SAFE }],
      pool: 'acme-pool', public: true, forks: 'ubuntu-latest',
    }, event);
    assert.ok(pool.evidence.some((line) => /it is not a wall: a fork pull request runs its own copy of the workflow and can name acme-pool itself/.test(line)), event);
    assert.match(pool.suggestion, /a fork pull request can still name acme-pool itself/);
  }
  for (const on of [['push'], ['push', 'issue_comment'], ['workflow_run']]) {
    const pool = analyze(publicPoolData(on), { moveTo: 'acme-pool' }).findings.find((f) => f.kind === 'paid-runner');
    assert.equal(pool.change.jobs[0].to, 'acme-pool', `${on}: no pull request payload to steer by`);
    assert.equal(pool.change.public, true, `${on}`);
    assert.equal(pool.change.forks, undefined, `${on}`);
    assert.ok(pool.evidence.some((line) => /only if the pool's runner group admits this public repository/.test(line)), `${on}`);
    assert.ok(pool.evidence.some((line) => /any fork pull request can add a workflow of its own that names acme-pool/.test(line)), `${on}`);
    assert.ok(pool.evidence.some((line) => /"Require approval for all external contributors" \(Settings, Actions, General\)/.test(line)), `${on}`);
  }
});

test('the pull request text of a public move says the expression is not a wall, and what the owner can do', () => {
  const data = publicPoolData(['push', 'pull_request']);
  const report = analyze(data, { moveTo: 'acme-pool' });
  const finding = report.findings.find((f) => f.kind === 'paid-runner');
  const { body } = prText(report, finding);
  assert.match(body, /\*\*This repository is public\. Read this before merging\.\*\*/);
  assert.match(body, /reach acme-pool only if the pool's runner group admits this public repository/);
  assert.match(body, /"Who can reach what"/);
  assert.match(body, /It is not a wall: a fork pull request runs its own copy of the workflow and can name acme-pool itself\./);
  assert.match(body, /"Require approval for all external contributors" \(Settings, Actions, General\)/);
  const pushOnly = analyze(publicPoolData(['push']), { moveTo: 'acme-pool' });
  const text = prText(pushOnly, pushOnly.findings.find((f) => f.kind === 'paid-runner')).body;
  assert.match(text, /any fork pull request can add a workflow of its own that names acme-pool/);
  assert.doesNotMatch(text, /runs-on expression/);
});

test('--move-to takes one plain label of the owner\'s own pool, and a job on that label is the owner\'s, at no charge', () => {
  for (const bad of ['', 'a b', "x' || 'y", '${{ x }}', 'a\nb', ['a'], '-', '.inf', '1e3', '0x1F', '_pool']) {
    assert.throws(() => analyze(dataset([], []), { moveTo: bad }), /--move-to takes one runner label/, JSON.stringify(bad));
  }
  for (const label of ['linux', 'linux-pool-4vcpu', 'ubuntu-latest', 'blacksmith-4vcpu-ubuntu-2404']) {
    const r = run({ sha: `h-${label}`, until: 5 });
    // The label alone, without `self-hosted`: only --move-to makes the job the owner's.
    const data = dataset([r], [{ ...job(r, { end: 5 }), labels: [label] }]);
    assert.ok(analyze(data).totals.listUsd > 0, `${label} without --move-to is priced as a hosted runner`);
    const report = analyze(data, { moveTo: label });
    assert.equal(report.totals.listUsd, 0, label);
    assert.equal(report.jobs[0].runner, `self-hosted (${label})`, label);
  }
  const moves = publicPoolData(['push']);
  moves.visibility = 'private';
  const linux = analyze(moves, { moveTo: 'linux' }).findings.find((f) => f.kind === 'paid-runner');
  assert.equal(linux.change.jobs[0].to, 'linux');
  assert.ok(linux.evidence.some((line) => /^linux reads by its name like a runner GitHub hosts; .* understated$/.test(line)));
  const acme = analyze(moves, { moveTo: 'acme-pool' }).findings.find((f) => f.kind === 'paid-runner');
  assert.equal(acme.evidence.some((line) => /reads by its name/.test(line)), false);
  const r = run({ sha: 'p0', until: 5 });
  const data = dataset([r], [{ ...job(r, { end: 5 }), labels: ['acme-4vcpu-ubuntu-2404'] }]);
  assert.equal(analyze(data).jobs[0].runner, 'acme-4vcpu-ubuntu-2404', 'without --move-to the label is not known');
  const report = analyze(data, { moveTo: 'acme-4vcpu-ubuntu-2404' });
  assert.equal(report.totals.listUsd, 0);
  assert.equal(report.jobs[0].runner, 'self-hosted (acme-4vcpu-ubuntu-2404)');
});

test('the report renders, and the result line stays under 4096 bytes', () => {
  const r = run({ sha: 'aaa' });
  const report = analyze(dataset([r], [job(r)]));
  assert.match(markdown(report), /^# CI report: acme\/app/);
  const line = resultLine([report, report, report]);
  assert.ok(Buffer.byteLength(line) < 4096);
  assert.equal(JSON.parse(line).ok, true);
});

test('logs are read for flaky failures first, then other failures, then the costliest jobs', () => {
  const r = run({ sha: 'p' });
  const flake = job(r, { attempt: 1, conclusion: 'failure', start: 0, end: 1 });
  const pass = job(r, { attempt: 2, start: 2, end: 30 });
  const r2 = run({ sha: 'q' });
  const broken = job(r2, { conclusion: 'failure', start: 0, end: 1 });
  const picked = pickLogs([r, r2], [flake, pass, broken], { flakeLogs: 5, failureLogs: 5, sampleLogs: 5, samplesPerJob: 1 });
  assert.deepEqual([...picked.entries()], [[flake.id, 'flake'], [broken.id, 'failure'], [pass.id, 'sample']]);
});
