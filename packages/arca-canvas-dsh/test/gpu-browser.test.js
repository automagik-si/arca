import assert from 'node:assert/strict';
import { mkdir, readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';

const here = fileURLToPath(new URL('.', import.meta.url));
const pageJs = await readFile(join(here, '../page.js'));
const pageCss = await readFile(join(here, '../page.css'));
const html = '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="arca-canvas/page.css"></head><body><main id="arca-root" data-arca-base="/arca-canvas" data-poll-ms="50"></main><script src="arca-canvas/page.js" defer></script></body></html>';

const GB = 1e9;
const GPU_ID = 'act-0123456789ab';
const DOCKER_ID = 'b'.repeat(64);
const NESTED_DOCKER_ID = 'c'.repeat(64);
const EFFECT = 'Adds the card to alpha as a physical GPU device. Its memory is shared with every other user of the card, not isolated.';

function telemetry(overrides = {}) {
  return {
    utilizationPct: 12,
    memoryUsedBytes: 6.1 * GB,
    memoryTotalBytes: 17.2 * GB,
    powerDrawW: 61.2,
    powerLimitW: 165,
    temperatureC: 63.5,
    smClockMhz: 2100,
    eventReasons: '0x0000000000000004',
    ...overrides,
  };
}

function gpuCard(pci, overrides = {}) {
  return {
    pci,
    uuid: null,
    vendor: 'NVIDIA',
    product: 'NVIDIA GeForce RTX 4060 Ti',
    driver: 'nvidia',
    driverVersion: '590.1',
    numaNode: 0,
    vramTotalBytes: 17.2 * GB,
    use: 'driver-bound',
    telemetry: telemetry(),
    reason: null,
    processes: [],
    processReason: null,
    ...overrides,
  };
}

function machine(name, machineOverrides, gpuValue, overrides = {}) {
  return {
    name,
    address: '192.0.2.10',
    at: '2026-11-08T16:00:00.000Z',
    error: null,
    nodeName: name,
    machine: { version: '0.1.0', node: name.repeat(64).slice(0, 64), mode: 'kernel', incusReachable: true, uptimeS: 3600, appliedSeq: 4, ...machineOverrides },
    instancesKnown: true,
    projects: [],
    instances: [],
    services: [],
    metrics: {
      at: 1794160000000,
      latest: {
        at: 1794160000000,
        cpu: { cores: 8, usedPct: 25 },
        memory: { totalBytes: 16 * GB, availableBytes: 12 * GB },
        gpu: gpuValue,
      },
    },
    alerts: { firing: [], unknown: [], resolvedRecent: [] },
    plans: [],
    journal: [],
    journalKnown: true,
    ...overrides,
  };
}

function fixture() {
  return {
    at: '2026-11-08T16:00:00.000Z',
    approvalUrl: 'https://example.com/approve',
    pageHost: 'alpha',
    defaultProject: 'Studio',
    projects: [{ name: 'Studio', domain: '', members: [] }],
    hosts: [
      machine('alpha', {}, {
        reason: null,
        skipped: null,
        cards: [
          gpuCard('0000:01:00.0', {
            processes: [
              { pci: '0000:01:00.0', pid: 10, vramBytes: 6.1 * GB, owner: { kind: 'instance', project: 'studio', name: 'app' } },
              { pci: '0000:01:00.0', pid: 11, vramBytes: 2 * GB, owner: { kind: 'instance', project: 'media', name: 'render', container: NESTED_DOCKER_ID } },
              { pci: '0000:01:00.0', pid: 12, vramBytes: 3 * GB, owner: { kind: 'docker', container: DOCKER_ID } },
              { pci: '0000:01:00.0', pid: 13, vramBytes: 0.5 * GB, owner: { kind: 'host' } },
            ],
          }),
          gpuCard('0000:02:00.0', { telemetry: telemetry({ eventReasons: '0x0000000000000001' }) }),
        ],
      }),
      machine('beta', {}, {
        reason: null,
        skipped: null,
        cards: [gpuCard('0000:03:00.0', {
          product: 'NVIDIA GeForce RTX 4090',
          telemetry: null,
          vramTotalBytes: null,
          reason: 'not measurable from here',
          processes: [],
          processReason: 'not measurable from here',
        })],
      }),
      machine('gamma', { mode: 'watch-only' }, { reason: null, skipped: null, cards: [] }, {
        services: [{ name: 'web1', template: 'web', state: 'running', ipv4: '10.0.0.7', ports: [8080], lanPort: 8080 }],
        error: 'service_list: Arca refused the request (403).',
      }),
    ],
  };
}

function digestFor(id) {
  return `${id.slice(4)}${'0'.repeat(52)}`;
}

function approvalPlan() {
  const now = Date.now();
  const plan = {
    kind: 'action',
    id: GPU_ID,
    action: 'gpu_attach',
    summary: 'Attach the GPU at 0000:01:00.0 to instance alpha in project arcak-gpu',
    args: { project: 'arcak-gpu', instance: 'alpha', kind: 'container', uuid: '11111111-2222-3333-4444-555555555555', pci: '0000:01:00.0' },
    argsDigest: digestFor(GPU_ID),
    createdAt: now,
    expiresAt: now + 15 * 60 * 1000,
    details: [
      { label: 'Project', value: 'arcak-gpu' },
      { label: 'Instance', value: 'alpha (container)' },
      { label: 'GPU', value: 'NVIDIA GeForce RTX 4060 Ti (0000:01:00.0)' },
      { label: 'Effect', value: EFFECT },
    ],
  };
  plan.statement = JSON.stringify({
    action: plan.action,
    expiresAt: new Date(plan.expiresAt).toISOString(),
    host: 'alpha.example',
    issuedAt: new Date(plan.createdAt).toISOString(),
    nonce: '0123456789abcdef0123456789abcdef',
    paramsDigest: plan.argsDigest,
    target: plan.id,
  });
  return plan;
}

function writeSse(res, events) {
  res.writeHead(200, { 'Content-Type': 'text/event-stream; charset=utf-8', 'Cache-Control': 'no-store' });
  let index = 0;
  const next = () => {
    if (index >= events.length) { res.end(); return; }
    const [name, data] = events[index++];
    res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
    setTimeout(next, 5);
  };
  next();
}

async function body(req) {
  let raw = '';
  for await (const chunk of req) raw += chunk;
  return raw;
}

test('GPU cards render per machine, name their users, and keep the approval safety rows whole', async (t) => {
  const data = fixture();
  const plan = approvalPlan();
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/arca-canvas') { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(html); return; }
    if (path === '/arca-canvas/page.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end(pageJs); return; }
    if (path === '/arca-canvas/page.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end(pageCss); return; }
    if (path === '/arca-canvas/snapshot') { res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(data)); return; }
    if (path === '/arca-canvas/chat/history') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        session: { id: 'session-gpu', name: 'Doctor' }, messages: [],
        usage: { day: '2026-11-08', dailyTokens: 0, dailyBudget: 1_000_000 },
        limits: { toolCallsPerTurn: 40, outputTokensPerTurn: 64_000 },
      }));
      return;
    }
    if (path === '/approve/plans') {
      res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
      res.end(JSON.stringify({ plans: [plan], passkeysTrusted: 1 }));
      return;
    }
    if (path === '/approve') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end('<!doctype html><html><body data-credential-ids=\'["AQID"]\'></body></html>');
      return;
    }
    if (path === '/arca-canvas/chat' && req.method === 'POST') {
      await body(req);
      writeSse(res, [
        ['start', { session: 'Doctor' }],
        ['text', { text: 'I found the card and its users. ' }],
        ['tool', {
          callId: 'call-gpu-1', name: 'arca_propose', state: 'done', host: 'alpha',
          proposal: { id: GPU_ID }, summary: 'Proposed: Attach the GPU',
        }],
        ['text', { text: 'Approve the card to attach it.' }],
        ['done', { reason: 'complete' }],
      ]);
      return;
    }
    res.writeHead(404).end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections?.(); server.close(resolve); }));
  const browser = await chromium.launch({ headless: true });
  t.after(() => browser.close());
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  await page.goto('http://127.0.0.1:' + server.address().port + '/arca-canvas');

  await page.waitForFunction(() => document.querySelectorAll('.arca-machine[data-host="alpha"] .arca-gpu').length === 2);
  const alpha = page.locator('.arca-machine[data-host="alpha"]');
  const lines = alpha.locator('.arca-gpu');
  assert.deepEqual(await lines.evaluateAll((nodes) => nodes.map((node) => node.dataset.gpu)), ['0000:01:00.0', '0000:02:00.0']);
  assert.deepEqual(await lines.locator('.arca-gpu-label').evaluateAll((nodes) => nodes.map((node) => node.textContent)), ['GPU 1', 'GPU 2']);
  assert.match(await lines.first().getAttribute('title'), /NVIDIA GeForce RTX 4060 Ti/);
  assert.match(await lines.first().getAttribute('title'), /0000:01:00\.0/);
  assert.match(await lines.first().innerText(), /GPU 1[\s\S]*12%[\s\S]*6\.1 \/ 17 GB/);
  assert.equal(await lines.first().locator('.arca-gpu-note').innerText(), 'held back by its power limit');
  assert.equal(await lines.nth(1).locator('.arca-gpu-note').count(), 0, 'bit 0x1 is not the power cap');

  const remote = page.locator('.arca-machine[data-host="beta"] .arca-gpu');
  assert.equal(await remote.count(), 1);
  const remoteText = await remote.innerText();
  assert.match(remoteText, /GPU 4090/);
  assert.match(remoteText, /not measurable from here/);
  assert.doesNotMatch(remoteText, /0%/);
  assert.equal(await page.locator('.arca-machine[data-host="gamma"] .arca-gpu').count(), 0, 'no GPU means no GPU line');

  await page.locator('.arca-machine[data-host="alpha"] .arca-machine-more').click();
  const alphaDetail = page.locator('.arca-detail-panel');
  await alphaDetail.waitFor();
  assert.match(await alphaDetail.innerText(), /Who is using it/);
  const owners = await alphaDetail.locator('.arca-detail-owner').evaluateAll((nodes) => nodes.map((node) => [
    node.dataset.owner, node.querySelector('.arca-detail-owner-where').textContent, node.querySelector('.arca-detail-owner-vram').textContent,
  ]));
  assert.deepEqual(owners, [
    ['instance', 'Studio / app', '6.1 GB'],
    ['docker', `Docker container ${DOCKER_ID.slice(0, 12)}`, '3 GB'],
    ['instance', `Media / renderDocker ${NESTED_DOCKER_ID.slice(0, 12)}`, '2 GB'],
    ['host', 'on the host', '0.5 GB'],
  ]);
  assert.match(await alphaDetail.innerText(), /61\.2 W of 165 W/);
  assert.match(await alphaDetail.innerText(), /63\.5 °C/);
  assert.equal(await alphaDetail.locator('.arca-detail-power').count(), 1, 'only the 0x4 card is power limited');
  await alphaDetail.locator('.arca-detail-close').click();
  assert.equal(await alphaDetail.isVisible(), false);

  await page.locator('.arca-machine[data-host="gamma"] .arca-machine-more').click();
  const gammaDetail = page.locator('.arca-detail-panel');
  await gammaDetail.waitFor();
  assert.equal(await gammaDetail.locator('.arca-detail-watch').innerText(), "Arca watches this machine; it doesn't run services here yet");
  assert.equal(await gammaDetail.locator('.arca-detail-service').count(), 0, 'a watch-only host shows no service cards');
  const gammaText = await gammaDetail.innerText();
  assert.doesNotMatch(gammaText, /refused|error|could not be read/i);
  await gammaDetail.locator('.arca-detail-close').click();

  if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
    await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: theme });
      await page.waitForTimeout(250);
      await page.locator('.arca-machines').screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `canvas-gpu-strip-${theme}.png`) });
    }
    await page.emulateMedia({ colorScheme: 'light' });
  }

  await page.locator('.arca-composer-input').fill('who is using the gpu?');
  await page.locator('.arca-send').click();
  const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${GPU_ID}"]`);
  await card.waitFor();
  assert.equal(await card.locator('.arca-approval-summary').innerText(), plan.summary);
  const rows = await card.locator('.arca-approval-fact[data-detail="true"]').evaluateAll((nodes) => nodes.map((node) => [
    node.querySelector('dt').textContent, node.querySelector('dd').textContent,
  ]));
  assert.deepEqual(rows, plan.details.map((row) => [row.label, row.value]));
  assert.equal(rows[3][0], 'Effect');
  assert.equal(rows[3][1], EFFECT, 'the Effect safety sentence is never shortened');
  assert.equal(await card.locator('[data-technical="action"] dd').innerText(), 'gpu_attach');

  if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
    await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
    for (const theme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: theme });
      await card.scrollIntoViewIfNeeded();
      await page.waitForTimeout(250);
      await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `canvas-gpu-approval-${theme}.png`) });
    }
    await page.emulateMedia({ colorScheme: 'light' });
    await page.locator('.arca-machine[data-host="alpha"] .arca-machine-more').click();
    for (const theme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: theme });
      await page.waitForTimeout(250);
      await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `canvas-gpu-detail-${theme}.png`) });
    }
  }
});
