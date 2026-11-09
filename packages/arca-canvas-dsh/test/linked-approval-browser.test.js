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

// The same change proposed on two machines carries the same id, because the id is a digest of the change.
const ID = 'act-00aa11bb22cc';

function machine(name) {
  return {
    name,
    address: '192.0.2.10',
    at: '2026-11-09T12:00:00.000Z',
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

function plan(statementHost, linkedHost) {
  const now = Date.now();
  const row = {
    kind: 'action',
    id: ID,
    action: 'service_create',
    summary: `Create the service nats on ${statementHost}`,
    args: { name: 'nats', template: 'nats' },
    argsDigest: `${ID.slice(4)}${'0'.repeat(52)}`,
    createdAt: now,
    expiresAt: now + 15 * 60 * 1000,
    details: [{ label: 'Name', value: 'nats' }, { label: 'Template', value: 'nats' }],
    host: statementHost,
  };
  row.statement = JSON.stringify({
    action: row.action,
    expiresAt: new Date(row.expiresAt).toISOString(),
    host: statementHost,
    issuedAt: new Date(now).toISOString(),
    nonce: '0123456789abcdef0123456789abcdef',
    paramsDigest: row.argsDigest,
    target: row.id,
  });
  if (linkedHost) row.linkedHost = linkedHost;
  return row;
}

async function body(req) {
  let raw = '';
  for await (const chunk of req) raw += chunk;
  return raw;
}

test('a linked machine gets its own card, approves through the page host with its name, and an unlinked one gets none', async (t) => {
  let plans = [plan('alpha.example'), plan('beta.example', 'beta'), plan('gamma.example', 'gamma')];
  const approvals = [];
  const declines = [];
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/arca-canvas') { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(html); return; }
    if (path === '/arca-canvas/page.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end(pageJs); return; }
    if (path === '/arca-canvas/page.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end(pageCss); return; }
    if (path === '/arca-canvas/snapshot') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        at: '2026-11-09T12:00:00.000Z',
        approvalUrl: 'https://example.com/approve',
        pageHost: 'alpha',
        projects: [],
        defaultProject: '',
        hosts: [machine('alpha'), machine('beta'), machine('gamma')],
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
      // gamma's row is not one the page host lists as linked, so the canvas must drop it.
      res.end(JSON.stringify({ plans, passkeysTrusted: 1, linkedHosts: [{ name: 'beta', read: true }] }));
      return;
    }
    if (path === '/approve') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end('<!doctype html><html><body data-credential-ids=\'["AQID"]\'></body></html>');
      return;
    }
    if (path === '/approve/approve' && req.method === 'POST') {
      const payload = JSON.parse(await body(req));
      approvals.push(payload);
      plans = plans.filter((row) => !(row.id === payload.plan && (row.linkedHost ?? '') === (payload.host ?? '')));
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ plan: payload.plan, id: payload.plan, action: 'service_create', result: { created: true }, host: payload.host }));
      return;
    }
    if (path === '/approve/decline' && req.method === 'POST') {
      const payload = JSON.parse(await body(req));
      declines.push(payload);
      plans = plans.filter((row) => !(row.id === payload.plan && (row.linkedHost ?? '') === (payload.host ?? '')));
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ plan: payload.plan, retained: [] }));
      return;
    }
    if (path === '/arca-canvas/chat' && req.method === 'POST') {
      await body(req);
      res.writeHead(200, { 'Content-Type': 'text/event-stream; charset=utf-8', 'Cache-Control': 'no-store' });
      const events = [
        ['start', { session: 'Doctor' }],
        ['tool', { callId: 'c-alpha', name: 'arca_propose', state: 'done', host: 'alpha', proposal: { id: ID } }],
        ['tool', { callId: 'c-beta', name: 'arca_propose', state: 'done', host: 'beta', proposal: { id: ID } }],
        ['tool', { callId: 'c-gamma', name: 'arca_propose', state: 'done', host: 'gamma', proposal: { id: ID } }],
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

  await page.locator('.arca-composer-input').fill('create nats on alpha, beta and gamma');
  await page.locator('.arca-send').click();
  const own = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${ID}"]`);
  const linked = page.locator(`.arca-chat-view .arca-proposal[data-proposal="beta|${ID}"]`);
  await own.waitFor();
  await linked.waitFor();
  assert.equal(await page.locator('.arca-chat-view .arca-proposal').count(), 2, 'gamma is not linked, so it has no card');
  assert.equal(await own.locator('.arca-approval-fact[data-detail="host"] dd').innerText(), 'alpha.example');
  assert.equal(await linked.locator('.arca-approval-fact[data-detail="host"] dd').innerText(), 'beta.example');
  assert.equal(await linked.locator('.arca-approval-fact[data-detail="host"]').getAttribute('data-linked-host'), 'beta');

  await linked.locator('.arca-approve').click();
  await page.waitForFunction((id) => document.querySelector(`.arca-proposal[data-proposal="beta|${id}"]`)?.dataset.state === 'done', ID);
  assert.equal(approvals.length, 1);
  assert.equal(approvals[0].plan, ID);
  assert.equal(approvals[0].host, 'beta', 'the page host is told which machine to relay to');
  const statement = JSON.parse(atob(approvals[0].approval.split('.')[0]));
  assert.equal(statement.host, 'beta.example', "the signed statement names the linked machine's host");
  assert.equal(await own.getAttribute('data-state'), 'waiting', "the page host's card with the same id still waits");

  await own.locator('.arca-decline').click();
  await page.waitForFunction((id) => document.querySelector(`.arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'declined', ID);
  assert.deepEqual(declines, [{ plan: ID }], "the page host's own card declines without a host");
  assert.deepEqual(errors, []);
});
