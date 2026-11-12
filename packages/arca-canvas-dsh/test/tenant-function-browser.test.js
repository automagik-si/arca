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
const DIGEST = `sha256:${'a'.repeat(64)}`;

// The kernel's own card for each verb (crates/arca/src/action_proposal.rs and function.rs): the summary, the rows,
// and the verb's result; `done` is what the finished card says. Every value is synthetic.
const CARDS = [
  {
    id: 'act-bbbb00000001', action: 'tenant_create', destructive: false,
    summary: 'Create a tenant',
    args: {},
    details: [{ label: 'Caps', value: 'this node\'s defaults' }],
    result: { id: 'abcd2345', block: '198.51.100.0/28' },
    done: 'The tenant abcd2345 was created.',
  },
  {
    id: 'act-bbbb00000002', action: 'tenant_remove', destructive: true,
    summary: 'Remove the tenant abcd2345 and everything in it',
    args: { id: 'abcd2345' },
    details: [
      { label: 'Tenant', value: 'abcd2345' },
      { label: 'Effect', value: 'Deletes every service of this tenant, with its data, and then the tenant\'s project.' },
    ],
    result: { removed: true },
    done: 'The tenant and everything in it were removed.',
  },
  {
    id: 'act-bbbb00000003', action: 'deploy_remove', destructive: true,
    summary: 'Remove everything the deploy shop-1 created, in tenant abcd2345',
    args: { tenant: 'abcd2345', deploy: 'shop-1' },
    details: [
      { label: 'Deploy', value: 'shop-1' },
      { label: 'Parts', value: 'web, db' },
      { label: 'Effect', value: 'Deletes every service this deploy created in the tenant, with its data. Nothing else of the tenant is touched.' },
      { label: 'Tenant', value: 'abcd2345' },
    ],
    result: { removed: true, deploy: 'shop-1', parts: ['web', 'db'] },
    done: 'Everything the deploy shop-1 created was removed (2 parts).',
  },
  {
    id: 'act-bbbb00000004', action: 'function_create', destructive: false,
    summary: 'Create the function relay from registry.example/acme/relay and run it every 5 minutes',
    args: { name: 'relay', image: 'registry.example/acme/relay', digest: DIGEST, triggers: [{ timer: { everySeconds: 300 } }], secrets: ['RELAY_KEY'] },
    details: [
      { label: 'Function', value: 'relay' },
      { label: 'Program', value: `registry.example/acme/relay@${DIGEST}` },
      { label: 'Runs', value: 'every 5 minutes' },
      { label: 'Secrets', value: 'RELAY_KEY' },
    ],
    result: {
      name: 'relay', kind: 'container', image: 'registry.example/acme/relay', digest: DIGEST, command: [], env: {},
      secrets: [{ name: 'RELAY_KEY', set: false }], links: [], triggers: [], limits: {}, concurrency: 1, ipv4: '198.51.100.20',
    },
    done: 'The function relay was created. 1 secret still to be set.',
  },
  {
    id: 'act-bbbb00000005', action: 'function_remove', destructive: true,
    summary: 'Remove the function relay',
    args: { name: 'relay' },
    details: [
      { label: 'Function', value: 'relay' },
      { label: 'Effect', value: 'Stops the run that is going, if one is, and deletes the function with its secrets. Its lines in the run log are kept.' },
    ],
    result: { removed: true },
    done: 'The function was removed.',
  },
];
// A verb the canvas does not list is never offered a card, whatever the kernel sends.
const UNKNOWN = {
  id: 'act-bbbb00000006', action: 'function_secret_set', destructive: false,
  summary: 'Set a secret of the function relay', args: { name: 'relay' }, details: [],
};

function machine(name) {
  return {
    name,
    address: '192.0.2.10',
    at: '2026-11-12T12:00:00.000Z',
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

function plan({ id, action, summary, args, details }) {
  const now = Date.now();
  const row = {
    kind: 'action',
    id,
    action,
    summary,
    args,
    argsDigest: `${id.slice(4)}${'0'.repeat(52)}`,
    createdAt: now,
    expiresAt: now + 15 * 60 * 1000,
    details,
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

test('a tenant, a deploy and a function change each draw their card, a removal in the alert colour, and finish in their own words', async (t) => {
  let plans = [...CARDS, UNKNOWN].map(plan);
  const approvals = [];
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/arca-canvas') { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(html); return; }
    if (path === '/arca-canvas/page.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end(pageJs); return; }
    if (path === '/arca-canvas/page.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end(pageCss); return; }
    if (path === '/arca-canvas/snapshot') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        at: '2026-11-12T12:00:00.000Z',
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
      res.end(JSON.stringify({ plan: payload.plan, id: payload.plan, action: card.action, result: card.result }));
      return;
    }
    if (path === '/arca-canvas/chat' && req.method === 'POST') {
      await body(req);
      res.writeHead(200, { 'Content-Type': 'text/event-stream; charset=utf-8', 'Cache-Control': 'no-store' });
      const events = [
        ['start', { session: 'Doctor' }],
        ...[...CARDS, UNKNOWN].map(({ id }, index) => ['tool', {
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
  await page.emulateMedia({ colorScheme: 'light' });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${server.address().port}/arca-canvas`);

  await page.locator('.arca-composer-input').fill('make a tenant and a function, and remove the old ones');
  await page.locator('.arca-send').click();
  await page.locator(`.arca-chat-view .arca-proposal[data-proposal="${CARDS.at(-1).id}"]`).waitFor();
  assert.equal(await page.locator('.arca-chat-view .arca-proposal').count(), CARDS.length, 'an unlisted verb has no card');
  assert.equal(await page.locator(`.arca-proposal[data-proposal="${UNKNOWN.id}"]`).count(), 0);

  // The alert colour of the light theme (--arca-bad), and what an ordinary card's Approve button has instead.
  const alert = 'rgb(212, 52, 58)';
  for (const entry of CARDS) {
    const card = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${entry.id}"]`);
    assert.equal(await card.getAttribute('data-state'), 'waiting', entry.action);
    assert.equal(await card.locator('.arca-approval-summary').innerText(), entry.summary);
    const rows = await card.locator('.arca-approval-fact[data-detail="true"]').evaluateAll((nodes) => nodes.map((node) => [
      node.querySelector('dt').textContent, node.querySelector('dd').textContent,
    ]));
    assert.deepEqual(rows, entry.details.map(({ label, value }) => [label, value]), entry.action);
    assert.equal(await card.locator('.arca-approval-fact[data-technical="action"] dd').textContent(), entry.action);
    assert.equal(await card.locator('.arca-approve').isEnabled(), true, entry.action);
    assert.equal(await card.locator('.arca-decline').isEnabled(), true, entry.action);
    assert.equal(await card.getAttribute('data-destructive'), entry.destructive ? 'true' : null, entry.action);
    const colours = await card.evaluate((node) => ({
      border: getComputedStyle(node).borderTopColor,
      summary: getComputedStyle(node.querySelector('.arca-approval-summary')).color,
      approve: getComputedStyle(node.querySelector('.arca-approve')).backgroundColor,
      approveText: getComputedStyle(node.querySelector('.arca-approve')).color,
    }));
    if (entry.destructive) {
      assert.equal(colours.approve, alert, entry.action);
      assert.equal(colours.summary, alert, entry.action);
      assert.equal(colours.approveText, 'rgb(255, 255, 255)', entry.action);
    } else {
      assert.notEqual(colours.approve, alert, entry.action);
      assert.notEqual(colours.summary, alert, entry.action);
    }
  }
  // A removal's border differs from an ordinary card's.
  const borders = await page.locator('.arca-chat-view .arca-proposal').evaluateAll((nodes) => nodes.map((node) => [
    node.dataset.destructive === 'true', getComputedStyle(node).borderTopColor,
  ]));
  const plain = new Set(borders.filter(([removal]) => !removal).map(([, colour]) => colour));
  const removal = new Set(borders.filter(([removal]) => removal).map(([, colour]) => colour));
  assert.equal(plain.size, 1);
  assert.equal(removal.size, 1);
  assert.notEqual([...plain][0], [...removal][0]);

  if (process.env.ARCA_CANVAS_EVIDENCE_DIR) {
    await mkdir(process.env.ARCA_CANVAS_EVIDENCE_DIR, { recursive: true });
    const removalCard = page.locator(`.arca-chat-view .arca-proposal[data-proposal="${CARDS[2].id}"]`);
    for (const theme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: theme });
      await removalCard.scrollIntoViewIfNeeded();
      await page.waitForTimeout(250);
      await page.screenshot({ path: join(process.env.ARCA_CANVAS_EVIDENCE_DIR, `canvas-tenant-function-${theme}.png`) });
    }
    await page.emulateMedia({ colorScheme: 'light' });
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
