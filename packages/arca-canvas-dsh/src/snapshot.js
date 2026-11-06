import { snapshotError } from './arca-client.js';

export function emptySnapshot(config = {}) {
  return {
    at: new Date().toISOString(),
    error: null,
    approvalUrl: config.approvalUrl ?? '',
    machine: null,
    instances: [],
    databases: [],
    metrics: null,
    alerts: [],
    plans: [],
    phones: [],
    desktops: [],
  };
}

function rows(value) {
  if (!Array.isArray(value)) throw new Error('Invalid collection');
  return value;
}

export async function collectSnapshot(client, config = {}, droidd = null) {
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
  if (droidd) {
    try {
      const phones = rows(await droidd.call('droid_list'));
      const desktops = rows(await droidd.call('desktop_list'));
      await droidd.call('droidd_status');
      // Only ready phones can answer a status read. Any failed read drops this
      // optional source as a whole, without changing the Arca error field.
      const entries = [];
      for (const phone of phones) {
        const status = phone.state === 'ready'
          ? await droidd.call('phone_status', { droid: phone.name }) : null;
        entries.push({ name: phone.name, state: phone.state,
          network: status?.network?.type ?? null,
          battery: status?.battery?.level ?? null,
          size: status?.size ?? null });
      }
      const desktopEntries = desktops.map(({ name, status, memoryBytes }) => ({ name, status, memoryBytes }));
      snapshot.phones = entries;
      snapshot.desktops = desktopEntries;
    } catch {
      // Device data is optional. Never publish upstream errors or credentials.
    }
  }
  snapshot.error = errors.filter(Boolean).join(' ') || null;
  return snapshot;
}

export function createSnapshotCache(client, pollMs, config = {}, droidd = null) {
  let cached = { ...emptySnapshot(config), error: 'Waiting for the first Arca snapshot.' };
  let timer;
  let stopped = false;
  const refresh = async () => {
    cached = await collectSnapshot(client, config, droidd);
    if (!stopped) {
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
    },
  };
}
