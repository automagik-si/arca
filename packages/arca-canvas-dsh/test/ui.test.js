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

test('default selection prefers configured, owner, service, then other Incus projects', () => {
  const dashboard = buildDashboard(fixture);
  assert.equal(dashboard.defaultProject.name, 'Studio');
  assert.equal(projectHash(dashboard.defaultProject), '#project/Studio');
  assert.equal(parseProjectHash('#project/Studio'), 'Studio');
  assert.equal(parseProjectHash('#project/Arca%20tests'), 'Arca tests');
  assert.equal(parseProjectHash('#host/alpha'), null);
  assert.equal(parseProjectHash('#project/%'), null);
  assert.equal(selectProject(dashboard.projects, 'Automation').name, 'Automation');
  assert.equal(selectProject(dashboard.projects, 'missing'), null);
  const configured = buildDashboard({ ...fixture, defaultProject: 'Automation' });
  assert.equal(configured.defaultProject.name, 'Automation');
  const ownerFirst = buildDashboard({ ...fixture, defaultProject: '', projects: fixture.projects });
  assert.equal(ownerFirst.defaultProject.name, 'Studio');
  const fallback = buildDashboard({ ...fixture, defaultProject: '', projects: [] });
  // The standard fixture carries one unlisted service, which outranks the other Incus projects.
  assert.equal(fallback.defaultProject.name, 'Web1');
  assert.equal(fallback.defaultProject.kind, 'service');
});

test('arcak-svc never becomes a project and every unlisted service is first-class', () => {
  const dashboard = buildDashboard({
    hosts: [{
      name: 'alpha',
      instancesKnown: true,
      projects: ['arcak-svc', 'shared'],
      instances: [
        { name: 'web1', project: 'arcak-svc', kind: 'container', status: 'Running' },
        { name: 'nats', project: 'arcak-svc', kind: 'container', status: 'Running' },
        { name: 'worker', project: 'shared', kind: 'container', status: 'Running' },
      ],
    }],
  });
  assert.deepEqual(dashboard.projects.map((entry) => [entry.name, entry.kind]), [
    ['Nats', 'service'], ['Web1', 'service'], ['Shared', 'incus'],
  ]);
  for (const service of dashboard.projects.filter((entry) => entry.kind === 'service')) {
    assert.equal(service.host, 'alpha');
    assert.equal(service.rawName, '');
    assert.equal(service.cards.length, 1);
  }
  assert.equal(dashboard.projects.some((entry) => /arcak-svc|Arca services/i.test(`${entry.name} ${entry.rawName}`)), false);
  assert.equal(dashboard.defaultProject.name, 'Nats');
  assert.equal(dashboard.defaultProject.kind, 'service');
});

test('an owner project may group one service while the rest stay first-class', () => {
  const dashboard = buildDashboard({
    projects: [{ name: 'Messaging', members: [{ host: 'alpha', incusProject: 'arcak-svc', instance: 'nats' }] }],
    defaultProject: 'Messaging',
    hosts: [{
      name: 'alpha',
      instancesKnown: true,
      projects: ['arcak-svc'],
      instances: [
        { name: 'web1', project: 'arcak-svc', kind: 'container', status: 'Running' },
        { name: 'nats', project: 'arcak-svc', kind: 'container', status: 'Running' },
      ],
    }],
  });
  assert.deepEqual(dashboard.projects.map((entry) => entry.name), ['Messaging', 'Web1']);
  assert.equal(project(dashboard, 'Messaging').cards[0].name, 'nats');
  assert.equal(dashboard.projects.filter((entry) => entry.kind === 'service').length, 1);
  assert.equal(dashboard.projects.some((entry) => entry.kind === 'service' && entry.cards.some((card) => card.name === 'nats')), false);
});

test('default selection never picks an internal project', () => {
  const dashboard = buildDashboard({
    hosts: [{
      name: 'alpha',
      instancesKnown: true,
      projects: ['aaa', 'arcak-svc'],
      instances: [
        { name: 'zulu', project: 'arcak-svc', kind: 'container', status: 'Running' },
        { name: 'worker', project: 'aaa', kind: 'container', status: 'Running' },
      ],
    }],
  });
  assert.equal(dashboard.defaultProject.name, 'Zulu');
  assert.equal(dashboard.defaultProject.kind, 'service');
  assert.deepEqual(dashboard.projects.map((entry) => entry.name), ['Zulu', 'Aaa']);
  assert.equal(dashboard.projects.some((entry) => entry.name === 'Arca services' || entry.rawName === 'arcak-svc'), false);
});

test('members on a host whose instance report failed are unknown, not missing', () => {
  const failed = fixture.hosts.map((host) => host.name === 'beta'
    ? { ...host, instancesKnown: false, instances: [] }
    : host);
  const dashboard = buildDashboard({
    ...fixture,
    projects: [{
      name: 'Mixed',
      members: [
        { host: 'alpha', incusProject: 'plumbing', instance: 'app' },
        { host: 'beta', incusProject: 'sandbox', instance: 'api' },
      ],
    }],
    defaultProject: 'Mixed',
    hosts: failed,
  });
  const mixed = project(dashboard, 'Mixed');
  assert.equal(mixed.cards.length, 1);
  assert.equal(mixed.missing, 0);
  assert.equal(mixed.unknown, 1);
  assert.match(mixed.status, /unknown/);
  assert.equal(mixed.status.includes('Nothing live yet'), false);
  assert.equal(mixed.tone, 'warn');

  const only = buildDashboard({
    ...fixture,
    projects: [{ name: 'Beta only', members: [{ host: 'beta', incusProject: 'sandbox', instance: 'api' }] }],
    defaultProject: 'Beta only',
    hosts: failed,
  });
  const beta = project(only, 'Beta only');
  assert.equal(beta.cards.length, 0);
  assert.equal(beta.missing, 0);
  assert.equal(beta.unknown, 1);
  assert.equal(beta.status, 'Host data unavailable');
  assert.equal(beta.tone, 'warn');
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
    ['alpha', true, 4], ['beta', true, 3],
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

test('a journal target without a name never shows the internal project id', () => {
  const [record] = journalFeed([{ name: 'alpha', journal: [{
    id: 3,
    entry: 'create-app',
    target: { kind: 'instance', project: 'arcak-svc' },
    state: 'Intent',
    at_ms: 1,
  }] }]);
  assert.equal(record.target, 'instance in Arca service');
  assert.equal(JSON.stringify(record).includes('arcak-svc'), false);
  const [other] = journalFeed([{ name: 'alpha', journal: [{
    id: 4,
    entry: 'open-volume',
    target: { kind: 'volume', project: 'plumbing' },
    state: 'Applied',
    at_ms: 1,
  }] }]);
  assert.equal(other.target, 'volume in Plumbing');
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
  assert.notEqual(projectLabel('arcak-svc'), 'Arca services');
  assert.equal(instanceKindLabel('virtual-machine'), 'VM');
  assert.equal(instanceKindLabel('container'), 'Container');
  assert.equal(statusTone('Running'), 'ok');
  assert.equal(statusTone('Stopped'), 'neutral');
  assert.equal(statusTone('Applied'), 'ok');
  assert.equal(statusTone('Confirmed'), 'ok');
  assert.equal(statusTone('Failed'), 'bad');
  assert.equal(statusTone('Reverted'), 'neutral');
  assert.equal(statusTone('Intent'), 'warn');
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

test('a service Open link needs a configured host address and a real service port', () => {
  const dashboard = buildDashboard(fixture);
  const web = project(dashboard, 'Web1').cards[0];
  assert.equal(web.url, 'http://192.0.2.10:8080');
  assert.equal(web.service.name, 'web1');
  assert.equal(web.service.ports[0], 8080);
  assert.equal(web.service.url, 'http://192.0.2.10:8080');

  const noAddress = buildDashboard({
    ...fixture,
    hosts: fixture.hosts.map((host) => host.name === 'alpha' ? { ...host, address: '' } : host),
  });
  assert.equal(project(noAddress, 'Web1').cards[0].url, '');

  const noPort = buildDashboard({
    ...fixture,
    hosts: fixture.hosts.map((host) => host.name === 'alpha'
      ? { ...host, services: [{ ...host.services[0], ports: [] }] } : host),
  });
  assert.equal(project(noPort, 'Web1').cards[0].url, '');

  // The container bridge address and the service's published dash never become a URL on their own.
  const noService = buildDashboard({
    ...fixture,
    hosts: fixture.hosts.map((host) => host.name === 'alpha' ? { ...host, services: [] } : host),
  });
  assert.equal(project(noService, 'Web1').cards[0].url, '');
  assert.doesNotMatch(JSON.stringify(project(noService, 'Web1')), /10\.0\.0\.7/);
});

test('a remote host with unknown totals reports cores n/a and memory n/a with reasons', () => {
  const host = structuredClone(fixture.hosts[0]);
  host.metrics.latest.cpu = { cores: 0, usedPct: null, reason: 'the Incus resources read failed' };
  host.metrics.latest.memory = {
    totalBytes: null, availableBytes: null, usedBytes: 2_000_000_000,
    reason: 'the Incus resources read failed',
  };
  const machine = buildDashboard({ ...fixture, hosts: [host] }).hosts[0];
  assert.equal(machine.cpuLabel, 'cores n/a');
  assert.equal(machine.cpu.usedPct, null);
  assert.equal(machine.cpu.reason, 'the Incus resources read failed');
  assert.equal(machine.memoryLabel, 'n/a');
  assert.equal(machine.memory.usedBytes, 2_000_000_000);
  assert.equal(machine.memory.usedPct, null);
  assert.equal(machine.memory.reason, 'the Incus resources read failed');

  const nullLoad = structuredClone(fixture.hosts[0]);
  nullLoad.metrics.latest.cpu = { cores: 8, usedPct: null, reason: null };
  const nullMachine = buildDashboard({ ...fixture, hosts: [nullLoad] }).hosts[0];
  assert.equal(nullMachine.cpuLabel, 'n/a', 'null load with known cores is CPU n/a');
  assert.equal(nullMachine.cpu.usedPct, null);
  assert.equal(cpuLabel(hostCpu(nullLoad)), 'n/a');
  // Real readings are unchanged.
  assert.equal(cpuLabel(hostCpu(fixture.hosts[0])), '2 / 8 cores');
  assert.equal(memoryLabel(hostMemory(fixture.hosts[0])), '4 / 16 GB');
});

test('the host usedBytes figure wins over the total-available derivation', () => {
  const host = structuredClone(fixture.hosts[0]);
  host.metrics.latest.memory = { totalBytes: 16_000_000_000, availableBytes: 12_000_000_000, usedBytes: 5_000_000_000 };
  const machine = buildDashboard({ ...fixture, hosts: [host] }).hosts[0];
  assert.equal(machine.memory.usedBytes, 5_000_000_000);
  assert.equal(machine.memory.usedPct, 31.25);
  assert.equal(machine.memoryLabel, '5 / 16 GB');
});
