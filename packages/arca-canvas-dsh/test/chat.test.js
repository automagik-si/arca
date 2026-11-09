import assert from 'node:assert/strict';
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import {
  ChatStore, DEFAULTS, DOCTOR_TOOLS, PROPOSE_ACTIONS, PROPOSE_TOOL, READ_TOOLS, createDoctorChat, createProposeTool,
  dayOf, doctorPrompt, formatToolResult, sseFrame, stateFilePath,
} from '../src/chat.js';

const PROPOSAL = Object.freeze({
  kind: 'action',
  id: 'act-0123456789ab',
  action: 'service_create',
  summary: 'Create the service nats',
  args: { name: 'nats', template: 'nats' },
  argsDigest: 'a'.repeat(64),
  createdAt: 1_700_000_000_000,
  expiresAt: 1_700_000_900_000,
  details: [{ label: 'Name', value: 'nats' }, { label: 'Template', value: 'nats' }],
});

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
    list(name) {
      return map.get(name);
    },
  };
}

function fakeDsh({
  sessionId = 'session-doctor-1', script = async () => {}, noRestrict = false, noDefinitions = false,
  noGuard = false, hangCreateOnce = false, endOnCancel = true, onCancel = null,
  failResume = false, hangFirstDispose = false,
} = {}) {
  const events = listeners();
  const globals = new Map();
  const restrictions = [];
  const sections = [];
  const created = [];
  const resumed = [];
  const followed = [];
  const cancels = [];
  const executed = [];
  const presented = [];
  let title = null;
  let callCounter = 1;
  let turnCounter = 0;
  let currentTurn = 0;
  let turnOpen = false;
  let releaseCreate;
  const createGate = hangCreateOnce ? new Promise((resolve) => { releaseCreate = resolve; }) : null;
  const guards = [];
  const agentCtx = {
    tools: {
      restrict(filter) {
        restrictions.push(filter);
        return () => {};
      },
      guard(guard) {
        guards.push(guard);
        return () => {};
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
  const globalTools = {
    register(definition, options = {}) {
      if (globals.has(definition.name)) throw new Error(`duplicate tool ${definition.name}`);
      globals.set(definition.name, { definition, when: options.when });
      return () => globals.delete(definition.name);
    },
    restrict(filter) {
      restrictions.push(filter);
      return () => {};
    },
    definitions() {
      return [...globals.values()].map(({ definition }) => definition);
    },
  };
  if (noRestrict) {
    delete globalTools.restrict;
    agentCtx.tools.restrict = undefined;
  }
  if (noGuard) agentCtx.tools.guard = undefined;
  if (noDefinitions) delete globalTools.definitions;
  const agent = {
    id: sessionId,
    session: { id: sessionId },
    ctx: agentCtx,
    followup(message) {
      followed.push(message);
      turnCounter += 1;
      currentTurn = turnCounter;
      turnOpen = true;
      events.emit('session/event', agent.session, { type: 'turn/start', data: { turn: currentTurn } });
      queueMicrotask(() => {
        Promise.resolve(script(api)).catch((error) => {
          events.emit('agent/error', { agent, turn: currentTurn, error });
        });
      });
    },
    cancel(cause) {
      cancels.push(cause);
      if (onCancel) {
        onCancel(cause, api);
        return;
      }
      if (endOnCancel && turnOpen) queueMicrotask(() => api.end());
    },
    whenIdle() {
      return Promise.resolve();
    },
  };
  const preExecute = (name) => {
    const handlers = [...(events.list('tools/pre-execute') ?? [])];
    const exec = { name, agent, callId: `attempt-${callCounter++}`, signal: new AbortController().signal };
    const run = (index) => {
      if (index >= handlers.length) return Promise.resolve({ kind: 'allow' });
      return Promise.resolve(handlers[index](exec, () => run(index + 1)));
    };
    return run(0);
  };
  const api = {
    agent,
    globals,
    executed,
    attempt: preExecute,
    get turn() {
      return currentTurn;
    },
    text(value) {
      // Real DSH text chunks carry no turn field: only durable session events do.
      events.emit('agent/assistant-stream', {
        agent,
        frame: { type: 'chunk', index: 0, chunk: { type: 'text-delta', index: 0, text: value } },
      });
    },
    usage(usage) {
      events.emit('session/event', agent.session, { type: 'assistant/message', data: { turn: currentTurn, usage } });
    },
    async call(name, args = {}) {
      const registered = globals.get(name);
      assert.ok(registered, `${name} is registered`);
      const callId = `call-${callCounter++}`;
      const value = await registered.definition.execute(args, {
        callId, agent, signal: new AbortController().signal,
      });
      // DSH hands the model the render() payload, never the raw tool value. This fake records exactly that
      // payload so a test can assert what the model would read.
      const content = registered.definition.output.render(args, value);
      presented.push({ name, args, value, content });
      executed.push({ name, args });
      events.emit('session/event', agent.session, {
        type: 'tool/result',
        data: {
          turn: currentTurn,
          message: { role: 'tool', toolCallId: callId, content, isError: false },
        },
      });
      return value;
    },
    end() {
      if (!turnOpen) return;
      turnOpen = false;
      events.emit('session/event', agent.session, { type: 'turn/end', data: { turn: currentTurn, reason: { kind: 'completed' } } });
    },
    error(error) {
      events.emit('agent/error', { agent, turn: currentTurn, error });
      api.end();
    },
    errorOnly(error) {
      events.emit('agent/error', { agent, turn: currentTurn, error });
    },
    lateText(value) {
      events.emit('agent/assistant-stream', {
        agent,
        frame: { type: 'chunk', index: 0, chunk: { type: 'text-delta', index: 0, text: value } },
      });
    },
    lateUsage(turn, usage) {
      events.emit('session/event', agent.session, { type: 'assistant/message', data: { turn, usage } });
    },
    lateEnd(turn) {
      events.emit('session/event', agent.session, { type: 'turn/end', data: { turn, reason: { kind: 'completed' } } });
    },
    registerLate(name) {
      globals.set(name, {
        definition: {
          name, description: 'late tool', parameters: { type: 'object', properties: {}, additionalProperties: false },
          output: { schema: {}, render: () => [] }, execute: async () => ({}),
        },
        when: undefined,
      });
    },
  };
  const dsh = {
    tools: globalTools,
    agents: {
      async create(options) {
        created.push(options);
        agent.session = { id: options.sessionId };
        if (createGate && created.length === 1) await createGate;
        await options.setup?.(agentCtx);
        const first = created.length === 1;
        return {
          agent,
          dispose: async () => {
            if (hangFirstDispose && first) await new Promise(() => {});
          },
        };
      },
      async resume(options) {
        resumed.push(options);
        if (failResume) {
          const error = new Error(`session "${options.resumeSessionId}" is already owned by an active write handle`);
          error.name = 'SessionAlreadyOwnedError';
          throw error;
        }
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
  return {
    dsh, api, created, resumed, followed, cancels, executed, presented, globals, guards,
    restrictions, sections, releaseCreate: () => releaseCreate?.(),
    title: () => title, events,
  };
}

// A minimal DSH whose first create/resume hangs until released, so a newer attempt can publish a live
// handle before that stale attempt settles.
function raceDsh({ conflictResume = false } = {}) {
  let resolveGate;
  let rejectGate;
  const gate = new Promise((resolve, reject) => { resolveGate = resolve; rejectGate = reject; });
  const globals = new Map();
  const guards = [];
  const disposed = [];
  const createdIds = [];
  const resumeIds = [];
  let createAttempts = 0;
  let resumeAttempts = 0;
  let firstResumePending = false;
  let turns = 0;
  let title = null;
  const toolRegistry = {
    register(definition, options = {}) {
      globals.set(definition.name, { definition, when: options.when });
      return () => globals.delete(definition.name);
    },
    restrict() {
      return () => {};
    },
    definitions() {
      return [...globals.values()].map(({ definition }) => definition);
    },
  };
  const makeAgent = (id) => {
    const map = new Map();
    const bus = {
      on(name, handler) {
        if (!map.has(name)) map.set(name, new Set());
        map.get(name).add(handler);
        return () => map.get(name)?.delete(handler);
      },
      emit(name, ...args) {
        for (const handler of [...(map.get(name) ?? [])]) handler(...args);
      },
    };
    const agent = {
      id,
      session: { id },
      ctx: {
        tools: {
          restrict() { return () => {}; },
          guard(guard) { guards.push(guard); return () => {}; },
        },
        systemPrompt: { section() { return () => {}; } },
        on: bus.on,
      },
      followup() {
        turns += 1;
        const turn = turns;
        bus.emit('session/event', agent.session, { type: 'turn/start', data: { turn } });
        bus.emit('agent/assistant-stream', {
          agent,
          frame: { type: 'chunk', chunk: { type: 'text-delta', index: 0, text: 'live ok' } },
        });
        bus.emit('session/event', agent.session, {
          type: 'turn/end', data: { turn, reason: { kind: 'completed' } },
        });
      },
      cancel() {},
      whenIdle() { return Promise.resolve(); },
    };
    return {
      agent,
      dispose: async () => { disposed.push(id); },
    };
  };
  const dsh = {
    tools: toolRegistry,
    agents: {
      async create(options) {
        createAttempts += 1;
        createdIds.push(options.sessionId);
        if (createAttempts === 1) await gate;
        const handle = makeAgent(options.sessionId);
        await options.setup?.(handle.agent.ctx);
        return handle;
      },
      async resume(options) {
        resumeAttempts += 1;
        resumeIds.push(options.resumeSessionId);
        if (conflictResume && firstResumePending) {
          const error = new Error(`session "${options.resumeSessionId}" is already owned by an active write handle`);
          error.name = 'SessionAlreadyOwnedError';
          throw error;
        }
        if (resumeAttempts === 1) {
          firstResumePending = true;
          try {
            await gate;
          } finally {
            firstResumePending = false;
          }
        }
        const handle = makeAgent(options.resumeSessionId);
        await options.setup?.(handle.agent.ctx);
        return handle;
      },
    },
    sessionTitle: {
      get() { return title ? { title } : undefined; },
      rename(_session, value) { title = value; return { title: value }; },
    },
  };
  return {
    dsh, globals, guards, disposed, createdIds, resumeIds,
    resolveGate: () => resolveGate(), rejectGate: () => rejectGate(new Error('stale setup failed')),
  };
}

async function harness(t, options = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const statePath = join(dir, 'chat.json');
  const fake = options.fake ?? fakeDsh(options);
  const store = options.store ?? new ChatStore(statePath, {
    now: options.now,
    ...(options.readFile ? { readFile: options.readFile } : {}),
  });
  const clientCalls = [];
  const chat = createDoctorChat({
    hosts: [{ name: 'alpha' }, { name: 'beta' }],
    approvalUrl: 'https://example.com/approve',
    resolveDsh: () => (options.noDsh ? null : fake.dsh),
    clientFor(name) {
      return options.clientFor?.(name) ?? {
        async call(tool, args, signal) {
          clientCalls.push({ host: name, tool, args, signal });
          return { tool, host: name };
        },
      };
    },
    store,
    statePath,
    log: options.log ?? (() => {}),
    turnMs: options.turnMs ?? 2000,
    setupMs: options.setupMs ?? 1000,
    cancelMs: options.cancelMs ?? 1000,
    idleMs: options.idleMs ?? 1000,
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
  return { chat, fake, statePath, dir, port: server.address().port, clientCalls };
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

test('the proposal tool offers exactly the seven allowed actions, never a database delete', () => {
  assert.deepEqual([...PROPOSE_ACTIONS], [
    'service_create', 'service_remove', 'service_publish', 'service_unpublish',
    'service_connect', 'service_disconnect', 'postgres_create',
  ]);
  assert.equal(PROPOSE_ACTIONS.includes('postgres_delete'), false);
  const tool = createProposeTool({ hosts: [{ name: 'alpha' }] });
  assert.equal(tool.name, PROPOSE_TOOL);
  assert.deepEqual(tool.parameters.properties.action.enum, [...PROPOSE_ACTIONS]);
  assert.equal(tool.parameters.properties.action.enum.includes('postgres_delete'), false);
  assert.equal(tool.parameters.required.includes('action'), true);
  assert.equal(tool.parameters.required.includes('args'), true);
  assert.match(tool.description, /postgres_create \{name\}/);
  assert.doesNotMatch(tool.description, /postgres_delete|image/);
});

test('the prompt pins the propose-and-approve contract', () => {
  const prompt = doctorPrompt({ hosts: [{ name: 'alpha' }], approvalUrl: 'https://example.com/approve' });
  assert.match(prompt, /arca_propose/);
  assert.match(prompt, /tap Approve on the card/);
  assert.match(prompt, /Never claim the change is done/);
  assert.match(prompt, /never propose a database delete/);
  assert.match(prompt, /https:\/\/example\.com\/approve/);
  assert.match(prompt, /Never invent/i);
  assert.doesNotMatch(prompt, /postgres_link|service_remove|postgres_delete|service_create/);
  assert.match(stateFilePath({ DSH_HOME: '/tmp/example-home' }), /^\/tmp\/example-home\/storages\/arca-canvas-chat\.json$/);
  assert.equal(stateFilePath({ DSH_HOME: '/tmp/example-home' }, '/tmp/other.json'), '/tmp/other.json');
});

test('the Doctor prompt is a snapshot for a plain-language owner conversation', () => {
  const prompt = doctorPrompt({
    hosts: [{ name: 'alpha' }, { name: 'beta' }],
    approvalUrl: 'https://example.com/approve',
    now: Date.parse('2026-11-08T12:00:00Z'),
  });
  const snapshot = [
    'You are the Arca Doctor, a friendly helper in the Arca canvas.',
    'You talk to the owner in plain language, not to an engineer.',
    'You can read live Arca data from these hosts: alpha, beta.',
    'Today is 2026-11-08.',
    '',
    'Rules:',
    '- Answer only from data returned by your tools in this conversation. Never invent, estimate or make up a',
    '  number, status, name, port or address. Rounding for display is fine; the figure behind it must be real.',
    '- Call a tool when the answer depends on current state. Do not answer data questions from memory.',
    '- Keep answers short: a sentence or a few bullets. Plain sentences only; no markdown tables, headings or',
    '  code fences.',
    '- Storage sizes are always in GB with one decimal, for example 17.0 GB, 3.4 GB used or 96.3 GB total.',
    '- Network rates keep the unit the tool gives: KB/s, MB/s or GB/s, with one decimal. Never show raw bytes.',
    '- Percentages are whole numbers, for example 70%, not 70.29%.',
    '- Leave out low-level system detail, raw timers and internal ids. Add technical detail only when the owner',
    '  explicitly asks for it.',
    '- If a read fails or a host did not answer, say "could not be read just now". Only a reading the host can',
    '  never give, such as remote CPU load, is "not measurable from here". Never repeat an internal reason or',
    '  error string.',
    '- You cannot link, unlink, delete, start, stop, restart, create, adopt, apply, approve, mint or change',
    '  anything by yourself. arca_propose is the only way to ask for a change: it records a request for the',
    '  owner and changes nothing.',
    '- For a change request, call arca_propose with the action and its arguments, then tell the owner in one',
    '  plain sentence what will happen and to tap Approve on the card. Never claim the change is done before',
    '  the result arrives. If the owner declines it, or it expires or fails, say so plainly.',
    '- Do not propose a change the owner did not ask for, and never propose a database delete.',
    '- The approval page is at https://example.com/approve. The canvas card uses the same passkey.',
  ].join('\n');
  assert.equal(prompt, snapshot);
  assert.match(prompt, /If a read fails or a host did not answer, say "could not be read just now"\./);
  assert.match(prompt, /Only a reading the host can\s+never give, such as remote CPU load, is "not measurable from here"\./);
  assert.match(prompt, /Network rates keep the unit the tool gives: KB\/s, MB\/s or GB\/s/);
  assert.doesNotMatch(prompt, /Sizes are always in GB/);
  assert.doesNotMatch(prompt, /\bIncus\b|kernel mode|uptime/i);
  assert.match(doctorPrompt({ hosts: [{ name: 'alpha' }], now: Date.parse('2026-11-08T12:00:00Z') }),
    /No approval page is configured on this canvas/);
});

test('a node_metrics result reaches the fake model as plain GB, rounded percentages and no internal reason', async (t) => {
  const internalReason = 'not available for a remote host';
  const metrics = {
    at: 1_700_000_000_000,
    minutes: 5,
    latest: {
      at: 1_700_000_000_000,
      cpu: { usedPct: 70.29, cores: 8, coresKnown: true, reason: null },
      load: { one: 1.25, five: null, fifteen: 0.8, reason: internalReason },
      memory: {
        totalBytes: 96_300_000_000,
        availableBytes: 79_300_000_000,
        usedBytes: 17_000_000_000,
        swapTotalBytes: 8_000_000_000,
        swapFreeBytes: null,
        reason: null,
      },
      psi: { memorySomeAvg10: 0, memoryFullAvg10: 0, cpuSomeAvg10: 1.49 },
      cpuTempC: 54.2,
      gpuBusyPct: 29.6,
      pools: [{ name: 'data', usedBytes: 3_400_000_000, totalBytes: 96_300_000_000, freeBytes: 92_900_000_000 }],
      network: { rxBytesPerSec: 1_200_000_000, txBytesPerSec: 800_000_000 },
      projects: [{ project: 'default', total: 3, running: 2 }],
    },
    mean: {
      at: 1_700_000_000_000,
      cpu: { usedPct: 64.8, cores: 8, coresKnown: true, reason: null },
      load: { one: 1.0, five: 1.0, fifteen: 1.0, reason: null },
      memory: {
        totalBytes: 96_300_000_000,
        availableBytes: null,
        usedBytes: null,
        swapTotalBytes: 8_000_000_000,
        swapFreeBytes: null,
        reason: internalReason,
      },
      psi: { memorySomeAvg10: null, memoryFullAvg10: null, cpuSomeAvg10: null },
      cpuTempC: null,
      gpuBusyPct: null,
      pools: [],
      network: null,
      projects: [],
    },
  };
  const { fake, port } = await harness(t, {
    clientFor: () => ({ async call() { return metrics; } }),
    script: async (api) => {
      await api.call('node_metrics', {});
      api.text('checked');
      api.end();
    },
  });
  await post(port, { message: 'how is alpha doing?' });
  const seen = fake.presented.find((entry) => entry.name === 'node_metrics');
  assert.ok(seen, 'the tool was presented to the fake model');
  assert.equal(seen.content[0].type, 'text');
  const text = seen.content.map((block) => block.text).join('\n');
  assert.match(text, /17\.0 GB used of 96\.3 GB, 79\.3 GB available/);
  assert.match(text, /70% used across 8 cores/);
  assert.match(text, /65% used across 8 cores/);
  assert.match(text, /3\.4 GB used, 96\.3 GB total/);
  assert.match(text, /not measurable from here/);
  assert.doesNotMatch(text, /not available for a remote host/);
  assert.doesNotMatch(text, /70\.29|64\.8/);
  for (const raw of ['17000000000', '96300000000', '79300000000', '3400000000', '8000000000']) {
    assert.equal(text.includes(raw), false, `${raw} is never shown to the model`);
  }
});

test('a remote CPU keeps its known core count while its load is unmeasurable', () => {
  const text = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: {
      cpu: { usedPct: null, cores: 16, coresKnown: true, reason: 'not available for a remote host' },
      memory: { totalBytes: 32_000_000_000, availableBytes: 24_000_000_000, usedBytes: 8_000_000_000, reason: null },
      // The real remote payload sets load.reason too: a remote host can never give its load averages.
      load: { one: null, five: null, fifteen: null, reason: 'not available for a remote host' },
      pools: [],
      network: null,
      psi: null,
      projects: [],
    },
    mean: null,
  });
  assert.match(text, /CPU: 16 cores; load not measurable from here\./);
  assert.match(text, /Load: not measurable from here\./);
  assert.match(text, /Memory: 8\.0 GB used of 32\.0 GB, 24\.0 GB available\./);
  assert.doesNotMatch(text, /not available for a remote host/);
  assert.doesNotMatch(text, /CPU:.*%/);

  const derived = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: {
      cpu: { usedPct: 88, cores: 0, coresKnown: false, reason: null },
      memory: { totalBytes: 32_000_000_000, availableBytes: 24_000_000_000, usedBytes: null, reason: null },
      load: { one: 1, five: 1, fifteen: 1, reason: null },
      pools: [],
      network: null,
      psi: null,
      projects: [],
    },
    mean: null,
  });
  assert.match(derived, /CPU: core count no reading yet\./);
  assert.doesNotMatch(derived, /88%/);
  assert.match(derived, /Memory: 8\.0 GB used of 32\.0 GB, 24\.0 GB available\./);
});

test('a failed core and memory read says could not be read just now', () => {
  const failedReason = 'the Incus resources read failed';
  const text = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: {
      cpu: { usedPct: null, cores: 0, coresKnown: false, reason: failedReason },
      memory: { totalBytes: null, availableBytes: null, usedBytes: null, reason: failedReason },
      load: { one: null, five: null, fifteen: null, reason: null },
      pools: [],
      network: null,
      psi: null,
      projects: [],
    },
    mean: null,
  });
  assert.match(text, /CPU: core count could not be read just now\./);
  assert.match(text, /Memory: could not be read just now\./);
  assert.match(text, /Load: no reading yet\./);
  assert.doesNotMatch(text, /not measurable from here/);
  assert.doesNotMatch(text, /the Incus resources read failed/);
});

test('alert reasons become plain words and keep their exact resolution', () => {
  const text = formatToolResult('alerts_list', {
    firing: [],
    unknown: [
      { rule: 'cpu-high', labels: {}, state: 'ok', reason: 'missing' },
      { rule: 'load-high', labels: {}, state: 'ok', reason: 'stale' },
      { rule: 'mem-high', labels: {}, state: 'ok', reason: 'unavailable' },
      { rule: 'other', labels: {}, state: 'ok', reason: 'vendor-specific' },
    ],
    resolvedRecent: [
      { rule: 'disk-pool-full', state: 'resolved', reason: 'rule-disabled' },
      { rule: 'cpu-high', state: 'resolved', reason: 'cleared' },
      { rule: 'app-down', state: 'resolved', reason: 'subject-gone' },
      { rule: 'backup', state: 'resolved', reason: 'no data' },
    ],
    disabled: [],
    sampleAgeMs: null,
    watchAgeMs: null,
    commitMs: null,
    storeError: null,
    storeReset: false,
    storeBytes: 1_500_000_000,
    healthLine: 'ok',
  });
  const parsed = JSON.parse(text);
  assert.equal(parsed.resolvedRecent[0].reason, 'the alert was switched off');
  assert.notEqual(parsed.resolvedRecent[0].reason, 'the problem cleared');
  assert.equal(parsed.resolvedRecent[1].reason, 'the problem cleared');
  assert.equal(parsed.resolvedRecent[2].reason, 'the thing it watched is gone');
  assert.equal(parsed.resolvedRecent[3].reason, 'no data to check');
  assert.equal(parsed.unknown[0].reason, 'no reading yet');
  assert.equal(parsed.unknown[1].reason, 'reading is old');
  assert.equal(parsed.unknown[2].reason, 'not measurable from here');
  assert.equal(parsed.unknown[3].reason, 'vendor-specific');
  assert.equal(parsed.storeBytes, '1.5 GB');
});

test('one core is singular and rates choose the unit after rounding', () => {
  const sample = (cpu, network) => ({
    cpu,
    memory: { totalBytes: 8_000_000_000, availableBytes: 4_000_000_000, usedBytes: 4_000_000_000, reason: null },
    load: { one: 1, five: 1, fifteen: 1, reason: null },
    pools: [],
    network,
    psi: null,
    projects: [],
  });
  const loaded = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: sample(
      { usedPct: 50, cores: 1, coresKnown: true, reason: null },
      { rxBytesPerSec: 1_000_000, txBytesPerSec: 1_000 },
    ),
    mean: null,
  });
  assert.match(loaded, /CPU: 50% used across 1 core\./);
  assert.match(loaded, /Network: 1\.0 MB\/s received, 1\.0 KB\/s sent\./);

  const justBelow = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: sample(
      { usedPct: 50, cores: 1, coresKnown: true, reason: null },
      { rxBytesPerSec: 999_960, txBytesPerSec: 999_960_000 },
    ),
    mean: null,
  });
  assert.match(justBelow, /Network: 1\.0 MB\/s received, 1\.0 GB\/s sent\./);

  const idle = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: sample({ usedPct: null, cores: 1, coresKnown: true, reason: null }, null),
    mean: null,
  });
  assert.match(idle, /CPU: 1 core; load no reading yet\./);
});

test('network rates use a readable unit at the reported size', () => {
  const text = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: {
      cpu: { usedPct: 10, cores: 8, coresKnown: true, reason: null },
      memory: {
        totalBytes: 16_000_000_000, availableBytes: 8_000_000_000, usedBytes: 8_000_000_000, reason: null,
      },
      load: { one: 1, five: 1, fifteen: 1, reason: null },
      pools: [],
      network: { rxBytesPerSec: 45_000_000, txBytesPerSec: 120_000 },
      psi: null,
      projects: [],
    },
    mean: null,
  });
  assert.match(text, /Network: 45\.0 MB\/s received, 120\.0 KB\/s sent\./);
});

test('a missing latest reading and a one-minute average use the right label', () => {
  const sample = {
    cpu: { usedPct: 10, cores: 8, coresKnown: true, reason: null },
    memory: {
      totalBytes: 16_000_000_000, availableBytes: 8_000_000_000, usedBytes: 8_000_000_000, reason: null,
    },
    load: { one: 1, five: 1, fifteen: 1, reason: null },
    pools: [],
    network: null,
    psi: null,
    projects: [],
  };
  const text = formatToolResult('node_metrics', { host: 'alpha', minutes: 1, latest: null, mean: sample });
  assert.match(text, /^Host alpha\.\nLatest reading: no reading yet\.\nAverage over the last 1 minute:/);
  assert.doesNotMatch(text, /1 minutes|Latest reading for the last/);
  const plural = formatToolResult('node_metrics', { host: 'alpha', minutes: 5, latest: null, mean: sample });
  assert.match(plural, /Average over the last 5 minutes:/);
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
  assert.equal(fake.globals.has('service_list'), true);
  for (const name of ['action_propose', 'postgres_link', 'postgres_unlink', 'postgres_delete', 'service_start', 'service_remove', 'plan_apply', 'token_mint', 'adopt', 'bash', 'run_code']) {
    assert.equal(fake.globals.has(name), false, `${name} is never registered`);
  }
  assert.deepEqual([...fake.globals.keys()].sort(), [...DOCTOR_TOOLS].sort());
  assert.equal(fake.globals.has(PROPOSE_TOOL), true);
  assert.equal(fake.globals.has('action_propose'), false, 'the backend verb is not registered under its own name');
  const restrictions = fake.restrictions.at(-1);
  assert.deepEqual([...restrictions.allow].sort(), [...DOCTOR_TOOLS].sort());
  assert.equal(restrictions.allow.includes(PROPOSE_TOOL), true);
  assert.equal(fake.globals.get('arca_status').when({ agent: { id: 'session-doctor-1' } }), true);
  assert.equal(fake.globals.get('arca_status').when({ agent: { id: 'another-session' } }), false);
  assert.equal(fake.globals.get(PROPOSE_TOOL).when({ agent: { id: 'session-doctor-1' } }), true);
  const guard = fake.guards.at(-1);
  assert.equal(guard({ name: 'arca_status' }), undefined);
  assert.equal(guard({ name: PROPOSE_TOOL }), undefined);
  assert.match(guard({ name: 'bash' }), /read or propose/);
  assert.match(guard({ name: 'run_code' }), /read or propose/);
  assert.match(guard({ name: 'action_propose' }), /read or propose/);
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

test('a change request records a kernel proposal and never applies it', async (t) => {
  const calls = [];
  const { fake, port, statePath } = await harness(t, {
    script: async (api) => {
      const denied = {};
      for (const name of ['bash', 'run_code', 'postgres_link']) denied[name] = (await api.attempt(name)).kind;
      const value = await api.call(PROPOSE_TOOL, {
        action: 'service_create', args: { name: 'nats', template: 'nats' },
      });
      assert.equal(value.kind, 'action');
      assert.equal(value.id, PROPOSAL.id);
      assert.deepEqual(value.details, PROPOSAL.details);
      api.text(`I can create the nats service for you (${JSON.stringify(denied)}). Tap Approve on the card.`);
      api.usage({ inputTokens: 5, outputTokens: 8 });
      api.end();
    },
    clientFor: () => ({
      async call(tool, args) {
        calls.push({ tool, args });
        return structuredClone(PROPOSAL);
      },
    }),
  });
  const result = await post(port, { message: 'create the nats service' });
  assert.equal(result.events.some((event) => event.event === 'approval'), false, 'no model-text approval card');
  const tools = result.events.filter((event) => event.event === 'tool');
  const proposed = tools.find((event) => event.data.proposal);
  assert.ok(proposed, 'the tool event carries the kernel proposal id');
  assert.equal(proposed.data.name, PROPOSE_TOOL);
  assert.equal(proposed.data.host, 'alpha');
  assert.equal(proposed.data.proposal.id, PROPOSAL.id);
  assert.equal(tools.some((event) => event.data.name === PROPOSE_TOOL && event.data.state === 'done'), true);
  const text = result.events.filter((event) => event.event === 'text').map((event) => event.data.text).join('');
  assert.match(text, /Tap Approve on the card/);
  assert.match(text, /"bash":"deny"/);
  assert.match(text, /"run_code":"deny"/);
  assert.match(text, /"postgres_link":"deny"/);
  assert.deepEqual(calls, [{ tool: 'action_propose', args: { action: 'service_create', args: { name: 'nats', template: 'nats' } } }]);
  assert.deepEqual(fake.executed.map((call) => call.name), [PROPOSE_TOOL], 'no write verb ran');
  const stored = JSON.parse(await readFile(statePath, 'utf8'));
  const record = stored.messages.at(-1).tools[0];
  assert.equal(record.name, PROPOSE_TOOL);
  assert.deepEqual(record.proposal, { id: PROPOSAL.id });
  assert.equal(stored.messages.at(-1).text.includes('Tap Approve'), true);
});

test('the proposal tool refuses a disallowed action before any Arca call', async (t) => {
  const seen = [];
  const { fake, port } = await harness(t, {
    script: async (api) => {
      const failure = {};
      for (const call of [
        [PROPOSE_TOOL, { action: 'postgres_delete', args: { name: 'legacy' } }],
        [PROPOSE_TOOL, { action: 'service_create', args: 'not-an-object' }],
      ]) {
        try {
          await api.call(call[0], call[1]);
          failure[call[1].action] = 'allowed';
        } catch (error) {
          failure[call[1].action] = String(error.message);
        }
      }
      api.text(JSON.stringify(failure));
      api.end();
    },
    clientFor: () => ({ async call(tool, args) { seen.push({ tool, args }); return structuredClone(PROPOSAL); } }),
  });
  const result = await post(port, { message: 'delete the legacy database' });
  const text = result.events.filter((event) => event.event === 'text').map((event) => event.data.text).join('');
  assert.match(text, /postgres_delete.*cannot be proposed/);
  assert.match(text, /service_create.*arguments object/);
  assert.deepEqual(seen, [], 'a refused proposal never reaches Arca');
  assert.deepEqual(fake.executed, [], 'no tool call was dispatched');
});

test('a postgres_create proposal never carries an image field', async (t) => {
  const calls = [];
  const { port } = await harness(t, {
    script: async (api) => {
      await api.call(PROPOSE_TOOL, {
        action: 'postgres_create', args: { name: 'orders', image: 'attacker/image' },
      });
      api.end();
    },
    clientFor: () => ({
      async call(tool, args) {
        calls.push({ tool, args });
        return { ...structuredClone(PROPOSAL), action: 'postgres_create', summary: 'Create the database orders', args: { name: 'orders' } };
      },
    }),
  });
  await post(port, { message: 'create the orders database' });
  assert.deepEqual(calls, [{ tool: 'action_propose', args: { action: 'postgres_create', args: { name: 'orders' } } }]);
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

test('a cap reached mid-response stops the next tool from dispatching', async (t) => {
  let blocked = '';
  const { fake, port } = await harness(t, {
    outputTokensPerTurn: 10,
    script: async (api) => {
      api.usage({ inputTokens: 1, outputTokens: 11 });
      try {
        await api.call('arca_status', {});
      } catch (error) {
        blocked = error.message;
      }
      api.text('after the cap');
      api.end();
    },
  });
  const result = await post(port, { message: 'go past the cap' });
  assert.equal(blocked, 'Doctor turn cap reached: output-tokens');
  assert.equal(fake.executed.length, 0, 'the tool after the cap is never dispatched');
  assert.equal(result.events.filter((event) => event.event === 'tool').length, 0);
  assert.equal(result.events.find((event) => event.event === 'cap')?.data.reason, 'output-tokens');
  assert.equal(result.events.at(-1).data.reason, 'output-tokens');
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

test('agent setup fails closed when a tool fence cannot be installed', async (t) => {
  for (const [label, options] of [
    ['restrict', { noRestrict: true }],
    ['guard', { noGuard: true }],
    ['definitions', { noDefinitions: true }],
  ]) {
    const { chat, fake, port } = await harness(t, { ...options, setupMs: 500 });
    const result = await post(port, { message: `fail ${label}` });
    assert.equal(result.events[0].event, 'start');
    assert.equal(result.events.find((event) => event.event === 'error')?.data.message, 'The Doctor could not answer.');
    assert.equal(result.events.at(-1).data.reason, 'error');
    // installTools rejects before create; a missing agent-scope guard is only detectable inside setup.
    if (label !== 'guard') assert.equal(fake.created.length, 0, `${label}: no agent is created`);
    assert.equal(chat.sessionId, null, `${label}: no agent is published`);
    assert.equal(fake.followed.length, 0, `${label}: no model call`);
  }
});

test('the dispatch guard denies run_code and a tool registered after setup', async (t) => {
  const { fake, port } = await harness(t, {
    script: async (api) => {
      api.registerLate('late_tool');
      const decisions = {};
      for (const name of ['arca_status', 'bash', 'run_code', 'late_tool']) decisions[name] = (await api.attempt(name)).kind;
      api.text(JSON.stringify(decisions));
      api.end();
    },
  });
  const result = await post(port, { message: 'guard check' });
  const text = result.events.filter((event) => event.event === 'text').map((event) => event.data.text).join('');
  assert.deepEqual(JSON.parse(text), {
    arca_status: 'allow', bash: 'deny', run_code: 'deny', late_tool: 'deny',
  });
  assert.equal(fake.executed.length, 0);
  assert.equal([...fake.globals.keys()].includes('late_tool'), true, 'the late tool really was registered');
});

test('a cancelled run must end before the next; its late events are dropped', async (t) => {
  let first = true;
  const { fake, port, statePath } = await harness(t, {
    turnMs: 25,
    cancelMs: 500,
    endOnCancel: false,
    script: async (api) => {
      if (first) {
        first = false;
        await new Promise(() => {});
        return;
      }
      api.text('fresh');
      api.usage({ inputTokens: 1, outputTokens: 2 });
      api.lateUsage(1, { inputTokens: 500, outputTokens: 500 });
      api.end();
    },
  });
  const firstPost = post(port, { message: 'first' });
  await until(() => fake.followed.length === 1);
  await new Promise((resolve) => setTimeout(resolve, 50));
  const busy = await post(port, { message: 'while cancelling' });
  assert.equal(busy.status, 429, 'the next turn waits for the cancelled run to end');
  fake.api.lateEnd(1);
  const firstResult = await firstPost;
  assert.equal(firstResult.events.at(-1).data.reason, 'timeout');

  // The cancelled run's stream is over: a late text frame has no listener left to reach.
  fake.api.lateText('stale text');
  const second = await post(port, { message: 'second' });
  assert.equal(second.events.at(-1).data.reason, 'complete');
  const text = second.events.filter((event) => event.event === 'text').map((event) => event.data.text).join('');
  assert.equal(text, 'fresh');
  const stored = JSON.parse(await readFile(statePath, 'utf8'));
  assert.equal(stored.dailyTokens, 3, 'only the fresh turn usage is counted');
  assert.equal(second.events.some((event) => event.event === 'usage' && event.data.turnOutputTokens > 2), false);
});

test('a hanging agent setup is bounded and busy is cleared for the next request', async (t) => {
  const { fake, port } = await harness(t, {
    hangCreateOnce: true, setupMs: 20,
    script: async (api) => { api.text('ok'); api.end(); },
  });
  const first = await post(port, { message: 'hang' });
  assert.equal(first.events.at(-1).data.reason, 'error');
  assert.equal(first.events.filter((event) => event.event === 'error').length, 1);
  fake.releaseCreate();
  await new Promise((resolve) => setTimeout(resolve, 25));
  const second = await post(port, { message: 'after hang' });
  assert.equal(second.events.at(-1).data.reason, 'complete');
  assert.equal(fake.followed.length, 1);
});

test('a client disconnect during setup makes no model call', async (t) => {
  const { fake, port } = await harness(t, {
    hangCreateOnce: true, setupMs: 500,
    script: async (api) => { api.text('ok'); api.end(); },
  });
  const controller = new AbortController();
  const pending = fetch(`http://127.0.0.1:${port}/chat`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ message: 'disconnect' }),
    signal: controller.signal,
  }).catch(() => null);
  await until(() => fake.created.length === 1);
  await new Promise((resolve) => setTimeout(resolve, 20));
  controller.abort();
  fake.releaseCreate();
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(fake.followed.length, 0, 'no followup after the client went away');
  await pending;
  const second = await post(port, { message: 'after disconnect' });
  assert.equal(second.events.at(-1).data.reason, 'complete');
  assert.equal(fake.followed.length, 1);
});

test('a corrupt state file is quarantined and the rest of the day stays exhausted', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-corrupt-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const path = join(dir, 'chat.json');
  await writeFile(path, '{"dailyTokens": 0,');
  const today = new ChatStore(path, { now: Date.parse('2026-11-08T12:00:00Z') });
  const state = await today.read();
  assert.equal(state.budgetExhausted, true);
  const files = await readdir(dir);
  assert.equal(files.some((name) => name.includes('.corrupt-')), true, 'the bad file is kept aside');
  const onDisk = JSON.parse(await readFile(path, 'utf8'));
  assert.equal(onDisk.budgetExhausted, true);
  const tomorrow = new ChatStore(path, { now: Date.parse('2026-11-09T00:01:00Z') });
  const rolled = await tomorrow.read();
  assert.equal(rolled.budgetExhausted, false);
  assert.equal(rolled.dailyTokens, 0);

  const { fake, port, statePath } = await harness(t);
  await writeFile(statePath, 'not json');
  const reply = await post(port, { message: 'budget?' });
  const budget = reply.events.find((event) => event.event === 'budget');
  assert.ok(budget);
  assert.equal(budget.data.message, 'Daily budget reached, try tomorrow.');
  assert.equal(reply.events.at(-1).data.reason, 'budget');
  assert.equal(fake.followed.length, 0);
});

test('a transient state read error is retried once and never quarantined', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-retry-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const path = join(dir, 'chat.json');
  await writeFile(path, JSON.stringify({
    version: 1, sessionId: 'saved-doctor', day: dayOf(Date.now()), dailyTokens: 42, messages: [],
  }));
  let calls = 0;
  const flaky = async (...args) => {
    calls += 1;
    if (calls === 1) throw Object.assign(new Error('state file busy'), { code: 'EAGAIN' });
    return readFile(...args);
  };
  const state = await new ChatStore(path, { readFile: flaky }).read();
  assert.equal(calls, 2, 'the read is retried exactly once');
  assert.equal(state.sessionId, 'saved-doctor');
  assert.equal(state.dailyTokens, 42);
  assert.equal(state.budgetExhausted, false);
  assert.deepEqual((await readdir(dir)).filter((name) => name.includes('.corrupt-')), []);
  assert.equal(JSON.parse(await readFile(path, 'utf8')).dailyTokens, 42, 'the state file is untouched');
});

test('a persistent state read error fails closed without quarantining the file', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-read-fail-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const path = join(dir, 'chat.json');
  const contents = JSON.stringify({
    version: 1, sessionId: 'saved-doctor', day: dayOf(Date.now()), dailyTokens: 42, messages: [],
  });
  await writeFile(path, contents);
  let calls = 0;
  const broken = async () => {
    calls += 1;
    throw Object.assign(new Error('state file busy'), { code: 'EAGAIN' });
  };
  await assert.rejects(new ChatStore(path, { readFile: broken }).read(), /state file busy/);
  assert.equal(calls, 2);
  assert.deepEqual((await readdir(dir)).filter((name) => name.includes('.corrupt-')), []);
  assert.equal(await readFile(path, 'utf8'), contents, 'a read error never renames or overwrites the file');
});

test('a state read that fails twice during usage and transcript saves exhausts the day', async (t) => {
  const logged = [];
  let reads = 0;
  const flaky = async (...args) => {
    reads += 1;
    // 1 handle read, 2 ensureAgent read, 3 session-id patch, 4 runTurn read, 5 user-message patch;
    // 6-7 usage and 8-9 transcript retries fail.
    if (reads >= 6 && reads <= 9) throw Object.assign(new Error('state file busy'), { code: 'EAGAIN' });
    return readFile(...args);
  };
  const { fake, port } = await harness(t, {
    readFile: flaky,
    log: (line) => logged.push(line),
    script: async (api) => {
      api.text('answer');
      api.usage({ inputTokens: 10, outputTokens: 20 });
      api.end();
    },
  });
  const first = await post(port, { message: 'first' });
  assert.equal(first.events.at(-1).data.reason, 'complete');
  assert.equal(reads, 9, 'both saves retried the read and failed');
  assert.ok(logged.some((line) => /usage counter not written \(EAGAIN\); daily budget exhausted for the day/.test(line)));
  assert.ok(logged.some((line) => /transcript not written \(EAGAIN\); daily budget exhausted for the day/.test(line)));

  // Once the file is readable again the day is still exhausted: no model call, no lost tokens.
  const second = await post(port, { message: 'second' });
  const budget = second.events.find((event) => event.event === 'budget');
  assert.ok(budget);
  assert.equal(second.events.at(-1).data.reason, 'budget');
  assert.equal(fake.followed.length, 1, 'the exhausted day makes no second model call');
});

test('a failed state write leaves no temporary file behind', async (t) => {
  if (process.getuid?.() === 0) return t.skip('root ignores the read-only directory');
  const dir = await mkdtemp(join(tmpdir(), 'arca-chat-write-'));
  t.after(async () => {
    await chmod(dir, 0o700).catch(() => {});
    await rm(dir, { recursive: true, force: true });
  });
  const locked = join(dir, 'locked');
  await mkdir(locked);
  await chmod(locked, 0o500);
  const store = new ChatStore(join(locked, 'chat.json'));
  await assert.rejects(store.patch((state) => state));
  await chmod(locked, 0o700);
  assert.deepEqual(await readdir(locked), [], 'no .tmp file survives a failed write');
});

test('tools name the host, reject an unknown host, and pass the execution signal', async (t) => {
  const { port, clientCalls } = await harness(t, {
    script: async (api) => {
      let unknown = '';
      try {
        await api.call('arca_status', { host: 'missing' });
      } catch (error) {
        unknown = error.message;
      }
      const value = await api.call('arca_status', { host: 'alpha' });
      api.text(JSON.stringify({ unknown, value }));
      api.end();
    },
  });
  const result = await post(port, { message: 'hosts' });
  const text = result.events.filter((event) => event.event === 'text').map((event) => event.data.text).join('');
  const data = JSON.parse(text);
  assert.match(data.unknown, /Unknown host "missing"/);
  assert.match(data.unknown, /alpha, beta/);
  assert.equal(data.value.host, 'alpha');
  assert.equal(clientCalls.length, 1, 'an unknown host never falls back to the first host');
  assert.equal(clientCalls[0].host, 'alpha');
  assert.ok(clientCalls[0].signal instanceof AbortSignal);
  assert.equal(clientCalls[0].signal.aborted, false);
});

test('the start event names the Doctor session and carries the stored id when known', async (t) => {
  const script = async (api) => { api.text('ok'); api.end(); };
  const first = await harness(t, { script });
  const firstReply = await post(first.port, { message: 'hello' });
  const firstStart = firstReply.events.find((event) => event.event === 'start');
  assert.deepEqual(firstStart.data, { session: 'Doctor' });

  const second = await harness(t, { script });
  await writeFile(second.statePath, JSON.stringify({
    version: 1, sessionId: 'saved-doctor', day: dayOf(Date.now()), dailyTokens: 0, messages: [],
  }));
  const secondReply = await post(second.port, { message: 'hello again' });
  const secondStart = secondReply.events.find((event) => event.event === 'start');
  assert.equal(secondStart.data.session, 'Doctor');
  assert.equal(secondStart.data.sessionId, 'saved-doctor');
  assert.equal(second.fake.resumed.length, 1);
});

test('partial usage reported by an interrupted assistant message is counted', async (t) => {
  const { port, statePath } = await harness(t, {
    turnMs: 30,
    script: async (api) => {
      api.text('partial answer');
      api.usage({ inputTokens: 10, outputTokens: 20 });
      await new Promise(() => {});
    },
  });
  const result = await post(port, { message: 'slow' });
  assert.equal(result.events.at(-1).data.reason, 'timeout');
  await new Promise((resolve) => setTimeout(resolve, 20));
  const stored = JSON.parse(await readFile(statePath, 'utf8'));
  assert.equal(stored.dailyTokens, 30, 'usage from the interrupted assistant/message is counted');
  assert.equal(stored.messages.at(-1).text, 'partial answer');
});

test('a hanging setup shares the turn deadline and cannot extend the request', async (t) => {
  const { port } = await harness(t, { hangCreateOnce: true, setupMs: 5000, turnMs: 50 });
  const started = Date.now();
  const result = await post(port, { message: 'deadline' });
  const elapsed = Date.now() - started;
  assert.equal(result.events.at(-1).data.reason, 'error');
  assert.ok(elapsed < 1000, `setup must not outlive the turn deadline (${elapsed}ms)`);
});

test('an agent error with no turn/end releases the turn and the next request', async (t) => {
  let calls = 0;
  const { port } = await harness(t, {
    turnMs: 40,
    cancelMs: 20,
    endOnCancel: false,
    script: async (api) => {
      calls += 1;
      if (calls === 1) {
        api.errorOnly(new Error('no turn end follows'));
        return;
      }
      api.text('recovered');
      api.end();
    },
  });
  const started = Date.now();
  const first = await post(port, { message: 'error' });
  const elapsed = Date.now() - started;
  assert.equal(first.events.at(-1).data.reason, 'error');
  assert.ok(elapsed < 500, `the error released the turn in ${elapsed}ms`);
  const second = await post(port, { message: 'after error' });
  assert.equal(second.status, 200);
  assert.equal(second.events.at(-1).data.reason, 'complete');
});

test('a stale create cannot replace or dispose the live agent', async (t) => {
  const race = raceDsh();
  let sequence = 0;
  const { chat, port } = await harness(t, {
    fake: race,
    setupMs: 25,
    turnMs: 500,
    cancelMs: 20,
    uuid: () => `candidate-${++sequence}`,
  });
  const first = await post(port, { message: 'stale create' });
  assert.equal(first.events.at(-1).data.reason, 'error');
  assert.equal(race.createdIds.length, 1);
  const second = await post(port, { message: 'live create' });
  assert.equal(second.events.at(-1).data.reason, 'complete');
  assert.equal(race.createdIds.length, 2);
  const liveId = chat.sessionId;
  assert.equal(liveId, race.createdIds[1]);
  assert.deepEqual(race.disposed, []);

  race.resolveGate();
  await new Promise((resolve) => setTimeout(resolve, 40));
  assert.deepEqual(race.disposed, [race.createdIds[0]], 'only the stale handle is disposed');
  assert.equal(chat.sessionId, liveId, 'the live handle is still published');
  assert.equal(race.globals.get('arca_status').when({ agent: { id: liveId } }), true);
  assert.equal(race.globals.get('arca_status').when({ agent: { id: race.createdIds[0] } }), false);

  await chat.close();
  assert.ok(race.disposed.includes(liveId), 'close disposes the live handle');
});

test('a stale resume overlapping a newer setup reuses the live session id', async (t) => {
  const race = raceDsh({ conflictResume: true });
  let sequence = 0;
  const { chat, port, statePath } = await harness(t, {
    fake: race,
    setupMs: 200,
    turnMs: 1000,
    cancelMs: 20,
    uuid: () => `candidate-${++sequence}`,
  });
  await writeFile(statePath, JSON.stringify({
    version: 1, sessionId: 'saved-doctor', day: dayOf(Date.now()), dailyTokens: 0, messages: [],
  }));
  const first = await post(port, { message: 'stale resume' });
  assert.equal(first.events.at(-1).data.reason, 'error');
  // The first resume is still holding the session write handle. The second attempt races it and must wait,
  // not treat the write conflict as a missing session and create a new id.
  const secondPromise = post(port, { message: 'live resume' });
  await until(() => race.resumeIds.length === 2);
  race.resolveGate();
  const second = await secondPromise;
  assert.equal(second.events.at(-1).data.reason, 'complete');
  assert.equal(race.createdIds.length, 0, 'the newer setup never forks a new DSH session');
  assert.deepEqual([...new Set(race.resumeIds)], ['saved-doctor'], 'both setups stay on the stored session id');
  assert.equal(chat.sessionId, 'saved-doctor');
  assert.equal(JSON.parse(await readFile(statePath, 'utf8')).sessionId, 'saved-doctor');
  await chat.close();
});

test('a write conflict with no live holder recovers with a fresh session', async (t) => {
  const logged = [];
  let first = true;
  let sequence = 0;
  const { chat, fake, port } = await harness(t, {
    failResume: true,
    hangFirstDispose: true,
    endOnCancel: false,
    turnMs: 25,
    cancelMs: 20,
    setupMs: 500,
    log: (line) => logged.push(line),
    uuid: () => `candidate-${++sequence}`,
    script: async (api) => {
      if (first) {
        first = false;
        await new Promise(() => {});
        return;
      }
      api.text('recovered');
      api.end();
    },
  });
  const firstReply = await post(port, { message: 'hang' });
  assert.equal(firstReply.events.at(-1).data.reason, 'timeout');
  // The discarded handle's dispose never finishes, so the next resume sees a write conflict with no
  // overlapping attempt left in this process. The Doctor must recover, not stay down for good.
  const second = await post(port, { message: 'after the conflict' });
  assert.equal(second.status, 200);
  assert.equal(second.events.at(-1).data.reason, 'complete');
  assert.equal(fake.created.length, 2);
  assert.notEqual(fake.created[0].sessionId, fake.created[1].sessionId, 'a fresh session recovers the Doctor');
  assert.equal(chat.sessionId, fake.created[1].sessionId);
  assert.equal(fake.followed.length, 2);
  assert.ok(logged.some((line) => line
    === 'Arca canvas chat: stored session is still owned (SessionAlreadyOwnedError); starting a new one'));
});

test('a stale resume rejection cannot corrupt the live agent', async (t) => {
  const race = raceDsh();
  let sequence = 0;
  const { chat, port, statePath } = await harness(t, {
    fake: race,
    setupMs: 25,
    turnMs: 500,
    cancelMs: 20,
    uuid: () => `candidate-${++sequence}`,
  });
  await writeFile(statePath, JSON.stringify({
    version: 1, sessionId: 'saved-doctor', day: dayOf(Date.now()), dailyTokens: 0, messages: [],
  }));
  const first = await post(port, { message: 'stale resume' });
  assert.equal(first.events.at(-1).data.reason, 'error');
  const second = await post(port, { message: 'live resume' });
  assert.equal(second.events.at(-1).data.reason, 'complete');
  assert.equal(chat.sessionId, 'saved-doctor');
  assert.deepEqual(race.disposed, []);

  race.rejectGate();
  await new Promise((resolve) => setTimeout(resolve, 40));
  assert.deepEqual(race.disposed, [], 'a rejected stale resume disposes nothing');
  assert.equal(chat.sessionId, 'saved-doctor');
  assert.equal(race.globals.get('arca_status').when({ agent: { id: 'saved-doctor' } }), true);

  await chat.close();
  assert.deepEqual(race.disposed, ['saved-doctor']);
});
