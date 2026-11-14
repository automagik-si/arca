// What a job's minutes cost: the runner its labels name, and that runner's list price per minute.
//
// The prices are list prices in US dollars per minute, as published in 2026. They are an estimate to rank findings by,
// not an invoice: check them against the provider's own page before quoting one. A job is billed by the whole minute,
// rounded up. Standard GitHub-hosted runners are free for public repositories; larger runners and third-party
// runners are billed whatever the repository's visibility. A self-hosted runner's minutes cost what its owner pays
// for the machine, which the analysis does not know: they are counted and priced at 0.

export const PRICES_AS_OF = '2026-11';

/** GitHub-hosted runners: x64 Linux by cores, arm64 Linux, Windows, macOS. */
const GITHUB = {
  linux: { 1: 0.002, 2: 0.006, 4: 0.012, 8: 0.022, 16: 0.042, 32: 0.082, 64: 0.162 },
  arm: { 2: 0.005, 4: 0.008, 8: 0.014, 16: 0.026, 32: 0.05, 64: 0.098 },
  windows: { 2: 0.01, 4: 0.022, 8: 0.042, 16: 0.082 },
  macos: { 3: 0.062, 4: 0.062, 12: 0.102 },
};
/** Blacksmith (a third-party runner provider): Linux x64 by vCPU. */
const BLACKSMITH = { 2: 0.004, 4: 0.008, 8: 0.016, 16: 0.032, 32: 0.064 };

/**
 * The runner a job ran on, from its labels: `{ runner, kind, perMinute, freeForPublic }`. `kind` is `github`,
 * `blacksmith`, `self-hosted` or `unknown`. `ownPool` is the label of the owner's own runner pool (`--move-to`): a
 * job carrying it ran there, whatever else its labels say, so it is self-hosted and priced at 0.
 */
export function runnerOf(labels, ownPool = null) {
  const all = (labels ?? []).map((label) => String(label).toLowerCase());
  if (all.length === 0) return { runner: 'none (never ran)', kind: 'none', perMinute: 0, freeForPublic: true };
  if (ownPool && all.includes(ownPool.toLowerCase())) {
    return { runner: `self-hosted (${ownPool})`, kind: 'self-hosted', perMinute: 0, freeForPublic: false };
  }
  if (all.includes('self-hosted')) {
    return { runner: `self-hosted (${all.filter((l) => l !== 'self-hosted').join(', ')})`, kind: 'self-hosted', perMinute: 0, freeForPublic: false };
  }
  for (const label of all) {
    const m = /^blacksmith-(\d+)vcpu/.exec(label);
    if (m) {
      const cores = Number(m[1]);
      return { runner: label, kind: 'blacksmith', perMinute: BLACKSMITH[cores] ?? (cores / 2) * BLACKSMITH[2], freeForPublic: false };
    }
  }
  for (const label of all) {
    const cores = Number(/(\d+)[-_]?(?:cores?|vcpus?)\b/.exec(label)?.[1] ?? 0);
    const arm = /arm/.test(label);
    if (/^(ubuntu|linux)/.test(label)) {
      const table = arm ? GITHUB.arm : GITHUB.linux;
      const n = cores || 2;
      return { runner: label, kind: 'github', perMinute: table[n] ?? null, freeForPublic: cores === 0 };
    }
    if (/^windows/.test(label)) {
      const n = cores || 2;
      return { runner: label, kind: 'github', perMinute: GITHUB.windows[n] ?? null, freeForPublic: cores === 0 };
    }
    if (/^macos/.test(label)) {
      const n = /xlarge/.test(label) ? 12 : /large/.test(label) ? 12 : 3;
      return { runner: label, kind: 'github', perMinute: GITHUB.macos[n], freeForPublic: !/large/.test(label) };
    }
  }
  return { runner: all.join(', ') || 'unknown', kind: 'unknown', perMinute: null, freeForPublic: false };
}

/** A job's billed minutes: its run time, rounded up to the whole minute; 0 for a job that never started. */
export function billedMinutes(job) {
  if (!job.startedAt || !job.completedAt) return 0;
  const ms = Date.parse(job.completedAt) - Date.parse(job.startedAt);
  return ms > 0 ? Math.ceil(ms / 60000) : 0;
}

/**
 * What a job costs: `list` at list price whatever the repository, and `billed` with the free minutes of public
 * repositories taken off.
 */
export function jobCost(job, visibility, ownPool = null) {
  const runner = runnerOf(job.labels, ownPool);
  const minutes = billedMinutes(job);
  const list = runner.perMinute == null ? 0 : minutes * runner.perMinute;
  const billed = visibility === 'public' && runner.freeForPublic ? 0 : list;
  return { ...runner, minutes, list, billed };
}
