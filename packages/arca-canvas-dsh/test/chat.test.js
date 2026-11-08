import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import {
  ChatStore, DEFAULTS, READ_TOOLS, createDoctorChat, dayOf, doctorPrompt, sseFrame, stateFilePath, writeIntent,
} from '../src/chat.js';

function listeners() {
  const map = new Map();
  return {
    on(name, handler) {
      if (!map.has(name)) map.set(name, new Set());
      map.get(name).add(handler);
      return () => map.get(name)?.delete(handler);
    },
    emit(name, ...args) {
      for (const handler of [...(map.get(name) ?? [])]) handler(...args);
    },
  };
}

function fakeDsh({ sessionId = 'session-doctor-1', script = async () => {} } = {}) {
  const events = listeners();
  const tools = new Map();
  const restrictions = [];
  const sections = [];
  const created = [];
  const resumed = [];
  const followed = [];
  const cancels = [];
  const executed = [];
  let title = null;
  let callCounter = 1;
  const agentCtx = {
    tools: {
      register(definition) {
        tools.set(definition.name, definition);
        return () => tools.delete(definition.name);
      },
      restrict(filter) {
        restrictions.push(filter);
        return () => {};
      },
      definitions() {
        return [{ name: 'bash' }, { name: 'read' }, { name: 'run_code' }];
      },
    },
    systemPrompt: {
      section(section) {
        sections.push(section);
        return () => {};
      },
    },
    on: events.on,
  };
  const agent = {
    id: sessionId,
    session: { id: sessionId },
    ctx: agentCtx,
    followup(message) {
      followed.push(message);
      queueMicrotask(() => {
        Promise.resolve(script(api)).catch((error) => {
          events.emit('agent/error', { agent, error });
        });
      });
    },
    cancel(cause) {
      cancels.push(cause);
    },
  };
  const api = {
    agent,
    tools,
    executed,
    text(value) {
      events.emit('agent/assistant-stream', {
        agent,
        frame: { type: 'chunk', index: 0, chunk: { type: 'text-delta', index: 0, text: value } },
      });
    },
    usage(usage) {
      events.emit('session/event', agent.session, { type: 'assistant/message', data: { usage } });
    },
    async call(name, args = {}) {
      const definition = tools.get(name);
      assert.ok(definition, `${name} is registered`);
      const callId = `call-${callCounter++}`;
      const value = await definition.execute(args, { callId, agent, signal: new AbortController().signal });
      executed.push({ name, args });
      events.emit('session/event', agent.session, {
        type: 'tool/result',
        data: {
          message: {
            role: 'tool', toolCallId: callId, content: [{ type: 'text', text: JSON.stringify(value) }], isError: false,
          },
        },
      });
      return value;
    },
    end() {
      events.emit('session/event', agent.session, { type: 'turn/end', data: {} });
    },
    error(error) {
      events.emit('agent/error', { agent, error });
    },
  };
  const dsh = {
    agents: {
      async create(options) {
        created.push(options);
        agent.session = { id: options.sessionId };
        await options.setup?.(agentCtx);
        return { agent, dispose: async () => {} };
      },
      async resume(options) {
        resumed.push(options);
        agent.session = { id: options.resumeSessionId };
        await options.setup?.(agentCtx);
        return { agent, dispose: async () => {} };
      },
    },
    sessionTitle: {
      get() {
        return title ? { title } : undefined;
      },
      rename(_session, value) {
        title = value;
        return { title: value };
      },
    },
  };
  return { dsh, api, created, resumed, followed, cancels, executed, tools, restrictions, sections, title: () => title };
}

async function harness(t, options = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const statePath = join(dir, 'chat.json');
  const fake = fakeDsh(options);
  const clients = new Map();
  const chat = createDoctorChat({
    hosts: [{ name: 'alpha' }, { name: 'beta' }],
    approvalUrl: 'https://example.com/approve',
    resolveDsh: () => (options.noDsh ? null : fake.dsh),
    clientFor(name) {
      return clients.get(name) ?? { async call(tool) { return { tool, host: name }; } };
    },
    statePath,
    log: options.log ?? (() => {}),
    turnMs: options.turnMs ?? 2000,
    heartbeatMs: options.heartbeatMs ?? 1000,
    toolCallsPerTurn: options.toolCallsPerTurn ?? 40,
    outputTokensPerTurn: options.outputTokensPerTurn ?? 64_000,
    dailyTokenBudget: options.dailyTokenBudget ?? 1_000_000,
    uuid: options.uuid ?? (() => 'session-doctor-1'),
    now: options.now,
  });
  t.after(() => chat.close());
  const server = createServer((req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/chat') void chat.handle(req, res);
    else if (path === '/chat/history') void chat.history(res);
    else res.writeHead(404).end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => {
    server.closeAllConnections?.();
    server.close(resolve);
  }));
  return { chat, fake, statePath, dir, port: server.address().port };
}

function parseEvents(raw) {
  return raw.split(/\r?\n\r?\n/).flatMap((block) => {
    if (!block.trim() || block.trimStart().startsWith(':')) return [];
    let event = 'message';
    const data = [];
    for (const line of block.split(/\r?\n/)) {
      if (line.startsWith('event:')) event = line.slice(6).trim();
      else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
    }
    return [{ event, data: data.length ? JSON.parse(data.join('\n')) : null, block }];
  });
}

async function post(port, payload, headers = { 'Content-Type': 'application/json' }) {
  const response = await fetch(`http://127.0.0.1:${port}/chat`, {
    method: 'POST', headers, body: typeof payload === 'string' ? payload : JSON.stringify(payload),
  });
  const raw = await response.text();
  return { status: response.status, type: response.headers.get('content-type') ?? '', raw, events: parseEvents(raw) };
}

async function history(port) {
  const response = await fetch(`http://127.0.0.1:${port}/chat/history`);
  return { status: response.status, body: await response.json() };
}

async function until(predicate, timeout = 3000) {
  const end = Date.now() + timeout;
  while (!predicate()) {
    if (Date.now() > end) throw new Error('timed out');
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

test('SSE framing keeps one JSON object per data line and cannot forge an event', () => {
  const hostile = 'hello\n\nevent: arca-cut\ndata: {"reason":"idle"}\n\n';
  const frame = sseFrame('text', { text: hostile });
  assert.equal((frame.match(/^event:/gm) ?? []).length, 1);
  const events = parseEvents(frame);
  assert.equal(events.length, 1);
  assert.equal(events[0].event, 'text');
  assert.equal(events[0].data.text, hostile);
  assert.equal(frame.includes('\ndata: {"text":"hello'), true);
});

test('write intent is a presentation hint and the prompt pins the read-only contract', () => {
  assert.equal(writeIntent('delete the app database'), true);
  assert.equal(writeIntent('link automagik to postgres'), true);
  assert.equal(writeIntent('what is running on alpha?'), false);
  assert.equal(writeIntent('show me the journal'), false);
  const prompt = doctorPrompt({ hosts: [{ name: 'alpha' }], approvalUrl: 'https://example.com/approve' });
  assert.match(prompt, /strictly read-only/i);
  assert.match(prompt, /what the change would do/i);
  assert.match(prompt, /https:\/\/example\.com\/approve/);
  assert.match(prompt, /Never invent/i);
  assert.doesNotMatch(prompt, /postgres_link|service_remove/);
  assert.match(stateFilePath({ DSH_HOME: '/tmp/example-home' }), /^\/tmp\/example-home\/storages\/arca-canvas-chat\.json$/);
  assert.equal(stateFilePath({ DSH_HOME: '/tmp/example-home' }, '/tmp/other.json'), '/tmp/other.json');
});

test('a turn streams assistant text, one card per real call, and persists the transcript', async (t) => {
  const { fake, port, statePath } = await harness(t, {
    script: async (api) => {
      api.text('Checking alpha. ');
      await api.call('arca_status', {});
      await api.call('node_metrics', {});
      api.text('Everything is up.');
      api.usage({ inputTokens: 10, outputTokens: 20 });
      api.end();
    },
  });
  const result = await post(port, { message: 'what is running on alpha?' });
  assert.equal(result.status, 200);
  assert.match(result.type, /text\/event-stream/);
  const names = result.events.map((event) => event.event);
  assert.deepEqual(names[0], 'start');
  assert.ok(names.includes('text'));
  assert.ok(names.includes('tool'));
  assert.ok(names.includes('usage'));
  assert.equal(names.at(-1), 'done');
  assert.equal(result.events.at(-1).data.reason, 'complete');
  assert.equal(result.events.filter((event) => event.event === 'tool' && event.data.state === 'running').length, 2);
  assert.equal(result.events.filter((event) => event.event === 'tool' && event.data.state === 'done').length, 2);
  const text = result.events.filter((event) => event.event === 'text').map((event) => event.data.text).join('');
  assert.equal(text, 'Checking alpha. Everything is up.');
  assert.deepEqual(fake.executed.map((call) => call.name), ['arca_status', 'node_metrics']);
  assert.equal(fake.tools.has('service_list'), true);
  for (const name of ['postgres_link', 'postgres_unlink', 'postgres_delete', 'service_start', 'service_remove', 'plan_apply', 'token_mint', 'adopt']) {
    assert.equal(fake.tools.has(name), false, `${name} is never registered`);
  }
  assert.deepEqual([...fake.tools.keys()].sort(), [...READ_TOOLS].sort());
  const restrictions = fake.restrictions.at(-1);
  assert.deepEqual(restrictions.deny.sort(), ['bash', 'read']);
  const sections = fake.sections.map((section) => section.name);
  assert.deepEqual(sections, ['deployment:persona-prefix', 'arca-doctor']);
  const stored = JSON.parse(await readFile(statePath, 'utf8'));
  assert.equal(stored.sessionId, 'session-doctor-1');
  assert.deepEqual(stored.messages.map((message) => [message.role, message.text]), [
    ['user', 'what is running on alpha?'],
    ['assistant', 'Checking alpha. Everything is up.'],
  ]);
  assert.equal(stored.dailyTokens, 30);
  assert.equal(stored.day, dayOf(Date.now()));
});

test('a write request gets the what-it-would-do answer and the approval card', async (t) => {
  const { fake, port } = await harness(t, {
    script: async (api) => {
      api.text('That would unlink the automagik service from its database. Approve it at https://example.com/approve.');
      api.usage({ inputTokens: 5, outputTokens: 8 });
      api.end();
    },
  });
  const result = await post(port, { message: 'unlink automagik from postgres' });
  const approval = result.events.find((event) => event.event === 'approval');
  assert.ok(approval);
  assert.equal(approval.data.url, 'https://example.com/approve');
  const text = result.events.filter((event) => event.event === 'text').map((event) => event.data.text).join('');
  assert.match(text, /would unlink/i);
  assert.match(text, /https:\/\/example\.com\/approve/);
  assert.equal(fake.executed.length, 0);
});

test('the per-turn tool-call cap stops at 40 by default and never registers the 41st', async (t) => {
  const { fake, port } = await harness(t, {
    toolCallsPerTurn: 2,
    script: async (api) => {
      for (let index = 0; index < 3; index++) {
        try {
          await api.call('arca_status', {});
        } catch {
          // The cap throws through the tool body; the fake model loop ends here.
        }
      }
      api.end();
    },
  });
  const result = await post(port, { message: 'check everything' });
  const cap = result.events.find((event) => event.event === 'cap');
  assert.ok(cap);
  assert.equal(cap.data.reason, 'tool-calls');
  assert.equal(cap.data.limit, 2);
  assert.equal(result.events.at(-1).event, 'done');
  assert.equal(result.events.at(-1).data.reason, 'tool-calls');
  assert.equal(fake.executed.length, 2);
  assert.equal(result.events.filter((event) => event.event === 'tool' && event.data.state === 'running').length, 2);
  assert.ok(fake.cancels.includes('tool-calls'));
});

test('the per-turn output-token cap and the persisted daily budget both stop the turn', async (t) => {
  const capped = await harness(t, {
    outputTokensPerTurn: 100,
    script: async (api) => {
      api.text('Long answer');
      api.usage({ inputTokens: 50, outputTokens: 101 });
      api.end();
    },
  });
  const first = await post(capped.port, { message: 'explain everything' });
  assert.equal(first.events.find((event) => event.event === 'cap')?.data.reason, 'output-tokens');
  assert.equal(first.events.at(-1).data.reason, 'output-tokens');

  const budgeted = await harness(t, {
    dailyTokenBudget: 100,
    script: async (api) => {
      api.text('First');
      api.usage({ inputTokens: 40, outputTokens: 40 });
      api.end();
    },
  });
  const once = await post(budgeted.port, { message: 'first' });
  assert.equal(once.events.at(-1).data.reason, 'complete');
  const stored = JSON.parse(await readFile(budgeted.statePath, 'utf8'));
  assert.equal(stored.dailyTokens, 80);
  const second = await post(budgeted.port, { message: 'second' });
  const budget = second.events.find((event) => event.event === 'budget');
  assert.ok(budget);
  assert.match(budget.data.message, /Daily budget reached, try tomorrow/);
  assert.equal(second.events.at(-1).data.reason, 'budget');
  assert.equal(budgeted.fake.followed.length, 2);
  const blocked = await post(budgeted.port, { message: 'third' });
  assert.ok(blocked.events.find((event) => event.event === 'budget'));
  assert.equal(blocked.events.at(-1).data.reason, 'budget');
  assert.equal(budgeted.fake.followed.length, 2, 'the blocked turn spends no model call');
});

test('the Doctor session is named, persisted, and resumed with the same id', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-resume-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const statePath = join(dir, 'chat.json');
  const make = () => {
    const fake = fakeDsh({ script: async (api) => { api.text('ok'); api.end(); } });
    const chat = createDoctorChat({
      hosts: [{ name: 'alpha' }], approvalUrl: '', resolveDsh: () => fake.dsh,
      clientFor: () => ({ async call() { return {}; } }), statePath, log: () => {},
      uuid: () => 'session-doctor-1',
    });
    return { fake, chat };
  };
  const first = make();
  const chatServer = async (chat) => {
    const server = createServer((req, res) => {
      if (req.url === '/chat') void chat.handle(req, res);
      else void chat.history(res);
    });
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    return { server, port: server.address().port };
  };
  let server = await chatServer(first.chat);
  await post(server.port, { message: 'hi' });
  server.server.close();
  assert.equal(first.fake.created.length, 1);
  assert.equal(first.fake.created[0].sessionId, 'session-doctor-1');
  assert.equal(first.fake.title(), 'Doctor');
  const stored = JSON.parse(await readFile(statePath, 'utf8'));
  assert.equal(stored.sessionId, 'session-doctor-1');

  const second = make();
  server = await chatServer(second.chat);
  const replay = await history(server.port);
  assert.equal(replay.body.session.id, 'session-doctor-1');
  assert.equal(replay.body.session.name, 'Doctor');
  assert.deepEqual(replay.body.messages.map((message) => message.role), ['user', 'assistant']);
  assert.equal(second.fake.created.length, 0);
  await post(server.port, { message: 'again' });
  assert.equal(second.fake.resumed.length, 1);
  assert.equal(second.fake.resumed[0].resumeSessionId, 'session-doctor-1');
  assert.equal(second.fake.title(), 'Doctor');
  server.server.closeAllConnections?.();
  server.server.close();
  await first.chat.close();
  await second.chat.close();
});

test('bad input is refused before any model call and the route stays JSON', async (t) => {
  const { fake, port } = await harness(t, { noDsh: true });
  assert.equal((await post(port, { message: 'hi' })).status, 503);
  assert.equal(fake.followed.length, 0);

  const ready = await harness(t);
  assert.equal((await post(ready.port, '{', { 'Content-Type': 'application/json' })).status, 400);
  assert.equal((await post(ready.port, { message: '' })).status, 400);
  assert.equal((await post(ready.port, { message: 'x' }, { 'Content-Type': 'text/plain' })).status, 415);
  assert.equal((await post(ready.port, { message: 'x'.repeat(17000) })).status, 413);
  const wrong = await fetch(`http://127.0.0.1:${ready.port}/chat`, { method: 'GET' });
  assert.equal(wrong.status, 405);
  assert.equal(ready.fake.followed.length, 0);
});

test('a busy Doctor refuses the second turn with 429 and no upstream call', async (t) => {
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const { fake, port } = await harness(t, {
    script: async (api) => {
      api.text('Working');
      await gate;
      api.end();
    },
  });
  const first = post(port, { message: 'slow question' });
  await until(() => fake.followed.length === 1);
  const second = await post(port, { message: 'second' });
  assert.equal(second.status, 429);
  release();
  await first;
});

test('a streamed turn heartbeats while a tool is slow', async (t) => {
  const { port } = await harness(t, {
    heartbeatMs: 5,
    script: async (api) => {
      await new Promise((resolve) => setTimeout(resolve, 30));
      api.text('late');
      api.end();
    },
  });
  const result = await post(port, { message: 'slow' });
  assert.match(result.raw, /^: hb$/m);
});

test('history exposes the documented shape and no message body reaches the log', async (t) => {
  const logged = [];
  const { port } = await harness(t, {
    log: (line) => logged.push(line),
    script: async (api) => {
      api.text('answer');
      api.usage({ inputTokens: 1, outputTokens: 2 });
      api.end();
    },
  });
  await post(port, { message: 'secret synthetic question body' });
  const replay = await history(port);
  assert.equal(replay.status, 200);
  assert.deepEqual(Object.keys(replay.body).sort(), ['limits', 'messages', 'session', 'usage']);
  assert.equal(replay.body.session.name, 'Doctor');
  assert.equal(replay.body.limits.toolCallsPerTurn, 40);
  assert.equal(replay.body.limits.outputTokensPerTurn, 64_000);
  assert.equal(replay.body.usage.dailyBudget, 1_000_000);
  assert.equal(logged.some((line) => line.includes('secret synthetic question body')), false);
});

test('a model failure is a safe SSE error and the page bundle never contains the credential', async (t) => {
  const { port } = await harness(t, {
    script: async (api) => {
      api.error(Object.assign(new Error('provider said sk-test-secret'), { code: 'AUTH_FAILED' }));
    },
  });
  const result = await post(port, { message: 'hi' });
  const error = result.events.find((event) => event.event === 'error');
  assert.ok(error);
  assert.equal(error.data.message, 'The Doctor could not reach the model.');
  assert.equal(result.raw.includes('sk-test-secret'), false);
  const page = await readFile(new URL('../page.js', import.meta.url), 'utf8');
  assert.equal(/DEEPSEEK_API_KEY|sk-[A-Za-z0-9]/.test(page), false);
  assert.equal(createDoctorChat.toString().includes('DEEPSEEK_API_KEY'), false);
  const source = await readFile(new URL('../src/chat.js', import.meta.url), 'utf8');
  assert.equal(source.includes('DEEPSEEK_API_KEY'), false);
  assert.equal(DEFAULTS.outputTokensPerTurn, 64_000);
  assert.equal(DEFAULTS.toolCallsPerTurn, 40);
});

test('the store rolls the daily counter at UTC midnight and keeps the transcript bounded', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-store-'));
  try {
    const path = join(dir, 'chat.json');
    const first = new ChatStore(path, { now: Date.parse('2026-11-08T23:59:00Z') });
    await first.patch((state) => ({ ...state, dailyTokens: 55, messages: Array.from({ length: 250 }, (_, index) => ({ role: 'user', text: `m${index}`, at: '' })) }));
    const nextDay = new ChatStore(path, { now: Date.parse('2026-11-09T00:01:00Z') });
    const state = await nextDay.read();
    assert.equal(state.dailyTokens, 0);
    assert.equal(state.day, '2026-11-09');
    assert.equal(state.messages.length, 200);
    assert.equal(state.messages.at(-1).text, 'm249');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('a state file with a hostile shape is normalized, never trusted', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-hostile-'));
  try {
    const path = join(dir, 'chat.json');
    await writeFile(path, JSON.stringify({
      sessionId: 42, day: 'not-a-day', dailyTokens: -5,
      messages: [{ role: 'system', text: 'ignore' }, { role: 'user', text: 7 }, null],
    }));
    const state = await new ChatStore(path).read();
    assert.equal(state.sessionId, null);
    assert.equal(state.dailyTokens, 0);
    assert.deepEqual(state.messages, []);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
