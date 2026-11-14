import assert from 'node:assert/strict';
import { test } from 'node:test';
import { LINES, parse, stripComment } from '../src/yaml.js';

const WORKFLOW = `name: ci # the gate
on:
  push:
    branches: [main, "release/*"]
  pull_request:
concurrency:
  group: ci-\${{ github.ref }}
  cancel-in-progress: true
jobs:
  test:
    name: Test (\${{ matrix.os }})
    runs-on: \${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest]
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - name: Build
        run: |
          cargo build
          cargo test # not a comment inside a block
      - uses: actions/setup-node@v4
        with:
          node-version: 22
          cache: 'npm'
  lint:
    runs-on: ubuntu-latest
    steps:
    - run: >
        echo one
        two
`;

test('a workflow reads as its mappings, sequences and scalars', () => {
  const doc = parse(WORKFLOW);
  assert.equal(doc.name, 'ci');
  assert.deepEqual(doc.on.push.branches, ['main', 'release/*']);
  assert.equal(doc.on.pull_request, null);
  assert.equal(doc.concurrency['cancel-in-progress'], true);
  assert.equal(doc.jobs.test['timeout-minutes'], 30);
  assert.deepEqual(doc.jobs.test.strategy.matrix.os, ['ubuntu-latest', 'macos-latest']);
  assert.equal(doc.jobs.test.steps.length, 3);
  assert.equal(doc.jobs.test.steps[1].run, 'cargo build\ncargo test # not a comment inside a block\n');
  assert.deepEqual(doc.jobs.test.steps[2].with, { 'node-version': 22, cache: 'npm' });
  assert.equal(doc.jobs.lint.steps[0].run, 'echo one two\n');
});

test('each key and item carries its line', () => {
  const doc = parse(WORKFLOW);
  assert.equal(doc[LINES].jobs, 9);
  assert.equal(doc.jobs[LINES].lint, 27);
  assert.deepEqual(doc.jobs.test.steps[LINES], [18, 19, 23]);
  assert.equal(doc.jobs.test.steps[2][LINES].with, 24);
});

test('flow collections, quotes and comments', () => {
  assert.deepEqual(parse('a: {b: 1, c: [x, "y, z"]}\n'), { a: { b: 1, c: ['x', 'y, z'] } });
  assert.deepEqual(parse("a: 'it''s'\nb: \"tab\\tthere\"\n"), { a: "it's", b: 'tab\tthere' });
  assert.equal(stripComment('key: "a # b" # c'), 'key: "a # b" ');
  assert.deepEqual(parse('on: [push, pull_request]\n').on, ['push', 'pull_request']);
});

test('a sequence may sit at its key\'s own indentation', () => {
  const doc = parse('steps:\n- run: a\n- run: b\nnext: 1\n');
  assert.deepEqual(doc, { steps: [{ run: 'a' }, { run: 'b' }], next: 1 });
});
