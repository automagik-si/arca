import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { buildBrowser } from '../build.mjs';
import { approvalUrl } from '../src/approval.js';
import { layoutCanvas, spread } from '../src/layout.js';
import { mountCanvas, renderCanvas } from '../src/render.js';

const fixture = JSON.parse(await readFile(new URL('./fixtures.json', import.meta.url), 'utf8'));
const snapshot = {
  at: '2026-01-01T12:00:00Z', error: null, approvalUrl: '',
  machine: fixture.arca_status,
  instances: fixture.watch_report.projects.flatMap(({ project, instances }) => instances.map((row) => ({ ...row, project }))),
  databases: fixture.postgres_list.databases, metrics: fixture.node_metrics,
  alerts: fixture.alerts_list, plans: fixture.plan_list.plans,
};

// Text setters never parse markup; the renderer must not use an HTML API.
class Element {
  constructor(tag, ownerDocument) {
    this.tagName = tag; this.ownerDocument = ownerDocument; this.children = [];
    this.dataset = {}; this.style = {}; this.attributes = {};
  }
  set innerHTML(_) { throw new Error('HTML parsing is forbidden'); }
  set textContent(value) { this.value = value; this.children = []; }
  get textContent() { return (this.value ?? '') + this.children.map((node) => node.textContent).join(''); }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  setAttribute(name, value) { this.attributes[name] = value; }
}
function root() {
  const doc = { createElement: (tag) => new Element(tag, doc) };
  return new Element('main', doc);
}
const all = (node) => [node, ...node.children.flatMap(all)];

test('fixture columns use grid and spread positions without changing inputs', () => {
  const input = structuredClone(snapshot);
  assert.deepEqual(layoutCanvas(input).nodes.map(({ kind, x, y, w, h }) => ({ kind, x, y, w, h })), [
    { kind: 'machine', x: 0, y: 0, w: 230, h: 210 },
    { kind: 'instance', x: 270, y: 0, w: 260, h: 210 },
    { kind: 'instance', x: 270, y: 270, w: 260, h: 210 },
    { kind: 'database', x: 570, y: 0, w: 210, h: 136 },
  ]);
  assert.deepEqual(input, snapshot);
  assert.deepEqual(layoutCanvas({}).nodes, []);
  assert.deepEqual(spread([{ h: 300 }, { h: 300 }, { h: 300 }]).map(({ y }) => y), [0, 330, 660]);
  assert.equal(layoutCanvas({ instances: Array.from({ length: 5 }, (_, i) => ({ name: `${i}` })) }).height, 1246);
  assert.deepEqual(layoutCanvas({ ...snapshot, instances: [...snapshot.instances].reverse() }), layoutCanvas(snapshot));
});

test('alert collections become cards; services stay absent and plans remain a badge', () => {
  const layout = layoutCanvas({ ...snapshot, alerts: { firing: [{ rule: 'cpu' }], unknown: [{ rule: 'load' }], resolvedRecent: [{ rule: 'memory' }] }, services: [{ name: 'unused' }] });
  assert.deepEqual(layout.nodes.filter(({ kind }) => kind === 'alert').map(({ x, y, data }) => [x, y, data.state]), [[0, 520, 'firing'], [0, 710, 'unknown'], [0, 900, 'resolved']]);
  assert.ok(layout.nodes.every(({ kind }) => ['machine', 'instance', 'database', 'alert'].includes(kind)));
});

test('populated columns wrap within the 1012 px content width at 1440x900', () => {
  const rows = Array.from({ length: 5 }, (_, i) => ({ name: `${i}` }));
  const populated = { ...snapshot, instances: rows, databases: rows, alerts: rows, phones: rows, desktops: rows };
  // The machine is a singleton; all list-backed columns have several cards.
  for (const width of [1440 - 396 - 2 * 16, 780, 500, 200, 1400]) {
    const layout = layoutCanvas(populated, width);
    assert.equal(layout.width, width);
    assert.equal(layout.nodes.length, 26);
    for (const node of layout.nodes) {
      assert.ok(node.x >= 0 && node.x + node.w <= width, `${node.kind} exceeds ${width}`);
      assert.ok(node.y + node.h <= layout.height);
      for (const other of layout.nodes) {
        if (other === node) continue;
        assert.ok(node.x + node.w <= other.x || other.x + other.w <= node.x || node.y + node.h <= other.y || other.y + other.h <= node.y, 'cards must not overlap');
      }
    }
  }
  assert.equal(layoutCanvas(populated).width, 1012);
  assert.deepEqual(layoutCanvas(populated).nodes.filter(({ kind }) => kind === 'alert').map(({ x, y }) => [x, y]), [[0, 1210], [0, 1400], [0, 1590], [0, 1780], [0, 1970]]);
});

test('untrusted snapshot values remain text with no HTML nodes or executable links', () => {
  const malicious = '<img src=x onerror=alert(1)><script>alert(1)</script>';
  const target = root();
  renderCanvas(target, {
    ...snapshot, error: malicious, approvalUrl: 'javascript:alert(1)',
    machine: { node: malicious, mode: malicious },
    instances: [{ name: malicious, project: malicious, status: malicious, kind: malicious }],
    databases: [{ name: malicious, address: malicious, port: malicious, status: malicious }],
    alerts: { firing: [{ rule: malicious, message: malicious, target: malicious }] },
  });
  assert.ok(target.textContent.includes(malicious));
  assert.equal(all(target).some(({ tagName }) => ['img', 'script', 'a'].includes(tagName)), false);
  assert.ok(all(target).some(({ className, textContent }) => className === 'arca-steps-placeholder' && textContent.includes('hidden')));
  for (const className of ['arca-card-header', 'arca-live', 'arca-state']) assert.equal(all(target).filter((node) => node.className === className).length, 4);
});

test('approval links require an absolute HTTP(S) URL and waiting plans', () => {
  for (const value of ['', '/approve', 'javascript:alert(1)', 'data:text/html,test', 'https://', 'https://user:pass@example.com/approve']) assert.equal(approvalUrl(value), '');
  for (const url of ['https://example.com/approve', 'http://example.com/approve']) {
    const target = root();
    renderCanvas(target, { ...snapshot, approvalUrl: url });
    assert.equal(all(target).find(({ tagName }) => tagName === 'a').href, url);
  }
  const target = root();
  renderCanvas(target, { ...snapshot, plans: [], approvalUrl: 'https://example.com/approve' });
  assert.equal(all(target).some(({ tagName }) => tagName === 'a'), false);
});

test('stopping during a fetch prevents late rendering and further polls', async () => {
  const target = root();
  let resolve;
  const response = new Promise((done) => { resolve = done; });
  const stop = mountCanvas(target, { fetchSnapshot: () => response });
  assert.match(target.textContent, /Waiting for the first/);
  stop();
  resolve({ ok: true, json: async () => snapshot });
  await new Promise((done) => setImmediate(done));
  assert.doesNotMatch(target.textContent, /example-node/);
});

test('light and dark tokens preserve public approval values with no external CSS resources', async () => {
  const css = await readFile(new URL('../src/canvas.css', import.meta.url), 'utf8');
  const [light, dark] = css.split('@media (prefers-color-scheme: dark)');
  assert.match(light, /--arca-bg: #f4f4f7/);
  assert.match(light, /--arca-surface: #ffffff/);
  assert.match(dark, /--arca-bg: #0b0b0f/);
  assert.match(dark, /--arca-surface: #15161c/);
  for (const token of ['raised', 'glass', 'device', 'stage', 'dot']) {
    assert.ok(light.includes(`--arca-${token}:`));
    assert.ok(dark.includes(`--arca-${token}:`));
  }
  assert.doesNotMatch(css, /@import|url\s*\(|https?:\/\//i);
  assert.ok([...css.matchAll(/var\((--[\w-]+)/g)].every(([, token]) => token.startsWith('--arca-')));
  const manifest = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));
  assert.deepEqual(manifest.dsh.client.inject, ['dsh-better-sidebar']);
});

test('every committed browser artifact has the SHA-256 of a fresh temporary build', async (t) => {
  const temporary = await mkdtemp(join(tmpdir(), 'arca-canvas-build-'));
  t.after(() => rm(temporary, { recursive: true, force: true }));
  await buildBrowser(temporary);
  const sha256 = (buffer) => createHash('sha256').update(buffer).digest('hex');
  for (const filename of ['client.js', 'page.js', 'page.css']) {
    assert.equal(sha256(await readFile(new URL(`../${filename}`, import.meta.url))), sha256(await readFile(join(temporary, filename))), filename);
  }
});


test('empty columns never advance the next phone or desktop column', () => {
  const layout = layoutCanvas({ machine: { node: 'example' }, phones: [{ name: 'phone' }], desktops: [{ name: 'desktop' }] }, 2000);
  assert.deepEqual(layout.nodes.map(({ kind, x }) => [kind, x]), [['machine', 0], ['phone', 270], ['desktop', 540]]);
  assert.deepEqual(layoutCanvas({ desktops: [{ name: 'desktop' }] }, 2000).nodes.map(({ x }) => x), [0]);
});

test('resize batches in one frame, measures again, and disconnects on stop', () => {
  const target = root();
  let callback, observed, disconnected = 0, frames = 0, cancelled = 0, frame;
  target.ownerDocument.defaultView = {
    ResizeObserver: class {
      constructor(cb) { callback = cb; }
      observe(stage) { observed = stage; }
      disconnect() { disconnected++; }
    },
    getComputedStyle: () => ({ paddingLeft: '16', paddingRight: '16' }),
    requestAnimationFrame: (cb) => { frames++; frame = cb; return 1; },
    cancelAnimationFrame: () => { cancelled++; },
  };
  const stop = mountCanvas(target, { fetchSnapshot: () => new Promise(() => {}) });
  observed.clientWidth = 600;
  callback([{ contentRect: { width: 600 } }]);
  callback([{ contentRect: { width: 590 } }]);
  assert.equal(frames, 1);
  frame();
  assert.equal(all(target).find(({ className }) => className === 'arca-world').style.width, '568px');
  callback([{ contentRect: { width: 500 } }]);
  stop();
  assert.equal(disconnected, 2);
  assert.equal(cancelled, 1);
  callback([{ contentRect: { width: 400 } }]);
  assert.equal(frames, 2);
});
