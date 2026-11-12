import assert from 'node:assert/strict';
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { ArcaError } from '../src/arca-client.js';
import {
  ChatStore, DEFAULTS, DOCTOR_TOOLS, NOT_LINKED, PROPOSE_ACTIONS, PROPOSE_TOOL, READ_TOOLS, createDoctorChat, createProposeTool,
  createReadTools, dayOf, doctorPrompt, formatToolResult, sseFrame, stateFilePath,
} from '../src/chat.js';

const GiB = 1024 ** 3;

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
  failResume = false, hangFirstDispose = false, onAgentSetUp = null,
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
        onAgentSetUp?.();
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
function raceDsh({ conflictResume = false, releaseOnConflict = false } = {}) {
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
          // The first resume lets go after the collision, not during it. That is later than the retry of a chat
          // that does not wait for the holder, which collides again and fails; and it is before any timer, because an
          // immediate set while a request is being handled runs before the next timer. So the second attempt's bound
          // cannot pass first, and the second attempt never waits on the test.
          if (releaseOnConflict) setImmediate(resolveGate);
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
  let chat;
  let server;
  // One hook: the server goes first and the directory last. A server left listening keeps the test process alive for
  // good, so nothing may come between a failed test and its close: the hooks of a test stop at the first one that
  // throws, removing the directory can throw while the chat still writes its state there, and the chat's own close
  // waits for the agent's dispose, which a test may have made hang. That close gets a limit for the same reason.
  t.after(async () => {
    try {
      if (server) {
        await new Promise((resolve) => {
          server.closeAllConnections?.();
          server.close(resolve);
        });
      }
    } finally {
      try {
        if (chat) await within(chat.close(), 5000, 'the chat did not close');
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    }
  });
  const statePath = join(dir, 'chat.json');
  const fake = options.fake ?? fakeDsh(options);
  const store = options.store ?? new ChatStore(statePath, {
    now: options.now,
    ...(options.readFile ? { readFile: options.readFile } : {}),
  });
  const clientCalls = [];
  chat = createDoctorChat({
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
  // What a test may wait for in place of a pause: every request the chat has finished with, and every response
  // whose client went away before its end.
  const handled = [];
  let dropped = 0;
  server = createServer((req, res) => {
    const path = new URL(req.url, 'http://local').pathname;
    if (path === '/chat') {
      res.on('close', () => {
        if (!res.writableEnded) dropped += 1;
      });
      const done = chat.handle(req, res);
      handled.push(done.then(() => {}, () => {}));
    } else if (path === '/chat/history') void chat.history(res);
    else res.writeHead(404).end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    chat, fake, statePath, dir, port: server.address().port, clientCalls,
    dropped: () => dropped,
    settled: () => within(Promise.all(handled), 3000, 'the chat has not finished with its requests'),
  };
}

// The promise's own outcome, or a failure that says what was waited for once the limit has passed.
async function within(promise, ms, what) {
  let timer;
  const late = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`timed out: ${what}`)), ms);
  });
  try {
    return await Promise.race([promise, late]);
  } finally {
    clearTimeout(timer);
  }
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

// Harness options for a turn whose model run must time out. One deadline covers a turn's setup and its run, so a
// real deadline short enough to wait for can be over before setup ends: the run then never starts, nothing is
// cancelled and the request ends in 'error'. That is a slow machine and not the chat. So the deadline here is long
// and the chat reads the test's clock, which stands still until the first agent is set up and then jumps to the
// deadline. Setup always fits, the run always starts, and its timer has one millisecond left. Later turns of the same
// test get the whole deadline.
function runTimesOut() {
  const turnMs = 60_000;
  let at = Date.parse('2026-11-08T12:00:00Z');
  let jumped = false;
  return {
    turnMs,
    setupMs: turnMs,
    now: () => at,
    onAgentSetUp() {
      if (jumped) return;
      jumped = true;
      at += turnMs;
    },
  };
}

// A chat store that keeps its document in memory, for a test that pins a short real bound on setup. Such a test needs
// the bound to pass for a setup that hangs and to hold for the next one, which does not. With the real store every
// setup waits for the disk, and a slow disk outlasts a 20 ms bound. Nothing the chat awaits from this store leaves
// the microtask queue, and a timer only fires between tasks: from a request's arrival to the end of a setup that
// does not hang no timer can fire, on any machine.
function memoryStore(initial = {}) {
  let state = {
    version: 1, sessionId: null, day: dayOf(Date.now()), dailyTokens: 0, budgetExhausted: false, messages: [],
    ...initial,
  };
  let queue = Promise.resolve();
  return {
    get state() {
      return state;
    },
    async read() {
      return state;
    },
    patch(update) {
      const run = queue.then(async () => {
        state = await update(state);
        return state;
      });
      queue = run.then(() => undefined, () => undefined);
      return run;
    },
    async flush() {
      await queue;
    },
    exhaust() {},
  };
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

test('the proposal tool offers exactly the twenty-two allowed actions, never a database delete', () => {
  assert.deepEqual([...PROPOSE_ACTIONS], [
    'service_create', 'service_remove', 'service_import', 'service_release',
    'service_publish', 'service_unpublish',
    'service_connect', 'service_disconnect', 'postgres_create', 'gpu_attach', 'gpu_detach',
    'storage_add', 'snapshot_create', 'snapshot_delete',
    'instance_start', 'instance_stop', 'instance_restart',
    'tenant_create', 'tenant_remove', 'deploy_remove', 'function_create', 'function_remove',
  ]);
  assert.equal(PROPOSE_ACTIONS.includes('postgres_delete'), false);
  assert.equal(PROPOSE_ACTIONS.includes('gpu_delete'), false);
  const tool = createProposeTool({ hosts: [{ name: 'alpha' }], proposalHost: 'alpha' });
  assert.equal(tool.name, PROPOSE_TOOL);
  assert.match(tool.description, /propose on alpha and on machines linked to it for approvals; another machine is refused/);
  assert.match(tool.parameters.properties.host.description, /alpha and the machines linked to it for approvals can be proposed on/);
  assert.deepEqual(tool.parameters.properties.action.enum, [...PROPOSE_ACTIONS]);
  assert.equal(tool.parameters.properties.action.enum.includes('postgres_delete'), false);
  assert.equal(tool.parameters.required.includes('action'), true);
  assert.equal(tool.parameters.required.includes('args'), true);
  assert.match(tool.description, /postgres_create \{name\}/);
  // A database proposal never chooses its image; a function is an image, so only function_create names one.
  assert.doesNotMatch(tool.description, /postgres_delete|postgres_create \{[^}]*image/);
  assert.equal(tool.description.match(/\bimage\b/g).length, 1);
  assert.match(tool.description, /function_create \{name, image, digest, /);
});

// A fake reader per host. The page host's arca_status names the hosts its kernel relays approvals to.
function linkedClients(calls, linkedHosts) {
  return (name) => ({
    async call(verb, args) {
      calls.push({ host: name, verb, args });
      if (verb === 'arca_status') {
        return linkedHosts === null ? { mode: 'kernel' } : { mode: 'kernel', approval: { linkedHosts, trustedKeyFiles: [] } };
      }
      return { ...structuredClone(PROPOSAL), host: name };
    },
  });
}

test('a host the page host does not list as linked is refused before Arca records anything', async () => {
  for (const linked of [null, [], ['gamma']]) {
    const calls = [];
    const tool = createProposeTool({
      hosts: [{ name: 'alpha' }, { name: 'beta' }],
      proposalHost: 'alpha',
      clientFor: linkedClients(calls, linked),
    });
    const exec = { callId: 'call-alpha', signal: new AbortController().signal };
    const accepted = await tool.execute({ action: 'service_create', args: { name: 'nats', template: 'nats' } }, exec);
    assert.equal(accepted.id, PROPOSAL.id);
    assert.equal(accepted.host, 'alpha');
    await assert.rejects(
      tool.execute({ host: 'beta', action: 'service_create', args: { name: 'nats', template: 'nats' } }, exec),
      (error) => error.message === `beta: ${NOT_LINKED}.`,
    );
    assert.deepEqual(
      calls.map((call) => [call.host, call.verb]),
      [['alpha', 'action_propose'], ['alpha', 'arca_status']],
      'the unlinked host never reaches Arca',
    );
    assert.deepEqual(calls[0].args, { action: 'service_create', args: { name: 'nats', template: 'nats' } });
  }
  assert.equal(NOT_LINKED, 'this machine is not linked for approvals yet');
});

test('a host the page host lists as linked is proposed on its own kernel', async () => {
  const calls = [];
  const proposals = [];
  const tool = createProposeTool({
    hosts: [{ name: 'alpha' }, { name: 'beta' }],
    proposalHost: 'alpha',
    clientFor: linkedClients(calls, ['beta']),
    onProposal: (proposal) => proposals.push(proposal),
  });
  const exec = { callId: 'call-beta', signal: new AbortController().signal };
  const accepted = await tool.execute({ host: 'beta', action: 'service_create', args: { name: 'nats', template: 'nats' } }, exec);
  assert.equal(accepted.id, PROPOSAL.id);
  assert.equal(accepted.host, 'beta');
  assert.deepEqual(calls.map((call) => [call.host, call.verb]), [['alpha', 'arca_status'], ['beta', 'action_propose']]);
  assert.deepEqual(calls[1].args, { action: 'service_create', args: { name: 'nats', template: 'nats' } });
  assert.deepEqual(proposals.map(({ host, id }) => [host, id]), [['beta', PROPOSAL.id]]);
});

test('a linked host on another machine is proposed on exactly like one on the page host machine', async () => {
  // gamma runs on another machine: its own status says its readings are remote, and it keeps alpha's passkeys. Only the
  // page host's linkedHosts decides; where gamma runs does not.
  const calls = [];
  const tool = createProposeTool({
    hosts: [{ name: 'alpha' }, { name: 'gamma', socketPath: '/run/arca-alpha/gamma.sock' }],
    proposalHost: 'alpha',
    clientFor(name) {
      return {
        async call(verb, args) {
          calls.push({ host: name, verb, args });
          if (verb === 'arca_status' && name === 'alpha') {
            return { mode: 'kernel', metricsSource: 'local', approval: { linkedHosts: ['gamma'], trustedKeyFiles: [] } };
          }
          if (verb === 'arca_status') {
            return {
              mode: 'kernel',
              metricsSource: 'remote',
              approval: { linkedHosts: [], trustedKeyFiles: [], peerKeys: { from: 'alpha', keys: 1, since: null, reason: '' } },
            };
          }
          return { ...structuredClone(PROPOSAL), host: name };
        },
      };
    },
  });
  const accepted = await tool.execute(
    { host: 'gamma', action: 'service_create', args: { name: 'nats', template: 'nats' } },
    { callId: 'call-gamma', signal: new AbortController().signal },
  );
  assert.equal(accepted.host, 'gamma');
  assert.deepEqual(calls.map((call) => [call.host, call.verb]), [['alpha', 'arca_status'], ['gamma', 'action_propose']]);
});

test('a page host whose status cannot be read links nothing', async () => {
  const calls = [];
  const tool = createProposeTool({
    hosts: [{ name: 'alpha' }, { name: 'beta' }],
    proposalHost: 'alpha',
    clientFor(name) {
      return {
        async call(verb) {
          calls.push([name, verb]);
          if (verb === 'arca_status') throw new Error('socket closed');
          return structuredClone(PROPOSAL);
        },
      };
    },
  });
  await assert.rejects(
    tool.execute({ host: 'beta', action: 'service_create', args: { name: 'nats', template: 'nats' } }, { callId: 'c' }),
    (error) => error.message === `beta: ${NOT_LINKED}.`,
  );
  assert.deepEqual(calls, [['alpha', 'arca_status']]);
});

test('the prompt pins the propose-and-approve contract', () => {
  const prompt = doctorPrompt({ hosts: [{ name: 'alpha' }], approvalUrl: 'https://example.com/approve' });
  assert.match(prompt, /arca_propose/);
  assert.match(prompt, /tap Approve on the card/);
  assert.match(prompt, /Never claim the change is done/);
  assert.match(prompt, /propose changes on alpha and on any machine linked to it for approvals/);
  assert.match(prompt, /this machine is not linked for approvals yet/);
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
    '- Sizes are always in GB with one decimal. Memory and GPU memory use 1024-based GB, so a 16 GB card',
    '  reads 16 GB; disks use decimal GB (17.0 GB) to match the drive labels. Power is W and temperature is',
    '  °C with one decimal.',
    '- Network rates keep the unit the tool gives: KB/s, MB/s or GB/s, with one decimal. Never show raw bytes.',
    '- Percentages are whole numbers, for example 70%, not 70.29%.',
    '- When the owner asks about a GPU, name each card and say how busy it is. Say who is using it by',
    '  project and instance, by Docker container, or "on the host", with that user\'s memory in GB. A card',
    '  with no reading is "not measurable from here" or "could not be read just now"; never say 0% without',
    '  a reading. Say a card is held back by its power limit only when the reading says so.',
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
    '- For a GPU attach or detach, propose gpu_attach or gpu_detach with the instance and the card\'s exact',
    '  PCI address. The approval card shows the project, instance, card and safety effect; never shorten it.',
    '- For snapshot storage, read storage_list first. Propose storage_add with a disk whose offer is "whole disk"',
    '  (device) or a folder on a disk whose offer is "folder" (path); never a system disk. A whole disk is erased:',
    '  say so plainly. To snapshot an app, a data volume or a host folder such as /home, propose snapshot_create',
    '  onto a pool whose snapshots flag is true; to remove one, propose snapshot_delete with a name from',
    '  snapshot_list. A snapshot never starts and never changes what it copies.',
    '- To start, stop or restart an instance, propose instance_start, instance_stop or instance_restart with its',
    '  project and instance. Arca does this only for an instance it owns or has adopted; any other is refused. A',
    '  stop or a restart interrupts what the instance serves and runs only after the owner approves the card. A',
    '  stop is graceful and never forced: an instance that does not shut down in time stays running and the',
    '  change is refused; say so plainly.',
    '- To take over a service another manager created, where it stands, propose service_import with its name;',
    '  to hand it back, propose service_release. Both run only after the owner approves the card. A node',
    '  imports only the services its own configuration lists, and refuses the proposal itself when the import',
    '  cannot run. You cannot see that list or the check that failed: when a proposal is refused, say that it',
    '  was refused, that nothing changed, and that the reason is in the node\'s own output (arca service import',
    '  plan). Never guess a reason.',
    '- Arca lists and reads an imported service but never changes, removes, publishes or rewires it.',
    '  service_release hands it back to the manager that created it, and Arca stops listing it.',
    '- A tenant is a walled-off space on this machine for someone else\'s services, with its own limits. It has',
    '  no name: call it by its id. Read tenant_list, then tenant_status or tenant_usage with the id. To make one,',
    '  propose tenant_create; give caps only when the owner asks for more than this machine\'s defaults. To remove',
    '  one, propose tenant_remove with its id: every service of the tenant is deleted with its data; say so',
    '  plainly.',
    '- tenant_usage names the deploy each of a tenant\'s instances came from. To remove what one deploy created,',
    '  propose deploy_remove with the tenant\'s id and the deploy: its services are deleted with their data, and',
    '  nothing else of the tenant is touched. A deploy that is still provisional is refused; say so plainly.',
    '- A function is a small program, an image by its digest, that runs on a timer. Read function_list, and',
    '  function_runs with its name for its newest runs. To make one, propose function_create with its name,',
    '  image, digest and timer, never an image tag, and never a secret in env: name each secret in secrets. The',
    '  canvas cannot set a secret\'s value; say so. To remove one, propose function_remove with its name: a run',
    '  that is going is stopped and the function is deleted with its secrets; its past runs stay in the log.',
    '- A tenant, deploy or function change runs only after the owner approves its card, like every other change.',
    '- You can propose changes on alpha and on any machine linked to it for approvals. Another machine is',
    '  refused with "this machine is not linked for approvals yet"; say that plainly.',
    '- Do not propose a change the owner did not ask for, and never propose a database delete.',
    '- The approval page is at https://example.com/approve. The canvas card uses the same passkey.',
  ].join('\n');
  assert.equal(prompt, snapshot);
  assert.match(prompt, /If a read fails or a host did not answer, say "could not be read just now"\./);
  assert.match(prompt, /Only a reading the host can\s+never give, such as remote CPU load, is "not measurable from here"\./);
  assert.match(prompt, /Network rates keep the unit the tool gives: KB\/s, MB\/s or GB\/s/);
  assert.doesNotMatch(prompt, /Storage sizes are always/);
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
        totalBytes: 96.3 * GiB,
        availableBytes: 79.3 * GiB,
        usedBytes: 17 * GiB,
        swapTotalBytes: 8 * GiB,
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
        totalBytes: 96.3 * GiB,
        availableBytes: null,
        usedBytes: null,
        swapTotalBytes: 8 * GiB,
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
  for (const raw of [17 * GiB, 96.3 * GiB, 79.3 * GiB, 3_400_000_000, 8 * GiB]) {
    assert.equal(text.includes(String(raw)), false, `${raw} is never shown to the model`);
  }
});

test('a remote CPU keeps its known core count while its load is unmeasurable', () => {
  const text = formatToolResult('node_metrics', {
    host: 'alpha',
    minutes: 5,
    latest: {
      cpu: { usedPct: null, cores: 16, coresKnown: true, reason: 'not available for a remote host' },
      memory: { totalBytes: 32 * GiB, availableBytes: 24 * GiB, usedBytes: 8 * GiB, reason: null },
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
      memory: { totalBytes: 32 * GiB, availableBytes: 24 * GiB, usedBytes: null, reason: null },
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
    memory: { totalBytes: 8 * GiB, availableBytes: 4 * GiB, usedBytes: 4 * GiB, reason: null },
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
        totalBytes: 16 * GiB, availableBytes: 8 * GiB, usedBytes: 8 * GiB, reason: null,
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
      totalBytes: 16 * GiB, availableBytes: 8 * GiB, usedBytes: 8 * GiB, reason: null,
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
    ...runTimesOut(),
    // This test ends the cancelled run itself. The fallback that would end it for the chat must not get there first.
    cancelMs: 60_000,
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
  await until(() => fake.cancels.length === 1);
  assert.deepEqual(fake.cancels, ['timeout']);
  assert.equal(fake.followed.length, 1, 'the run started before its deadline passed');
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
    hangCreateOnce: true, setupMs: 20, store: memoryStore(),
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
  // No bound may pass here: the setup hangs until the test lets it go.
  const { fake, port, dropped, settled } = await harness(t, {
    hangCreateOnce: true, setupMs: 60_000, turnMs: 60_000,
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
  controller.abort();
  // The setup ends only after the chat has seen the client go, and the chat is asked again only after it has
  // finished with that request. Both are waited for, so the order is the test's and not the machine's.
  await until(() => dropped() === 1);
  fake.releaseCreate();
  await settled();
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
    ...runTimesOut(),
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
  const setupMs = 5000;
  const { port } = await harness(t, { hangCreateOnce: true, setupMs, turnMs: 50 });
  const started = Date.now();
  const result = await post(port, { message: 'deadline' });
  const elapsed = Date.now() - started;
  assert.equal(result.events.at(-1).data.reason, 'error');
  // Only the turn deadline can have ended a setup that hangs before its own bound.
  assert.ok(elapsed < setupMs, `setup must not outlive the turn deadline (${elapsed}ms)`);
});

test('an agent error with no turn/end releases the turn and the next request', async (t) => {
  let calls = 0;
  // The deadline is long on purpose: only the error may release this turn, and a slow setup must not spend it.
  const turnMs = 60_000;
  const { fake, port } = await harness(t, {
    turnMs,
    setupMs: turnMs,
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
  assert.deepEqual(fake.cancels, ['error']);
  // The fallback this test set to 20 ms released the turn. The default one would have taken this long.
  assert.ok(elapsed < DEFAULTS.cancelMs, `the error released the turn in ${elapsed}ms`);
  const second = await post(port, { message: 'after error' });
  assert.equal(second.status, 200);
  assert.equal(second.events.at(-1).data.reason, 'complete');
});

test('a stale create cannot replace or dispose the live agent', async (t) => {
  const race = raceDsh();
  let sequence = 0;
  const { chat, port } = await harness(t, {
    fake: race,
    store: memoryStore(),
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
  const race = raceDsh({ conflictResume: true, releaseOnConflict: true });
  let sequence = 0;
  const store = memoryStore({ sessionId: 'saved-doctor' });
  const { chat, port } = await harness(t, {
    fake: race,
    store,
    setupMs: 200,
    turnMs: 1000,
    cancelMs: 20,
    uuid: () => `candidate-${++sequence}`,
  });
  const first = await post(port, { message: 'stale resume' });
  assert.equal(first.events.at(-1).data.reason, 'error');
  // The first resume is still holding the session write handle. The second attempt races it and must wait,
  // not treat the write conflict as a missing session and create a new id. The first resume lets go when the
  // second collides with it.
  const second = await post(port, { message: 'live resume' });
  assert.equal(second.events.at(-1).data.reason, 'complete');
  assert.equal(race.resumeIds.length, 3, 'the hanging resume, the one that collided with it, and its retry');
  assert.equal(race.createdIds.length, 0, 'the newer setup never forks a new DSH session');
  assert.deepEqual([...new Set(race.resumeIds)], ['saved-doctor'], 'both setups stay on the stored session id');
  assert.equal(chat.sessionId, 'saved-doctor');
  assert.equal(store.state.sessionId, 'saved-doctor');
  await chat.close();
});

test('a write conflict with no live holder recovers with a fresh session', async (t) => {
  const logged = [];
  let first = true;
  let sequence = 0;
  const { chat, fake, port } = await harness(t, {
    ...runTimesOut(),
    failResume: true,
    hangFirstDispose: true,
    endOnCancel: false,
    cancelMs: 20,
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
  const { chat, port } = await harness(t, {
    fake: race,
    store: memoryStore({ sessionId: 'saved-doctor' }),
    setupMs: 25,
    turnMs: 500,
    cancelMs: 20,
    uuid: () => `candidate-${++sequence}`,
  });
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

test('the Doctor reads disks and snapshots and may propose storage and snapshot changes', () => {
  for (const name of ['storage_list', 'snapshot_list']) assert.equal(READ_TOOLS.includes(name), true, name);
  for (const name of ['storage_add', 'snapshot_create', 'snapshot_delete']) {
    assert.equal(PROPOSE_ACTIONS.includes(name), true, name);
  }
  const tool = createProposeTool({ hosts: [{ name: 'alpha' }], proposalHost: 'alpha' });
  assert.match(tool.description, /storage_add \{name, device\} or \{name, path\}/);
  assert.match(tool.description, /snapshot_delete \{name\}/);
  const disks = JSON.parse(formatToolResult('storage_list', {
    disks: [{ name: 'sdb', size: 1_000_204_886_016, role: 'free', offer: 'whole disk', children: [{ name: 'sdb1', size: 32_000_000_000 }] }],
    pools: [{ name: 'usb1', used: 0, total: 999_000_000_000, snapshots: true }],
  }));
  assert.equal(disks.disks[0].size, '1000.2 GB');
  assert.equal(disks.disks[0].children[0].size, '32.0 GB');
  assert.equal(disks.pools[0].total, '999.0 GB');
  assert.equal(disks.disks[0].role, 'free');
  const snapshots = JSON.parse(formatToolResult('snapshot_list', { snapshots: [{ name: 'shop--web--20261109T1405', size: 3_000_000_000 }] }));
  assert.equal(snapshots.snapshots[0].size, '3.0 GB');
});

test('the Doctor may propose a start, a stop and a restart of an instance, and each reaches action_propose', async () => {
  const actions = ['instance_start', 'instance_stop', 'instance_restart'];
  const calls = [];
  const proposals = [];
  const tool = createProposeTool({
    hosts: [{ name: 'alpha' }],
    proposalHost: 'alpha',
    clientFor(name) {
      return {
        async call(verb, args) {
          calls.push({ host: name, verb, args });
          const word = { instance_start: 'Start', instance_stop: 'Stop', instance_restart: 'Restart' }[args.action];
          return {
            id: `act-${String(actions.indexOf(args.action) + 1).repeat(12)}`,
            action: args.action,
            summary: `${word} the instance ${args.args.instance} in project ${args.args.project}`,
          };
        },
      };
    },
    onProposal(proposal) { proposals.push(proposal); },
  });
  assert.match(tool.description, /instance_start \{project, instance\}; instance_stop \{project, instance\}; instance_restart \{project, instance\}; /);
  for (const action of actions) {
    assert.equal(PROPOSE_ACTIONS.includes(action), true, action);
    assert.equal(tool.parameters.properties.action.enum.includes(action), true, action);
    const accepted = await tool.execute({ action, args: { project: 'studio', instance: 'web' } }, { callId: `call-${action}` });
    assert.equal(accepted.id, `act-${String(actions.indexOf(action) + 1).repeat(12)}`);
    const rendered = tool.output.render({}, accepted)[0].text;
    assert.match(rendered, /the instance web in project studio\. It waits for the owner's approval\./);
  }
  assert.deepEqual(calls, actions.map((action) => ({
    host: 'alpha', verb: 'action_propose', args: { action, args: { project: 'studio', instance: 'web' } },
  })));
  assert.deepEqual(proposals.map(({ id }) => id), ['act-111111111111', 'act-222222222222', 'act-333333333333']);
  // A force stop or a delete of an instance is not something the canvas can ask for.
  for (const action of ['instance_delete', 'instance_kill', 'instance_force_stop']) {
    await assert.rejects(
      tool.execute({ action, args: { project: 'studio', instance: 'web' } }, { callId: 'c' }),
      /That action cannot be proposed\./,
    );
  }

  const prompt = doctorPrompt({ hosts: [{ name: 'alpha' }], approvalUrl: 'https://example.com/approve' });
  assert.match(prompt, /propose instance_start, instance_stop or instance_restart/);
  assert.match(prompt, /only for an instance it owns or has adopted/);
  assert.match(prompt, /runs only after the owner approves the card/);
  assert.match(prompt, /graceful and never forced/);
  assert.match(prompt, /does not shut down in time stays running/);
});

test('the Doctor may propose an import and a release of a service, and each reaches action_propose', async () => {
  const actions = ['service_import', 'service_release'];
  // The kernel's own summaries (crates/arca/src/action_proposal.rs).
  const summaries = {
    service_import: 'Import the service db1, which another manager created, where it stands',
    service_release: 'Release the imported service db1 back to the manager that created it',
  };
  const calls = [];
  const proposals = [];
  const tool = createProposeTool({
    hosts: [{ name: 'alpha' }],
    proposalHost: 'alpha',
    clientFor(name) {
      return {
        async call(verb, args) {
          calls.push({ host: name, verb, args });
          if (args.args.name === 'unlisted') {
            // What the real client throws for a kernel refusal: the code alone, never the kernel's text.
            throw new ArcaError('rpc');
          }
          return {
            id: `act-${String(actions.indexOf(args.action) + 1).repeat(12)}`,
            action: args.action,
            summary: summaries[args.action],
          };
        },
      };
    },
    onProposal(proposal) { proposals.push(proposal); },
  });
  assert.match(tool.description, /service_remove \{name\}; service_import \{name\}; service_release \{name\}; /);
  for (const action of actions) {
    assert.equal(PROPOSE_ACTIONS.includes(action), true, action);
    assert.equal(tool.parameters.properties.action.enum.includes(action), true, action);
    const accepted = await tool.execute({ action, args: { name: 'db1' } }, { callId: `call-${action}` });
    assert.equal(accepted.id, `act-${String(actions.indexOf(action) + 1).repeat(12)}`);
    assert.equal(tool.output.render({}, accepted)[0].text, `${accepted.id}: ${summaries[action]}. It waits for the owner's approval.`);
  }
  assert.deepEqual(calls, actions.map((action) => ({
    host: 'alpha', verb: 'action_propose', args: { action, args: { name: 'db1' } },
  })));
  assert.deepEqual(proposals.map(({ id }) => id), ['act-111111111111', 'act-222222222222']);
  // The kernel refuses an import that cannot run. The tool fails with the client's fixed sentence, which names
  // no check, and no proposal is recorded.
  await assert.rejects(
    tool.execute({ action: 'service_import', args: { name: 'unlisted' } }, { callId: 'c' }),
    (error) => error.message === 'Arca could not complete the MCP request.',
  );
  assert.equal(proposals.length, 2);
  // The canvas cannot ask for the direct adoption path or a forced import.
  for (const action of ['service_adopt', 'service_import_force', 'adopt']) {
    await assert.rejects(tool.execute({ action, args: { name: 'db1' } }, { callId: 'c' }), /That action cannot be proposed\./);
  }

  const prompt = doctorPrompt({ hosts: [{ name: 'alpha' }], approvalUrl: 'https://example.com/approve' });
  assert.match(prompt, /propose service_import with its name/);
  assert.match(prompt, /to hand it back, propose service_release\. Both run only after the owner approves the card\./);
  assert.match(prompt, /imports only the services its own configuration lists, and refuses the proposal itself/);
  assert.match(prompt, /You cannot see that list or the check that failed/);
  assert.match(prompt, /say that it\s+was refused, that nothing changed, and that the reason is in the node's own output \(arca service import\s+plan\)\. Never guess a reason\./);
  assert.match(prompt, /Arca lists and reads an imported service but never changes, removes, publishes or rewires it\./);
  assert.match(prompt, /service_release hands it back to the manager that created it, and Arca stops listing it\./);
  assert.doesNotMatch(prompt, /fully moved|until it is released/);
});

test('the Doctor reads tenants and functions, and each read sends the kernel only the arguments it takes', async () => {
  const reads = ['tenant_list', 'tenant_status', 'tenant_usage', 'function_list', 'function_runs'];
  for (const name of reads) assert.equal(READ_TOOLS.includes(name), true, name);
  const calls = [];
  const tools = Object.fromEntries(createReadTools({
    hosts: [{ name: 'alpha' }],
    clientFor(host) {
      return {
        async call(verb, args) {
          calls.push({ host, verb, args });
          return { answered: verb };
        },
      };
    },
  }).map((tool) => [tool.name, tool]));
  for (const name of reads) {
    assert.equal(typeof tools[name]?.execute, 'function', name);
    assert.match(tools[name].description, /\w/, name);
  }
  // One tenant is read by its id, in the kernel's shape; a function's runs by its name.
  for (const name of ['tenant_status', 'tenant_usage']) {
    const { properties, required } = tools[name].parameters;
    assert.deepEqual(required, ['id'], name);
    assert.equal(properties.id.pattern, '^[a-z2-7]{8}$', name);
    assert.deepEqual(Object.keys(properties).sort(), ['host', 'id'], name);
  }
  assert.deepEqual(tools.function_runs.parameters.required, ['name']);
  assert.deepEqual(Object.keys(tools.function_runs.parameters.properties).sort(), ['host', 'limit', 'name', 'run']);
  assert.equal(tools.function_runs.parameters.properties.limit.maximum, 100);
  for (const name of ['tenant_list', 'function_list']) {
    assert.deepEqual(Object.keys(tools[name].parameters.properties), ['host'], name);
    assert.equal(tools[name].parameters.required, undefined, name);
  }

  assert.deepEqual(await tools.tenant_list.execute({ host: 'alpha' }), { answered: 'tenant_list', host: 'alpha' });
  await tools.tenant_status.execute({ id: 'abcd2345', extra: 'dropped' });
  await tools.tenant_usage.execute({ id: ' abcd2345 ' });
  await tools.function_list.execute({ name: 'dropped' });
  await tools.function_runs.execute({ name: 'relay', limit: 500, run: 'fr-1700000000000-1', extra: 'dropped' });
  await tools.function_runs.execute({ name: 'relay', limit: 0 });
  assert.deepEqual(calls, [
    { host: 'alpha', verb: 'tenant_list', args: {} },
    { host: 'alpha', verb: 'tenant_status', args: { id: 'abcd2345' } },
    { host: 'alpha', verb: 'tenant_usage', args: { id: 'abcd2345' } },
    { host: 'alpha', verb: 'function_list', args: {} },
    { host: 'alpha', verb: 'function_runs', args: { name: 'relay', limit: 100, run: 'fr-1700000000000-1' } },
    { host: 'alpha', verb: 'function_runs', args: { name: 'relay' } },
  ]);
});

test('the Doctor may propose a tenant, a deploy and a function change, and each reaches action_propose', async () => {
  const digest = `sha256:${'a'.repeat(64)}`;
  // Each action with its arguments and the kernel's own summary (crates/arca/src/action_proposal.rs and function.rs).
  const cases = [
    ['tenant_create', {}, 'Create a tenant'],
    ['tenant_create', { caps: { instances: 4, containers: 4, virtualMachines: 1, cpu: 4, memory: '8GiB', disk: '40GiB' } },
      'Create a tenant with caps above this node\'s defaults'],
    ['tenant_remove', { id: 'abcd2345' }, 'Remove the tenant abcd2345 and everything in it'],
    ['deploy_remove', { tenant: 'abcd2345', deploy: 'shop-1' }, 'Remove everything the deploy shop-1 created, in tenant abcd2345'],
    ['function_create', {
      name: 'relay', image: 'registry.example/acme/relay', digest, triggers: [{ timer: { everySeconds: 300 } }], secrets: ['RELAY_KEY'],
    }, 'Create the function relay from registry.example/acme/relay and run it every 5 minutes'],
    ['function_remove', { name: 'relay' }, 'Remove the function relay'],
  ];
  const calls = [];
  const tool = createProposeTool({
    hosts: [{ name: 'alpha' }],
    proposalHost: 'alpha',
    clientFor(name) {
      return {
        async call(verb, args) {
          calls.push({ host: name, verb, args });
          const index = cases.findIndex(([action, given]) => action === args.action && given === args.args);
          return { id: `act-${String(index + 1).repeat(12)}`, action: args.action, summary: cases[index][2] };
        },
      };
    },
  });
  assert.match(tool.description, /tenant_remove \{id\}; deploy_remove \{tenant, deploy\}; /);
  assert.match(tool.description, /function_create \{name, image, digest, triggers: \[\{timer: \{everySeconds\}\}\]\}/);
  assert.match(tool.description, /function_remove \{name\}\./);
  for (const [action, args, summary] of cases) {
    assert.equal(PROPOSE_ACTIONS.includes(action), true, action);
    assert.equal(tool.parameters.properties.action.enum.includes(action), true, action);
    const accepted = await tool.execute({ action, args }, { callId: `call-${action}` });
    assert.equal(tool.output.render({}, accepted)[0].text, `${accepted.id}: ${summary}. It waits for the owner's approval.`);
  }
  // The arguments reach the kernel as given: it checks them against the verb's own schema.
  assert.deepEqual(calls, cases.map(([action, args]) => ({ host: 'alpha', verb: 'action_propose', args: { action, args } })));
  // Setting a function's secret, rolling a deploy back or confirming one is not something the canvas can ask for.
  for (const action of ['function_secret_set', 'deploy_rollback', 'deploy_confirm', 'function_run']) {
    await assert.rejects(tool.execute({ action, args: { name: 'relay' } }, { callId: 'c' }), /That action cannot be proposed\./);
  }

  const prompt = doctorPrompt({ hosts: [{ name: 'alpha' }], approvalUrl: 'https://example.com/approve' });
  assert.match(prompt, /Read tenant_list, then tenant_status or tenant_usage with the id\./);
  assert.match(prompt, /every service of the tenant is deleted with its data; say so\s+plainly\./);
  assert.match(prompt, /tenant_usage names the deploy each of a tenant's instances came from\./);
  assert.match(prompt, /A deploy that is still provisional is refused; say so plainly\./);
  assert.match(prompt, /never an image tag, and never a secret in env/);
  assert.match(prompt, /canvas cannot set a secret's value; say so\./);
  assert.match(prompt, /A tenant, deploy or function change runs only after the owner approves its card/);
});
