import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { approvalUrl } from '../src/approval.js';
import { buildBrowser } from '../build.mjs';
import {
  alertRows, buildTree, kindLabel, ownerLabel, parseRoute, projectLabel, statusTone,
} from '../src/tree.js';

const here = fileURLToPath(new URL('.', import.meta.url));
const fixture = JSON.parse(await readFile(new URL('./tree-fixtures.json', import.meta.url), 'utf8'));

test('tree keeps hosts, Incus projects and empty projects distinct', () => {
  const tree = buildTree(fixture);
  assert.equal(tree.host.label, 'Example machine');
  assert.equal(tree.host.status, '12 projects · 32 instances');
  assert.equal(tree.projects.length, 12);
  assert.equal(tree.projects.reduce((sum, project) => sum + project.children.length, 0), 32);
  assert.equal(tree.projects.find((project) => project.project === 'empty').children.length, 0);
  assert.equal(tree.host.rawName, fixture.machine.node);
  assert.equal(tree.projects.find((project) => project.project === 'alpha').label, 'Alpha');
});

test('phones and desktops join only one exact instance name', () => {
  const tree = buildTree(fixture);
  const alpha = tree.projects.find((project) => project.project === 'alpha');
  const beta = tree.projects.find((project) => project.project === 'beta');
  assert.deepEqual(alpha.devices.map((device) => device.name), ['alpha-node-01']);
  assert.deepEqual(beta.devices.map((device) => device.name), ['beta-node-01']);
  assert.ok(tree.notPlaced.devices.some((device) => device.name === 'worker'));
  assert.ok(tree.notPlaced.devices.some((device) => device.name === 'phone-x'));
});

test('host devices retain each exact match or unmatched reason without changing the snapshot', () => {
  const snapshot = {
    machine: { node: 'example' },
    instances: [
      { name: 'phone', project: 'alpha' }, { name: 'desktop', project: 'beta' },
      { name: 'worker', project: 'alpha' }, { name: 'worker', project: 'beta' },
    ],
    phones: [{ name: 'phone' }, { name: 'worker' }, { name: 'missing' }],
    desktops: [{ name: 'desktop' }],
  };
  const original = structuredClone(snapshot);
  const tree = buildTree(snapshot);
  assert.deepEqual(tree.host.devices.map(({ name, matched, reason }) => ({ name, matched, reason })), [
    { name: 'phone', matched: { project: 'alpha', name: 'phone' }, reason: undefined },
    { name: 'worker', matched: undefined, reason: 'name matches more than one instance' },
    { name: 'missing', matched: undefined, reason: 'no instance with this name' },
    { name: 'desktop', matched: { project: 'beta', name: 'desktop' }, reason: undefined },
  ]);
  assert.deepEqual(tree.notPlaced.devices, tree.host.devices.filter((device) => device.reason));
  assert.deepEqual(snapshot, original);
});

test('a database is placed in one exactly named instance when known', () => {
  const tree = buildTree({
    ...fixture,
    databases: [
      { name: 'alpha-node-01', entry: 'alpha-node-01', status: 'ready' },
      { name: 'unknown-db', entry: 'unknown-db', status: 'ready' },
    ],
  });
  const instance = tree.projects.find((project) => project.project === 'alpha').children
    .find((row) => row.name === 'alpha-node-01');
  assert.equal(instance.databases.length, 1);
  assert.equal(instance.databases[0].name, 'alpha-node-01');
  assert.equal(tree.databases.rows.length, 1);
  assert.equal(tree.databases.rows[0].name, 'unknown-db');
});

test('routes follow host, project and selected instance', () => {
  const tree = buildTree(fixture);
  const host = parseRoute(tree.host.route, tree);
  assert.equal(host.kind, 'host');
  const project = parseRoute('#project/alpha', tree);
  assert.equal(project.kind, 'project');
  const instance = parseRoute('#project/alpha/instance/alpha-node-01', tree);
  assert.equal(instance.kind, 'instance');
  assert.equal(instance.node.name, 'alpha-node-01');
  assert.equal(parseRoute('#project/missing', tree).kind, 'not-found');
  assert.equal(parseRoute('#host/not-this-host', tree).kind, 'not-found');
});

test('project labels and status words stay plain', () => {
  assert.equal(projectLabel('default'), 'Default');
  assert.equal(projectLabel('arcak-adopt-home-assistant'), 'Adopted Home Assistant');
  assert.equal(kindLabel('virtual-machine'), 'VM');
  assert.equal(kindLabel('container'), 'Container');
  assert.equal(statusTone('Stopped'), 'neutral');
  assert.equal(statusTone('Running'), 'ok');
  assert.equal(statusTone('unreachable'), 'bad');
});

test('project status leads with running and stopped counts', () => {
  const tree = buildTree(fixture);
  const alpha = tree.projects.find((project) => project.project === 'alpha');
  assert.match(alpha.status, /^[0-9]+ running/);
  assert.match(alpha.status, /stopped/);
  const empty = tree.projects.find((project) => project.project === 'empty');
  assert.equal(empty.status, 'Empty Incus project');
});

test('alerts are firing-only and only kernel and retained owners are managed', () => {
  assert.deepEqual(alertRows({ firing: [{ rule: 'cpu' }], unknown: [{ rule: 'disk' }], resolvedRecent: [{ rule: 'old' }] }).map((row) => row.rule), ['cpu']);
  for (const [owner, label] of [
    ['kernel', 'Managed by Arca'], ['retained', 'Managed by Arca'],
    ['droidd', 'External'], ['foreign', 'External'], ['contested', 'External'],
    ['unknown', 'External'], ['custom', 'External'], ['', 'External'],
    [null, 'External'], [undefined, 'External'],
  ]) {
    assert.equal(ownerLabel({ class: owner }), label);
    assert.equal(ownerLabel(owner), label);
  }
});

test('approval links require absolute HTTP(S) URLs', () => {
  assert.equal(approvalUrl('https://example.com/approve'), 'https://example.com/approve');
  assert.equal(approvalUrl('http://127.0.0.1:8080/approve'), 'http://127.0.0.1:8080/approve');
  for (const value of ['', '/approve', 'javascript:alert(1)', 'ftp://example.com', 'https://user:pass@example.com/approve']) {
    assert.equal(approvalUrl(value), '');
  }
});

test('committed browser artifacts equal a fresh build and use no external CSS', async () => {
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
    const css = await readFile(join(here, '..', name), 'utf8');
    assert.doesNotMatch(css, /@import|url\(\s*['"]?https?:/i, name);
  }
});


test('multi-host routes encode every segment and resolve both qualified and primary links', () => {
  const names = ['alpha / %', 'beta / %'];
  const project = 'shared / %';
  const instance = 'worker / %';
  const tree = buildTree({ hosts: names.map((name) => ({
    ...fixture, name, projects: [project],
    instances: [{ project, name: instance, status: 'Running' }],
  })) });
  assert.notEqual(tree.hosts[0].projects[0].key, tree.hosts[1].projects[0].key);
  assert.notEqual(tree.hosts[0].projects[0].children[0].key, tree.hosts[1].projects[0].children[0].key);
  for (const model of tree.hosts) {
    const hostPart = encodeURIComponent(model.hostId);
    assert.equal(model.host.route, `#host/${hostPart}`);
    assert.equal(model.projects[0].route, `#project/${hostPart}/${encodeURIComponent(project)}`);
    assert.equal(model.projects[0].children[0].route, `#project/${hostPart}/${encodeURIComponent(project)}/instance/${encodeURIComponent(instance)}`);
    assert.equal(model.notPlaced.route, `#not-placed/${hostPart}`);
    assert.equal(model.databases.route, `#databases/${hostPart}`);
    for (const node of [model.host, model.projects[0], model.projects[0].children[0], model.notPlaced, model.databases]) {
      const route = parseRoute(node.route, tree);
      assert.equal(route.node, node);
      assert.equal(route.model, model);
    }
  }
  for (const fragment of ['', `#host/${fixture.machine.node}`, `#project/${encodeURIComponent(project)}`,
    `#project/${encodeURIComponent(project)}/instance/${encodeURIComponent(instance)}`, '#not-placed', '#databases']) {
    assert.equal(parseRoute(fragment, tree).model, tree.hosts[0]);
  }
  for (const fragment of ['#host/%', '#project/%/x', '#not-placed/missing', '#databases/missing', '#project/missing/shared']) {
    assert.equal(parseRoute(fragment, tree).kind, 'not-found');
  }
});
