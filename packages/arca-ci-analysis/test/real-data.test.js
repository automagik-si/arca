// The analysis on a cut of a real public repository's history (test/fixtures/genie-cut.json: 34 runs of its CI and
// Rolling PR Maintenance workflows, 14 days to 2026-11-14; branch names, pull request numbers, runner names and steps
// dropped). Each test is a mistake the first real run made.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { analyze, percent } from '../src/analyze.js';
import { markdown } from '../src/report.js';

const data = JSON.parse(readFileSync(new URL('./fixtures/genie-cut.json', import.meta.url), 'utf8'));

test('B5: on the owner\'s pool, long jobs move too; to the free runner, only short ones', () => {
  const pool = analyze(data, { moveTo: 'acme-pool' }).findings.find((f) => f.kind === 'paid-runner' && f.path === '.github/workflows/ci.yml');
  assert.deepEqual(pool.change.jobs.map((job) => job.id), ['secrets-scan', 'action-pins', 'unit', 'e2e', 'quality-gate']);
  assert.equal(pool.title, 'Run 5 job(s) of CI on acme-pool');
  assert.ok(pool.evidence.some((line) => /^unit: p50 4\.\d min/.test(line)), 'the Unit legs take minutes, not seconds');
  assert.ok(pool.evidence.some((line) => /pool's machines should be at least as large as blacksmith-4vcpu-ubuntu-2404/.test(line)));

  const free = analyze(data).findings.find((f) => f.kind === 'paid-runner' && f.path === '.github/workflows/ci.yml');
  assert.equal(free.change.jobs.some((job) => job.id === 'unit'), false, 'a 4-minute job is not moved to the free runner');
  assert.match(free.title, /short job\(s\) of CI on the free GitHub runner/);
  assert.ok(pool.weekly.usd > free.weekly.usd);
});

test('B6: a gate that needs other jobs is not flaky when one of them failed or was cancelled in its attempt', () => {
  const report = analyze(data);
  assert.equal(report.flaky.jobs.some((row) => row.name.startsWith('Quality Gate')), false);
  assert.ok(report.flaky.jobs.some((row) => row.name.startsWith('Unit (')), 'the job the gate reported on is still counted');
});

test('B7: a scheduled job failing then passing on the same commit is not called flaky', () => {
  const report = analyze(data);
  assert.equal(report.flaky.jobs.some((row) => row.name === 'maintain'), false);
  // The same runs, said to be pushes, are flaky: only the schedule makes the difference.
  const pushed = { ...data, runs: data.runs.map((run) => (run.event === 'schedule' ? { ...run, event: 'push' } : run)) };
  assert.ok(analyze(pushed).flaky.jobs.some((row) => row.name === 'maintain'));
});

test('B8: a share above 0 never reads 0%, and one below 1 never reads 100%', () => {
  assert.equal(percent(1 / 224), '<1%');
  assert.equal(percent(0), '0%');
  assert.equal(percent(0.006), '1%');
  assert.equal(percent(223 / 224), '>99%');
  assert.equal(percent(1), '100%');
  assert.equal(percent(null), '-');
  const report = analyze(data);
  assert.doesNotMatch(markdown(report), /\(0%\)/);
});
