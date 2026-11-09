import { hostname } from 'node:os';
import { snapshotError } from './arca-client.js';

const JOURNAL_PAGE_LIMIT = 500;
const JOURNAL_MAX_PAGES = 50;
const JOURNAL_KEEP = 200;
// A state change appends a new line under the same effect id. Every poll re-reads the newest ids, so a
// record that went Intent -> Failed (or Applied -> Confirmed/Reverted) is replaced, not skipped.
const JOURNAL_WINDOW = JOURNAL_KEEP;
// Pages a snapshot waits for. A deeper read (the first one, or a long gap) continues in the background
// so the page is not held hostage by a long journal.
const JOURNAL_SYNC_PAGES = 2;

export function emptySnapshot(config = {}) {
  return {
    at: new Date().toISOString(),
    error: null,
    approvalUrl: config.approvalUrl ?? '',
    nodeName: (config.nodeName ?? hostname()).trim() || 'Machine',
    machine: null,
    instances: [],
    instancesKnown: false,
    projects: [],
    databases: [],
    services: [],
    servicesKnown: false,
    metrics: null,
    alerts: [],
    plans: [],
    journal: [],
    journalKnown: false,
    journalPending: false,
    devices: 'off',
    devicesAt: null,
    phones: [],
    desktops: [],
  };
}

function rows(value) {
  if (!Array.isArray(value)) throw new Error('Invalid collection');
  return value;
}

// journal_list pages forward from `after` and returns the oldest records first, one newest record per
// effect id. Read from a trailing window so later states of an id are re-read, then merge by id with the
// latest record winning.
function journalId(record) {
  const id = Number(record?.id);
  return Number.isFinite(id) ? id : null;
}

function mergeJournal(previous, fetched) {
  const merged = [];
  const position = new Map();
  for (const record of [...previous, ...fetched]) {
    if (!record || typeof record !== 'object') continue;
    const id = journalId(record);
    if (id === null) {
      merged.push(record);
      continue;
    }
    const key = String(id);
    const at = position.get(key);
    if (at === undefined) {
      position.set(key, merged.length);
      merged.push(record);
    } else {
      merged[at] = record;
    }
  }
  merged.sort((a, b) => (journalId(a) ?? 0) - (journalId(b) ?? 0));
  return merged.slice(-JOURNAL_KEEP);
}

async function readJournalPage(client, after, signal) {
  const value = await client.call('journal_list', { after, limit: JOURNAL_PAGE_LIMIT }, signal);
  const records = rows(value.records).filter((record) => record && typeof record === 'object');
  let cursor = after;
  for (const record of records) {
    const id = journalId(record);
    if (id !== null && id > cursor) cursor = id;
  }
  // A short page ends the journal; a full page of ids that never moves forward would loop forever.
  return { records, cursor, done: records.length < JOURNAL_PAGE_LIMIT || cursor === after };
}

async function readJournal(client, after, maxPages, signal) {
  const fetched = [];
  let cursor = after;
  let pages = 0;
  let done = false;
  while (pages < maxPages) {
    const page = await readJournalPage(client, cursor, signal);
    pages++;
    fetched.push(...page.records);
    cursor = page.cursor;
    if (page.done) {
      done = true;
      break;
    }
  }
  return { fetched, cursor, done };
}

function highestJournalId(records) {
  let highest = null;
  for (const record of records) {
    const id = journalId(record);
    if (id !== null && (highest === null || id > highest)) highest = id;
  }
  return highest;
}

// Finish a deep read without making the snapshot wait for it. Each page lands in the cache as it arrives.
function catchUpJournal(client, state, after) {
  if (state.task) return;
  state.pending = true;
  state.task = (async () => {
    try {
      let cursor = after;
      for (let page = 0; page < JOURNAL_MAX_PAGES; page++) {
        if (state.signal?.aborted) break;
        const read = await readJournalPage(client, cursor, state.signal);
        if (state.signal?.aborted) break;
        state.records = mergeJournal(state.records, read.records);
        cursor = read.cursor;
        if (read.done) break;
      }
      if (!state.signal?.aborted) state.cursor = Math.max(Number(state.cursor) || 0, cursor);
    } catch {
      // Keep the records already cached; the next poll retries the read.
    } finally {
      state.task = null;
      state.pending = false;
    }
  })();
}

async function collectJournal(client, state) {
  if (state.task) return state.records;
  if (state.signal?.aborted) return state.records;
  const highest = Number(state.cursor) || 0;
  const hasCache = state.records.length > 0;
  const after = hasCache ? Math.max(0, highest - JOURNAL_WINDOW) : 0;
  const read = await readJournal(client, after, JOURNAL_SYNC_PAGES, state.signal);
  const fetchedHighest = highestJournalId(read.fetched);
  // The journal restarted from zero (state wiped): ids went backwards or disappeared. Drop the cache
  // for this host and read the new journal once from the start.
  const reset = hasCache && (read.fetched.length === 0 || (fetchedHighest !== null && fetchedHighest < highest));
  if (reset) {
    state.records = [];
    state.cursor = 0;
    const fresh = await readJournal(client, 0, JOURNAL_SYNC_PAGES, state.signal);
    state.records = mergeJournal([], fresh.fetched);
    state.cursor = fresh.cursor;
    if (!fresh.done) catchUpJournal(client, state, fresh.cursor);
    return state.records;
  }
  state.records = mergeJournal(state.records, read.fetched);
  state.cursor = Math.max(highest, read.cursor);
  if (!read.done) catchUpJournal(client, state, read.cursor);
  return state.records;
}

export async function collectSnapshot(client, config = {}, state = {}) {
  const snapshot = emptySnapshot(config);
  const journalState = state.journal ??= { cursor: 0, records: [], pending: false, task: null };
  // The cache's stop signal reaches a background catch-up, not only the foreground snapshot read.
  journalState.signal = state.signal;
  const read = async (key, tool, run) => {
    try {
      snapshot[key] = await run();
      return null;
    } catch (error) {
      return `${tool}: ${snapshotError(error)}`;
    }
  };
  const errors = await Promise.all([
    read('machine', 'arca_status', () => client.call('arca_status')),
    read('instances', 'watch_report', async () => {
      const projects = rows((await client.call('watch_report')).projects);
      const instances = projects.flatMap((project) =>
        rows(project.instances).map((instance) => ({ ...instance, project: project.project })));
      snapshot.projects = projects.map((project) => String(project.project)).filter(Boolean);
      snapshot.instancesKnown = true;
      return instances;
    }),
    read('databases', 'postgres_list', async () => rows((await client.call('postgres_list')).databases)),
    read('metrics', 'node_metrics', () => client.call('node_metrics')),
    read('alerts', 'alerts_list', () => client.call('alerts_list')),
    read('plans', 'plan_list', async () => rows((await client.call('plan_list')).plans)),
    read('services', 'service_list', async () => {
      const services = rows((await client.call('service_list')).services);
      snapshot.servicesKnown = true;
      return services;
    }),
    read('journal', 'journal_list', async () => {
      const records = await collectJournal(client, journalState);
      snapshot.journalKnown = true;
      snapshot.journalPending = journalState.pending === true;
      return records;
    }),
  ]);
  snapshot.error = errors.filter(Boolean).join(' ') || null;
  return snapshot;
}

const PHONE_STATUS_LIMIT = 2;
const PHONE_FAILURE_BACKOFF_MS = 60_000;

// The caller owns a single deadline for the entire device phase. One phase reads at most
// two phone statuses, rotating through the list, and a failed phone waits 60s before a retry.
export async function collectDevices(client, signal, options = {}) {
  if (!client) return { devices: 'off', phones: [], desktops: [] };
  const now = options.now ?? Date.now;
  const lastFailureAt = options.lastFailureAt ?? new Map();
  const previous = Array.isArray(options.previous) ? options.previous : [];
  const hasGood = Boolean(options.hasGood);
  try {
    const phones = rows(await client.call('droid_list', {}, signal));
    const desktops = rows(await client.call('desktop_list', {}, signal));
    await client.call('droidd_status', {}, signal);
    const listedAt = now();
    const prior = new Map(previous.map((entry) => [entry.name, entry]));
    const entries = phones.map((phone) => {
      const old = prior.get(phone.name) ?? {};
      return {
        name: phone.name, state: old.state ?? phone.state ?? 'unknown',
        network: old.network ?? null, battery: old.battery ?? null, size: old.size ?? null,
      };
    });
    const start = phones.length ? (options.cursor ?? 0) % phones.length : 0;
    const selected = [];
    for (let offset = 0; offset < phones.length && selected.length < PHONE_STATUS_LIMIT; offset++) {
      const index = (start + offset) % phones.length;
      const phone = phones[index];
      const failedAt = lastFailureAt.get(phone.name);
      if (failedAt !== undefined && listedAt - failedAt < PHONE_FAILURE_BACKOFF_MS) continue;
      selected.push({ phone, index });
    }
    options.cursor = selected.length
      ? (selected[selected.length - 1].index + 1) % phones.length
      : start;
    let unreachable = false;
    let rateLimited = false;
    let setting;
    for (const { phone, index } of selected) {
      const entry = entries[index];
      try {
        const status = await client.call('phone_status', { droid: phone.name }, signal);
        lastFailureAt.delete(phone.name);
        entry.state = phone.state ?? 'unknown';
        entry.network = status?.network?.type ?? null;
        entry.battery = status?.battery?.level ?? null;
        entry.size = status?.size ?? null;
      } catch (error) {
        if (error.status === 429) { rateLimited = true; setting = error.setting; break; }
        lastFailureAt.set(phone.name, now());
        entry.state = 'unreachable';
        unreachable = true;
        setting ??= error.setting;
        if (error.aborted || signal?.aborted) break;
      }
    }
    for (let index = 0; index < phones.length; index++) {
      if (selected.some((candidate) => candidate.index === index)) continue;
      const failedAt = lastFailureAt.get(phones[index].name);
      if (failedAt !== undefined && now() - failedAt < PHONE_FAILURE_BACKOFF_MS) {
        entries[index].state = 'unreachable';
        unreachable = true;
      } else entries[index].state = phones[index].state ?? 'unknown';
    }
    if (rateLimited) {
      return { devices: hasGood ? 'rate-limited' : 'unavailable', phones: [], desktops: [], setting, lists: false };
    }
    const desktopEntries = desktops.map(({ name, status, memoryBytes }) => ({ name, status, memoryBytes }));
    return { devices: unreachable ? 'unreachable' : 'ok', phones: entries, desktops: desktopEntries, setting, lists: true, at: listedAt };
  } catch (error) {
    return {
      devices: !hasGood ? 'unavailable' : error.status === 429 ? 'rate-limited' : 'unreachable',
      phones: [], desktops: [], setting: error.setting, lists: false,
    };
  }
}

export function createSnapshotCache(client, pollMs, config = {}, devices = null) {
  const now = config.now ?? (() => Date.now());
  const deviceMeta = { lastFailureAt: new Map(), cursor: 0, now };
  const collectState = {};
  let deviceState = { devices: devices ? 'unavailable' : 'off', devicesAt: null, phones: [], desktops: [] };
  let cached = { ...emptySnapshot(config), ...deviceState, error: 'Waiting for the first Arca snapshot.' };
  let timer;
  let stopped = false;
  let phase;
  let lastState;
  const stopController = new AbortController();
  collectState.signal = stopController.signal;
  const log = config.log ?? ((message) => console.info(message));
  const refreshDevices = () => {
    if (!devices || phase) return;
    const controller = new AbortController();
    phase = controller;
    const deadline = setTimeout(() => controller.abort(), config.devicesTimeoutMs ?? 5000);
    deadline.unref();
    Object.assign(deviceMeta, { previous: deviceState.phones, hasGood: Boolean(deviceState.devicesAt) });
    void collectDevices(devices, controller.signal, deviceMeta).then(({ setting, ...next }) => {
      if (stopped) return;
      if (next.devices === 'rate-limited') {
        // A 429 keeps the last complete list and its timestamp; never invent an empty stale list.
        if (deviceState.devicesAt) deviceState = { ...deviceState, devices: 'stale' };
        else deviceState = { devices: 'unavailable', devicesAt: null, phones: [], desktops: [] };
      } else if (next.devices === 'unreachable' && !next.lists) {
        if (deviceState.devicesAt) deviceState = { ...deviceState, devices: 'unreachable' };
        else deviceState = { devices: 'unavailable', devicesAt: null, phones: [], desktops: [] };
      } else if (next.devices === 'unavailable') {
        deviceState = { devices: 'unavailable', devicesAt: null, phones: [], desktops: [] };
      } else {
        deviceState = { devices: next.devices, phones: next.phones, desktops: next.desktops, devicesAt: new Date(now()).toISOString() };
      }
      cached = { ...cached, ...deviceState };
      if (lastState !== deviceState.devices) {
        lastState = deviceState.devices;
        // Only known setting names may enter a host log; never upstream details.
        const rejected = ['devicesUrl', 'devicesTokenPath'].includes(setting) ? ` (${setting})` : '';
        log(`Arca canvas devices: ${deviceState.devices}${rejected}`);
      }
    }).finally(() => {
      clearTimeout(deadline);
      controller.abort();
      phase = null;
    });
  };
  const refresh = async () => {
    refreshDevices();
    const snapshot = await collectSnapshot(client, config, collectState);
    if (!stopped) {
      cached = { ...snapshot, ...deviceState };
      timer = setTimeout(refresh, pollMs);
      timer.unref();
    }
  };
  void refresh();
  return {
    get: () => cached,
    stop() {
      stopped = true;
      clearTimeout(timer);
      stopController.abort();
      phase?.abort();
    },
  };
}
