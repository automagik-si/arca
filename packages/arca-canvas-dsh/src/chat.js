import { randomUUID } from 'node:crypto';
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { homedir } from 'node:os';

// The Doctor chat is read-only by construction: only these verbs are registered with DSH. Every write verb
// (postgres_link/unlink/delete, service_*, plan/approve/apply, token_*, adopt, ...) is absent from this list and
// cannot be reached from a model request.
export const READ_TOOLS = Object.freeze([
  'arca_status', 'watch_report', 'postgres_list', 'node_metrics',
  'alerts_list', 'plan_list', 'journal_list', 'service_list',
]);

export const DEFAULTS = Object.freeze({
  toolCallsPerTurn: 40,
  outputTokensPerTurn: 64_000,
  dailyTokenBudget: 1_000_000,
  heartbeatMs: 15_000,
  turnMs: 9 * 60 * 1000,
  bodyBytes: 64 * 1024,
  messageChars: 16 * 1024,
  resultChars: 16_000,
  historyMessages: 200,
});

const SESSION_NAME = 'Doctor';
const MODEL = Object.freeze({ provider: 'deepseek-official', model: 'deepseek-flash' });
const DAY_MS = 24 * 60 * 60 * 1000;

// A write-intent hint is presentation only: it can add an approval card, never an action. The model is told to answer
// "what it would do" and link /approve; the card makes that link reachable in the panel.
const WRITE_INTENT = new RegExp(
  '\\b(?:delete|remove|drop|unlink|link|start|stop|restart|recreate|apply|approve|adopt|mint|rotate|publish|'
  + 'unpublish|deploy|scale|rename|create|add|set|update|change|move|pair|connect|disconnect)\\b', 'i');

function capMessage(reason, limit) {
  return reason === 'tool-calls'
    ? `This answer stopped at the ${limit}-tool-call limit for one turn.`
    : `This answer stopped at the ${limit.toLocaleString('en-US')}-output-token limit for one turn.`;
}

export function writeIntent(message) {
  return WRITE_INTENT.test(String(message ?? ''));
}

export function safeApprovalUrl(value) {
  if (typeof value !== 'string' || !/^https?:\/\//i.test(value)) return '';
  try {
    const url = new URL(value);
    return ['http:', 'https:'].includes(url.protocol) && !url.username && !url.password ? url.href : '';
  } catch {
    return '';
  }
}

export function stateFilePath(env = process.env, configured = '') {
  const value = String(configured ?? '').trim();
  if (value) return value;
  const home = String(env.DSH_HOME ?? '').trim() || join(homedir(), '.dsh');
  return join(home, 'storages', 'arca-canvas-chat.json');
}

export function dayOf(ms) {
  return new Date(ms).toISOString().slice(0, 10);
}

function integer(value, fallback, min, max) {
  const number = Math.trunc(Number(value));
  return Number.isFinite(number) && number >= min && number <= max ? number : fallback;
}

function finite(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : 0;
}

function text(value, fallback = '') {
  return value == null ? fallback : String(value);
}

function messageText(content) {
  if (!Array.isArray(content)) return '';
  return content
    .filter((block) => block && block.type === 'text' && typeof block.text === 'string')
    .map((block) => block.text)
    .join('');
}

function bound(value, max) {
  const raw = String(value ?? '');
  return raw.length <= max ? raw : `${raw.slice(0, Math.max(0, max - 1))}…`;
}

function emptyState(ms) {
  return {
    version: 1,
    sessionId: null,
    day: dayOf(ms),
    dailyTokens: 0,
    messages: [],
  };
}

function normalizeState(raw, ms) {
  const fallback = emptyState(ms);
  if (!raw || typeof raw !== 'object') return fallback;
  const messages = Array.isArray(raw.messages)
    ? raw.messages.filter((message) => message && typeof message === 'object'
      && (message.role === 'user' || message.role === 'assistant') && typeof message.text === 'string')
      .slice(-DEFAULTS.historyMessages)
      .map((message) => ({
        role: message.role,
        text: message.text,
        at: typeof message.at === 'string' ? message.at : '',
        ...(Array.isArray(message.tools) ? {
          tools: message.tools.filter((tool) => tool && typeof tool === 'object').slice(0, 64).map((tool) => ({
            callId: text(tool.callId),
            name: READ_TOOLS.includes(tool.name) ? tool.name : 'tool',
            state: ['running', 'done', 'error'].includes(tool.state) ? tool.state : 'done',
            ...(typeof tool.host === 'string' && tool.host ? { host: text(tool.host) } : {}),
            ...(typeof tool.summary === 'string' && tool.summary ? { summary: bound(tool.summary, 240) } : {}),
          })),
        } : {}),
      }))
    : [];
  return {
    version: 1,
    sessionId: typeof raw.sessionId === 'string' && raw.sessionId ? raw.sessionId : null,
    day: typeof raw.day === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(raw.day) ? raw.day : fallback.day,
    dailyTokens: Math.max(0, finite(raw.dailyTokens)),
    messages,
  };
}

// One JSON document under the canvas DSH home. Writes are serialized and atomic, so a crash leaves the previous
// document (never a half-written counter) and a read never sees a partial update.
export class ChatStore {
  constructor(path, { now = Date.now } = {}) {
    this.path = path;
    this.now = typeof now === 'function' ? now : () => now;
    this.queue = Promise.resolve();
  }

  /** Resolve after every queued write has settled and reached durable storage. */
  async flush() {
    await this.queue;
  }

  async read() {
    return this.#roll(await this.#load());
  }

  async patch(update) {
    const run = this.queue.then(async () => {
      const current = this.#roll(await this.#load());
      const next = await update(current);
      await this.#write(next);
      return next;
    });
    this.queue = run.then(() => undefined, () => undefined);
    return run;
  }

  #roll(state) {
    const day = dayOf(this.now());
    if (state.day === day) return state;
    return { ...state, day, dailyTokens: 0 };
  }

  async #load() {
    try {
      return normalizeState(JSON.parse(await readFile(this.path, 'utf8')), this.now());
    } catch {
      return emptyState(this.now());
    }
  }

  async #write(state) {
    await mkdir(dirname(this.path), { recursive: true });
    const temporary = `${this.path}.${process.pid}.${randomUUID()}.tmp`;
    await writeFile(temporary, `${JSON.stringify(state)}\n`, { mode: 0o600 });
    await rename(temporary, this.path);
  }
}

export function sseFrame(event, data) {
  // One JSON object per data line. JSON.stringify escapes every newline inside a string, so model text can never
  // terminate the frame and forge an event such as `arca-cut`.
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
}

export function createSse(response, { heartbeatMs = DEFAULTS.heartbeatMs, onClose } = {}) {
  let closed = false;
  let ended = false;
  let timer;
  response.writeHead(200, {
    'Content-Type': 'text/event-stream; charset=utf-8',
    'Cache-Control': 'no-store',
    'X-Content-Type-Options': 'nosniff',
    'X-Accel-Buffering': 'no',
    Connection: 'keep-alive',
  });
  const write = (chunk) => {
    if (closed || ended) return false;
    try {
      response.write(chunk);
      return true;
    } catch {
      closed = true;
      return false;
    }
  };
  const stop = () => {
    if (timer) clearInterval(timer);
    timer = undefined;
  };
  timer = setInterval(() => write(': hb\n\n'), heartbeatMs);
  timer.unref?.();
  response.on('close', () => {
    stop();
    if (!ended) {
      closed = true;
      onClose?.();
    }
  });
  return {
    send(event, data) {
      return write(sseFrame(event, data));
    },
    comment(value) {
      return write(`: ${value}\n\n`);
    },
    end() {
      if (ended) return;
      ended = true;
      stop();
      try {
        response.end();
      } catch { /* the client is gone */ }
    },
    get closed() {
      return closed || ended;
    },
  };
}

export function doctorPrompt({ hosts = [], approvalUrl = '', now = Date.now() } = {}) {
  const names = hosts.map((host) => text(host?.name)).filter(Boolean);
  const hostLine = names.length ? names.join(', ') : 'the configured host';
  const approval = safeApprovalUrl(approvalUrl);
  const link = approval ? `the approval page at ${approval}` : 'the approval page (not configured on this canvas)';
  return [
    'You are the Arca Doctor, a read-only operations assistant embedded in the Arca canvas.',
    `You can read live Arca data from these hosts: ${hostLine}.`,
    `Today is ${new Date(now).toISOString().slice(0, 10)}.`,
    '',
    'Rules:',
    '- Answer only from data returned by your tools in this conversation. Never invent, estimate or round a',
    '  number, status, name, port or address. If a read fails or a host did not answer, say that plainly.',
    '- Call a tool when the answer depends on current state. Do not answer data questions from memory.',
    '- You are strictly read-only. You cannot link, unlink, delete, start, stop, restart, create, adopt, apply,',
    '  approve, mint or change anything. Never claim that you did.',
    `- If asked to change something, answer in two short parts: what the change would do (using the real names`,
    `  and data you can read), and that it needs the owner's passkey on ${link}. Do not attempt an inline`,
    '  approval and do not ask for a passkey in the chat.',
    '- Write plain text only. No markdown tables, no headings, no code fences unless quoting a real value.',
    '- Keep answers short and concrete: a sentence or a few bullets at most.',
    approval ? `- The approval link is ${approval}` : '- No approval URL is configured; say so instead of linking.',
  ].join('\n');
}

function toolClient(clientFor, hosts, name, onCall) {
  return {
    name,
    description: {
      arca_status: 'Read the Arca node status (version, mode, uptime, Incus reachability, applied sequence).',
      watch_report: 'Read the live instances grouped by Incus project.',
      postgres_list: 'List the PostgreSQL databases Arca knows about.',
      node_metrics: 'Read the latest CPU and memory metrics for the host.',
      alerts_list: 'List firing, unknown and recently resolved alerts.',
      plan_list: 'List pending Arca plans waiting for approval.',
      journal_list: 'Read the Arca activity journal, oldest first. Optional after cursor and limit (max 500).',
      service_list: 'List the managed services with name, state, ipv4, ports, template, links and published.',
    }[name],
    parameters: {
      type: 'object',
      properties: {
        host: {
          type: 'string',
          description: `Host to read. One of: ${hosts.map((host) => host.name).join(', ')}. Defaults to ${hosts[0]?.name ?? 'the first host'}.`,
          ...(hosts.length ? { enum: hosts.map((host) => host.name) } : {}),
        },
        ...(name === 'journal_list' ? {
          after: { type: 'integer', minimum: 0, description: 'Only records with an id greater than this cursor.' },
          limit: { type: 'integer', minimum: 1, maximum: 500, description: 'Page size, at most 500.' },
        } : {}),
      },
      additionalProperties: false,
    },
    output: {
      schema: {},
      render(args, value) {
        return [{ type: 'text', text: bound(JSON.stringify(value), DEFAULTS.resultChars) }];
      },
    },
    timeoutMs: 10_000,
    isConcurrencySafe() {
      return true;
    },
    async execute(args = {}, exec = {}) {
      const requested = text(args?.host).trim();
      const host = hosts.find((candidate) => candidate.name === requested) ?? hosts[0];
      if (!host) throw new Error('No Arca host is configured.');
      const client = clientFor(host.name);
      if (!client) throw new Error('No Arca reader is available for that host.');
      await onCall?.({ name, host: host.name, input: toolInput(name, args), callId: exec.callId });
      return client.call(name, toolInput(name, args));
    },
  };
}

function toolInput(name, args) {
  if (name !== 'journal_list') return {};
  const input = {};
  const after = Number(args?.after);
  const limit = Number(args?.limit);
  if (Number.isInteger(after) && after >= 0) input.after = after;
  if (Number.isInteger(limit) && limit > 0) input.limit = Math.min(500, limit);
  return input;
}

// Tool definitions are plain objects in the DSH registry contract (name, description, parameters, output,
// execute). They are registered in the Doctor's agent scope, so no other session can see them.
export function createReadTools({ hosts = [], clientFor, onCall } = {}) {
  return READ_TOOLS.map((name) => toolClient(clientFor, hosts, name, onCall));
}

class TurnCap extends Error {
  constructor(reason) {
    super(`Doctor turn cap reached: ${reason}`);
    this.reason = reason;
    this.name = 'TurnCap';
  }
}

function sseError(message) {
  return { message: bound(message, 200) };
}

export function createDoctorChat(options = {}) {
  const {
    hosts = [],
    approvalUrl = '',
    resolveDsh = () => null,
    clientFor = () => null,
    store,
    statePath = stateFilePath(),
    log = () => {},
    now = Date.now,
    uuid = randomUUID,
    heartbeatMs = DEFAULTS.heartbeatMs,
    turnMs = DEFAULTS.turnMs,
    toolCallsPerTurn = DEFAULTS.toolCallsPerTurn,
    outputTokensPerTurn = DEFAULTS.outputTokensPerTurn,
    dailyTokenBudget = DEFAULTS.dailyTokenBudget,
  } = options;
  const chatStore = store ?? new ChatStore(statePath, { now });
  const limits = {
    toolCallsPerTurn: integer(toolCallsPerTurn, DEFAULTS.toolCallsPerTurn, 1, 10_000),
    outputTokensPerTurn: integer(outputTokensPerTurn, DEFAULTS.outputTokensPerTurn, 1, 10_000_000),
    dailyTokenBudget: integer(dailyTokenBudget, DEFAULTS.dailyTokenBudget, 1, 1_000_000_000),
  };
  let agentHandle;
  let agent;
  let busy = false;
  let turn;
  let closed = false;

  const primaryApproval = safeApprovalUrl(approvalUrl);

  async function ensureAgent(dsh) {
    if (agent) return agent;
    const state = await chatStore.read();
    const sessionId = state.sessionId ?? uuid();
    const setup = (agentCtx) => {
      if (!agentCtx?.tools) throw new Error('The DSH tool registry is unavailable.');
      for (const definition of tools) agentCtx.tools.register(definition);
      // Hide every deployment tool from the Doctor. Scoped registrations above stay visible; the reserved
      // `run_code` transport name cannot be named in a restriction, so it is skipped. A registry that cannot
      // apply this fails the agent setup: the chat must not run with a shell or file tool available.
      if (typeof agentCtx.tools.restrict === 'function') {
        const names = (agentCtx.tools.definitions?.(undefined) ?? [])
          .map((definition) => definition.name)
          .filter((name) => name !== 'run_code' && !READ_TOOLS.includes(name));
        if (names.length) agentCtx.tools.restrict({ deny: names });
      }
      if (agentCtx.systemPrompt?.section) {
        agentCtx.systemPrompt.section({
          name: 'deployment:persona-prefix',
          order: 0,
          text: 'You are the Arca Doctor, a read-only operations assistant in the Arca canvas.',
        });
        agentCtx.systemPrompt.section({
          name: 'arca-doctor',
          order: 100,
          text: doctorPrompt({ hosts, approvalUrl: primaryApproval, now: now() }),
        });
      }
    };
    const agentOptions = {
      provider: MODEL.provider,
      model: MODEL.model,
      maxTokens: limits.outputTokensPerTurn,
    };
    let created = false;
    let createId = sessionId;
    if (state.sessionId) {
      try {
        agentHandle = await dsh.agents.resume({ resumeSessionId: state.sessionId, agentOptions, setup });
      } catch (error) {
        // A corrupt or missing stored session must not wedge chat: start a fresh Doctor session instead.
        createId = uuid();
        log(`Arca canvas chat: stored session could not resume (${safeErrorCode(error)}); starting a new one`);
      }
    }
    if (!agentHandle) {
      agentHandle = await dsh.agents.create({ sessionId: createId, agentOptions, setup });
      created = true;
    }
    agent = agentHandle.agent;
    if (dsh.sessionTitle?.rename) {
      try {
        const current = dsh.sessionTitle.get?.(agent.session)?.title;
        if (current !== SESSION_NAME) dsh.sessionTitle.rename(agent.session, SESSION_NAME);
      } catch (error) {
        log(`Arca canvas chat: session title not set (${safeErrorCode(error)})`);
      }
    }
    await chatStore.patch((next) => ({ ...next, sessionId: agent.session.id }));
    if (created) log('Arca canvas chat: Doctor session created');
    return agent;
  }

  function finaliseTool(callId, state, summary) {
    if (!turn) return;
    const card = turn.tools.find((tool) => tool.callId === callId);
    if (!card) return;
    card.state = state;
    if (summary) card.summary = summary;
    if (!sse.closed) sse.send('tool', toolPayload(card));
  }

  function toolPayload(card) {
    return {
      callId: card.callId,
      name: card.name,
      state: card.state,
      ...(card.input && Object.keys(card.input).length ? { input: card.input } : {}),
      ...(card.summary ? { summary: card.summary } : {}),
      ...(card.host ? { host: card.host } : {}),
    };
  }

  function cap(reason) {
    if (!turn || turn.reason) return;
    turn.reason = reason;
    const limit = reason === 'tool-calls' ? limits.toolCallsPerTurn : limits.outputTokensPerTurn;
    if (!sse.closed) sse.send('cap', { reason, limit, message: capMessage(reason, limit) });
    try {
      agent?.cancel?.(reason);
    } catch { /* already quiescent */ }
    settle();
  }

  function settle() {
    if (!turn) return;
    turn.resolve?.();
  }

  let sse;

  function onToolCall({ name, host, input, callId }) {
    if (!turn) throw new Error('No active Doctor turn.');
    if (turn.toolCalls >= limits.toolCallsPerTurn) {
      cap('tool-calls');
      throw new TurnCap('tool-calls');
    }
    turn.toolCalls++;
    const card = { callId, name, host, state: 'running', ...(input && Object.keys(input).length ? { input } : {}) };
    turn.tools.push(card);
    if (!sse.closed) sse.send('tool', toolPayload(card));
  }

  const tools = createReadTools({ hosts, clientFor, onCall: onToolCall });

  function usageTotal(usage) {
    if (!usage) return 0;
    const total = Number(usage.totalTokens);
    if (Number.isFinite(total) && total > 0) return total;
    return finite(usage.inputTokens) + finite(usage.outputTokens);
  }

  // Usage is counted synchronously, before any persistence await, so a cap reached on the final event of a turn
  // still stops the turn instead of racing the settle.
  function noteUsage(usage) {
    if (!turn) return;
    const output = Math.max(0, finite(usage?.outputTokens));
    const total = usageTotal(usage);
    turn.outputTokens += output;
    turn.dailyTokens += total;
    turn.persist = turn.persist.then(() => chatStore.patch((state) => ({
      ...state,
      dailyTokens: state.dailyTokens + total,
    }))).catch((error) => log(`Arca canvas chat: usage counter not written (${safeErrorCode(error)})`));
    if (!sse.closed) {
      sse.send('usage', {
        turnToolCalls: turn.toolCalls,
        turnOutputTokens: turn.outputTokens,
        dailyTokens: turn.dailyTokens,
        dailyBudget: limits.dailyTokenBudget,
        day: dayOf(now()),
      });
    }
    if (turn.outputTokens > limits.outputTokensPerTurn) {
      cap('output-tokens');
      return;
    }
    if (turn.dailyTokens >= limits.dailyTokenBudget) {
      if (turn.reason) return;
      turn.reason = 'budget';
      if (!sse.closed) {
        sse.send('budget', {
          scope: 'daily',
          used: turn.dailyTokens,
          budget: limits.dailyTokenBudget,
          message: 'Daily budget reached, try tomorrow.',
        });
      }
      try {
        agent?.cancel?.('budget');
      } catch { /* already quiescent */ }
      settle();
    }
  }

  async function runTurn(message) {
    const initial = await chatStore.read();
    turn = {
      toolCalls: 0, outputTokens: 0, dailyTokens: initial.dailyTokens, tools: [],
      reason: null, resolve: null, text: '', persist: Promise.resolve(),
    };
    const done = new Promise((resolve) => { turn.resolve = resolve; });
    const started = safeApprovalUrl(primaryApproval) && writeIntent(message) ? primaryApproval : '';
    if (started) {
      sse.send('approval', {
        url: started,
        title: 'Approval needed',
        detail: 'This chat is read-only. Open the Arca approval page to review the change and approve it with your passkey.',
      });
    }
    await chatStore.patch((state) => ({
      ...state,
      messages: [...state.messages, { role: 'user', text: message, at: new Date(now()).toISOString() }]
        .slice(-DEFAULTS.historyMessages),
    }));
    const offStream = agent.ctx.on('agent/assistant-stream', (payload) => {
      const frame = payload?.frame;
      if (!frame || frame.type !== 'chunk' || frame.chunk?.type !== 'text-delta') return;
      turn.text += frame.chunk.text;
      if (!sse.closed) sse.send('text', { text: frame.chunk.text });
    });
    const offEvents = agent.ctx.on('session/event', (session, event) => {
      if (!event || session?.id !== agent.session.id) return;
      if (event.type === 'assistant/message' && event.data?.usage) {
        noteUsage(event.data.usage);
      } else if (event.type === 'tool/result') {
        const callId = event.data?.message?.toolCallId;
        if (!callId) return;
        const failed = event.data?.message?.isError === true;
        const summary = bound(messageText(event.data?.message?.content), 240);
        finaliseTool(callId, failed ? 'error' : 'done', summary);
      } else if (event.type === 'turn/end') {
        settle();
      }
    });
    const offError = agent.ctx.on('agent/error', () => {
      if (!turn || turn.reason) return;
      turn.reason = 'error';
      if (!sse.closed) sse.send('error', sseError('The Doctor could not reach the model.'));
      settle();
    });
    const timeout = setTimeout(() => {
      if (!turn || turn.reason) return;
      turn.reason = 'timeout';
      try {
        agent.cancel?.('timeout');
      } catch { /* already quiescent */ }
      if (!sse.closed) sse.send('error', sseError('The Doctor took too long and the answer was stopped.'));
      settle();
    }, turnMs);
    timeout.unref?.();
    try {
      agent.followup({
        id: uuid(),
        role: 'user',
        content: [{ type: 'text', text: message }],
        source: { kind: 'user' },
      });
      await done;
    } finally {
      clearTimeout(timeout);
      offStream?.();
      offEvents?.();
      offError?.();
    }
    if (!turn.reason) turn.reason = 'complete';
    await Promise.allSettled([turn.persist]);
    await chatStore.flush().catch(() => {});
    if (turn.text || turn.tools.length) {
      await chatStore.patch((state) => ({
        ...state,
        messages: [...state.messages, {
          role: 'assistant',
          text: turn.text,
          at: new Date(now()).toISOString(),
          tools: turn.tools.map(({ callId, name, state, host, summary }) => ({
            callId, name, state,
            ...(host ? { host } : {}),
            ...(summary ? { summary: bound(summary, 240) } : {}),
          })),
        }].slice(-DEFAULTS.historyMessages),
      })).catch((error) => log(`Arca canvas chat: transcript not written (${safeErrorCode(error)})`));
    }
    if (!sse.closed) {
      sse.send('done', {
        reason: turn.reason,
        turnToolCalls: turn.toolCalls,
        turnOutputTokens: turn.outputTokens,
      });
    }
    sse.end();
    turn = undefined;
  }

  async function handle(request, response) {
    if (closed) return writeJson(response, 503, { error: 'chat-unavailable' });
    if (request.method !== 'POST') {
      response.setHeader('Allow', 'POST');
      return writeJson(response, 405, { error: 'method-not-allowed' });
    }
    const type = text(request.headers['content-type']).split(';')[0].trim().toLowerCase();
    if (type !== 'application/json') return writeJson(response, 415, { error: 'json-required' });
    let body;
    try {
      body = await readBody(request, DEFAULTS.bodyBytes);
    } catch (error) {
      return writeJson(response, error.status ?? 400, { error: error.status === 413 ? 'body-too-large' : 'body-unreadable' });
    }
    let payload;
    try {
      payload = JSON.parse(body);
    } catch {
      return writeJson(response, 400, { error: 'invalid-json' });
    }
    const message = typeof payload?.message === 'string' ? payload.message.trim() : '';
    if (!message) return writeJson(response, 400, { error: 'message-required' });
    if (message.length > DEFAULTS.messageChars) return writeJson(response, 413, { error: 'message-too-long' });
    if (busy) return writeJson(response, 429, { error: 'chat-busy' });
    const dsh = resolveDsh();
    if (!dsh?.agents) return writeJson(response, 503, { error: 'chat-unavailable' });

    // Reserve the single turn slot before the first await, so two simultaneous requests cannot both start a turn.
    busy = true;
    try {
      const state = await chatStore.read();
      // The first event goes out before any agent or model work so the kernel's idle timer never starts on a
      // connection that has not spoken yet.
      sse = createSse(response, {
        heartbeatMs,
        onClose() {
          if (!turn || turn.reason) return;
          turn.reason = 'closed';
          try {
            agent?.cancel?.('client-disconnect');
          } catch { /* already quiescent */ }
          settle();
        },
      });
      sse.send('start', { session: SESSION_NAME });
      if (state.dailyTokens >= limits.dailyTokenBudget) {
        sse.send('budget', {
          scope: 'daily',
          used: state.dailyTokens,
          budget: limits.dailyTokenBudget,
          message: 'Daily budget reached, try tomorrow.',
        });
        sse.send('done', { reason: 'budget', turnToolCalls: 0, turnOutputTokens: 0 });
        sse.end();
        return;
      }
      await ensureAgent(dsh);
      await runTurn(message);
    } catch (error) {
      log(`Arca canvas chat: turn failed (${safeErrorCode(error)})`);
      if (!response.headersSent) {
        return writeJson(response, 503, { error: 'chat-unavailable' });
      }
      if (!sse?.closed) {
        sse.send('error', sseError('The Doctor could not answer.'));
        sse.send('done', { reason: 'error', turnToolCalls: turn?.toolCalls ?? 0, turnOutputTokens: turn?.outputTokens ?? 0 });
        sse.end();
      }
    } finally {
      busy = false;
    }
  }

  async function history(response) {
    const state = await chatStore.read();
    const session = state.sessionId
      ? { id: state.sessionId, name: SESSION_NAME }
      : (agent?.session?.id ? { id: agent.session.id, name: SESSION_NAME } : null);
    // The kernel forwards at most 2 MiB. Drop the oldest replay messages until this body fits well inside it.
    const messages = state.messages.slice();
    while (messages.length > 1 && JSON.stringify(messages).length > 1_500_000) messages.shift();
    writeJson(response, 200, {
      session,
      messages: messages.slice(-DEFAULTS.historyMessages),
      usage: {
        day: state.day,
        dailyTokens: state.dailyTokens,
        dailyBudget: limits.dailyTokenBudget,
      },
      limits: {
        toolCallsPerTurn: limits.toolCallsPerTurn,
        outputTokensPerTurn: limits.outputTokensPerTurn,
      },
    });
  }

  async function close() {
    closed = true;
    try {
      agent?.cancel?.('disposed');
    } catch { /* already quiescent */ }
    settle();
    try {
      await agentHandle?.dispose?.();
    } catch { /* the runtime is already going away */ }
  }

  return {
    handle,
    history,
    close,
    ensureAgent: async () => ensureAgent(resolveDsh()),
    doctorPrompt: () => doctorPrompt({ hosts, approvalUrl: primaryApproval, now: now() }),
    get sessionId() {
      return agent?.session?.id ?? null;
    },
  };
}

function safeErrorCode(error) {
  const code = error?.code ?? error?.name;
  return typeof code === 'string' && /^[A-Za-z0-9_.-]{1,64}$/.test(code) ? code : 'error';
}

async function readBody(request, max) {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > max) {
      const error = new Error('body too large');
      error.status = 413;
      throw error;
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}

function writeJson(response, status, payload) {
  if (response.headersSent) return;
  const body = JSON.stringify(payload);
  response.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Cache-Control': 'no-store',
    'X-Content-Type-Options': 'nosniff',
    'Content-Length': Buffer.byteLength(body),
  });
  response.end(body);
}
