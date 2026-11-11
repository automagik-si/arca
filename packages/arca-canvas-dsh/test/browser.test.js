import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { chromium } from 'playwright';

const here = fileURLToPath(new URL('.', import.meta.url));
const pageJs = await readFile(join(here, '../page.js'));
const pageCss = await readFile(join(here, '../page.css'));
const fixture = JSON.parse(await readFile(join(here, 'dashboard-fixtures.json'), 'utf8'));
const html = '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="arca-canvas/page.css"></head><body><main id="arca-root" data-arca-base="/arca-canvas" data-poll-ms="50"></main><script src="arca-canvas/page.js" defer></script></body></html>';

const CHAT_ID = 'act-0123456789ab';
const EXTERNAL_ID = 'act-89abcdef0123';
const EXPIRING_ID = 'act-111111111111';
const FAILED_ID = 'act-222222222222';
const HOSTILE_ID = 'act-333333333333';
const DISALLOWED_ID = 'act-444444444444';
const MISMATCH_ID = 'act-555555555555';
const RETRY_ID = 'act-666666666666';
const GONE_ID = 'act-777777777777';
const HANG_ID = 'act-888888888888';
const UNKNOWN_ID = 'act-999999999999';
const REFUSAL_ID = 'act-bbbbbbbbbbbb';
const EMPTY_REFUSAL_ID = 'act-eeeeeeeeeeee';
const HOST_ID = 'act-cccccccccccc';
const STALE_ID = 'act-dddddddddddd';
const DELETE_ID = 'd'.repeat(64);

function digestFor(id) {
  return `${id.slice(4)}${'0'.repeat(52)}`;
}

const b64 = (value) => Buffer.from(value).toString('base64');
const b64url = (value) => b64(value).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');

function approvalStatement(plan) {
  return JSON.stringify({
    action: plan.action,
    expiresAt: new Date(plan.expiresAt).toISOString(),
    host: 'alpha.example',
    issuedAt: new Date(plan.createdAt).toISOString(),
    nonce: '0123456789abcdef0123456789abcdef',
    paramsDigest: plan.argsDigest,
    target: plan.id,
  });
}

function actionPlan({ id, action, summary, args, details }) {
  const now = Date.now();
  const plan = {
    kind: 'action', id, action, summary, args,
    argsDigest: digestFor(id), createdAt: now, expiresAt: now + 15 * 60 * 1000, details,
  };
  plan.statement = approvalStatement(plan);
  return plan;
}

function approvalFixture() {
  return [
    actionPlan({
      id: CHAT_ID, action: 'service_create', summary: 'Create the service nats',
      args: { name: 'nats', template: 'nats' },
      details: [{ label: 'Name', value: 'nats' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: EXTERNAL_ID, action: 'service_create', summary: 'Create the service billing',
      args: { name: 'billing', template: 'postgres' },
      details: [{ label: 'Name', value: 'billing' }, { label: 'Template', value: 'postgres' }],
    }),
    actionPlan({
      id: EXPIRING_ID, action: 'service_create', summary: 'Create the service ephemeral',
      args: { name: 'ephemeral', template: 'nats' },
      details: [{ label: 'Name', value: 'ephemeral' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: FAILED_ID, action: 'service_connect', summary: 'Connect the service web1 to nats',
      args: { from: 'web1', to: 'nats' },
      details: [{ label: 'From', value: 'web1' }, { label: 'To', value: 'nats' }],
    }),
    actionPlan({
      id: RETRY_ID, action: 'service_create', summary: 'Create the service retry',
      args: { name: 'retry', template: 'nats' },
      details: [{ label: 'Name', value: 'retry' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: GONE_ID, action: 'service_create', summary: 'Create the service gone',
      args: { name: 'gone', template: 'nats' },
      details: [{ label: 'Name', value: 'gone' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: HANG_ID, action: 'service_create', summary: 'Create the service hang',
      args: { name: 'hang', template: 'nats' },
      details: [{ label: 'Name', value: 'hang' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: REFUSAL_ID, action: 'service_create', summary: 'Create the service refusal',
      args: { name: 'refusal', template: 'nats' },
      details: [{ label: 'Name', value: 'refusal' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: EMPTY_REFUSAL_ID, action: 'service_create', summary: 'Create the service empty',
      args: { name: 'empty', template: 'nats' },
      details: [{ label: 'Name', value: 'empty' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: HOST_ID, action: 'service_create', summary: 'Create the service host',
      args: { name: 'host', template: 'nats' },
      details: [{ label: 'Name', value: 'host' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: STALE_ID, action: 'service_create', summary: 'Create the service stale',
      args: { name: 'stale', template: 'nats' },
      details: [{ label: 'Name', value: 'stale' }, { label: 'Template', value: 'nats' }],
    }),
    actionPlan({
      id: DISALLOWED_ID, action: 'postgres_delete', summary: 'Delete the database legacy',
      args: { name: 'legacy' }, details: [{ label: 'Name', value: 'legacy' }],
    }),
    (() => {
      const plan = actionPlan({
        id: MISMATCH_ID, action: 'service_create', summary: 'Create the service mismatch',
        args: { name: 'mismatch', template: 'nats' },
        details: [{ label: 'Name', value: 'mismatch' }, { label: 'Template', value: 'nats' }],
      });
      const statement = JSON.parse(plan.statement);
      statement.target = CHAT_ID;
      plan.statement = JSON.stringify(statement);
      return plan;
    })(),
    actionPlan({
      id: HOSTILE_ID, action: 'service_connect',
      summary: 'Connect <img src=x onerror="window.__arcaXss=1"> to nats',
      args: { from: '<img src=x onerror="window.__arcaXss=1">', to: 'nats' },
      details: [
        { label: 'From', value: '<img src=x onerror="window.__arcaXss=1">' },
        { label: 'To', value: 'nats' },
      ],
    }),
    {
      plan: DELETE_ID, kind: 'delete', createdAt: Date.now(), expiresAt: Date.now() + 15 * 60 * 1000,
      entry: { kind: 'database', name: 'old-db' }, objects: [], paramsDigest: 'e'.repeat(64), statement: '{}',
    },
  ];
}

function luminance(rgb) {
  const channels = rgb.match(/\d+/g).slice(0, 3).map((value) => {
    const channel = Number(value) / 255;
    return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  });
  return channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
}

function chatEvents(message) {
  if (message.includes('unauthorized')) return { status: 401, events: [] };
  if (message.includes('forbidden')) return { status: 403, events: [] };
  if (message.includes('unavailable')) return { status: 503, events: [] };
  if (message.includes('budget')) {
    return {
      status: 200,
      events: [
        ['start', { session: 'Doctor' }],
        ['budget', { scope: 'daily', used: 1000000, budget: 1000000, message: 'Daily budget reached, try tomorrow.' }],
        ['done', { reason: 'budget' }],
      ],
    };
  }
  if (message.includes('cut off')) {
    // No done event: a slow reader (or a proxy) lost the tail.
    return { status: 200, events: [['start', { session: 'Doctor' }], ['text', { text: 'This answer never finished.' }]] };
  }
  if (message.includes('hostile')) {
    return {
      status: 200,
      events: [
        ['start', { session: 'Doctor' }],
        ['text', { text: '<img src=x onerror="window.__arcaXss=1">' }],
        ['done', { reason: 'complete' }],
      ],
    };
  }
  if (message.includes('create nats')) {
    return {
      status: 200,
      events: [
        ['start', { session: 'Doctor' }],
        ['text', { text: 'I can create the nats service. ' }],
        ['tool', {
          callId: 'call-propose-1', name: 'arca_propose', state: 'running', host: 'alpha',
          input: { action: 'service_create', args: { name: 'nats', template: 'nats' } },
        }],
        ['tool', {
          callId: 'call-propose-1', name: 'arca_propose', state: 'done', host: 'alpha',
          proposal: { id: CHAT_ID }, summary: 'Proposed: Create the service nats',
        }],
        ['text', { text: 'Tap Approve on the card to allow it.' }],
        ['done', { reason: 'complete' }],
      ],
    };
  }
  if (message.includes('foreign proposal')) {
    return {
      status: 200,
      events: [
        ['start', { session: 'Doctor' }],
        ['text', { text: 'That change belongs to beta. ' }],
        ['tool', {
          callId: 'call-propose-3', name: 'arca_propose', state: 'done', host: 'beta',
          proposal: { id: EXTERNAL_ID }, summary: 'Proposed: Create the service billing',
        }],
        ['text', { text: 'Nothing is waiting for you on this canvas.' }],
        ['done', { reason: 'complete' }],
      ],
    };
  }
  if (message.includes('unknown proposal')) {
    return {
      status: 200,
      events: [
        ['start', { session: 'Doctor' }],
        ['text', { text: 'The change was proposed. ' }],
        ['tool', {
          callId: 'call-propose-4', name: 'arca_propose', state: 'done', host: 'alpha',
          proposal: { id: UNKNOWN_ID }, summary: 'Proposed: Create the service unknown',
        }],
        ['text', { text: 'Check the card. ' }],
        ['done', { reason: 'complete' }],
      ],
    };
  }
  if (message.includes('mismatch')) {
    return {
      status: 200,
      events: [
        ['start', { session: 'Doctor' }],
        ['text', { text: 'I will delete the app database and stop every host. ' }],
        ['tool', {
          callId: 'call-propose-2', name: 'arca_propose', state: 'running', host: 'alpha',
          input: { action: 'service_connect', args: { from: 'web1', to: 'nats' } },
        }],
        ['tool', {
          callId: 'call-propose-2', name: 'arca_propose', state: 'done', host: 'alpha',
          proposal: { id: HOSTILE_ID }, summary: 'Proposed: Connect the service to nats',
        }],
        ['text', { text: 'Approve the card when you are ready.' }],
        ['done', { reason: 'complete' }],
      ],
    };
  }
  return {
    status: 200,
    events: [
      ['start', { session: 'Doctor' }],
      ['text', { text: 'Alpha is online. ' }],
      ['tool', { callId: 'call-1', name: 'arca_status', state: 'running', host: 'alpha' }],
      ['tool', { callId: 'call-1', name: 'arca_status', state: 'done', host: 'alpha', summary: 'Answered from alpha' }],
      ['text', { text: 'Its CPU is 25%.' }],
      ['done', { reason: 'complete' }],
    ],
  };
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

test('v28 layout binds both hosts, the Doctor chat, the approval cards and the activity steps', async (t) => {
  let data = fixture;
  let approvalPlans = approvalFixture();
  const requests = [];
  const approvalRequests = [];
  const declines = [];
  let retryAttempts = 0;
  let holdNextPlans = null;
  const holdPlans = () => {
    let capturedResolve;
    const captured = new Promise((resolve) => { capturedResolve = resolve; });
    let releaseResolve;
    const released = new Promise((resolve) => { releaseResolve = resolve; });
    holdNextPlans = { capturedResolve, released };
    return { captured, release: () => releaseResolve() };
  };
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    requests.push({ method: req.method, path });
    if (path === '/arca-canvas') { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(html); return; }
    if (path === '/arca-canvas/page.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end(pageJs); return; }
    if (path === '/arca-canvas/page.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end(pageCss); return; }
    if (path === '/arca-canvas/snapshot') { res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(data)); return; }
    if (path === '/arca-canvas/chat/history') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        session: { id: 'session-doctor', name: 'Doctor' }, messages: [],
        usage: { day: '2026-11-08', dailyTokens: 0, dailyBudget: 1_000_000 },
        limits: { toolCallsPerTurn: 40, outputTokensPerTurn: 64_000 },
      }));
      return;
    }
    if (path === '/arca-canvas/chat' && req.method === 'POST') {
      const payload = JSON.parse(await body(req));
      const reply = chatEvents(String(payload.message ?? ''));
      if (reply.status !== 200) { res.writeHead(reply.status).end(); return; }
      writeSse(res, reply.events);
      return;
    }
    if (path === '/approve') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end('<!doctype html><html><body data-credential-ids=\'["AQID"]\'></body></html>');
      return;
    }
    if (path === '/approve/plans') {
      // Snapshot the row list before a held response waits: a poll that started before a click may
      // still carry the plan after the card has finished.
      const payload = JSON.stringify({ plans: approvalPlans, passkeysTrusted: 1 });
      const hold = holdNextPlans;
      if (hold) {
        holdNextPlans = null;
        hold.capturedResolve();
        await hold.released;
      }
      res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
      res.end(payload);
      return;
    }
    if (path === '/test/repropose-stale') {
      if (!approvalPlans.some((row) => row.id === STALE_ID)) {
        approvalPlans = [...approvalPlans, actionPlan({
          id: STALE_ID, action: 'service_create', summary: 'Create the service stale',
          args: { name: 'stale', template: 'nats' },
          details: [{ label: 'Name', value: 'stale' }, { label: 'Template', value: 'nats' }],
        })];
      }
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end('{"ok":true}');
      return;
    }
    if (path === '/test/host') {
      const plan = approvalPlans.find((row) => row.id === HOST_ID);
      const statement = JSON.parse(plan.statement);
      statement.host = 'beta.example';
      plan.statement = JSON.stringify(statement);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end('{"ok":true}');
      return;
    }
    if (path === '/test/expire') {
      const plan = approvalPlans.find((row) => row.id === EXPIRING_ID);
      plan.expiresAt = Date.now() - 1000;
      plan.statement = approvalStatement(plan);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end('{"ok":true}');
      return;
    }
    if (path === '/test/repropose') {
      if (!approvalPlans.some((row) => row.id === EXTERNAL_ID)) {
        approvalPlans = [...approvalPlans, actionPlan({
          id: EXTERNAL_ID, action: 'service_create', summary: 'Create the service billing',
          args: { name: 'billing', template: 'postgres' },
          details: [{ label: 'Name', value: 'billing' }, { label: 'Template', value: 'postgres' }],
        })];
      }
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end('{"ok":true}');
      return;
    }
    if (path === '/approve/approve' && req.method === 'POST') {
      const raw = await body(req);
      approvalRequests.push({ raw, body: JSON.parse(raw) });
      const payload = JSON.parse(raw);
      if (payload.plan === RETRY_ID) {
        retryAttempts += 1;
        if (retryAttempts === 1) {
          res.writeHead(403, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ refusal: 'BadSignature', message: 'the passkey answer did not check' }));
          return;
        }
      }
      if (payload.plan === FAILED_ID) {
        approvalPlans = approvalPlans.filter((row) => row.id !== payload.plan);
        res.writeHead(403, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ refusal: 'not_found', message: 'The service nats: not found' }));
        return;
      }
      if (payload.plan === REFUSAL_ID || payload.plan === EMPTY_REFUSAL_ID) {
        approvalPlans = approvalPlans.filter((row) => row.id !== payload.plan);
        if (payload.plan === REFUSAL_ID) {
          res.writeHead(403, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ refusal: 'stopped', message: 'The service refusal was stopped' }));
        } else {
          // An empty 500 after the passkey was spent: the outcome is unreadable.
          res.writeHead(500).end();
        }
        return;
      }
      if (payload.plan === GONE_ID) {
        approvalPlans = approvalPlans.filter((row) => row.id !== payload.plan);
        res.writeHead(403, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ refusal: 'NoSuchPlan', message: 'the proposal is no longer waiting' }));
        return;
      }
      if (payload.plan === HANG_ID) return; // never answer; the page's 250 ms test timeout applies
      const plan = approvalPlans.find((row) => row.id === payload.plan);
      approvalPlans = approvalPlans.filter((row) => row.id !== payload.plan);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        plan: payload.plan,
        id: payload.plan,
        action: plan?.action ?? 'service_create',
        result: {
          name: plan?.args?.name ?? 'nats', template: plan?.args?.template ?? 'nats',
          state: 'running', ipv4: '10.99.0.10', ports: [4222], links: [], published: null, lanPort: null,
        },
      }));
      return;
    }
    if (path === '/approve/decline' && req.method === 'POST') {
      const raw = await body(req);
      declines.push({ raw, body: JSON.parse(raw) });
      const payload = JSON.parse(raw);
      approvalPlans = approvalPlans.filter((row) => row.id !== payload.plan);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ plan: payload.plan, retained: [] }));
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
  await page.addInitScript(() => {
    const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, 'innerHTML');
    Object.defineProperty(Element.prototype, 'innerHTML', {
      configurable: true,
      get: descriptor.get,
      set() { throw new Error('innerHTML is forbidden'); },
    });
  });

  await page.addInitScript(() => {
    const b64url = (bytes) => {
      let text = '';
      for (const byte of bytes) text += String.fromCharCode(byte);
      return btoa(text).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    };
    window.__arcaPasskey = { calls: [] };
    Object.defineProperty(navigator, 'credentials', {
      configurable: true,
      value: {
        async get(options) {
          if (window.__arcaPasskey.cancelNext) {
            window.__arcaPasskey.cancelNext = false;
            throw new DOMException('The operation was cancelled.', 'NotAllowedError');
          }
          window.__arcaPasskey.calls.push({
            challenge: Array.from(new Uint8Array(options.publicKey.challenge)),
            rpId: options.publicKey.rpId,
            userVerification: options.publicKey.userVerification,
            allowCredentials: options.publicKey.allowCredentials.map((entry) => ({
              type: entry.type, id: Array.from(entry.id),
            })),
          });
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
    window.__arcaPasskey.b64url = b64url;
  });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const origin = `http://127.0.0.1:${server.address().port}`;
  await page.goto(`${origin}/arca-canvas`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-node-project');
  await page.waitForFunction(() => location.hash === '#project/Studio');

  const send = async (message) => {
    const input = page.locator('.arca-composer-input');
    await input.fill(message);
    await page.locator('.arca-send').click();
    await page.waitForFunction(() => document.querySelector('.arca-thread')?.dataset.streaming === 'false');
  };
  const goSteps = async () => {
    await page.locator('[role="tab"]').nth(1).click();
    await page.waitForFunction(() => document.querySelector('.arca-steps-view')?.hidden === false);
  };
  const goChat = async () => {
    await page.locator('[role="tab"]').first().click();
    await page.waitForFunction(() => document.querySelector('.arca-chat-view')?.hidden === false);
  };

  await t.test('all four regions exist and live data binds into each', async () => {
    assert.equal(await page.locator('.arca-topbar').count(), 1);
    assert.equal(await page.locator('.arca-machines').count(), 1);
    assert.equal(await page.locator('.arca-stage').count(), 1);
    assert.equal(await page.locator('.arca-panel').count(), 1);

    assert.equal(await page.locator('.arca-project-select').inputValue(), 'Studio');
    const options = await page.locator('.arca-project-select option').allTextContents();
    assert.deepEqual(options, ['Studio', 'Automation', 'Web1', 'Sandbox', 'Tools']);
    assert.deepEqual(await page.locator('.arca-project-select optgroup').evaluateAll((nodes) => nodes.map((node) => node.label)),
      ['Your projects', 'Services', 'Machine projects']);
    assert.doesNotMatch(await page.locator('.arca-project-select').innerText(), /arcak-svc|Arca services/i);
    assert.equal(await page.locator('.arca-mode').first().getAttribute('aria-pressed'), 'true');
    assert.equal(await page.locator('.arca-mode').nth(1).isDisabled(), true);
    assert.match(await page.locator('.arca-mode').nth(1).innerText(), /Arca Cloud[\s\S]*coming soon/i);
    assert.match(await page.locator('.arca-clock-chip').innerText(), /^\d{2}:\d{2}$/);
    assert.equal(await page.locator('.arca-summary').innerText(), '2 machines · 12 cores · 24 GB');

    const machines = page.locator('.arca-machine:not(.arca-placeholder)');
    assert.equal(await machines.count(), 2);
    assert.deepEqual(await machines.locator('.arca-machine-name').allTextContents(), ['alpha', 'beta']);
    for (const [index, expected] of [[0, ['2 / 8 cores', '4 / 16 GB']], [1, ['2 / 4 cores', '6 / 8 GB']]]) {
      const card = machines.nth(index);
      assert.equal(await card.locator('.arca-status').innerText(), 'Online');
      assert.deepEqual(await card.locator('.arca-meter-value').allTextContents(), expected);
      assert.equal(await card.locator('.arca-meter').count(), 2);
      assert.equal(await card.locator('.arca-meter-fill').count(), 2);
      const widths = await card.locator('.arca-meter-fill').evaluateAll((nodes) => nodes.map((node) => Number(node.getAttribute('width'))));
      assert.ok(widths.every((width) => width > 0 && width <= 100), `bar widths ${widths}`);
      assert.equal(await card.locator('.arca-machine-instances').innerText(), index === 0 ? '4 instances' : '3 instances');
    }
    assert.equal(await page.locator('.arca-placeholder').count(), 2);
    assert.match(await page.locator('.arca-placeholder').first().innerText(), /Storage[\s\S]*soon/i);
    assert.match(await page.locator('.arca-placeholder').nth(1).innerText(), /Phones[\s\S]*soon/i);

    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Studio');
    assert.deepEqual(await page.locator('.arca-canvas-chips .arca-chip').allTextContents(),
      ['4 things', '2 hosts', 'studio.example']);
    assert.equal(await page.locator('.arca-node-project').count(), 1);
    assert.equal(await page.locator('.arca-node-domain').count(), 1);
    assert.equal(await page.locator('.arca-node-instance').count(), 3);
    assert.equal(await page.locator('.arca-node-database').count(), 1);
    assert.equal(await page.locator('.arca-wire').count(), 5);
    assert.match(await page.locator('.arca-wire-base').first().getAttribute('d'), /^M [\d.]+ [\d.]+ C /);

    const app = page.locator('.arca-node-instance').filter({ hasText: 'app' });
    assert.match(await app.innerText(), /runs on alpha/);
    assert.equal(await app.locator('a.arca-card-link').getAttribute('href'), 'https://app.example.test');
    const database = page.locator('.arca-node-database');
    const databaseText = await database.innerText();
    for (const expected of ['PostgreSQL', 'ready', '10.0.0.5:5432', '1 link']) assert.match(databaseText, new RegExp(expected.replace('.', '\\.')));
    assert.equal(await database.locator('a.arca-card-link').count(), 0);
    assert.doesNotMatch(await page.locator('.arca-stage').innerText(), /orphan|orders|customers/);
    assert.doesNotMatch(await page.locator('.arca-stage').innerText(), /arcak-svc|Arca services/i);
    assert.doesNotMatch(await page.locator('.arca-project-select').innerText(), /Plumbing/);

    assert.equal(await page.locator('.arca-panel [role="tab"]').count(), 2);
    assert.equal(await page.locator('.arca-panel-sub').innerText(), 'Read only · deepseek-flash');
    assert.equal(await page.locator('[role="tab"]').first().innerText(), 'Chat');
    assert.equal(await page.locator('[role="tab"]').nth(1).innerText(), 'Steps');
    assert.equal(await page.locator('[role="tab"]').first().getAttribute('aria-selected'), 'true');
    assert.match(await page.locator('.arca-chat-view').innerText(), /Ask the Doctor/);
    assert.match(await page.locator('.arca-chat-banner').innerText(), /Read only .* approve with your passkey/);
    const dom = await page.evaluate(() => `${document.documentElement.textContent}\n${document.documentElement.outerHTML}`);
    assert.doesNotMatch(dom, /arcak-svc/i, 'the internal project id never reaches the DOM');
    assert.doesNotMatch(dom, /DEEPSEEK_API_KEY|sk-[A-Za-z0-9]/);

    const composer = page.locator('.arca-composer-input');
    assert.equal(await composer.getAttribute('readonly'), null);
    assert.equal(await composer.getAttribute('placeholder'), 'Ask the Doctor about your machines');
    assert.equal(await page.locator('.arca-model').innerText(), 'deepseek-flash');
    assert.equal(await page.locator('.arca-send').getAttribute('aria-disabled'), 'true');
    await composer.fill('hello');
    assert.equal(await page.locator('.arca-send').getAttribute('aria-disabled'), 'false');

    await goSteps();
    assert.equal(await page.locator('.arca-panel-sub').innerText(), 'Recent activity · all machines');
    assert.equal(await page.locator('.arca-step').count(), 4);
    assert.match(await page.locator('.arca-step').first().innerText(), /rotate-token[\s\S]*beta[\s\S]*Applied/);
    await page.waitForFunction((id) => Boolean(document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)), CHAT_ID);
    assert.equal(await page.locator('.arca-steps-view .arca-proposal').count(), 12, 'the listed action plans, less the removed delete and the mismatched statement');
    assert.equal(await page.locator(`.arca-steps-view .arca-proposal[data-proposal="${DISALLOWED_ID}"]`).count(), 0, 'a database delete proposal is not rendered');
    assert.equal(await page.locator(`.arca-steps-view .arca-proposal[data-proposal="${MISMATCH_ID}"]`).count(), 0, 'a statement that does not match its entry is dropped');
    const approval = page.locator('.arca-steps-view .arca-approval-delete a');
    assert.equal(await approval.getAttribute('href'), fixture.approvalUrl);
    assert.match(await approval.innerText(), /Review approvals/);
    await goChat();
    assert.equal(await page.locator('.arca-chat-view').isVisible(), true);
  });

  await t.test('the chat streams an answer and one card per real tool call', async () => {
    await send('what is running on alpha?');
    await page.waitForFunction(() => [...document.querySelectorAll('.arca-msg-assistant .arca-msg-text')].some((node) => node.textContent.includes('Its CPU is 25%')));
    assert.equal(await page.locator('.arca-msg-user .arca-msg-text').last().innerText(), 'what is running on alpha?');
    assert.match(await page.locator('.arca-msg-assistant').last().innerText(), /Alpha is online\.[\s\S]*Its CPU is 25%\./);
    const tools = page.locator('.arca-chat-view .arca-tool');
    assert.equal(await tools.count(), 1);
    assert.match(await tools.first().innerText(), /arca_status[\s\S]*alpha[\s\S]*Answered from alpha/);
    assert.equal(await page.locator('.arca-send').getAttribute('aria-disabled'), 'true');
    assert.equal(await page.locator('.arca-thread').getAttribute('data-session'), 'Doctor');

    if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
      await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
      for (const theme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme: theme });
        await page.waitForTimeout(250);
        await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `canvas-chat-${theme}.png`) });
      }
      await page.emulateMedia({ colorScheme: 'light' });
    }
  });

  await t.test('a proposal card renders the kernel summary and every details row', async () => {
    await send('create nats');
    const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${CHAT_ID}"]`);
    await card.waitFor();
    assert.equal(await card.getAttribute('data-state'), 'waiting');
    assert.equal(await card.locator('.arca-approval-summary').innerText(), 'Create the service nats');
    assert.equal(await card.locator('.arca-approval-fact[data-detail="host"] dd').innerText(), 'alpha.example');
    const rows = await card.locator('.arca-approval-fact[data-detail="true"]').evaluateAll((nodes) => nodes.map((node) => [
      node.querySelector('dt').textContent, node.querySelector('dd').textContent,
    ]));
    assert.deepEqual(rows, [['Name', 'nats'], ['Template', 'nats']]);
    assert.equal(await card.locator('[data-technical="action"] dd').innerText(), 'service_create');
    assert.equal(await card.locator('[data-technical="target"] dd').innerText(), CHAT_ID);
    assert.equal(await card.locator('[data-technical="digest"] dd').innerText(), digestFor(CHAT_ID).slice(0, 12));
    assert.match(await card.locator('.arca-approval-expiry').innerText(), /^Expires in \d+:\d{2}$/);
    assert.match(await page.locator('.arca-msg-assistant').last().innerText(), /I can create the nats service/);
    assert.doesNotMatch(await card.innerText(), /delete the app database/);
    const polls = requests.filter((entry) => entry.method === 'GET' && entry.path === '/approve/plans').length;
    let refreshed = false;
    for (let attempt = 0; attempt < 20 && !refreshed; attempt++) {
      await page.waitForTimeout(50);
      refreshed = requests.filter((entry) => entry.method === 'GET' && entry.path === '/approve/plans').length > polls;
    }
    assert.ok(refreshed, 'an open card is refreshed with the snapshot poll');

    if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
      await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
      for (const theme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme: theme });
        await card.scrollIntoViewIfNeeded();
        await page.waitForTimeout(250);
        await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `approval-card-waiting-${theme}.png`) });
      }
      await page.emulateMedia({ colorScheme: 'light' });
    }
  });

  await t.test('approving posts the exact statement and proof and shows the result', async () => {
    const plan = approvalPlans.find((row) => row.id === CHAT_ID);
    assert.ok(plan?.statement, 'the fake kernel sent a statement');
    const challenge = createHash('sha256')
      .update(`arca-kernel-approval/v1\narca-kernel-approve\n${plan.statement}`)
      .digest();
    const proof = {
      alg: 'ES256',
      authenticatorData: b64url(Buffer.from([4, 5])),
      clientDataJSON: b64url(Buffer.from('{"type":"webauthn.get"}')),
      credentialId: b64url(Buffer.from([1, 2, 3])),
      kind: 'webauthn',
      signature: b64url(Buffer.from([6, 7])),
      v: 1,
    };
    const approval = `${b64(plan.statement)}.${b64(JSON.stringify(proof))}`;
    const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${CHAT_ID}"]`);
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-chat-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'done', CHAT_ID);
    assert.equal(approvalRequests.length, 1);
    assert.equal(approvalRequests[0].raw, JSON.stringify({ plan: CHAT_ID, approval }));
    const passkey = await page.evaluate(() => window.__arcaPasskey.calls.at(-1));
    assert.deepEqual(passkey.challenge, Array.from(challenge));
    assert.equal(passkey.rpId, '127.0.0.1');
    assert.equal(passkey.userVerification, 'required');
    assert.deepEqual(passkey.allowCredentials, [{ type: 'public-key', id: [1, 2, 3] }]);
    assert.match(await card.locator('.arca-approval-status').innerText(), /The service nats was created and is running/);

    if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
      await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
      for (const theme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme: theme });
        await card.scrollIntoViewIfNeeded();
        await page.waitForTimeout(250);
        await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `approval-card-done-${theme}.png`) });
      }
      await page.emulateMedia({ colorScheme: 'light' });
    }
  });

  await t.test('a contradicting model answer never changes what the card shows', async () => {
    await send('mismatch check');
    const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${HOSTILE_ID}"]`);
    await card.waitFor();
    const summary = 'Connect <img src=x onerror="window.__arcaXss=1"> to nats';
    assert.equal(await card.locator('.arca-approval-summary').textContent(), summary);
    assert.equal(await card.locator('.arca-approval-fact[data-detail="host"] dd').innerText(), 'alpha.example');
    const rows = await card.locator('.arca-approval-fact[data-detail="true"]').evaluateAll((nodes) => nodes.map((node) => [
      node.querySelector('dt').textContent, node.querySelector('dd').textContent,
    ]));
    assert.deepEqual(rows, [
      ['From', '<img src=x onerror="window.__arcaXss=1">'],
      ['To', 'nats'],
    ]);
    assert.match(await page.locator('.arca-msg-assistant').last().innerText(), /delete the app database and stop every host/);
    assert.doesNotMatch(await card.innerText(), /delete the app database/);
    assert.equal(await card.locator('img').count(), 0, 'hostile kernel markup is text, never an element');
    assert.equal(await page.evaluate(() => window.__arcaXss), undefined);
  });

  await t.test('declining posts the exact proposal id and leaves nothing changed', async () => {
    await goSteps();
    const card = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${EXTERNAL_ID}"]`);
    await card.waitFor();
    await card.locator('.arca-decline').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'declined', EXTERNAL_ID);
    assert.equal(declines.length, 1);
    assert.equal(declines[0].raw, JSON.stringify({ plan: EXTERNAL_ID }));
    assert.match(await card.locator('.arca-approval-status').innerText(), /Declined. Nothing changed/);
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Studio');

    // A new waiting row with the same id after the decline is waiting again, not stuck on Declined.
    await fetch(`${origin}/test/repropose`);
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'waiting', EXTERNAL_ID, { timeout: 10000 });
    assert.equal(await card.locator('.arca-approve').innerText(), 'Approve');
    assert.equal(await card.locator('.arca-approval-status').count(), 0);
  });

  await t.test('the host row and the statement decide which cards exist', async () => {
    await goSteps();
    // A waiting row whose statement does not name its own target is dropped by the card parser.
    assert.equal(await page.locator(`.arca-steps-view .arca-proposal[data-proposal="${MISMATCH_ID}"]`).count(), 0);
    // A foreign host's tool event never attaches this page kernel's row.
    await goChat();
    await send('foreign proposal');
    const foreign = page.locator('.arca-msg-assistant').last();
    assert.equal(await foreign.locator('.arca-proposal').count(), 0);
    assert.equal(await foreign.locator('.arca-approval-missing').count(), 0);
    // An id the kernel did not list draws no card, only the explicit note.
    await send('unknown proposal');
    const unknown = page.locator('.arca-msg-assistant').last();
    assert.equal(await unknown.locator('.arca-proposal').count(), 0);
    assert.match(await unknown.locator('.arca-approval-missing').innerText(), /The proposal is not waiting any more\./);
  });

  await t.test('a plan whose host changes re-renders the card', async () => {
    await goSteps();
    const card = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${HOST_ID}"]`);
    await card.waitFor();
    assert.equal(await card.locator('.arca-approval-fact[data-detail="host"] dd').innerText(), 'alpha.example');
    await fetch(`${origin}/test/host`);
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)
      ?.querySelector('.arca-approval-fact[data-detail="host"] dd')?.textContent === 'beta.example', HOST_ID, { timeout: 10000 });
  });

  await t.test('a proposal expires with no buttons', async () => {
    await goSteps();
    const expiring = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${EXPIRING_ID}"]`);
    await expiring.waitFor();
    await fetch(`${origin}/test/expire`);
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'expired', EXPIRING_ID, { timeout: 10000 });
    assert.match(await expiring.locator('.arca-approval-status').innerText(), /Expired. Nothing changed/);
    assert.equal(await expiring.locator('button').count(), 0);
  });

  await t.test('an accepted approval whose action refuses shows the action error', async () => {
    const failed = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${FAILED_ID}"]`);
    await failed.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'failed', FAILED_ID);
    const text = await failed.innerText();
    assert.match(text, /The change was approved, but Arca could not run it: The service nats: not found\./);
    assert.doesNotMatch(text, /did not accept this approval|Nothing changed/);
  });

  await t.test('a failed approval whose entry still waits becomes waiting with Retry', async () => {
    const card = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${RETRY_ID}"]`);
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'waiting', RETRY_ID);
    assert.match(await card.locator('.arca-card-note').innerText(), /Arca could not check the passkey's answer/);
    const retry = card.locator('.arca-approve');
    assert.equal(await retry.innerText(), 'Retry');
    await retry.click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'done', RETRY_ID);
    assert.match(await card.locator('.arca-approval-status').innerText(), /The service retry was created and is running/);
  });

  await t.test('a handler refusal after the passkey is spent shows approved but could not run', async () => {
    const card = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${REFUSAL_ID}"]`);
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'failed', REFUSAL_ID);
    const text = await card.innerText();
    assert.match(text, /The change was approved, but Arca could not run it: The service refusal was stopped\./);
    assert.doesNotMatch(text, /did not accept this approval|Try again/);
    assert.equal(await card.locator('.arca-approve').count(), 0, 'a spent approval has no retry');

    const empty = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${EMPTY_REFUSAL_ID}"]`);
    await empty.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'failed', EMPTY_REFUSAL_ID);
    const emptyText = await empty.innerText();
    assert.match(emptyText, /Arca could not finish this\. Check Steps to see whether it ran\./);
    assert.doesNotMatch(emptyText, /Try again|The change was approved|did not accept this approval/);
    assert.equal(await empty.locator('.arca-approve').count(), 0, 'an unreadable outcome offers no retry');
  });

  await t.test('NoSuchPlan says it may have been handled elsewhere, never Nothing changed', async () => {
    const card = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${GONE_ID}"]`);
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'failed', GONE_ID);
    const text = await card.innerText();
    assert.match(text, /This proposal is no longer waiting\. It may have been approved or declined elsewhere\. Check Steps\./);
    assert.doesNotMatch(text, /Nothing changed/);
  });

  await t.test('a poll that started before the click never reverts a done card to waiting', async () => {
    const card = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${STALE_ID}"]`);
    await card.waitFor();
    assert.equal(await card.getAttribute('data-state'), 'waiting');
    // Hold a /approve/plans answer that still carries this plan, then finish the card.
    const held = holdPlans();
    await held.captured;
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'done', STALE_ID);
    held.release();
    await page.waitForTimeout(300);
    assert.equal(await card.getAttribute('data-state'), 'done', 'the stale poll may not put the card back to waiting');
    assert.match(await card.locator('.arca-approval-status').innerText(), /The service stale was created and is running/);

    // A poll that starts after the card settled and finds the id listed again is a re-proposal.
    await fetch(`${origin}/test/repropose-stale`);
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'waiting', STALE_ID, { timeout: 10000 });
    assert.equal(await card.locator('.arca-approval-status').count(), 0);
    assert.equal(await card.locator('.arca-approve').innerText(), 'Approve');
  });

  await t.test('a hanging approval returns to waiting after the timeout', async () => {
    await page.evaluate(() => {
      const timeout = AbortSignal.timeout.bind(AbortSignal);
      Object.defineProperty(AbortSignal, 'timeout', {
        configurable: true,
        value: (ms) => timeout(ms === 30000 ? 250 : ms),
      });
    });
    const card = page.locator(`.arca-steps-view .arca-proposal[data-proposal="${HANG_ID}"]`);
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-steps-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'waiting', HANG_ID, { timeout: 5000 });
    assert.match(await card.locator('.arca-card-note').innerText(), /No answer from Arca, try again/);
    assert.equal(await card.locator('.arca-approve').innerText(), 'Retry');
  });

  await t.test('a cancelled passkey prompt returns the card to waiting', async () => {
    await goChat();
    const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${HOSTILE_ID}"]`);
    await card.waitFor();
    assert.equal(await card.getAttribute('data-state'), 'waiting');
    const before = approvalRequests.length;
    await page.evaluate(() => { window.__arcaPasskey.cancelNext = true; });
    await card.locator('.arca-approve').click();
    await page.waitForFunction((id) => document.querySelector(`.arca-chat-view .arca-proposal[data-proposal="${id}"]`)?.dataset.state === 'waiting', HOSTILE_ID);
    assert.equal(approvalRequests.length, before, 'a cancelled prompt posts nothing');
    assert.match(await card.locator('.arca-card-note').innerText(), /Passkey cancelled/);
    assert.equal(await card.locator('.arca-approve').isEnabled(), true);
  });

  await t.test('model markup renders as text and never as an element', async () => {
    await send('hostile markup please');
    await page.waitForFunction(() => [...document.querySelectorAll('.arca-msg-assistant .arca-msg-text')]
      .some((node) => node.textContent.includes('<img src=x onerror=')));
    const message = page.locator('.arca-msg-assistant .arca-msg-text').last();
    assert.match(await message.innerText(), /<img src=x onerror="window\.__arcaXss=1">/);
    assert.equal(await page.locator('.arca-chat-view img').count(), 0);
    assert.equal(await page.evaluate(() => window.__arcaXss), undefined);
  });

  await t.test('the budget card and the cut-off note render without a done event', async () => {
    await send('budget please');
    await page.waitForFunction(() => document.querySelector('.arca-budget')?.textContent?.includes('Daily budget reached, try tomorrow'));
    assert.match(await page.locator('.arca-budget').innerText(), /1000000 of 1000000 tokens used today/);
    await send('cut off now');
    await page.waitForFunction(() => document.querySelector('.arca-cutoff')?.textContent?.includes('answer was cut off'));
    const answer = page.locator('.arca-msg-assistant').last();
    assert.match(await answer.innerText(), /never finished[\s\S]*The answer was cut off/);
  });

  await t.test('401, 403 and 503 render error cards without a model call', async () => {
    await send('unauthorized');
    await page.waitForFunction(() => document.querySelector('.arca-error')?.textContent?.includes('Sign in to Arca'));
    assert.match(await page.locator('.arca-error').innerText(), /Sign in to Arca to chat with the Doctor/);
    await send('forbidden');
    await page.waitForFunction(() => document.querySelector('.arca-error')?.textContent?.includes('not allowed'));
    assert.match(await page.locator('.arca-error').innerText(), /not allowed to chat/);
    await send('unavailable');
    await page.waitForFunction(() => document.querySelector('.arca-error')?.textContent?.includes('not available'));
    assert.match(await page.locator('.arca-error').innerText(), /not available right now/);
  });

  await t.test('a null CPU load keeps the core count and draws no bar or zero', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host, index) => index !== 0 ? host : {
        ...host,
        metrics: { ...host.metrics, latest: { ...host.metrics.latest, cpu: { cores: 8, usedPct: null } } },
      }),
    };
    await page.waitForFunction(() => document.querySelector('.arca-machine .arca-meter-value')?.textContent === 'n/a / 8 cores');
    const card = page.locator('.arca-machine:not(.arca-placeholder)').first();
    assert.equal(await card.locator('.arca-meter-value').first().innerText(), 'n/a / 8 cores');
    assert.equal(await card.locator('.arca-meter').first().innerText().then((value) => value.includes('0%')), false);
    assert.equal(await card.locator('.arca-meter').first().locator('.arca-meter-fill').count(), 0);
    assert.equal(await card.locator('.arca-meter-fill').count(), 1, 'the real memory bar is unchanged');
    data = fixture;
    await page.waitForFunction(() => document.querySelector('.arca-machine .arca-meter-value')?.textContent === '2 / 8 cores');
  });

  await t.test('a remote host reason leaves the known core count visible', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host, index) => index !== 0 ? host : {
        ...host,
        metrics: {
          ...host.metrics,
          latest: {
            ...host.metrics.latest,
            cpu: { cores: 16, coresKnown: true, usedPct: null, reason: 'not available for a remote host' },
          },
        },
      }),
    };
    await page.waitForFunction(() => document.querySelector('.arca-machine .arca-meter-value')?.textContent === 'n/a / 16 cores');
    const card = page.locator('.arca-machine:not(.arca-placeholder)').first();
    assert.equal(await card.locator('.arca-meter-value').first().innerText(), 'n/a / 16 cores');
    assert.match(await card.innerText(), /16 cores/);
    assert.doesNotMatch(await card.innerText(), /cores n\/a/);
    assert.equal(await card.locator('.arca-meter').first().getAttribute('title'), 'not available for a remote host');
    assert.equal(await card.locator('.arca-meter').first().locator('.arca-meter-fill').count(), 0);
    assert.equal(await card.locator('.arca-meter-fill').count(), 1, 'the real memory bar is unchanged');
    data = fixture;
    await page.waitForFunction(() => document.querySelector('.arca-machine .arca-meter-value')?.textContent === '2 / 8 cores');
  });

  await t.test('cores n/a and Mem n/a keep the kernel reason as a tooltip', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host, index) => index !== 0 ? host : {
        ...host,
        metrics: {
          ...host.metrics,
          latest: {
            ...host.metrics.latest,
            cpu: { cores: 0, coresKnown: false, usedPct: null, reason: 'the Incus resources read failed' },
            memory: { totalBytes: null, availableBytes: null, usedBytes: 2_000_000_000, reason: 'the Incus resources read failed' },
          },
        },
      }),
    };
    await page.waitForFunction(() => document.querySelector('.arca-machine .arca-meter-value')?.textContent === 'cores n/a');
    const card = page.locator('.arca-machine:not(.arca-placeholder)').first();
    assert.equal(await card.locator('.arca-meter-value').first().innerText(), 'cores n/a');
    assert.equal(await card.locator('.arca-meter').first().getAttribute('title'), 'the Incus resources read failed');
    assert.equal(await card.locator('.arca-meter-value').nth(1).innerText(), 'n/a');
    assert.equal(await card.locator('.arca-meter').nth(1).getAttribute('title'), 'the Incus resources read failed');
    assert.equal(await card.locator('.arca-meter-fill').count(), 0);
    assert.doesNotMatch(await card.innerText(), /0 cores|0%/);
    data = fixture;
    await page.waitForFunction(() => document.querySelector('.arca-machine .arca-meter-value')?.textContent === '2 / 8 cores');
  });

  await t.test('the service Open link needs a real host address and a real port', async () => {
    await page.locator('.arca-project-select').selectOption('Web1');
    await page.waitForFunction(() => location.hash === '#project/Web1');
    const link = page.locator('.arca-node-instance a.arca-card-link');
    assert.equal(await link.getAttribute('href'), 'http://192.0.2.10:8080');
    assert.equal(await link.innerText(), 'Open');

    data = {
      ...fixture,
      hosts: fixture.hosts.map((host) => host.name === 'alpha'
        ? { ...host, address: '', services: [{ ...host.services[0], ports: [8080] }] }
        : host),
    };
    await page.waitForFunction(() => document.querySelector('.arca-node-instance a.arca-card-link') === null);
    assert.equal(await page.locator('.arca-node-instance a.arca-card-link').count(), 0);

    data = {
      ...fixture,
      hosts: fixture.hosts.map((host) => host.name === 'alpha'
        ? { ...host, address: '192.0.2.10', services: [{ ...host.services[0], lanPort: null }] }
        : host),
    };
    await page.waitForFunction(() => document.querySelector('.arca-node-instance') !== null);
    assert.equal(await page.locator('.arca-node-instance a.arca-card-link').count(), 0);
    assert.match(await page.locator('.arca-node-instance').innerText(), /web1/);
    data = fixture;
    await page.locator('.arca-project-select').selectOption('Studio');
    await page.waitForFunction(() => location.hash === '#project/Studio');
  });

  await t.test('project picker, hash and reload selection follow the configured default', async () => {
    assert.equal(new URL(page.url()).hash, '#project/Studio');
    await page.locator('.arca-project-select').selectOption('Automation');
    await page.waitForFunction(() => location.hash === '#project/Automation');
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Automation');
    assert.equal(await page.locator('.arca-node-domain').count(), 0);
    assert.equal(await page.locator('.arca-node-database').count(), 0);
    assert.equal(await page.locator('.arca-node-instance').count(), 1);
    assert.match(await page.locator('.arca-node-instance').innerText(), /queue[\s\S]*runs on beta/);
    assert.equal(await page.locator('.arca-canvas-chips .arca-chip').first().innerText(), '1 thing');

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForSelector('.arca-node-project');
    assert.equal(await page.locator('.arca-project-select').inputValue(), 'Automation');
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Automation');

    await page.evaluate(() => { location.hash = '#project/Tools'; });
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Tools');
    assert.equal(await page.locator('.arca-node-instance').count(), 1);
    assert.match(await page.locator('.arca-node-instance').innerText(), /rogue[\s\S]*runs on beta/);

    await page.evaluate(() => { location.hash = '#project/does-not-exist'; });
    await page.waitForFunction(() => location.hash === '#project/Studio');
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Studio');
    assert.equal(await page.locator('.arca-project-select').inputValue(), 'Studio');

    await page.locator('.arca-project-select').selectOption('Web1');
    await page.waitForFunction(() => location.hash === '#project/Web1');
    assert.equal(await page.locator('.arca-canvas-title').innerText(), 'Web1');
    assert.match(await page.locator('.arca-node-project .arca-nk').innerText(), /Service · alpha/);
    assert.match(await page.locator('.arca-canvas-chips').innerText(), /alpha · service/);
    assert.doesNotMatch(await page.locator('.arca-stage').innerText(), /arcak-svc|Arca services/i);
    await page.evaluate(() => { location.hash = '#project/Studio'; });
    await page.waitForFunction(() => location.hash === '#project/Studio');
  });

  await t.test('missing journal and metrics degrade without invented rows', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host) => ({
        ...host, metrics: null, journal: [], journalKnown: false, plans: [],
      })),
    };
    await goSteps();
    await page.waitForFunction(() => document.querySelector('.arca-steps-view')?.textContent?.includes('Activity journal unavailable'));
    assert.equal(await page.locator('.arca-step').count(), 0);
    await goChat();
    assert.equal(await page.locator('.arca-machine').first().innerText().then((value) => value.includes('No metrics')), true);
    assert.equal(await page.locator('.arca-summary').innerText(), '2 machines');
    data = fixture;
    await goSteps();
    await page.waitForFunction(() => document.querySelectorAll('.arca-step').length === 4);
    await goChat();
  });

  await t.test('a nameless internal target is labelled and never leaks its project id', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host, index) => ({
        ...host,
        journal: index === 0
          ? [{ id: 99, entry: 'create-app', target: { kind: 'instance', project: 'arcak-svc' }, state: 'Intent', at_ms: 1794160200000 }]
          : [],
      })),
    };
    await goSteps();
    await page.waitForFunction(() => document.querySelector('.arca-step')?.textContent?.includes('instance in Arca service'));
    assert.match(await page.locator('.arca-step').first().innerText(), /instance in Arca service[\s\S]*Intent/);
    const dom = await page.evaluate(() => `${document.documentElement.textContent}\n${document.documentElement.outerHTML}`);
    assert.doesNotMatch(dom, /arcak-svc/i);
    data = fixture;
    await goChat();
  });

  await t.test('a named service ACL target renders as a firewall and never as arcak-svc', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host, index) => ({
        ...host,
        journal: index === 0
          ? [{ id: 101, entry: 'open-automagik', target: 'arcak-svc-svc-automagik', state: 'Applied', at_ms: 1794160300000 }]
          : [],
      })),
    };
    await goSteps();
    await page.waitForFunction(() => document.querySelector('.arca-step')?.textContent?.includes('Automagik firewall'));
    assert.match(await page.locator('.arca-step').first().innerText(), /Automagik firewall[\s\S]*Applied/);
    const dom = await page.evaluate(() => `${document.documentElement.textContent}\n${document.documentElement.outerHTML}`);
    assert.doesNotMatch(dom, /arcak-svc/i);
    data = fixture;
    await goChat();
  });

  await t.test('a deep journal catch-up reads the latest activity instead of claiming an empty journal', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host) => ({ ...host, journal: [], journalKnown: true, journalPending: true })),
    };
    await goSteps();
    await page.waitForFunction(() => document.querySelector('.arca-steps-view')?.textContent?.includes('Reading the latest activity'));
    assert.equal(await page.locator('.arca-step').count(), 0);
    data = fixture;
    await goChat();
  });

  await t.test('a failed host report marks its owner members unknown', async () => {
    data = {
      ...fixture,
      projects: [{ name: 'Unknown host', members: [{ host: 'beta', incusProject: 'sandbox', instance: 'api' }] }],
      defaultProject: 'Unknown host',
      hosts: fixture.hosts.map((host) => host.name === 'beta'
        ? { ...host, instancesKnown: false, instances: [] }
        : host),
    };
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Unknown host');
    const projectCard = page.locator('.arca-node-project');
    assert.match(await projectCard.innerText(), /unknown[\s\S]*host data unavailable/i);
    assert.doesNotMatch(await projectCard.innerText(), /not found|Nothing live yet/i);
    assert.match(await page.locator('.arca-node-empty').innerText(), /Host data unavailable/);
    assert.doesNotMatch(await page.locator('.arca-node-empty').innerText(), /No live things/);
    data = fixture;
    await page.evaluate(() => { location.hash = '#project/Studio'; });
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Studio');
  });

  await t.test('hostile snapshot strings render as text and never as markup', async () => {
    const hostile = '<a href="javascript:window.__arcaXss=1">x</a>';
    data = {
      ...fixture,
      projects: [{ name: 'Hostile', members: [{ host: 'alpha', incusProject: hostile, instance: 'hostile-node' }] }],
      defaultProject: 'Hostile',
      hosts: fixture.hosts.map((host) => host.name === 'alpha' ? {
        ...host,
        projects: [hostile],
        instances: [...host.instances, { name: 'hostile-node', project: hostile, kind: 'container', status: 'Running' }],
        journal: [{ id: 9, entry: hostile, target: { kind: hostile, name: hostile }, state: 'Applied', at_ms: 1794160200000 }],
      } : host),
    };
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Hostile');
    assert.equal(await page.evaluate(() => window.__arcaXss), undefined);
    assert.equal(await page.locator('.arca-stage a[href^="javascript:"]').count(), 0);
    assert.equal(await page.locator('.arca-stage img').count(), 0);
    await goSteps();
    await page.waitForFunction((value) => document.querySelector('.arca-step')?.textContent?.includes(value), hostile);
    assert.equal(await page.locator('.arca-step').filter({ hasText: hostile }).count(), 1);
    data = fixture;
    await goChat();
    await page.evaluate(() => { location.hash = '#project/Studio'; });
    await page.waitForFunction(() => document.querySelector('.arca-canvas-title')?.textContent === 'Studio');
  });

  await t.test('light and dark theme tokens and the settled canvas rectangles', async () => {
    const measurements = {};
    for (const [theme, expected] of [
      ['light', { bg: 'rgb(244, 244, 247)', surface: 'rgb(255, 255, 255)', ink: 'rgb(12, 13, 17)', muted: 'rgb(93, 97, 112)' }],
      ['dark', { bg: 'rgb(11, 11, 15)', surface: 'rgb(21, 22, 28)', ink: 'rgb(241, 242, 246)', muted: 'rgb(164, 169, 183)' }],
    ]) {
      await page.emulateMedia({ colorScheme: theme });
      await page.waitForTimeout(250);
      const actual = await page.evaluate(() => {
        const style = (selector) => getComputedStyle(document.querySelector(selector));
        return {
          bg: style('.arca-canvas').backgroundColor,
          surface: style('.arca-topbar').backgroundColor,
          ink: style('.arca-canvas-title').color,
          muted: style('.arca-panel-sub').color,
          panel: style('.arca-panel').backgroundColor,
          composer: style('.arca-composer-box').borderTopColor,
          colorScheme: style('.arca-canvas').colorScheme,
          rectangles: Object.fromEntries([
            '.arca-topbar', '.arca-machines', '.arca-stage', '.arca-panel',
            '.arca-world', '.arca-node-project', '.arca-node-instance', '.arca-wire', '.arca-composer',
          ].map((selector) => [selector, document.querySelector(selector).getBoundingClientRect().toJSON()])),
          tag: (() => {
            const svg = document.querySelector('.arca-foot svg[aria-label="Open Intelligence badge"]');
            const pill = getComputedStyle(svg.querySelector('rect'));
            const words = svg.querySelector('text');
            return {
              count: document.querySelectorAll('svg[aria-label="Open Intelligence badge"]').length,
              box: svg.getBoundingClientRect().toJSON(),
              foot: document.querySelector('.arca-foot').getBoundingClientRect().toJSON(),
              ground: pill.fill, line: pill.stroke, ink: getComputedStyle(words).fill, words: words.textContent,
              wordsRight: words.getBoundingClientRect().right,
              link: Boolean(svg.closest('a, button')), anchors: document.querySelectorAll('.arca-foot a, .arca-foot button').length,
            };
          })(),
          nodes: {
            project: document.querySelectorAll('.arca-node-project').length,
            instances: document.querySelectorAll('.arca-node-instance').length,
            databases: document.querySelectorAll('.arca-node-database').length,
            domain: document.querySelectorAll('.arca-node-domain').length,
            wires: document.querySelectorAll('.arca-wire').length,
          },
        };
      });
      assert.equal(actual.bg, expected.bg, `${theme} background`);
      assert.equal(actual.surface, expected.surface, `${theme} surface`);
      assert.equal(actual.ink, expected.ink, `${theme} ink`);
      assert.equal(actual.muted, expected.muted, `${theme} muted`);
      assert.equal(actual.panel, expected.surface, `${theme} panel surface`);
      assert.equal(actual.colorScheme, 'light dark');
      assert.deepEqual(actual.nodes, { project: 1, instances: 3, databases: 1, domain: 1, wires: 5 });
      const topbar = actual.rectangles['.arca-topbar'];
      const machines = actual.rectangles['.arca-machines'];
      const stage = actual.rectangles['.arca-stage'];
      const panel = actual.rectangles['.arca-panel'];
      assert.deepEqual([topbar.width, topbar.height], [1440, 56]);
      assert.ok(machines.width >= 1000 && machines.height >= 140);
      assert.ok(stage.width >= 1000 && stage.height >= 500);
      assert.equal(panel.width, 396);
      assert.equal(Math.round(topbar.bottom), Math.round(panel.top));
      assert.ok(actual.rectangles['.arca-node-instance'].width >= 300);
      assert.equal(Math.round(actual.rectangles['.arca-composer'].bottom), 900);
      // The Open Intelligence tag: one picture, 170 px, on this theme's ground and ink, at the foot of the canvas
      // column; the Doctor panel and its composer keep the place asserted above.
      assert.equal(actual.tag.count, 1);
      assert.deepEqual([actual.tag.box.width, actual.tag.box.height], [170, 32]);
      assert.equal(actual.tag.ground, expected.bg, `${theme} tag ground`);
      assert.equal(actual.tag.line, expected.ink, `${theme} tag outline`);
      assert.equal(actual.tag.ink, expected.ink, `${theme} tag words`);
      assert.equal(actual.tag.words, 'OPEN INTELLIGENCE');
      assert.ok(actual.tag.wordsRight < actual.tag.box.right, 'the words stay inside the pill');
      assert.deepEqual([actual.tag.link, actual.tag.anchors], [false, 0]);
      assert.equal(Math.round(actual.tag.foot.bottom), 900);
      assert.equal(Math.round(actual.tag.foot.right), Math.round(panel.left));
      assert.equal(Math.round(actual.tag.foot.top), Math.round(stage.bottom));
      assert.ok(actual.tag.box.width < topbar.width && actual.tag.box.height < topbar.height);
      const contrast = (Math.max(luminance(actual.muted), luminance(actual.panel)) + 0.05)
        / (Math.min(luminance(actual.muted), luminance(actual.panel)) + 0.05);
      assert.ok(contrast >= 4.5, `${theme} muted contrast ${contrast}`);
      measurements[theme] = { viewport: { width: 1440, height: 900 }, ...actual, contrast };
      if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
        await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
        await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `canvas-v28-${theme}.png`) });
      }
    }
    if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
      await writeFile(join(process.env.ARCA_CANVAS_EVIDENCE_DIR, 'rectangles.json'), `${JSON.stringify(measurements, null, 2)}\n`);
    }
    await page.emulateMedia({ colorScheme: 'light' });
  });

  await t.test('requests stay read-only and the stream never uses innerHTML', async () => {
    assert.ok(requests.length > 0);
    const writes = requests.filter((entry) => entry.method !== 'GET');
    assert.ok(writes.length > 0);
    assert.ok(writes.every((entry) => entry.method === 'POST' && ['/arca-canvas/chat', '/approve/approve', '/approve/decline'].includes(entry.path)));
    assert.doesNotMatch(pageJs.toString(), /\.innerHTML\b/);
    assert.ok(pageJs.length < 2 * 1024 * 1024);
    assert.equal(pageJs.toString().includes('DEEPSEEK_API_KEY'), false);
    assert.equal(pageJs.toString().includes('deepseek.env'), false);
    await page.locator('.arca-project-select').focus();
    assert.equal(await page.locator('.arca-project-select').evaluate((node) => node === document.activeElement), true);
    await page.keyboard.press('ArrowDown');
    assert.deepEqual(errors, []);
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.arca-panel');
  const stacked = await page.evaluate(() => ({
    machines: document.querySelector('.arca-machines').getBoundingClientRect().toJSON(),
    stage: document.querySelector('.arca-stage').getBoundingClientRect().toJSON(),
    panel: document.querySelector('.arca-panel').getBoundingClientRect().toJSON(),
    foot: document.querySelector('.arca-foot').getBoundingClientRect().toJSON(),
    tag: document.querySelector('.arca-foot svg').getBoundingClientRect().toJSON(),
  }));
  assert.ok(stacked.foot.top >= stacked.panel.bottom - 1, 'the Open Intelligence tag closes the stacked page');
  assert.deepEqual([stacked.tag.width, stacked.tag.height], [170, 32]);
  assert.ok(stacked.panel.top >= stacked.stage.bottom - 1, 'panel stacks below the canvas');
  assert.ok(stacked.stage.top >= stacked.machines.bottom - 1, 'canvas follows the machines strip');
});
