import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';

const here = fileURLToPath(new URL('.', import.meta.url));
const pageJs = await readFile(join(here, '../page.js'));
const pageCss = await readFile(join(here, '../page.css'));
const html = '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="arca-canvas/page.css"></head><body><main id="arca-root" data-arca-base="/arca-canvas" data-poll-ms="50"></main><script src="arca-canvas/page.js" defer></script></body></html>';

// The kernel's own card for each verb (crates/arca/src/action_proposal.rs): the summary, the Name / Effect rows,
// and the verb's own result.
const CARDS = [
  {
    id: 'act-bbbb00000001', action: 'service_import',
    summary: 'Import the service db1, which another manager created, where it stands',
    effect: 'Arca takes over this service where it stands. It writes one key on the instance, user.arcakernel.entry = adopt-db1, and nothing else: the service is not stopped, moved or changed.',
    result: { imported: true, preImage: 'c'.repeat(64), service: { name: 'db1', project: 'app1', state: 'running' } },
    done: 'The service was imported. Arca lists it now and changes nothing on it.',
  },
  {
    id: 'act-bbbb00000002', action: 'service_release',
    summary: 'Release the imported service db1 back to the manager that created it',
    effect: 'Arca gives this service back to the manager that created it. It clears the one key the import wrote and removes its own record: the service is not stopped, moved or changed.',
    result: { released: true, restored: true, preImage: 'c'.repeat(64) },
    done: 'The service was released back to the manager that created it.',
  },
];
const UNKNOWN_ID = 'act-bbbb00000003';

function machine(name) {
  return {
    name,
    address: '192.0.2.10',
    at: '2026-11-10T12:00:00.000Z',
    error: null,
    nodeName: name,
    machine: { version: '0.1.0', node: name.repeat(64).slice(0, 64), mode: 'kernel', incusReachable: true, uptimeS: 60, appliedSeq: 1 },
    instancesKnown: true,
    projects: [],
    instances: [],
    services: [],
    metrics: null,
    alerts: { firing: [], unknown: [], resolvedRecent: [] },
    plans: [],
    journal: [],
    journalKnown: true,
  };
}

function plan({ id, action, summary, effect }) {
  const now = Date.now();
  const row = {
    kind: 'action',
    id,
    action,
    summary,
    args: { name: 'db1' },
    argsDigest: `${id.slice(4)}${'0'.repeat(52)}`,
    createdAt: now,
    expiresAt: now + 15 * 60 * 1000,
    details: [
      { label: 'Name', value: 'db1' },
      { label: 'Effect', value: effect },
    ],
  };
  row.statement = JSON.stringify({
    action: row.action,
    expiresAt: new Date(row.expiresAt).toISOString(),
    host: 'alpha.example',
    issuedAt: new Date(now).toISOString(),
    nonce: '0123456789abcdef0123456789abcdef',
    paramsDigest: row.argsDigest,
    target: row.id,
  });
  return row;
}

async function body(req) {
  let raw = '';
  for await (const chunk of req) raw += chunk;
  return raw;
}

test('an import and a release of a service each draw their card and finish in plain words', async (t) => {
  let plans = [
    ...CARDS.map(plan),
    // A verb the canvas does not list is never offered a card, whatever the kernel sends.
    plan({ id: UNKNOWN_ID, action: 'service_adopt', summary: 'Adopt the service db1', effect: 'Adopts the service.' }),
  ];
  const approvals = [];
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/arca-canvas') { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(html); return; }
    if (path === '/arca-canvas/page.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end(pageJs); return; }
    if (path === '/arca-canvas/page.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end(pageCss); return; }
    if (path === '/arca-canvas/snapshot') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        at: '2026-11-10T12:00:00.000Z',
        approvalUrl: 'https://example.com/approve',
        pageHost: 'alpha',
        projects: [],
        defaultProject: '',
        hosts: [machine('alpha')],
      }));
      return;
    }
    if (path === '/arca-canvas/chat/history') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ session: { id: 's', name: 'Doctor' }, messages: [], usage: null, limits: null }));
      return;
    }
    if (path === '/approve/plans') {
      res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
      res.end(JSON.stringify({ plans, passkeysTrusted: 1 }));
      return;
    }
    if (path === '/approve') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end('<!doctype html><html><body data-credential-ids=\'["AQID"]\'></body></html>');
      return;
    }
    if (path === '/approve/approve' && req.method === 'POST') {
      const payload = JSON.parse(await body(req));
      approvals.push(payload.plan);
      const card = CARDS.find((entry) => entry.id === payload.plan);
      plans = plans.filter((row) => row.id !== payload.plan);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        plan: payload.plan, id: payload.plan, action: card.action,
        result: card.result,
      }));
      return;
    }
    if (path === '/arca-canvas/chat' && req.method === 'POST') {
      await body(req);
      res.writeHead(200, { 'Content-Type': 'text/event-stream; charset=utf-8', 'Cache-Control': 'no-store' });
      const events = [
        ['start', { session: 'Doctor' }],
        ...[...CARDS.map((card) => card.id), UNKNOWN_ID].map((id, index) => ['tool', {
          callId: `c-${index}`, name: 'arca_propose', state: 'done', host: 'alpha', proposal: { id },
        }]),
        ['text', { text: 'Approve the cards.' }],
        ['done', { reason: 'complete' }],
      ];
      for (const [name, data] of events) res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
      res.end();
      return;
    }
    res.writeHead(404).end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => { server.closeAllConnections?.(); server.close(resolve); }));
  const browser = await chromium.launch({ headless: true });
  t.after(() => browser.close());
  const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage();
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'credentials', {
      configurable: true,
      value: {
        async get() {
          return {
            rawId: Uint8Array.from([1, 2, 3]),
            response: {
              authenticatorData: Uint8Array.from([4, 5]),
              clientDataJSON: new TextEncoder().encode('{"type":"webauthn.get"}'),
              signature: Uint8Array.from([6, 7]),
            },
          };
        },
      },
    });
  });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${server.address().port}/arca-canvas`);

  await page.locator('.arca-composer-input').fill('import db1, then release it');
  await page.locator('.arca-send').click();
  await page.locator(`.arca-chat-view .arca-proposal[data-proposal="${CARDS[1].id}"]`).waitFor();
  assert.equal(await page.locator('.arca-chat-view .arca-proposal').count(), 2, 'an unlisted service verb has no card');
  assert.equal(await page.locator(`.arca-proposal[data-proposal="${UNKNOWN_ID}"]`).count(), 0);

  for (const entry of CARDS) {
    const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${entry.id}"]`);
    assert.equal(await card.getAttribute('data-state'), 'waiting', entry.action);
    assert.equal(await card.locator('.arca-approval-summary').innerText(), entry.summary);
    const rows = await card.locator('.arca-approval-fact[data-detail="true"]').evaluateAll((nodes) => nodes.map((node) => [
      node.querySelector('dt').textContent, node.querySelector('dd').textContent,
    ]));
    assert.deepEqual(rows, [['Name', 'db1'], ['Effect', entry.effect]], entry.action);
    assert.equal(await card.locator('.arca-approval-fact[data-detail="host"] dd').innerText(), 'alpha.example');
    assert.equal(await card.locator('.arca-approval-fact[data-technical="action"] dd').textContent(), entry.action);
    assert.equal(await card.locator('.arca-approval-slot').count(), 0, 'only a GPU card has a Slot row');
    assert.equal(await card.locator('.arca-approve').isEnabled(), true, entry.action);
    assert.equal(await card.locator('.arca-decline').isEnabled(), true, entry.action);
  }

  for (const entry of CARDS) {
    const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${entry.id}"]`);
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-chat-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'done', entry.id);
    assert.equal(await card.locator('.arca-approval-status').innerText(), entry.done, entry.action);
  }
  assert.deepEqual(approvals, CARDS.map((entry) => entry.id));
  assert.deepEqual(errors, []);
});
