// Reads one repository's GitHub Actions history for a window into a dataset: plain JSON that the analysis reads and
// that a test can hold as a fixture. Only GETs (see github.js). Logs are read for a bounded sample of jobs and kept
// only as what was parsed from them, never as text.
//
// The requests are bounded before the first one is made (requestPlan): the jobs of at most `maxRuns` runs, the newest
// in the window, at most `maxWorkflows` workflow files and the logs that pickLogs names. A busy repository has
// thousands of runs a week, and one jobs request per run would spend an hour of a token's rate limit. When a request
// fails part-way (the budget is spent, the rate limit will not reset in time, GitHub keeps failing), the dataset
// keeps what was read and says what is missing in `partial`.

import { parseLog } from './logs.js';

const DAY = 24 * 60 * 60 * 1000;

/** The defaults: how far back, how many runs and workflows to read, and how many logs a night may read. */
export const DEFAULTS = {
  days: 7,
  maxRuns: 300,
  maxWorkflows: 20,
  flakeLogs: 25,
  failureLogs: 15,
  sampleLogs: 60,
  samplesPerJob: 3,
};

/**
 * About how many REST requests one repository's collection makes with these options (retries, jobs lists over 100
 * entries and runs still in progress add to it; the client's budget is the limit): the repository, the runs list (one
 * page a day, plus one a hundred runs), one jobs list a run, the workflow files, the two cache lists, the logs, and
 * the two requests that mint an App installation token when the client signs in as an App.
 */
export function requestPlan(options = {}) {
  const o = { ...DEFAULTS, ...defined(options) };
  const days = Math.ceil(o.days);
  const logs = o.flakeLogs + o.failureLogs + o.sampleLogs;
  return 1 + days + Math.ceil(o.maxRuns / 100) + o.maxRuns + o.maxWorkflows + 2 + logs + (o.app ? 2 : 0);
}

/**
 * Options that keep the collection of `repos` repositories within `budget` requests in all: each repository gets an
 * equal share, and when the defaults do not fit it, the runs and the logs read shrink together. Throws when even a
 * small sample (20 runs and 5 logs a repository) does not fit.
 */
export function fitBudget(repos, budget, options = {}) {
  const o = { ...DEFAULTS, ...defined(options) };
  const share = Math.floor(budget / Math.max(1, repos));
  if (requestPlan(o) <= share) return o;
  const logs = o.flakeLogs + o.failureLogs + o.sampleLogs;
  const fixed = requestPlan({ ...o, maxRuns: 0, flakeLogs: 0, failureLogs: 0, sampleLogs: 0 });
  // What is left of the share goes to runs and logs in the defaults' proportion (300 runs to 100 logs).
  const factor = Math.max(0, share - fixed - 1) / (o.maxRuns * 1.01 + logs);
  const fitted = {
    ...o,
    maxRuns: Math.floor(o.maxRuns * factor),
    flakeLogs: Math.floor(o.flakeLogs * factor),
    failureLogs: Math.floor(o.failureLogs * factor),
    sampleLogs: Math.floor(o.sampleLogs * factor),
  };
  if (fitted.maxRuns < 20 || fitted.flakeLogs + fitted.failureLogs + fitted.sampleLogs < 5) {
    throw new Error(
      `a budget of ${budget} requests is too small for ${repos} repositories over ${o.days} days (each needs at least ${requestPlan({ ...o, maxRuns: 20, flakeLogs: 2, failureLogs: 1, sampleLogs: 2 })}); raise --budget or name fewer repositories`,
    );
  }
  return fitted;
}

export async function collect(gh, repo, options = {}) {
  const o = { ...DEFAULTS, ...defined(options) };
  const until = options.until ? new Date(options.until) : new Date();
  const since = new Date(until.getTime() - o.days * DAY);
  const log = options.log ?? (() => {});
  // A spent budget or a rate limit that will not reset in time stops the collection: nothing more can be read. Any
  // other failure (GitHub still failing after its retries) skips that one request and goes on.
  let stopped = null;
  let failed = 0;
  let firstFailure = null;
  const stop = (error) => {
    if (error.code !== 'budget' && error.code !== 'rate-limit') {
      failed += 1;
      firstFailure ??= error.message;
      log(`${repo}: skipped one request (${error.message})`);
      return;
    }
    if (!stopped) {
      stopped = error.message;
      log(`${repo}: stopped (${error.message}); keeping what was read`);
    }
  };

  const info = await gh.get(repo, `/repos/${repo}`);
  if (!info) throw new Error(`${repo} cannot be read with this access`);

  // The runs list answers at most 1000 runs per query, so it is read one day at a time, the newest day first, until
  // `maxRuns` completed runs are in hand.
  let runs = [];
  let listed = 0;
  let full = true;
  let daysUnlisted = 0;
  let reached = until.getTime(); // the start of the oldest day the listing got to
  for (let end = until.getTime(); end > since.getTime(); end -= DAY) {
    const start = Math.max(end - DAY, since.getTime());
    if (stopped) {
      daysUnlisted += 1;
      continue;
    }
    const range = `${iso(start)}..${iso(end - 1000)}`;
    try {
      const page = await gh.all(repo, `/repos/${repo}/actions/runs?created=${encodeURIComponent(range)}`, 'workflow_runs', {
        max: o.maxRuns - runs.length + 100,
      });
      listed += page.length;
      for (const run of page) if (run.status === 'completed') runs.push(slimRun(run));
      reached = start;
    } catch (error) {
      stop(error);
      daysUnlisted += 1;
    }
    if (runs.length >= o.maxRuns) {
      full = end - DAY <= since.getTime();
      break;
    }
  }
  runs.sort((a, b) => b.createdAt.localeCompare(a.createdAt));
  const sampled = runs.length > o.maxRuns || !full;
  runs = runs.slice(0, o.maxRuns);
  // A sample covers from its oldest run on, and the analysis scales to a week by that span, not by the days asked; a
  // listing that stopped covers the days it got to.
  const from = sampled && runs.length ? Date.parse(runs.at(-1).createdAt) : stopped ? reached : since.getTime();
  const cut = sampled || from > since.getTime();
  log(`${repo}: ${runs.length} completed runs since ${iso(from)}${sampled ? ` (the newest ${o.maxRuns})` : ''}`);

  const jobs = [];
  const jobsRead = new Set();
  let next = 0;
  const worker = async () => {
    while (!stopped && next < runs.length) {
      const run = runs[next++];
      try {
        const list = await gh.all(repo, `/repos/${repo}/actions/runs/${run.id}/jobs?filter=all`, 'jobs');
        for (const job of list) jobs.push(slimJob(job, run));
        jobsRead.add(run.id);
      } catch (error) {
        stop(error);
      }
    }
  };
  await Promise.all(Array.from({ length: 6 }, worker));
  const runsWithoutJobs = runs.filter((run) => !jobsRead.has(run.id)).length;
  runs = runs.filter((run) => jobsRead.has(run.id));
  log(`${repo}: ${jobs.length} jobs`);

  const workflows = {};
  const counts = new Map();
  for (const run of runs) if (run.path?.startsWith('.github/workflows/')) counts.set(run.path, (counts.get(run.path) ?? 0) + 1);
  const paths = [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, o.maxWorkflows).map(([path]) => path);
  for (const path of paths) {
    if (stopped) break;
    try {
      const file = await gh.get(repo, `/repos/${repo}/contents/${path}?ref=${encodeURIComponent(info.default_branch)}`);
      if (file?.content) workflows[path] = Buffer.from(file.content, 'base64').toString('utf8');
    } catch (error) {
      stop(error);
    }
  }

  let usage = null;
  let caches = [];
  if (!stopped) {
    try {
      usage = await gh.get(repo, `/repos/${repo}/actions/cache/usage`);
      caches = await gh.all(repo, `/repos/${repo}/actions/caches?sort=size_in_bytes&direction=desc`, 'actions_caches', {
        max: 100,
      });
    } catch (error) {
      stop(error);
    }
  }

  const logs = {};
  const wanted = [...pickLogs(runs, jobs, o)];
  for (const [jobId, why] of wanted) {
    if (stopped) break;
    try {
      const text = await gh.get(repo, `/repos/${repo}/actions/jobs/${jobId}/logs`, { raw: true });
      if (text) logs[jobId] = { why, ...parseLog(text) };
    } catch (error) {
      stop(error);
    }
  }
  log(`${repo}: ${Object.keys(logs).length} logs read; ${gh.requests} requests`);

  return {
    repo,
    defaultBranch: info.default_branch,
    visibility: info.private ? 'private' : 'public',
    generatedAt: new Date().toISOString(),
    window: {
      since: iso(from),
      until: iso(until.getTime()),
      days: cut ? Math.max(0.01, Math.round(((until.getTime() - from) / DAY) * 100) / 100) : o.days,
      ...(sampled ? { sampled: { runs: runs.length + runsWithoutJobs, listed, maxRuns: o.maxRuns, daysAsked: o.days } } : {}),
    },
    partial:
      stopped || failed
        ? {
            reason: stopped ?? `GitHub kept failing on ${failed} request(s), the first: ${firstFailure}`,
            daysUnlisted,
            runsWithoutJobs,
            workflowsUnread: paths.length - Object.keys(workflows).length,
            logsUnread: wanted.length - Object.keys(logs).length,
          }
        : null,
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

function defined(options) {
  return Object.fromEntries(Object.entries(options).filter(([, value]) => value !== undefined));
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
