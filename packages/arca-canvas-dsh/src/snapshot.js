import { hostname } from 'node:os';
import { snapshotError } from './arca-client.js';

export function emptySnapshot(config = {}) {
  return {
    at: new Date().toISOString(),
    error: null,
    approvalUrl: config.approvalUrl ?? '',
    nodeName: (config.nodeName ?? hostname()).trim() || 'Machine',
    machine: null,
    instances: [],
    databases: [],
    metrics: null,
    alerts: [],
    plans: [],
    devices: 'off',
    phones: [],
    desktops: [],
  };
}

function rows(value) {
  if (!Array.isArray(value)) throw new Error('Invalid collection');
  return value;
}

export async function collectSnapshot(client, config = {}) {
  const snapshot = emptySnapshot(config);
  const reads = [
    ['machine', 'arca_status', (value) => value],
    ['instances', 'watch_report', (value) => rows(value.projects).flatMap((project) =>
      rows(project.instances).map((instance) => ({ ...instance, project: project.project })))],
    ['databases', 'postgres_list', (value) => rows(value.databases)],
    ['metrics', 'node_metrics', (value) => value],
    ['alerts', 'alerts_list', (value) => value],
    ['plans', 'plan_list', (value) => rows(value.plans)],
  ];
  const errors = await Promise.all(reads.map(async ([key, tool, convert]) => {
    try {
      snapshot[key] = convert(await client.call(tool));
      return null;
    } catch (error) {
      return `${tool}: ${snapshotError(error)}`;
    }
  }));
  snapshot.error = errors.filter(Boolean).join(' ') || null;
  return snapshot;
}

// The caller owns a single deadline for the entire device phase.
export async function collectDevices(client, signal) {
  if (!client) return { devices: 'off', phones: [], desktops: [] };
  try {
    const phones = rows(await client.call('droid_list', {}, signal));
    const desktops = rows(await client.call('desktop_list', {}, signal));
    await client.call('droidd_status', {}, signal);
    let unavailable = false;
    let setting;
    const entries = [];
    for (const phone of phones) {
      let status = null;
      let state = phone.state;
      if (state === 'ready') {
        try { status = await client.call('phone_status', { droid: phone.name }, signal); }
        catch (error) { unavailable = true; state = 'unknown'; setting ??= error.setting; }
      }
      entries.push({ name: phone.name, state,
        network: status?.network?.type ?? null,
        battery: status?.battery?.level ?? null, size: status?.size ?? null });
    }
    const desktopEntries = desktops.map(({ name, status, memoryBytes }) => ({ name, status, memoryBytes }));
    return { devices: unavailable ? 'unavailable' : 'ok', phones: entries, desktops: desktopEntries, setting };
  } catch (error) {
    return { devices: 'unavailable', phones: [], desktops: [], setting: error.setting };
  }
}

export function createSnapshotCache(client, pollMs, config = {}, devices = null) {
  let deviceState = { devices: devices ? 'unavailable' : 'off', phones: [], desktops: [] };
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
    void collectDevices(devices, controller.signal).then(({ setting, ...next }) => {
      if (stopped) return;
      deviceState = next;
      cached = { ...cached, ...deviceState };
      if (lastState !== next.devices) {
        lastState = next.devices;
        // Only known setting names may enter a host log; never upstream details.
        const rejected = ['devicesUrl', 'devicesTokenPath'].includes(setting) ? ` (${setting})` : '';
        log(`Arca canvas devices: ${next.devices}${rejected}`);
      }
    }).finally(() => {
      clearTimeout(deadline);
      controller.abort();
      phase = null;
    });
  };
  const refresh = async () => {
    refreshDevices();
    const snapshot = await collectSnapshot(client, config);
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
