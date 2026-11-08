import { hostname } from 'node:os';
import { snapshotError } from './arca-client.js';

const JOURNAL_PAGE_LIMIT = 500;
const JOURNAL_MAX_PAGES = 50;
const JOURNAL_KEEP = 200;

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
    metrics: null,
    alerts: [],
    plans: [],
    journal: [],
    journalKnown: false,
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

// journal_list pages forward from `after` and returns the oldest records first. Read every page,
// retain the newest records, and remember the cursor so later polls only ask for new ones.
async function collectJournal(client, state) {
  const fetched = [];
  let cursor = state.cursor;
  for (let page = 0; page < JOURNAL_MAX_PAGES; page++) {
    const value = await client.call('journal_list', { after: cursor, limit: JOURNAL_PAGE_LIMIT });
    const records = rows(value.records).filter((record) => record && typeof record === 'object');
    fetched.push(...records);
    let advanced = false;
    for (const record of records) {
      const id = Number(record.id);
      if (Number.isFinite(id) && id > cursor) {
        cursor = id;
        advanced = true;
      }
    }
    if (!records.length || !advanced) break;
  }
  const seen = new Set(state.records.map((record) => record.id));
  const merged = [...state.records];
  for (const record of fetched) {
    if (record.id !== undefined) {
      if (seen.has(record.id)) continue;
      seen.add(record.id);
    }
    merged.push(record);
  }
  merged.sort((a, b) => (Number(a.id) || 0) - (Number(b.id) || 0));
  state.records = merged.slice(-JOURNAL_KEEP);
  state.cursor = cursor;
  return state.records;
}

export async function collectSnapshot(client, config = {}, state = {}) {
  const snapshot = emptySnapshot(config);
  const journalState = state.journal ??= { cursor: 0, records: [] };
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
    read('journal', 'journal_list', async () => {
      const records = await collectJournal(client, journalState);
      snapshot.journalKnown = true;
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
      phase?.abort();
    },
  };
}
