import assert from 'node:assert/strict';
import { test } from 'node:test';
import { billedMinutes, jobCost, runnerOf } from '../src/cost.js';

test('a runner and its price from the job labels', () => {
  assert.deepEqual(runnerOf(['ubuntu-latest']), { runner: 'ubuntu-latest', kind: 'github', perMinute: 0.006, freeForPublic: true });
  assert.equal(runnerOf(['ubuntu-24.04-arm']).perMinute, 0.005);
  assert.equal(runnerOf(['ubuntu-latest-8-cores']).perMinute, 0.022);
  assert.equal(runnerOf(['ubuntu-latest-8-cores']).freeForPublic, false);
  assert.equal(runnerOf(['blacksmith-4vcpu-ubuntu-2404']).perMinute, 0.008);
  assert.equal(runnerOf(['self-hosted', 'linux', 'x64', 'arca']).kind, 'self-hosted');
  assert.equal(runnerOf(['macos-latest']).perMinute, 0.062);
  assert.equal(runnerOf(['something-else']).kind, 'unknown');
});

test('a job is billed by the whole minute, and standard runners are free on a public repository', () => {
  const job = { labels: ['ubuntu-latest'], startedAt: '2026-11-14T10:00:00Z', completedAt: '2026-11-14T10:02:01Z' };
  assert.equal(billedMinutes(job), 3);
  assert.equal(jobCost(job, 'public').billed, 0);
  assert.equal(jobCost(job, 'private').billed, 3 * 0.006);
  assert.equal(billedMinutes({ startedAt: null, completedAt: null }), 0);
});
