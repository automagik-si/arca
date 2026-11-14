// Reads one repository's GitHub Actions history for a window into a dataset: plain JSON that the analysis reads and
// that a test can hold as a fixture. Only GETs (see github.js). Logs are read for a bounded sample of jobs and kept
// only as what was parsed from them, never as text.

import { parseLog } from './logs.js';

const DAY = 24 * 60 * 60 * 1000;

/** The defaults: how far back, and how many logs a night may read. */
export const DEFAULTS = { days: 7, flakeLogs: 25, failureLogs: 15, sampleLogs: 60, samplesPerJob: 3 };

export async function collect(gh, repo, options = {}) {
  const { days, flakeLogs, failureLogs, sampleLogs, samplesPerJob } = {
    ...DEFAULTS,
    ...Object.fromEntries(Object.entries(options).filter(([, value]) => value !== undefined)),
  };
  const until = options.until ? new Date(options.until) : new Date();
  const since = new Date(until.getTime() - days * DAY);
  const log = options.log ?? (() => {});

  const info = await gh.get(repo, `/repos/${repo}`);
  if (!info) throw new Error(`${repo} cannot be read with this access`);

  // The runs list answers at most 1000 runs per query, so it is read one day at a time.
  const runs = [];
  for (let start = since.getTime(); start < until.getTime(); start += DAY) {
    const end = Math.min(start + DAY, until.getTime());
    const range = `${iso(start)}..${iso(end - 1000)}`;
    const page = await gh.all(repo, `/repos/${repo}/actions/runs?created=${encodeURIComponent(range)}`, 'workflow_runs');
    for (const run of page) if (run.status === 'completed') runs.push(slimRun(run));
  }
  log(`${repo}: ${runs.length} completed runs since ${iso(since.getTime())}`);

  const jobs = [];
  await Promise.all(
    runs.map(async (run) => {
      const list = await gh.all(repo, `/repos/${repo}/actions/runs/${run.id}/jobs?filter=all`, 'jobs');
      for (const job of list) jobs.push(slimJob(job, run));
    }),
  );
  log(`${repo}: ${jobs.length} jobs`);

  const workflows = {};
  const paths = [...new Set(runs.map((run) => run.path).filter((path) => path?.startsWith('.github/workflows/')))];
  await Promise.all(
    paths.map(async (path) => {
      const file = await gh.get(repo, `/repos/${repo}/contents/${path}?ref=${encodeURIComponent(info.default_branch)}`);
      if (file?.content) workflows[path] = Buffer.from(file.content, 'base64').toString('utf8');
    }),
  );

  const usage = await gh.get(repo, `/repos/${repo}/actions/cache/usage`);
  const caches = await gh.all(repo, `/repos/${repo}/actions/caches?sort=size_in_bytes&direction=desc`, 'actions_caches', {
    max: 100,
  });

  const logs = {};
  for (const [jobId, why] of pickLogs(runs, jobs, { flakeLogs, failureLogs, sampleLogs, samplesPerJob })) {
    const text = await gh.get(repo, `/repos/${repo}/actions/jobs/${jobId}/logs`, { raw: true });
    if (text) logs[jobId] = { why, ...parseLog(text) };
  }
  log(`${repo}: ${Object.keys(logs).length} logs read; ${gh.requests} requests`);

  return {
    repo,
    defaultBranch: info.default_branch,
    visibility: info.private ? 'private' : 'public',
    generatedAt: new Date().toISOString(),
    window: { since: iso(since.getTime()), until: iso(until.getTime()), days },
    runs,
    jobs,
    workflows,
    caches: {
      activeBytes: usage?.active_caches_size_in_bytes ?? null,
      activeCount: usage?.active_caches_count ?? null,
      largest: caches.map((cache) => ({
        key: cache.key,
        ref: cache.ref,
        bytes: cache.size_in_bytes,
        createdAt: cache.created_at,
        lastAccessedAt: cache.last_accessed_at,
      })),
    },
    logs,
  };
}

function iso(ms) {
  return new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

function slimRun(run) {
  return {
    id: run.id,
    workflowId: run.workflow_id,
    workflow: run.name,
    path: run.path?.split('@')[0] ?? null,
    event: run.event,
    headSha: run.head_sha,
    headBranch: run.head_branch,
    attempt: run.run_attempt,
    conclusion: run.conclusion,
    createdAt: run.created_at,
    startedAt: run.run_started_at,
    updatedAt: run.updated_at,
    prs: (run.pull_requests ?? []).map((pr) => pr.number),
  };
}

function slimJob(job, run) {
  return {
    id: job.id,
    runId: run.id,
    attempt: job.run_attempt,
    name: job.name,
    workflow: run.workflow,
    path: run.path,
    headSha: run.headSha,
    conclusion: job.conclusion,
    createdAt: job.created_at,
    startedAt: job.started_at,
    completedAt: job.completed_at,
    labels: job.labels ?? [],
    runnerName: job.runner_name ?? null,
    runnerGroup: job.runner_group_name ?? null,
    steps: (job.steps ?? []).map((step) => ({
      name: step.name,
      number: step.number,
      conclusion: step.conclusion,
      startedAt: step.started_at,
      completedAt: step.completed_at,
    })),
  };
}

/**
 * Which jobs' logs to read, and why: first the failed attempts of jobs that later passed on the same commit (their
 * failing tests are the flaky ones), then the newest other failures (so a slow test can be said never to fail), then the newest few successful jobs of each job name, the costliest first (for
 * cache hits and test times).
 */
export function pickLogs(runs, jobs, { flakeLogs, failureLogs, sampleLogs, samplesPerJob }) {
  const picked = new Map();
  const passedOn = new Set();
  for (const job of jobs) if (job.conclusion === 'success') passedOn.add(`${job.path}\0${job.headSha}\0${job.name}`);
  const failed = jobs
    .filter((job) => job.conclusion === 'failure' && passedOn.has(`${job.path}\0${job.headSha}\0${job.name}`))
    .sort((a, b) => (b.completedAt ?? '').localeCompare(a.completedAt ?? ''));
  for (const job of failed.slice(0, flakeLogs)) picked.set(job.id, 'flake');
  const otherFailures = jobs
    .filter((job) => job.conclusion === 'failure' && !picked.has(job.id))
    .sort((a, b) => (b.completedAt ?? '').localeCompare(a.completedAt ?? ''));
  for (const job of otherFailures.slice(0, failureLogs)) picked.set(job.id, 'failure');

  const byKey = new Map();
  for (const job of jobs) {
    if (job.conclusion !== 'success') continue;
    const key = `${job.path}\0${job.name}`;
    if (!byKey.has(key)) byKey.set(key, { minutes: 0, jobs: [] });
    const entry = byKey.get(key);
    entry.minutes += minutes(job);
    entry.jobs.push(job);
  }
  const keys = [...byKey.values()].sort((a, b) => b.minutes - a.minutes);
  let sampled = 0;
  for (const entry of keys) {
    const newest = entry.jobs.sort((a, b) => (b.completedAt ?? '').localeCompare(a.completedAt ?? ''));
    for (const job of newest.slice(0, samplesPerJob)) {
      if (sampled >= sampleLogs) break;
      if (!picked.has(job.id)) {
        picked.set(job.id, 'sample');
        sampled += 1;
      }
    }
  }
  return picked;
}

export function minutes(job) {
  if (!job.startedAt || !job.completedAt) return 0;
  return Math.max(0, Date.parse(job.completedAt) - Date.parse(job.startedAt)) / 60000;
}
