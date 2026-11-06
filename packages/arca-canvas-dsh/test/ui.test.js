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
  at: '2026-01-01T12:00:00Z', error: null, approvalUrl: '', nodeName: 'Example machine',
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
  assert.equal(layoutCanvas({ instances: Array.from({ length: 5 }, (_, i) => ({ name: `${i}` })) }).height, 766);
  assert.deepEqual(layoutCanvas({ ...snapshot, instances: [...snapshot.instances].reverse() }), layoutCanvas(snapshot));
});

test('only firing alerts become cards; missing checks get one muted count', () => {
  for (const [alerts, count, missing] of [
    [{ firing: [{ rule: 'cpu', state: 'ok' }], unknown: [{ rule: 'load' }], resolvedRecent: [{ rule: 'memory' }] }, 1, 1],
    [fixture.alerts_list, 0, 2],
    [{ resolvedRecent: [{ rule: 'cpu', state: 'resolved' }] }, 0, 0],
    [[{ rule: 'cpu', state: 'firing' }, { rule: 'load', state: 'unknown' }, { rule: 'memory', state: 'resolved' }], 1, 0],
  ]) {
    const target = root();
    renderCanvas(target, { ...snapshot, alerts });
    assert.equal(all(target).filter(({ dataset }) => dataset.kind === 'alert').length, count);
    const hints = all(target).filter(({ className }) => className === 'arca-no-data');
    assert.equal(hints.length, missing ? 1 : 0);
    if (missing) assert.equal(hints[0].textContent, `${missing} check${missing === 1 ? ' has' : 's have'} no data`);
    assert.ok(layoutCanvas({ alerts }).nodes.every(({ data }) => data.state === 'firing'));
  }
});

test('populated columns wrap within the 1012 px content width at 1440x900', () => {
  const rows = Array.from({ length: 5 }, (_, i) => ({ name: `${i}` }));
  const populated = { ...snapshot, instances: rows, databases: rows, alerts: { firing: rows }, phones: rows, desktops: rows };
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
});

test('untrusted snapshot values remain text with no HTML nodes or executable links', () => {
  const malicious = '<img src=x onerror=alert(1)><script>alert(1)</script>';
  const target = root();
  renderCanvas(target, {
    ...snapshot, nodeName: malicious, error: malicious, approvalUrl: 'javascript:alert(1)',
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
  assert.equal(all(target).some(({ className }) => className === 'arca-plans'), false);
  renderCanvas(target, { ...snapshot, approvalUrl: '' });
  assert.equal(all(target).find(({ className }) => className === 'arca-plans').textContent, '1 waiting plan');
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
  assert.doesNotMatch(target.textContent, /Example machine/);
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

test('muted text-only device chip appears only for unavailable with Arca cards and no error banner', () => {
  for (const devices of ['off', 'ok', 'unavailable', '<img src=x>']) {
    const target = root();
    renderCanvas(target, { ...snapshot, devices });
    const chips = all(target).filter((node) => node.className === 'arca-devices');
    assert.equal(chips.length, devices === 'unavailable' ? 1 : 0);
    if (chips.length) assert.equal(chips[0].textContent, 'Devices unavailable');
    assert.equal(all(target).filter((node) => node.className === 'arca-card').length, 4);
    assert.match(all(target).find((node) => node.className === 'arca-status').textContent, /^Snapshot /);
  }
});

test('machine title is a human name and Arca uptime uses sensible units', () => {
  for (const [seconds, label] of [[45, '45 s'], [85, '85 s'], [180, '3 m'], [3 * 3600, '3 h'], [2 * 86400, '2 d']]) {
    const target = root();
    renderCanvas(target, { ...snapshot, machine: { ...snapshot.machine, uptimeS: seconds } });
    const card = all(target).find(({ dataset }) => dataset.kind === 'machine');
    assert.equal(all(card).find(({ tagName }) => tagName === 'strong').textContent, 'Example machine');
    assert.ok(card.textContent.includes(`Arca up ${label}`));
    assert.ok(!card.textContent.includes(snapshot.machine.node));
    assert.doesNotMatch(card.textContent, /Uptime/);
  }
  const target = root();
  renderCanvas(target, { machine: snapshot.machine, nodeName: '  ' });
  const machine = all(target).find(({ dataset }) => dataset.kind === 'machine');
  assert.equal(all(machine).find(({ tagName }) => tagName === 'strong').textContent, 'Machine');
});

test('only kernel and retained are managed; all other owner classes are external', () => {
  for (const [ownerClass, label] of [['kernel', 'Managed by Arca'], ['retained', 'Managed by Arca'],
    ['droidd', 'External'], ['foreign', 'External'], ['contested', 'External'],
    [undefined, 'External'], ['future-class', 'External']]) {
    for (const owner of [ownerClass, ownerClass === undefined ? undefined : { class: ownerClass }]) {
      const target = root();
      renderCanvas(target, { instances: [{ name: 'worker', owner }] });
      const card = all(target).find(({ dataset }) => dataset.kind === 'instance');
      assert.ok(card.textContent.includes(label));
      assert.doesNotMatch(card.textContent, /Owner |droidd|kernel|foreign|retained|contested|future-class/);
      // The summary counts kinds, never ownership: every instance still contributes.
      const summary = all(target).find(({ className }) => className === 'arca-summary').textContent;
      assert.match(summary, /1 instance/);
    }
  }
});

test('41-card fixture splits long kinds, fits, avoids overlaps and brings phones forward', () => {
  const layout = layoutCanvas(fixture.canvas41);
  assert.equal(layout.nodes.length, 41);
  const instances = layout.nodes.filter(({ kind }) => kind === 'instance');
  assert.equal(instances.length, 30);
  assert.ok(new Set(instances.map(({ x }) => x)).size > 1);
  assert.ok(Math.min(...layout.nodes.filter(({ kind }) => kind === 'phone').map(({ y }) => y)) < 2 * 844);
  for (const node of layout.nodes) {
    assert.ok(node.x >= 0 && node.x + node.w <= 1012);
    for (const other of layout.nodes) {
      if (other === node) continue;
      assert.ok(node.x + node.w <= other.x || other.x + other.w <= node.x || node.y + node.h <= other.y || other.y + other.h <= node.y, 'cards must not overlap');
    }
  }
  const target = root();
  renderCanvas(target, fixture.canvas41);
  assert.equal(all(target).find(({ className }) => className === 'arca-summary').textContent,
    '1 machine · 30 instances · 1 database · 5 phones · 1 desktop2 checks have no data');
});
