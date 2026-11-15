import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseLog } from '../src/logs.js';

const at = (line) => `2026-11-14T10:00:00.0000000Z ${line}`;

test('cache restores: exact hit, partial, miss, and the compression used', () => {
  const log = [
    '##[group]Run actions/cache@abc',
    '  key: deps-linux-111',
    '##[endgroup]',
    'Cache Size: ~48 MB (50544728 B)',
    '[command]/usr/bin/tar -xf /tmp/cache.tzst -P -C /w --use-compress-program unzstd',
    'Cache restored successfully',
    'Cache restored from key: deps-linux-111',
    '##[group]Run actions/cache@abc',
    '  key: build-linux-222',
    'Cache restored from key: build-linux-',
    '##[group]Run actions/cache@abc',
    '  key: other-333',
    'Cache not found for input keys: other-333',
    '##[group]Run oven-sh/setup-bun@def',
    'Cache hit for: bun-xyz',
    'Cache Size: ~10 MB (10000000 B)',
    '[command]/usr/bin/tar -xf /tmp/cache.tgz -P -C /w -z',
  ].map(at).join('\n');
  const { cache } = parseLog(log);
  assert.deepEqual(
    cache.map((c) => [c.outcome, c.compression, c.bytes]),
    [
      ['hit', 'zstd', 50544728],
      ['partial', null, null],
      ['miss', null, null],
      ['hit', 'gzip', 10000000],
    ],
  );
});

test('tests from bun, node, cargo, nextest and go, with failures and slow ones by name', () => {
  const log = [
    '(pass) suite > quick [1.00ms]',
    '(fail) suite > breaks [12.00ms]',
    '(pass) suite > slow one [2500.00ms]',
    '✔ node fast (3.1ms)',
    '✖ node broken (4ms)',
    '✖ failing tests:',
    'test a::b ... ok',
    'test a::c ... FAILED',
    'test a::d ... ignored',
    'test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 3.50s',
    '        PASS [   3.000s] crate tests::heavy',
    '--- FAIL: TestThing (0.50s)',
  ].map(at).join('\n');
  const parsed = parseLog(log);
  assert.equal(parsed.tests.count, 9);
  assert.deepEqual(parsed.tests.failed.sort(), ['TestThing', 'a::c', 'node broken', 'suite > breaks'].sort());
  assert.deepEqual(parsed.tests.slow.map((t) => t.name), ['crate tests::heavy', 'suite > slow one']);
  assert.deepEqual(parsed.suites.map((s) => [s.passed, s.failed, s.ms]), [[1, 1, 3500]]);
});
