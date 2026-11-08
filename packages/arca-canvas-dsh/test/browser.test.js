import assert from 'node:assert/strict';
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
  if (message.includes('delete')) {
    return {
      status: 200,
      events: [
        ['start', { session: 'Doctor' }],
        ['approval', { url: 'https://example.com/approve', detail: 'This chat is read-only. Review the change and approve it with your passkey.' }],
        ['text', { text: 'That would delete the app database from the arcak-svc project. Approve it at https://example.com/approve.' }],
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

test('v28 layout binds both hosts, the Doctor chat and the activity steps', async (t) => {
  let data = fixture;
  const requests = [];
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
    assert.match(await page.locator('.arca-chat-banner').innerText(), /Read only .* changes need your passkey/);
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
    const approval = page.locator('.arca-steps-view .arca-approval a');
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

  await t.test('a write request answers what it would do and links the passkey page', async () => {
    await send('delete the app database');
    await page.waitForFunction(() => [...document.querySelectorAll('.arca-msg-assistant .arca-msg-text')].some((node) => node.textContent.includes('would delete')));
    const approval = page.locator('.arca-msg-assistant .arca-approval a');
    assert.equal(await approval.getAttribute('href'), 'https://example.com/approve');
    assert.match(await page.locator('.arca-msg-assistant .arca-approval').innerText(), /read-only/i);
    assert.match(await page.locator('.arca-msg-assistant').last().innerText(), /Approve it at https:\/\/example\.com\/approve/);
    const dom = await page.evaluate(() => document.documentElement.textContent);
    assert.doesNotMatch(dom, /arcak-svc/i, 'the model cannot leak the internal project id into the DOM');
    assert.match(await page.locator('.arca-msg-assistant').last().innerText(), /Arca service project/);
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

  await t.test('a null CPU load renders n/a with no bar and never zero', async () => {
    data = {
      ...fixture,
      hosts: fixture.hosts.map((host, index) => index !== 0 ? host : {
        ...host,
        metrics: { ...host.metrics, latest: { ...host.metrics.latest, cpu: { cores: 8, usedPct: null } } },
      }),
    };
    await page.waitForFunction(() => document.querySelector('.arca-machine .arca-meter-value')?.textContent === 'n/a');
    const card = page.locator('.arca-machine:not(.arca-placeholder)').first();
    assert.equal(await card.locator('.arca-meter-value').first().innerText(), 'n/a');
    assert.equal(await card.locator('.arca-meter').first().innerText().then((value) => value.includes('0%')), false);
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
            cpu: { cores: 0, usedPct: null, reason: 'the Incus resources read failed' },
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
    assert.equal(await page.locator('.arca-steps-view .arca-approval').count(), 0);
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
    assert.ok(writes.every((entry) => entry.method === 'POST' && entry.path === '/arca-canvas/chat'));
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
  }));
  assert.ok(stacked.panel.top >= stacked.stage.bottom - 1, 'panel stacks below the canvas');
  assert.ok(stacked.stage.top >= stacked.machines.bottom - 1, 'canvas follows the machines strip');
});
