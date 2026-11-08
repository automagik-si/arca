import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import {
  buildDashboard, cpuLabel, fleetSummary, hostCpu, hostMemory, instanceKindLabel, journalFeed,
  memoryLabel, ownerLabel, parseProjectHash, projectHash, projectLabel, selectProject, statusTone,
} from '../src/model.js';

const fixture = JSON.parse(await readFile(new URL('./dashboard-fixtures.json', import.meta.url), 'utf8'));
const clone = () => structuredClone(fixture);

function project(dashboard, name) {
  return dashboard.projects.find((entry) => entry.name === name);
}

test('owner members land in their project across hosts', () => {
  const dashboard = buildDashboard(fixture);
  const studio = project(dashboard, 'Studio');
  assert.ok(studio, 'Studio project exists');
  assert.equal(studio.kind, 'owner');
  assert.equal(studio.domain, 'studio.example');
  assert.deepEqual(studio.cards.map((card) => card.name), ['app', 'db', 'api', 'db']);
  assert.deepEqual(studio.cards.map((card) => card.kind), ['instance', 'instance', 'instance', 'database']);
  assert.deepEqual(studio.hosts, ['alpha', 'beta']);
  assert.equal(studio.cards[0].host, 'alpha');
  assert.equal(studio.cards[2].host, 'beta');
  assert.equal(project(dashboard, 'Automation').cards[0].name, 'queue');
  assert.equal(project(dashboard, 'Automation').domain, '');
});

test('an unlisted instance falls back to its Incus project', () => {
  const dashboard = buildDashboard(fixture);
  const sandbox = project(dashboard, 'Sandbox');
  assert.ok(sandbox, 'alpha sandbox renders as a fallback project');
  assert.deepEqual(sandbox.cards.map((card) => card.name), ['worker']);
  assert.equal(sandbox.kind, 'incus');
  assert.equal(sandbox.rawName, 'sandbox');
  assert.equal(sandbox.host, 'alpha');
  assert.equal(project(dashboard, 'Tools').cards[0].name, 'rogue');
});

test('an Incus project fully assigned to owner projects never renders', () => {
  const dashboard = buildDashboard(fixture);
  assert.equal(project(dashboard, 'Plumbing'), undefined);
  assert.equal(dashboard.projects.filter((entry) => entry.kind === 'incus' && entry.rawName === 'plumbing').length, 0);
  // beta/sandbox has only members of owner projects, so only alpha's unlisted worker creates a Sandbox project.
  assert.equal(dashboard.projects.filter((entry) => entry.kind === 'incus' && entry.rawName === 'sandbox').length, 1);
  assert.equal(dashboard.projects.filter((entry) => entry.kind === 'incus' && entry.rawName === 'sandbox')[0].host, 'alpha');
});

test('database cards use only real snapshot fields and never invent rows', () => {
  const studio = project(buildDashboard(fixture), 'Studio');
  const database = studio.cards.find((card) => card.kind === 'database');
  assert.equal(database.name, 'db');
  assert.equal(database.type, 'PostgreSQL');
  assert.equal(database.status, 'ready');
  assert.equal(database.address, '10.0.0.5');
  assert.equal(database.port, 5432);
  assert.equal(database.links.length, 1);
  assert.equal(database.instance, 'db');
  assert.equal(JSON.stringify(studio).includes('orphan'), false, 'unmatched databases do not appear');
  assert.equal(JSON.stringify(studio).includes('orders'), false);
  assert.equal(JSON.stringify(studio).includes('customers'), false);
});

test('instance links appear only for a real absolute URL', () => {
  const studio = project(buildDashboard(fixture), 'Studio');
  const app = studio.cards.find((card) => card.name === 'app');
  const db = studio.cards.find((card) => card.kind === 'instance' && card.name === 'db');
  assert.equal(app.url, 'https://app.example.test');
  assert.equal(db.url, '');
  const dashboard = buildDashboard({
    ...fixture,
    hosts: fixture.hosts.map((host) => ({
      ...host,
      instances: host.instances.map((instance) => ({ ...instance, url: 'javascript:alert(1)' })),
    })),
  });
  assert.equal(project(dashboard, 'Studio').cards[0].url, '');
});

test('default project, hash round trip and unknown selection', () => {
  const dashboard = buildDashboard(fixture);
  assert.equal(dashboard.defaultProject.name, 'Studio');
  assert.equal(projectHash(dashboard.defaultProject), '#project/Studio');
  assert.equal(parseProjectHash('#project/Studio'), 'Studio');
  assert.equal(parseProjectHash('#project/Arca%20tests'), 'Arca tests');
  assert.equal(parseProjectHash('#host/alpha'), null);
  assert.equal(parseProjectHash('#project/%'), null);
  assert.equal(selectProject(dashboard.projects, 'Automation').name, 'Automation');
  assert.equal(selectProject(dashboard.projects, 'missing'), null);
  const alternate = buildDashboard({ ...fixture, defaultProject: 'Automation' });
  assert.equal(alternate.defaultProject.name, 'Automation');
  const fallback = buildDashboard({ ...fixture, defaultProject: '', projects: [] });
  assert.equal(fallback.defaultProject.name, 'Plumbing');
});

test('fleet summary sums real cores and memory and omits absent parts', () => {
  const dashboard = buildDashboard(fixture);
  assert.equal(dashboard.summary, '2 machines · 12 cores · 24 GB');
  assert.equal(cpuLabel(hostCpu(fixture.hosts[0])), '2 / 8 cores');
  assert.equal(memoryLabel(hostMemory(fixture.hosts[0])), '4 / 16 GB');
  assert.equal(cpuLabel(hostCpu(fixture.hosts[1])), '2 / 4 cores');
  assert.equal(memoryLabel(hostMemory(fixture.hosts[1])), '6 / 8 GB');
  const noMetrics = fixture.hosts.map((host) => ({ ...host, metrics: null }));
  assert.equal(fleetSummary(noMetrics), '2 machines');
  const oneMetric = fixture.hosts.map((host, index) => index === 0 ? host : { ...host, metrics: null });
  assert.equal(fleetSummary(oneMetric), '2 machines · 8 cores · 16 GB');
  assert.equal(fleetSummary([{ name: 'alpha' }]), '1 machine');
});

test('machines expose online state, capacity and unknown instance counts', () => {
  const dashboard = buildDashboard(fixture);
  assert.equal(dashboard.hosts.length, 2);
  assert.deepEqual(dashboard.hosts.map((machine) => [machine.name, machine.online, machine.instanceCount]), [
    ['alpha', true, 3], ['beta', true, 3],
  ]);
  const offline = buildDashboard({
    ...fixture,
    hosts: [{ ...fixture.hosts[0], machine: { ...fixture.hosts[0].machine, incusReachable: false }, instancesKnown: false }],
  });
  assert.equal(offline.hosts[0].online, false);
  assert.equal(offline.hosts[0].status, 'Offline');
  assert.equal(offline.hosts[0].instanceCount, null);
});

test('journal cards are newest-first with host, entry, target and state', () => {
  const dashboard = buildDashboard(fixture);
  assert.deepEqual(dashboard.journal.map((record) => [record.entry, record.host, record.target, record.state]), [
    ['rotate-token', 'beta', 'token viewer', 'Applied'],
    ['deploy-app', 'alpha', 'instance app', 'Applied'],
    ['snapshot-api', 'beta', 'instance api', 'Applied'],
    ['backup-db', 'alpha', 'volume db-data', 'Applied'],
  ]);
  assert.equal(journalFeed(fixture.hosts, 2).length, 2);
  assert.deepEqual(journalFeed([{ name: 'alpha', journal: [{ entry: 'deploy-app', target: { kind: 'instance', name: 'app' }, state: 'Failed', at_ms: 1 }] }])[0],
    { key: 'alpha:0', host: 'alpha', entry: 'deploy-app', state: 'Failed', tone: 'bad', target: 'instance app', plugin: '', at: 1 });
});

test('journal and plan data stay absent when a host fails', () => {
  const dashboard = buildDashboard({
    ...fixture,
    hosts: [{ ...fixture.hosts[0], journal: [], journalKnown: false, plans: [] }],
  });
  assert.deepEqual(dashboard.journal, []);
  assert.equal(dashboard.plans, 0);
  assert.equal(dashboard.projects.some((entry) => entry.cards.some((card) => card.name === 'api')), false);
});

test('labels keep product language plain', () => {
  assert.equal(projectLabel('default'), 'Default');
  assert.equal(projectLabel('arcak-adopt-home-assistant'), 'Adopted Home Assistant');
  assert.equal(instanceKindLabel('virtual-machine'), 'VM');
  assert.equal(instanceKindLabel('container'), 'Container');
  assert.equal(statusTone('Running'), 'ok');
  assert.equal(statusTone('Stopped'), 'neutral');
  assert.equal(statusTone('Applied'), 'ok');
  assert.equal(statusTone('Failed'), 'bad');
  assert.equal(ownerLabel({ class: 'kernel' }), 'Managed by Arca');
  assert.equal(ownerLabel({ class: 'foreign' }), 'External');
});

test('committed browser artifacts equal a fresh build and use no external CSS', async () => {
  const { createHash } = await import('node:crypto');
  const { mkdtemp, rm } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  const { fileURLToPath } = await import('node:url');
  const { buildBrowser } = await import('../build.mjs');
  const here = fileURLToPath(new URL('.', import.meta.url));
  const dir = await mkdtemp(join(tmpdir(), 'arca-canvas-build-'));
  try {
    await buildBrowser(dir);
    for (const name of ['client.js', 'page.js', 'page.css']) {
      const fresh = createHash('sha256').update(await readFile(join(dir, name))).digest('hex');
      const committed = createHash('sha256').update(await readFile(join(here, '..', name))).digest('hex');
      assert.equal(committed, fresh, name);
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
  for (const name of ['src/canvas.css', 'page.css']) {
    const css = await readFile(new URL(`../${name}`, import.meta.url), 'utf8');
    assert.doesNotMatch(css, /@import|url\(\s*['"]?https?:/i, name);
  }
});
