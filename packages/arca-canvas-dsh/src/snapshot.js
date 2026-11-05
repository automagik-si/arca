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

export function createSnapshotCache(client, pollMs, config = {}) {
  let cached = { ...emptySnapshot(config), error: 'Waiting for the first Arca snapshot.' };
  let timer;
  let stopped = false;
  const refresh = async () => {
    cached = await collectSnapshot(client, config);
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
