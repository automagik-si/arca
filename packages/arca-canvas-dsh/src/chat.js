import { randomUUID } from 'node:crypto';
import { mkdir, open, readFile as readStateFile, rename, rm } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { homedir } from 'node:os';
import { cpuCoresKnown, proposalId } from './model.js';
import { snapshotError } from './arca-client.js';

// The Doctor chat cannot change anything by itself: only these verbs are registered with DSH. Every write verb
// (postgres_link/unlink/delete, service_*, plan/approve/apply, token_*, adopt, ...) is absent from this list and
// cannot be reached from a model request. `arca_propose` is the one non-read tool: it records an inert proposal
// for the owner to approve with the passkey, and changes nothing.
export const READ_TOOLS = Object.freeze([
  'arca_status', 'watch_report', 'postgres_list', 'node_metrics',
  'alerts_list', 'plan_list', 'journal_list', 'service_list',
]);
export const PROPOSE_TOOL = 'arca_propose';
export const DOCTOR_TOOLS = Object.freeze([...READ_TOOLS, PROPOSE_TOOL]);
export const PROPOSE_ACTIONS = Object.freeze([
  'service_create', 'service_remove', 'service_publish', 'service_unpublish',
  'service_connect', 'service_disconnect', 'postgres_create',
]);

export const DEFAULTS = Object.freeze({
  setupMs: 60_000,
  cancelMs: 5_000,
  idleMs: 5_000,
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

function capMessage(reason, limit) {
  return reason === 'tool-calls'
    ? `This answer stopped at the ${limit}-tool-call limit for one turn.`
    : `This answer stopped at the ${limit.toLocaleString('en-US')}-output-token limit for one turn.`;
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
    budgetExhausted: false,
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
            name: DOCTOR_TOOLS.includes(tool.name) ? tool.name : 'tool',
            state: ['running', 'done', 'error'].includes(tool.state) ? tool.state : 'done',
            ...(typeof tool.host === 'string' && tool.host ? { host: text(tool.host) } : {}),
            ...(typeof tool.summary === 'string' && tool.summary ? { summary: bound(tool.summary, 240) } : {}),
            ...(proposalId(tool.proposal) ? { proposal: { id: proposalId(tool.proposal) } } : {}),
          })),
        } : {}),
      }))
    : [];
  return {
    version: 1,
    sessionId: typeof raw.sessionId === 'string' && raw.sessionId ? raw.sessionId : null,
    day: typeof raw.day === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(raw.day) ? raw.day : fallback.day,
    dailyTokens: Math.max(0, finite(raw.dailyTokens)),
    // A quarantined read stays exhausted for the rest of its UTC day, even if a later read succeeds.
    budgetExhausted: raw.budgetExhausted === true,
    messages,
  };
}

// One JSON document under the canvas DSH home. Writes are serialized and atomic, so a crash leaves the previous
// document (never a half-written counter) and a read never sees a partial update.
export class ChatStore {
  constructor(path, { now = Date.now, readFile = readStateFile } = {}) {
    this.path = path;
    this.now = typeof now === 'function' ? now : () => now;
    this.readFile = readFile;
    this.queue = Promise.resolve();
    this.exhaustedDay = null;
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

  /** Mark the rest of the UTC day exhausted after a state read/write failure that could lose token counts. */
  exhaust() {
    this.exhaustedDay = dayOf(this.now());
  }

  #roll(state) {
    const day = dayOf(this.now());
    const rolled = state.day === day ? state : { ...state, day, dailyTokens: 0, budgetExhausted: false };
    // A quarantine or an uncounted usage write stays exhausted for the day, even if a later read succeeds.
    return this.exhaustedDay === day ? { ...rolled, budgetExhausted: true } : rolled;
  }

  async #load() {
    let raw;
    try {
      raw = await this.readFile(this.path, 'utf8');
    } catch (error) {
      if (error?.code === 'ENOENT') return this.#missing();
      // A read error is not corruption: the next read may succeed. Retry once before failing the read, and
      // never rename or overwrite a file that was merely busy for a moment.
      try {
        raw = await this.readFile(this.path, 'utf8');
      } catch (retry) {
        if (retry?.code === 'ENOENT') return this.#missing();
        throw retry;
      }
    }
    try {
      return normalizeState(JSON.parse(raw), this.now());
    } catch {
      return this.#quarantine();
    }
  }

  #missing() {
    // A quarantine whose marker could not be written still stays exhausted for this process and day.
    if (this.exhaustedDay === dayOf(this.now())) {
      return { ...emptyState(this.now()), budgetExhausted: true };
    }
    return emptyState(this.now());
  }

  // A corrupt counter must never reset to zero (that would hand out a fresh budget). Keep the bad file for
  // inspection and persist an exhausted marker for today, so every later read and turn still refuses.
  async #quarantine() {
    this.exhaustedDay = dayOf(this.now());
    const state = { ...emptyState(this.now()), budgetExhausted: true };
    const stamp = new Date(this.now()).toISOString().replace(/[:.]/g, '-');
    try {
      await rename(this.path, `${this.path}.corrupt-${stamp}-${randomUUID().slice(0, 8)}`);
    } catch { /* keep the exhausted state even when the file cannot be moved */ }
    try {
      await this.#write(state);
    } catch { /* a read-only store still fails closed in memory */ }
    return state;
  }

  async #write(state) {
    await mkdir(dirname(this.path), { recursive: true });
    const temporary = `${this.path}.${process.pid}.${randomUUID()}.tmp`;
    try {
      const handle = await open(temporary, 'w', 0o600);
      try {
        await handle.writeFile(`${JSON.stringify(state)}\n`);
        await handle.sync();
      } finally {
        await handle.close();
      }
      await rename(temporary, this.path);
    } catch (error) {
      await rm(temporary, { force: true }).catch(() => {});
      throw error;
    }
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

export function doctorPrompt({ hosts = [], approvalUrl = '', proposalHost = '', now = Date.now() } = {}) {
  const names = hosts.map((host) => text(host?.name)).filter(Boolean);
  const hostLine = names.length ? names.join(', ') : 'the configured host';
  const proposeHost = text(proposalHost).trim() || names[0] || 'the configured host';
  const approval = safeApprovalUrl(approvalUrl);
  const approvalLine = approval
    ? `- The approval page is at ${approval}. The canvas card uses the same passkey.`
    : '- No approval page is configured on this canvas; say so instead of proposing a change.';
  return [
    'You are the Arca Doctor, a friendly helper in the Arca canvas.',
    'You talk to the owner in plain language, not to an engineer.',
    `You can read live Arca data from these hosts: ${hostLine}.`,
    `Today is ${new Date(now).toISOString().slice(0, 10)}.`,
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
    `- You can propose changes only on ${proposeHost} from here for now. Another host is refused because its`,
    '  approval page is not this one.',
    '- Do not propose a change the owner did not ask for, and never propose a database delete.',
    approvalLine,
  ].join('\n');
}

const NOT_MEASURABLE = 'not measurable from here';
const COULD_NOT_READ = 'could not be read just now';
const NO_READING = 'no reading yet';
const REMOTE_HOST_REASON = 'not available for a remote host';
const KILOBYTE = 1_000;
const MEGABYTE = 1_000_000;
const GIGABYTE = 1_000_000_000;

// The kernel explains a missing sample with one of two reasons (crates/arca-metrics/src/sample.rs): a remote
// host can never give the reading, or a read failed. A local reading with no reason simply is not there yet.
function readingStatus(reason) {
  const value = typeof reason === 'string' ? reason.trim() : '';
  if (!value) return NO_READING;
  if (value === REMOTE_HOST_REASON) return NOT_MEASURABLE;
  return COULD_NOT_READ;
}

const RESOLVED_REASONS = Object.freeze({
  cleared: 'the problem cleared',
  'rule-disabled': 'the alert was switched off',
  'subject-gone': 'the thing it watched is gone',
  'no data': 'no data to check',
});

const UNKNOWN_REASONS = Object.freeze({
  missing: 'no reading yet',
  stale: 'reading is old',
  unavailable: NOT_MEASURABLE,
});

function measured(value) {
  if (value === null || value === undefined || value === '') return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

export function formatGigabytes(bytes) {
  const value = measured(bytes);
  return value === null || value < 0 ? '' : `${(value / GIGABYTE).toFixed(1)} GB`;
}

function formatRate(bytes) {
  const value = measured(bytes);
  if (value === null || value < 0) return '';
  // Pick the unit from the rounded value: 999,960 B/s rounds to 1000.0 KB/s, so it is shown as 1.0 MB/s.
  if (value / MEGABYTE >= 999.95) return `${(value / GIGABYTE).toFixed(1)} GB/s`;
  if (value / KILOBYTE >= 999.95) return `${(value / MEGABYTE).toFixed(1)} MB/s`;
  return `${(value / KILOBYTE).toFixed(1)} KB/s`;
}

function formatPercent(value) {
  const number = measured(value);
  return number === null ? '' : `${Math.round(number)}%`;
}

function coreLabel(cores) {
  return `${cores} ${cores === 1 ? 'core' : 'cores'}`;
}

function cpuLine(cpu) {
  if (!cpu) return `CPU: ${NO_READING}.`;
  const cores = measured(cpu.cores);
  // The same shared rule as the machine card and the fleet total: a reason only explains a missing load.
  if (!cpuCoresKnown(cpu)) return `CPU: core count ${readingStatus(cpu.reason)}.`;
  const percent = formatPercent(cpu.usedPct);
  return percent
    ? `CPU: ${percent} used across ${coreLabel(cores)}.`
    : `CPU: ${coreLabel(cores)}; load ${readingStatus(cpu.reason)}.`;
}

function memoryLine(memory) {
  if (!memory) return `Memory: ${NO_READING}.`;
  if (memory.reason) return `Memory: ${readingStatus(memory.reason)}.`;
  const total = measured(memory.totalBytes);
  const available = measured(memory.availableBytes);
  const reportedUsed = measured(memory.usedBytes);
  const used = reportedUsed ?? (total !== null && available !== null ? Math.max(0, total - available) : null);
  const totalText = formatGigabytes(total);
  if (!totalText) return `Memory: ${NO_READING}.`;
  const usedText = formatGigabytes(used);
  const availableText = formatGigabytes(available);
  if (usedText && availableText) return `Memory: ${usedText} used of ${totalText}, ${availableText} available.`;
  if (usedText) return `Memory: ${usedText} used of ${totalText}.`;
  if (availableText) return `Memory: ${availableText} available of ${totalText}.`;
  return `Memory: ${totalText} total.`;
}

function loadLine(load) {
  if (!load) return `Load: ${NO_READING}.`;
  if (load.reason) return `Load: ${readingStatus(load.reason)}.`;
  const values = [
    ['one', '1 minute'], ['five', '5 minutes'], ['fifteen', '15 minutes'],
  ].flatMap(([key, label]) => {
    const number = measured(load[key]);
    return number === null ? [] : [`${number.toFixed(1)} (${label})`];
  });
  return values.length ? `Load: ${values.join(', ')}.` : `Load: ${NO_READING}.`;
}

function pressureLines(psi) {
  if (!psi) return [];
  const lines = [];
  const memory = [];
  const memorySome = formatPercent(psi.memorySomeAvg10);
  const memoryFull = formatPercent(psi.memoryFullAvg10);
  if (memorySome) memory.push(`${memorySome} some`);
  if (memoryFull) memory.push(`${memoryFull} full`);
  if (memory.length) lines.push(`Memory pressure: ${memory.join(', ')}.`);
  const cpuSome = formatPercent(psi.cpuSomeAvg10);
  if (cpuSome) lines.push(`CPU pressure: ${cpuSome}.`);
  return lines;
}

function poolLines(pools) {
  if (!Array.isArray(pools)) return [];
  return pools.map((pool) => {
    const name = text(pool?.name).trim();
    const parts = [];
    const used = formatGigabytes(pool?.usedBytes);
    const total = formatGigabytes(pool?.totalBytes);
    const free = formatGigabytes(pool?.freeBytes);
    if (used) parts.push(`${used} used`);
    if (total) parts.push(`${total} total`);
    if (free) parts.push(`${free} free`);
    return `${name ? `Disk pool ${name}` : 'Disk pool'}: ${parts.length ? `${parts.join(', ')}.` : `${NO_READING}.`}`;
  });
}

function networkLine(network) {
  if (!network) return '';
  const received = formatRate(network.rxBytesPerSec);
  const sent = formatRate(network.txBytesPerSec);
  const parts = [];
  if (received) parts.push(`${received} received`);
  if (sent) parts.push(`${sent} sent`);
  return parts.length ? `Network: ${parts.join(', ')}.` : `Network: ${NO_READING}.`;
}

function projectLine(projects) {
  if (!Array.isArray(projects) || !projects.length) return '';
  const rows = projects.map((project) => {
    const name = text(project?.project).trim() || 'unnamed';
    const total = measured(project?.total);
    const running = measured(project?.running);
    const counts = [];
    if (total !== null) counts.push(`${total} total`);
    if (running !== null) counts.push(`${running} running`);
    return `${name}${counts.length ? ` (${counts.join(', ')})` : ''}`;
  });
  return `Projects: ${rows.join('; ')}.`;
}

function sampleLines(sample) {
  const lines = [
    cpuLine(sample.cpu),
    memoryLine(sample.memory),
    loadLine(sample.load),
  ];
  const temperature = measured(sample.cpuTempC);
  if (temperature !== null) lines.push(`CPU temperature: ${temperature.toFixed(1)} C.`);
  const gpu = formatPercent(sample.gpuBusyPct);
  if (gpu) lines.push(`GPU: ${gpu} busy.`);
  lines.push(...poolLines(sample.pools));
  const network = networkLine(sample.network);
  if (network) lines.push(network);
  lines.push(...pressureLines(sample.psi));
  const projects = projectLine(sample.projects);
  if (projects) lines.push(projects);
  return lines;
}

function formatNodeMetrics(value) {
  const data = value && typeof value === 'object' ? value : {};
  const host = text(data.host).trim() || 'the host';
  const minutes = measured(data.minutes);
  const lines = [`Host ${host}.`];
  if (data.latest) {
    lines.push('Latest reading:');
    lines.push(...sampleLines(data.latest));
  } else {
    lines.push(`Latest reading: ${NO_READING}.`);
  }
  if (data.mean) {
    const span = minutes !== null
      ? `Average over the last ${minutes} minute${minutes === 1 ? '' : 's'}:`
      : 'Average:';
    lines.push(span);
    lines.push(...sampleLines(data.mean));
  }
  return lines.join('\n');
}

// The model must never see a raw byte count or an internal host reason it could repeat to the owner. Metrics
// get a short plain-language summary with GB sizes and whole percentages; every other result keeps its shape
// but has byte, rate and percentage fields replaced by the same display strings. Alerts get their reason
// vocabulary translated in place so a resolved alert cannot be confused with a different resolution.
export function formatToolResult(name, value) {
  if (name === 'node_metrics') return formatNodeMetrics(value);
  if (name === 'alerts_list') return JSON.stringify(presentAlertValues(value));
  return JSON.stringify(presentToolValues(value));
}

function presentAlertValues(value) {
  const presented = presentToolValues(value);
  if (!presented || typeof presented !== 'object' || Array.isArray(presented)) return presented;
  const translate = (rows, labels) => {
    if (!Array.isArray(rows)) return;
    for (const row of rows) {
      if (row && typeof row === 'object' && typeof row.reason === 'string') {
        row.reason = labels[row.reason] ?? row.reason;
      }
    }
  };
  translate(presented.resolvedRecent, RESOLVED_REASONS);
  translate(presented.unknown, UNKNOWN_REASONS);
  return presented;
}

function presentToolValues(value) {
  if (Array.isArray(value)) return value.map(presentToolValues);
  if (!value || typeof value !== 'object') return value;
  const out = {};
  for (const [key, item] of Object.entries(value)) {
    if (typeof item === 'number' && /BytesPerSec$/.test(key)) out[key] = formatRate(item);
    else if (typeof item === 'number' && /Bytes$/.test(key)) out[key] = formatGigabytes(item);
    else if (typeof item === 'number' && /Pct$/.test(key)) out[key] = formatPercent(item);
    else out[key] = presentToolValues(item);
  }
  return out;
}

function toolClient(clientFor, hosts, name, onCall) {
  return {
    name,
    description: {
      arca_status: 'Check whether this machine is up, its version and its last applied update.',
      watch_report: 'List the instances on this host and their state, grouped by project.',
      postgres_list: 'List the databases Arca knows about.',
      node_metrics: 'Read how busy the CPU is and how much memory and disk the host is using.',
      alerts_list: 'List current warnings and recently resolved ones.',
      plan_list: 'List changes waiting for the owner\'s approval.',
      journal_list: 'Read recent Arca activity, oldest first. Optional after cursor and limit (max 500).',
      service_list: 'List the managed services with name, state, address, ports and links.',
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
        return [{ type: 'text', text: bound(formatToolResult(name, value), DEFAULTS.resultChars) }];
      },
    },
    timeoutMs: 10_000,
    isConcurrencySafe() {
      return true;
    },
    async execute(args = {}, exec = {}) {
      const requested = text(args?.host).trim();
      if (requested && !hosts.some((candidate) => candidate.name === requested)) {
        throw new Error(`Unknown host "${requested}". Valid hosts: ${hosts.map((candidate) => candidate.name).join(', ') || '(none)'}.`);
      }
      const host = hosts.find((candidate) => candidate.name === requested) ?? hosts[0];
      if (!host) throw new Error('No Arca host is configured.');
      const client = clientFor(host.name);
      if (!client) throw new Error(`No Arca reader is available for host "${host.name}".`);
      const input = toolInput(name, args);
      await onCall?.({ name, host: host.name, input, callId: exec.callId });
      return withHost(await client.call(name, input, exec.signal), host.name);
    },
  };
}

// Every read result names its host, so a model answer cannot attribute one machine's numbers to another.
function withHost(value, host) {
  if (value && typeof value === 'object' && !Array.isArray(value)) return { ...value, host };
  return { host, data: value };
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
// execute). They are registered globally with a `when` condition for this Doctor session only, then the
// Doctor's scope is restricted to their allow-list and a dispatch guard refuses everything else.
export function createReadTools({ hosts = [], clientFor, onCall } = {}) {
  return READ_TOOLS.map((name) => toolClient(clientFor, hosts, name, onCall));
}

const PROPOSE_ARGUMENTS = 'service_create {name, template}; service_remove {name}; '
  + 'service_publish {name, reach: "tailnet"}; service_unpublish {name}; '
  + 'service_connect {from, to}; service_disconnect {from, to}; postgres_create {name}.';

// The one non-read tool: it records an inert proposal for the owner and changes nothing. The kernel checks the
// action name and its arguments again, so this list is an explicit fence rather than the only validation.
export function createProposeTool({ hosts = [], clientFor, onCall, onProposal, proposalHost = '' } = {}) {
  const pageHost = hosts.find((host) => host.name === proposalHost)?.name ?? hosts[0]?.name ?? '';
  return {
    name: PROPOSE_TOOL,
    description: `Ask the owner to approve one change. It records a proposal and changes nothing. Actions: ${PROPOSE_ARGUMENTS} You can propose only on ${pageHost || 'the configured host'} from here for now.`,
    parameters: {
      type: 'object',
      properties: {
        host: {
          type: 'string',
          description: `Host to propose on. One of: ${hosts.map((host) => host.name).join(', ')}. Defaults to ${hosts[0]?.name ?? 'the first host'}. Only ${pageHost || 'the first host'} can be proposed from this page; another host is refused.`,
          ...(hosts.length ? { enum: hosts.map((host) => host.name) } : {}),
        },
        action: {
          type: 'string',
          enum: [...PROPOSE_ACTIONS],
          description: 'The change to ask for.',
        },
        args: {
          type: 'object',
          description: `The action's arguments. ${PROPOSE_ARGUMENTS}`,
        },
      },
      required: ['action', 'args'],
      additionalProperties: false,
    },
    output: {
      schema: {},
      render(args, value) {
        const id = proposalId(value);
        const summary = text(value?.summary).trim() || 'A change';
        return [{
          type: 'text',
          text: bound(`${id ? `${id}: ` : ''}${summary}. It waits for the owner's approval.`, DEFAULTS.resultChars),
        }];
      },
    },
    timeoutMs: 10_000,
    isConcurrencySafe() {
      return false;
    },
    async execute(args = {}, exec = {}) {
      const requested = text(args?.host).trim();
      if (requested && !hosts.some((candidate) => candidate.name === requested)) {
        throw new Error(`Unknown host "${requested}". Valid hosts: ${hosts.map((candidate) => candidate.name).join(', ') || '(none)'}.`);
      }
      const host = hosts.find((candidate) => candidate.name === requested) ?? hosts[0];
      if (!host) throw new Error('No Arca host is configured.');
      if (pageHost && host.name !== pageHost) {
        throw new Error(`I can only propose changes on ${pageHost} from here for now.`);
      }
      const client = clientFor(host.name);
      if (!client) throw new Error(`No Arca reader is available for host "${host.name}".`);
      const action = text(args?.action).trim();
      if (!PROPOSE_ACTIONS.includes(action)) throw new Error('That action cannot be proposed.');
      let payload = args?.args;
      if (!payload || typeof payload !== 'object' || Array.isArray(payload)) {
        throw new Error('The action needs an arguments object.');
      }
      if (action === 'postgres_create') {
        // The kernel owns the database image; a proposal can never choose one.
        const { image: _ignored, ...rest } = payload;
        payload = rest;
      }
      await onCall?.({
        name: PROPOSE_TOOL, host: host.name, input: { action, args: payload }, callId: exec.callId,
      });
      let value;
      try {
        value = await client.call('action_propose', { action, args: payload }, exec.signal);
      } catch (error) {
        throw new Error(snapshotError(error));
      }
      const id = proposalId(value);
      if (!id) throw new Error('Arca did not return a waiting proposal.');
      onProposal?.({ callId: exec.callId, host: host.name, value, id });
      return { ...value, host: host.name };
    },
  };
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
    proposalHost = '',
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
    setupMs = DEFAULTS.setupMs,
    cancelMs = DEFAULTS.cancelMs,
    idleMs = DEFAULTS.idleMs,
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
  let toolDisposers = [];
  let toolsInstalled = false;
  let liveSessionId = null;
  let lastTurn = 0;
  let lastAgentId = null;
  let runSequence = 0;
  let ensureGeneration = 0;
  let cancelCurrent = null;
  // DSH gives one write handle per persisted session. Track every setup that is opening one so a newer setup
  // that races the same session can wait for the holder and retry, instead of reading the conflict as a
  // missing session and forking a fresh id.
  const sessionAttempts = new Map();

  const primaryApproval = safeApprovalUrl(approvalUrl);
  const primaryProposalHost = text(proposalHost).trim() || text(hosts[0]?.name).trim();

  function beginSessionAttempt(sessionId) {
    const entry = { sessionId, promise: null, resolve: null };
    entry.promise = new Promise((resolve) => { entry.resolve = resolve; });
    let entries = sessionAttempts.get(sessionId);
    if (!entries) sessionAttempts.set(sessionId, entries = new Set());
    entries.add(entry);
    return entry;
  }

  function endSessionAttempt(entry) {
    const entries = sessionAttempts.get(entry.sessionId);
    entries?.delete(entry);
    if (entries && !entries.size) sessionAttempts.delete(entry.sessionId);
    entry.resolve();
  }

  function alreadyOwned(error) {
    return error?.name === 'SessionAlreadyOwnedError';
  }

  function discardAgent() {
    const handle = agentHandle;
    agentHandle = undefined;
    agent = undefined;
    liveSessionId = null;
    try {
      void handle?.dispose?.();
    } catch { /* the runtime is already going away */ }
  }

  // The allow-list is the primary fence: every global tool, including one registered after setup, is hidden from
  // the Doctor unless its name is in DOCTOR_TOOLS. The tools are registered globally with a `when` condition for this
  // Doctor session only; the restriction is applied in the agent scope. A registry that cannot do that fails setup.
  function installTools(dsh) {
    if (toolsInstalled) return;
    const registry = dsh?.tools;
    if (!registry || typeof registry.register !== 'function'
      || typeof registry.restrict !== 'function' || typeof registry.definitions !== 'function') {
      throw new Error("The DSH tool registry cannot enforce the Doctor's tool set.");
    }
    const registered = [];
    try {
      for (const definition of tools) {
        registered.push(registry.register(definition, {
          when: (context) => Boolean(context?.agent && context.agent.id === liveSessionId),
        }));
      }
      const known = new Set((registry.definitions(undefined) ?? []).map((definition) => definition.name));
      const missing = DOCTOR_TOOLS.filter((name) => !known.has(name));
      if (missing.length) throw new Error(`The DSH tool registry is missing ${missing.join(', ')}.`);
      toolDisposers = registered;
      toolsInstalled = true;
    } catch (error) {
      for (const dispose of registered) {
        try { dispose(); } catch { /* registration rollback */ }
      }
      throw error;
    }
  }

  // Open one persisted session for write. Another setup may already hold it: wait for that attempt to settle
  // (it either publishes the same live session or disposes its stale handle), then retry the same id. A
  // session-write conflict is never a reason to create a new id.
  async function resumeStored(dsh, sessionId, agentOptions, setup, own) {
    const open = () => dsh.agents.resume({ resumeSessionId: sessionId, agentOptions, setup });
    try {
      return await open();
    } catch (error) {
      if (!alreadyOwned(error)) throw error;
      const peers = [...(sessionAttempts.get(sessionId) ?? [])].filter((entry) => entry !== own);
      if (!peers.length) throw error;
      await Promise.allSettled(peers.map((entry) => entry.promise));
      if (agent?.session?.id === sessionId) return null;
      return open();
    }
  }

  async function ensureAgent(dsh) {
    if (agent) return agent;
    const attempt = ++ensureGeneration;
    const state = await chatStore.read();
    let candidateId = state.sessionId ?? uuid();
    installTools(dsh);
    const setup = (agentCtx) => {
      if (!agentCtx?.tools || typeof agentCtx.tools.restrict !== 'function'
        || typeof agentCtx.tools.guard !== 'function') {
        throw new Error('The DSH tool restriction is unavailable.');
      }
      agentCtx.tools.restrict({ allow: [...DOCTOR_TOOLS] });
      // A late registration is hidden by the allow-list; this dispatch guard is the second fence and also denies
      // the reserved `run_code` transport, which a restriction cannot name.
      agentCtx.on('tools/pre-execute', (exec, next) => {
        if (!exec || !DOCTOR_TOOLS.includes(exec.name)) {
          return Promise.resolve({ kind: 'deny', reason: 'The Doctor may only read or propose: that tool is not available.' });
        }
        return next();
      });
      // The guard runs after an allow decision, so a later policy cannot open the door this check closes.
      // It is required exactly like the restriction: a setup that cannot install both fences fails closed.
      agentCtx.tools.guard((exec) => (DOCTOR_TOOLS.includes(exec?.name)
        ? undefined
        : 'The Doctor may only read or propose: that tool is not available.'));
      if (agentCtx.systemPrompt?.section) {
        agentCtx.systemPrompt.section({
          name: 'deployment:persona-prefix',
          order: 0,
          text: 'You are the Arca Doctor, a friendly helper in the Arca canvas.',
        });
        agentCtx.systemPrompt.section({
          name: 'arca-doctor',
          order: 100,
          text: doctorPrompt({ hosts, approvalUrl: primaryApproval, proposalHost: primaryProposalHost, now: now() }),
        });
      }
    };
    const agentOptions = {
      provider: MODEL.provider,
      model: MODEL.model,
      maxTokens: limits.outputTokensPerTurn,
    };
    const sessionAttempt = state.sessionId ? beginSessionAttempt(state.sessionId) : null;
    try {
      let candidateHandle;
      let created = false;
      if (state.sessionId) {
        try {
          candidateHandle = await resumeStored(dsh, state.sessionId, agentOptions, setup, sessionAttempt);
          if (!candidateHandle) {
            // Another setup published this exact session while we waited. Reuse the live session, never fork it.
            if (attempt !== ensureGeneration) throw new Error('Doctor setup was superseded');
            return agent;
          }
        } catch (error) {
          // A stale attempt must not change the live session or start a replacement.
          if (attempt !== ensureGeneration) throw error;
          if (alreadyOwned(error)) {
            // No overlapping attempt is left to wait for: the previous holder may be a discarded agent whose
            // dispose never finished. Do not leave the Doctor down; log the conflict and recover with a fresh
            // session, as before this batch.
            log(`Arca canvas chat: stored session is still owned (${safeErrorCode(error)}); starting a new one`);
          } else {
            // A current attempt may recover a corrupt or missing stored session by creating a fresh one.
            log(`Arca canvas chat: stored session could not resume (${safeErrorCode(error)}); starting a new one`);
          }
          candidateId = uuid();
        }
      }
      if (!candidateHandle) {
        if (attempt !== ensureGeneration) throw new Error('Doctor setup was superseded');
        candidateHandle = await dsh.agents.create({ sessionId: candidateId, agentOptions, setup });
        created = true;
      }
      if (closed || attempt !== ensureGeneration) {
        // A timed-out or closed setup resolved late. Dispose only its own handle; the live handle is untouched.
        try { await candidateHandle?.dispose?.(); } catch { /* already gone */ }
        throw new Error('Doctor setup was superseded');
      }
      // Publish only after the generation check. No shared state is written by a stale attempt.
      agentHandle = candidateHandle;
      agent = candidateHandle.agent;
      liveSessionId = agent.session.id;
      if (lastAgentId !== agent.session.id) {
        // A fresh session starts its turn numbering over; stale counters from another session must not hide it.
        lastAgentId = agent.session.id;
        lastTurn = 0;
      }
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
    } finally {
      if (sessionAttempt) endSessionAttempt(sessionAttempt);
    }
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
      ...(proposalId(card.proposal) ? { proposal: { id: proposalId(card.proposal) } } : {}),
    };
  }

  function cap(reason) {
    if (!turn || turn.reason) return;
    turn.reason = reason;
    const limit = reason === 'tool-calls' ? limits.toolCallsPerTurn : limits.outputTokensPerTurn;
    if (!sse.closed) sse.send('cap', { reason, limit, message: capMessage(reason, limit) });
    cancelCurrent?.(reason);
  }

  let sse;

  function onToolCall({ name, host, input, callId }) {
    if (!turn) throw new Error('No active Doctor turn.');
    // A cap can land between two steps of the same model response (usage arrives after the text). Any tool
    // call that follows it must not dispatch, or the cap overshoots by one response. This reason is checked
    // before the coarse cancellation state so the refusal names the cap that caused it.
    if (turn.reason) throw new TurnCap(turn.reason);
    if (turn.cancelled) throw new Error('The Doctor turn was stopped.');
    if (turn.toolCalls >= limits.toolCallsPerTurn) {
      cap('tool-calls');
      throw new TurnCap('tool-calls');
    }
    turn.toolCalls++;
    const card = { callId, name, host, state: 'running', ...(input && Object.keys(input).length ? { input } : {}) };
    turn.tools.push(card);
    if (!sse.closed) sse.send('tool', toolPayload(card));
  }

  function onProposal({ callId, id }) {
    if (!turn) return;
    const card = turn.tools.find((tool) => tool.callId === callId);
    if (!card) return;
    card.proposal = { id };
    if (!sse.closed) sse.send('tool', toolPayload(card));
  }

  const tools = [
    ...createReadTools({ hosts, clientFor, onCall: onToolCall }),
    createProposeTool({ hosts, clientFor, onCall: onToolCall, onProposal, proposalHost: primaryProposalHost }),
  ];

  function usageTotal(usage) {
    if (!usage) return 0;
    const total = Number(usage.totalTokens);
    if (Number.isFinite(total) && total > 0) return total;
    return finite(usage.inputTokens) + finite(usage.outputTokens);
  }

  function noteStoreFailure(kind, error) {
    // The tokens were counted in memory but could not reach the state file. Keep the rest of the day safe
    // rather than silently handing out a fresh budget if the file becomes readable again.
    chatStore.exhaust();
    log(`Arca canvas chat: ${kind} not written (${safeErrorCode(error)}); daily budget exhausted for the day`);
  }

  // Usage rides the durable `assistant/message` event (normal and interrupted); see dsh-agent-loop/lib/index.js:891-903
  // and :949-953. It is counted synchronously, before persistence, so a cap on the final event still stops the turn.
  function noteUsage(usage) {
    if (!turn) return;
    const output = Math.max(0, finite(usage?.outputTokens));
    const total = usageTotal(usage);
    turn.outputTokens += output;
    turn.dailyTokens += total;
    turn.persist = turn.persist.then(() => chatStore.patch((state) => ({
      ...state,
      dailyTokens: state.dailyTokens + total,
    }))).catch((error) => noteStoreFailure('usage counter', error));
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
      cancelCurrent?.('budget');
    }
  }

  async function runTurn(message, deadlineAt) {
    const initial = await chatStore.read();
    const run = {
      id: ++runSequence,
      turn: null,
      reason: null,
      cancelled: false,
      toolCalls: 0,
      outputTokens: 0,
      dailyTokens: initial.dailyTokens,
      tools: [],
      text: '',
      persist: Promise.resolve(),
      resolve: null,
    };
    turn = run;
    const doctorAgent = agent;
    const done = new Promise((resolve) => { run.resolve = resolve; });
    await chatStore.patch((state) => ({
      ...state,
      messages: [...state.messages, { role: 'user', text: message, at: new Date(now()).toISOString() }]
        .slice(-DEFAULTS.historyMessages),
    }));

    let cancelTimer;
    let turnTimer;
    let finished = false;

    const settle = () => {
      if (finished) return;
      finished = true;
      run.resolve?.();
    };

    // Every event that belongs to a turn carries its number. An event at or below the last completed turn is stale
    // and is dropped, so a late event from a cancelled run can never be charged to the next one.
    const matchesTurn = (number) => {
      if (!Number.isInteger(number)) return false;
      if (run.turn === null) {
        if (number <= lastTurn) return false;
        run.turn = number;
      }
      return number === run.turn;
    };

    const cancelRun = (reason) => {
      if (finished) return;
      if (!run.reason) run.reason = reason;
      if (run.cancelled) return;
      run.cancelled = true;
      try {
        doctorAgent?.cancel?.(reason);
      } catch { /* already quiescent */ }
      // The loop appends turn/end in its finally. If that never arrives, drop the agent so a stale run cannot
      // accept another turn, then release the response.
      cancelTimer = setTimeout(() => {
        discardAgent();
        settle();
      }, cancelMs);
      cancelTimer.unref?.();
    };
    cancelCurrent = cancelRun;

    const offStream = doctorAgent.ctx.on('agent/assistant-stream', (payload) => {
      const frame = payload?.frame;
      if (!frame || frame.type !== 'chunk' || frame.chunk?.type !== 'text-delta') return;
      if (Number.isInteger(frame.turn) && !matchesTurn(frame.turn)) return;
      run.text += frame.chunk.text;
      if (!sse.closed) sse.send('text', { text: frame.chunk.text });
    });
    const offEvents = doctorAgent.ctx.on('session/event', (session, event) => {
      if (!event || session?.id !== doctorAgent.session.id) return;
      if (event.type === 'turn/start') {
        if (run.turn === null && Number.isInteger(event.data?.turn) && event.data.turn > lastTurn) {
          run.turn = event.data.turn;
        }
        return;
      }
      if (event.type === 'assistant/message') {
        if (!matchesTurn(event.data?.turn)) return;
        if (event.data?.usage) noteUsage(event.data.usage);
        return;
      }
      if (event.type === 'tool/result') {
        if (!matchesTurn(event.data?.turn)) return;
        const callId = event.data?.message?.toolCallId;
        if (!callId) return;
        const failed = event.data?.message?.isError === true;
        finaliseTool(callId, failed ? 'error' : 'done', bound(messageText(event.data?.message?.content), 240));
        return;
      }
      if (event.type === 'turn/end') {
        if (!Number.isInteger(event.data?.turn)) return;
        if (!matchesTurn(event.data.turn)) return;
        lastTurn = Math.max(lastTurn, event.data.turn);
        if (cancelTimer) clearTimeout(cancelTimer);
        if (turnTimer) clearTimeout(turnTimer);
        settle();
      }
    });
    const offError = doctorAgent.ctx.on('agent/error', (payload) => {
      const number = payload?.turn;
      if (Number.isInteger(number) && run.turn !== null && number !== run.turn) return;
      if (!turn || turn.reason) return;
      turn.reason = 'error';
      if (!sse.closed) sse.send('error', sseError('The Doctor could not reach the model.'));
      // DSH can emit agent/error without a turn/end (for example a failed turn/start append). Cancel and let
      // turn/end or the bounded cancel fallback release the slot; never wait on a boundary that may not come.
      cancelRun('error');
    });
    turnTimer = setTimeout(() => {
      if (!turn) return;
      if (!turn.reason) {
        turn.reason = 'timeout';
        if (!sse.closed) sse.send('error', sseError('The Doctor took too long and the answer was stopped.'));
      }
      // Always cancel at the deadline, even after an error reason. The cancel fallback resolves done if the
      // loop never appends its turn/end boundary.
      cancelRun('timeout');
    }, Math.max(1, deadlineAt - now()));
    turnTimer.unref?.();

    try {
      if (sse.closed) {
        run.reason = 'closed';
        settle();
      } else {
        doctorAgent.followup({
          id: uuid(),
          role: 'user',
          content: [{ type: 'text', text: message }],
          source: { kind: 'user' },
        });
        await done;
      }
    } finally {
      if (turnTimer) clearTimeout(turnTimer);
      if (cancelTimer) clearTimeout(cancelTimer);
      offStream?.();
      offEvents?.();
      offError?.();
      cancelCurrent = null;
    }
    if (!run.reason) run.reason = 'complete';
    await Promise.allSettled([run.persist]);
    await chatStore.flush().catch(() => {});
    if (run.text || run.tools.length) {
      await chatStore.patch((state) => ({
        ...state,
        messages: [...state.messages, {
          role: 'assistant',
          text: run.text,
          at: new Date(now()).toISOString(),
          tools: run.tools.map(({ callId, name, state, host, summary, proposal }) => ({
            callId, name, state,
            ...(host ? { host } : {}),
            ...(summary ? { summary: bound(summary, 240) } : {}),
            ...(proposalId(proposal) ? { proposal: { id: proposalId(proposal) } } : {}),
          })),
        }].slice(-DEFAULTS.historyMessages),
      })).catch((error) => noteStoreFailure('transcript', error));
    }
    // A new turn may not start until the previous run is actually idle. If the loop cannot prove that quickly,
    // drop the agent; the next request resumes the persisted session instead of overlapping this run.
    let idle = false;
    try {
      idle = await within(agent?.whenIdle?.() ?? Promise.resolve(), idleMs);
    } catch {
      idle = false;
    }
    if (!idle) discardAgent();
    if (!sse.closed) {
      sse.send('done', {
        reason: run.reason,
        turnToolCalls: run.toolCalls,
        turnOutputTokens: run.outputTokens,
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
    // Setup and the model run share one deadline, so the whole response stays inside the kernel's ten minutes.
    const deadlineAt = now() + turnMs;
    busy = true;
    try {
      const state = await chatStore.read();
      sse = createSse(response, {
        heartbeatMs,
        onClose() {
          cancelCurrent?.('closed');
        },
      });
      sse.send('start', {
        session: SESSION_NAME,
        ...(state.sessionId ? { sessionId: state.sessionId } : {}),
      });
      if (state.budgetExhausted || state.dailyTokens >= limits.dailyTokenBudget) {
        sse.send('budget', {
          scope: 'daily',
          used: state.budgetExhausted ? limits.dailyTokenBudget : state.dailyTokens,
          budget: limits.dailyTokenBudget,
          message: 'Daily budget reached, try tomorrow.',
        });
        sse.send('done', { reason: 'budget', turnToolCalls: 0, turnOutputTokens: 0 });
        sse.end();
        return;
      }
      const setup = ensureAgent(dsh);
      const ready = await within(setup, Math.min(setupMs, Math.max(1, deadlineAt - now())));
      if (!ready) {
        // Invalidate the pending attempt: when it resolves it disposes itself instead of publishing an agent.
        ensureGeneration += 1;
        discardAgent();
        throw new Error('Doctor setup timed out');
      }
      if (sse.closed) return;
      await runTurn(message, deadlineAt);
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
        budgetExhausted: state.budgetExhausted,
      },
      limits: {
        toolCallsPerTurn: limits.toolCallsPerTurn,
        outputTokensPerTurn: limits.outputTokensPerTurn,
      },
    });
  }

  async function close() {
    closed = true;
    ensureGeneration += 1;
    cancelCurrent?.('disposed');
    for (const dispose of toolDisposers) {
      try { dispose(); } catch { /* registration already gone */ }
    }
    toolDisposers = [];
    try {
      agent?.cancel?.('disposed');
    } catch { /* already quiescent */ }
    settleTurn();
    try {
      await agentHandle?.dispose?.();
    } catch { /* the runtime is already going away */ }
  }

  function settleTurn() {
    const current = turn;
    turn = undefined;
    current?.resolve?.();
  }

  return {
    handle,
    history,
    close,
    ensureAgent: async () => ensureAgent(resolveDsh()),
    doctorPrompt: () => doctorPrompt({ hosts, approvalUrl: primaryApproval, proposalHost: primaryProposalHost, now: now() }),
    get sessionId() {
      return agent?.session?.id ?? null;
    },
  };
}

function within(promise, ms) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => resolve(false), Math.max(1, ms));
    timer.unref?.();
    Promise.resolve(promise).then(
      () => { clearTimeout(timer); resolve(true); },
      (error) => { clearTimeout(timer); reject(error); },
    );
  });
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
