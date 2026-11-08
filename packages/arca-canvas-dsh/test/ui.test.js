import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import {
  buildTree, kindLabel, parseRoute, projectLabel, statusTone,
} from '../src/tree.js';

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
