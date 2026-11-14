import assert from 'node:assert/strict';
import test from 'node:test';
import { ArcaError } from '../src/arca-client.js';
import { ERROR_KIND_TEXT, FLEET_TEXT, buildFleet, compareVersions, frequencyMs, plainAge } from '../src/fleet.js';
import { collectSnapshot } from '../src/snapshot.js';

const NOW = Date.UTC(2026, 10, 14, 12, 0, 0);
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const formatDate = (at) => new Date(at).toISOString().slice(0, 16);

// An Arca OS node's os_update answer, all fields present, current, trusted and automatic.
function state(overrides = {}) {
  return {
    known: true, asOf: NOW - MIN, version: '202611100000', channel: 'stable', checkFrequency: '6h', automatic: true,
    lastCheckAt: NOW - 4 * MIN, lastSuccessAt: NOW - 4 * MIN,
    lastUpdatedVersion: '202611100000', lastUpdatedAt: NOW - 2 * DAY,
    needsReboot: false, pendingVersion: null, previousVersion: '202611010000', goodVersion: '202611100000',
    lastResult: null, lastReason: null, lastErrorKind: null, trust: null,
    ...overrides,
  };
}

const host = (name, osUpdate, osUpdateRead = osUpdate ? 'ok' : 'missing') => ({ name, osUpdate, osUpdateRead });
const fleet = (hosts) => buildFleet(hosts, { now: NOW, formatDate });
const row = (hosts, name) => fleet(hosts).rows.find((entry) => entry.name === name);

test('a current, trusted, automatic node has its cells and no warning', () => {
  const result = fleet([host('alpha', state())]);
  assert.deepEqual(result.rows.map((entry) => entry.name), ['alpha']);
  const [alpha] = result.rows;
  assert.equal(alpha.version, '202611100000');
  assert.equal(alpha.channel, 'stable · every 6h');
  assert.equal(alpha.automatic, 'On');
  assert.equal(alpha.lastCheck, '4 min ago');
  assert.equal(alpha.lastCheckStale, false);
  assert.equal(alpha.lastUpdate, '202611100000 · 2 days ago');
  assert.equal(alpha.lastError, '—');
  assert.equal(alpha.warning, null);
  assert.equal(result.count, '1 of 1 node report update state');
  assert.equal(result.asOf, NOW - MIN);
});

test('a trust failure warns first, with both fingerprints and the count when more than one', () => {
  const trust = { embeddedCaFingerprint: 'AA:BB:CC', presentedCaFingerprint: '11:22:33', failures: 3, escalated: true };
  const failed = row([
    host('alpha', state()),
    host('beta', state({ version: '202611010000', lastErrorKind: 'trust', trust, automatic: false, needsReboot: true })),
  ], 'beta');
  assert.equal(failed.warning.kind, 'trust', 'trust outranks behind, restart and automatic off');
  assert.equal(failed.warning.tone, 'bad');
  assert.equal(failed.warning.text, 'Its update key is not trusted, so it will not update until this is fixed.');
  assert.match(failed.warning.detail, /AA:BB:CC/);
  assert.match(failed.warning.detail, /11:22:33/);
  assert.match(failed.warning.detail, /3 checks in a row/);
  assert.equal(failed.lastError, "the server's key is not trusted");
  const once = row([host('beta', state({ lastErrorKind: 'trust', trust: { ...trust, failures: 1 } }))], 'beta');
  assert.doesNotMatch(once.warning.detail, /in a row/, 'one failure gives no count');
});

test('behind is measured against the newest version on the same channel of the fleet', () => {
  const hosts = [
    host('alpha', state({ version: '202611100000' })),
    host('beta', state({ version: '202611010000' })),
    host('gamma', state({ version: '202609010000', channel: 'testing' })),
  ];
  const beta = row(hosts, 'beta');
  assert.equal(beta.warning.kind, 'behind');
  assert.equal(beta.warning.text, "Behind the fleet's newest version (202611100000) on stable.");
  assert.equal(beta.warning.detail, FLEET_TEXT.behindDetail);
  assert.match(beta.warning.detail, /newest version on your fleet/);
  assert.equal(row(hosts, 'alpha').warning, null, 'the newest node is not behind');
  assert.equal(row(hosts, 'gamma').warning, null, 'a channel of its own is not behind another channel');
});

test('a pending update waiting for a restart replaces the behind warning', () => {
  const hosts = [
    host('alpha', state({ version: '202611100000' })),
    host('beta', state({ version: '202611010000', pendingVersion: '202611100000' })),
    host('gamma', state({ version: '202611100000', needsReboot: true })),
  ];
  assert.equal(row(hosts, 'beta').warning.text, 'An update is ready and waiting for a restart.');
  assert.equal(row(hosts, 'gamma').warning.kind, 'restart');
});

test('automatic updates off warns plainly, and a never frequency is shown as it is', () => {
  const off = row([host('alpha', state({ automatic: false }))], 'alpha');
  assert.equal(off.automatic, 'Off');
  assert.equal(off.warning.text, 'Automatic updates are off on this node.');
  const never = row([host('alpha', state({ checkFrequency: 'never' }))], 'alpha');
  assert.equal(never.channel, 'stable · never');
  assert.equal(never.warning, null, 'never with automatic on is not a warning by itself');
});

test('stale is twice the check frequency, at least one hour', () => {
  const old = row([host('alpha', state({ lastSuccessAt: NOW - 13 * HOUR }))], 'alpha');
  assert.equal(old.lastCheckStale, true);
  assert.equal(old.warning.kind, 'stale');
  assert.equal(old.warning.text, `Has not checked since ${formatDate(NOW - 13 * HOUR)}.`);
  assert.equal(row([host('alpha', state({ lastSuccessAt: NOW - 11 * HOUR }))], 'alpha').warning, null);
  const quick = (ago) => row([host('alpha', state({ checkFrequency: '10m', lastSuccessAt: NOW - ago }))], 'alpha');
  assert.equal(quick(50 * MIN).lastCheckStale, false, 'the one-hour floor');
  assert.equal(quick(61 * MIN).lastCheckStale, true);
  const never = row([host('alpha', state({ lastSuccessAt: null }))], 'alpha');
  assert.equal(never.lastCheck, 'never');
});

test('a node that is not Arca OS, or does not report, is neutral and keeps its name', () => {
  const plain = row([host('plain', state({ known: false, version: null, channel: null }))], 'plain');
  assert.equal(plain.known, false);
  assert.equal(plain.version, 'not an Arca OS node');
  assert.equal(plain.warning.text, 'This node does not report update state.');
  assert.equal(plain.warning.tone, 'neutral');
  const result = fleet([host('alpha', state()), host('plain', state({ known: false }))]);
  assert.equal(result.count, '1 of 2 nodes report update state');
});

test('a failed read keeps the name, says so, and shows empty cells', () => {
  const failed = row([host('beta', null, 'failed')], 'beta');
  assert.equal(failed.name, 'beta');
  assert.equal(failed.warning.text, 'Could not read the update state just now.');
  assert.equal(failed.warning.tone, 'neutral');
  for (const cell of ['version', 'channel', 'automatic', 'lastCheck', 'lastUpdate', 'lastError']) assert.equal(failed[cell], '—');
});

test('every error kind is said in words, and an unknown kind is not invented', () => {
  for (const [kind, words] of Object.entries(ERROR_KIND_TEXT)) {
    assert.equal(row([host('alpha', state({ lastErrorKind: kind }))], 'alpha').lastError, words);
  }
  assert.equal(ERROR_KIND_TEXT.network, 'could not reach the update server');
  assert.equal(row([host('alpha', state({ lastErrorKind: 'weird' }))], 'alpha').lastError, '—');
});

test('rows keep the config order and the fleet line reads the newest read', () => {
  const result = fleet([
    host('zeta', state({ asOf: NOW - 5 * MIN })),
    host('alpha', state({ asOf: NOW - MIN })),
    host('mid', null),
  ]);
  assert.deepEqual(result.rows.map((entry) => entry.name), ['zeta', 'alpha', 'mid']);
  assert.equal(result.asOf, NOW - MIN);
  assert.equal(result.count, '2 of 3 nodes report update state');
});

test('helpers: Go durations, plain ages and version order', () => {
  assert.equal(frequencyMs('6h'), 6 * HOUR);
  assert.equal(frequencyMs('1h30m0s'), 90 * MIN);
  assert.equal(frequencyMs('never'), null);
  assert.equal(frequencyMs('soon'), null);
  assert.equal(plainAge(NOW - 30_000, NOW), 'just now');
  assert.equal(plainAge(NOW - HOUR, NOW), '1 hour ago');
  assert.ok(compareVersions('5.10.0', '5.9.0') > 0);
  assert.ok(compareVersions('202611010000', '202611100000') < 0);
});

// A fake kernel client: each tool answers its value, or throws when the value is an Error.
function client(values) {
  return {
    async call(name) {
      const value = values[name];
      if (value instanceof Error) throw value;
      if (value === undefined) throw new ArcaError('rpc');
      return value;
    },
  };
}

const READS = {
  arca_status: { mode: 'kernel', incusReachable: true },
  watch_report: { projects: [] },
  postgres_list: { databases: [] },
  node_metrics: { latest: null, mean: null },
  alerts_list: [],
  plan_list: { plans: [] },
  service_list: { services: [] },
  journal_list: { records: [] },
};

test('the snapshot carries os_update without touching the host error', async () => {
  const snapshot = await collectSnapshot(client({ ...READS, os_update: state() }));
  assert.equal(snapshot.osUpdateRead, 'ok');
  assert.equal(snapshot.osUpdate.version, '202611100000');
  assert.equal(snapshot.error, null);
});

test('an older kernel without os_update degrades to "does not report update state"', async () => {
  const snapshot = await collectSnapshot(client(READS));
  assert.equal(snapshot.osUpdate, null);
  assert.equal(snapshot.osUpdateRead, 'missing');
  assert.equal(snapshot.error, null, 'a missing verb is never a host error');
  const [old] = buildFleet([{ name: 'old', ...snapshot }], { now: NOW }).rows;
  assert.equal(old.warning.text, 'This node does not report update state.');
  // A snapshot from a canvas without the field at all reads the same.
  const [bare] = buildFleet([{ name: 'bare' }], { now: NOW }).rows;
  assert.equal(bare.warning.text, 'This node does not report update state.');
});

test('a failed os_update read is "could not read just now", never a host error', async () => {
  const snapshot = await collectSnapshot(client({ ...READS, os_update: new ArcaError('unavailable') }));
  assert.equal(snapshot.osUpdateRead, 'failed');
  assert.equal(snapshot.error, null);
  const [down] = buildFleet([{ name: 'down', ...snapshot }], { now: NOW }).rows;
  assert.equal(down.warning.text, 'Could not read the update state just now.');
});
