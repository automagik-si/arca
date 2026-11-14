// A dataset (collect.js) in, a report out: what the repository's CI took over the window, per workflow, job and step;
// queue time; flaky jobs and tests; slow tests; cache hits; cost; and the ranked findings, each with one small change.
// Pure: no network, no clock but the dataset's own.

import { createHash } from 'node:crypto';
import { jobCost, runnerOf } from './cost.js';
import { cachingSteps, cancelsInProgress, jobFor, readWorkflow } from './workflow.js';

const DONE = new Set(['success', 'failure']);

export function percentile(values, p) {
  const sorted = values.filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  const rank = Math.ceil((p / 100) * sorted.length);
  return sorted[Math.min(sorted.length, Math.max(1, rank)) - 1];
}

const seconds = (from, to) => (from && to ? Math.max(0, (Date.parse(to) - Date.parse(from)) / 1000) : null);
const round = (n, digits = 0) => (n == null ? null : Math.round(n * 10 ** digits) / 10 ** digits);
const sum = (values) => values.reduce((a, b) => a + (b ?? 0), 0);

/**
 * A runner label as `--move-to` takes it: one plain runs-on label, which the pull request writes into a workflow. It
 * starts with a letter, so YAML reads it written plain as a string (not `-`, `.inf` or `1e3`). That is the only check:
 * a label whose name looks GitHub-hosted is still taken (`linux` is a self-hosted default), and the move says so.
 */
export function checkLabel(label) {
  if (label == null) return null;
  if (typeof label !== 'string' || !/^[A-Za-z][A-Za-z0-9._-]{0,99}$/.test(label)) {
    throw new Error(`--move-to takes one runner label (a letter, then letters, digits, '.', '_' and '-'), not ${JSON.stringify(label)}`);
  }
  return label;
}

/** An evidence line when the pool's label reads, by its name alone, as a runner GitHub or Blacksmith hosts. */
function hostedLooking(label) {
  const { kind } = runnerOf([label]);
  if (kind !== 'github' && kind !== 'blacksmith') return [];
  const who = kind === 'github' ? 'GitHub' : 'Blacksmith';
  return [
    `${label} reads by its name like a runner ${who} hosts; this report prices jobs on it at $0 as your own pool, so if ${who} runs them, the billed cost here is understated`,
  ];
}

/** Events whose payload carries `pull_request` and whose run can be a fork pull request's code. */
const PR_EVENTS = ['pull_request', 'pull_request_target', 'pull_request_review', 'pull_request_review_comment'];

/**
 * The runs-on of a moved job in a public workflow that pull requests start: a pull request whose head is not this
 * repository (a fork, or a deleted head) goes to ubuntu-latest, anything else to the pool. It steers runs of the
 * workflow as written; a fork pull request runs its own copy of the workflow and can name the pool itself.
 */
export const forkSafe = (label) =>
  `\${{ github.event.pull_request && github.event.pull_request.head.repo.full_name != github.repository && 'ubuntu-latest' || '${label}' }}`;

/**
 * @param {object} data a dataset (collect.js)
 * @param {object} [options]
 * @param {string} [options.moveTo] the runs-on label of the owner's own runner pool, where short billed jobs move
 */
export function analyze(data, options = {}) {
  const moveTo = checkLabel(options.moveTo ?? null);
  const scale = 7 / (data.window.days || 7);
  const workflows = Object.fromEntries(
    Object.entries(data.workflows ?? {}).map(([path, text]) => [path, readWorkflow(path, text)]),
  );
  const runsById = new Map(data.runs.map((run) => [run.id, run]));
  const jobs = data.jobs.map((job) => ({
    ...job,
    seconds: DONE.has(job.conclusion) ? seconds(job.startedAt, job.completedAt) : null,
    // Queue counts jobs that a runner took: a skipped job never waits for one.
    queue: job.startedAt && job.labels?.length && job.conclusion !== 'skipped' ? seconds(job.createdAt, job.startedAt) : null,
    cost: jobCost(job, data.visibility, moveTo),
    key: `${job.path}::${job.name}`,
  }));

  // Totals and runners.
  const byRunner = groupBy(jobs.filter((job) => job.cost.kind !== 'none'), (job) => job.cost.runner);
  const runners = [...byRunner.entries()]
    .map(([runner, list]) => ({
      runner,
      kind: list[0].cost.kind,
      jobs: list.length,
      minutes: sum(list.map((job) => job.cost.minutes)),
      listUsd: round(sum(list.map((job) => job.cost.list)), 2),
      billedUsd: round(sum(list.map((job) => job.cost.billed)), 2),
      queueP50: round(percentile(list.map((job) => job.queue), 50)),
      queueP95: round(percentile(list.map((job) => job.queue), 95)),
    }))
    .sort((a, b) => b.minutes - a.minutes);

  // Workflows.
  const workflowRows = [...groupBy(data.runs, (run) => run.path).entries()]
    .map(([path, runs]) => {
      const own = jobs.filter((job) => job.path === path);
      return {
        path,
        name: runs[0].workflow,
        runs: runs.length,
        success: runs.filter((run) => run.conclusion === 'success').length,
        failure: runs.filter((run) => run.conclusion === 'failure').length,
        cancelled: runs.filter((run) => run.conclusion === 'cancelled').length,
        wallP50: round(percentile(runs.map((run) => seconds(run.startedAt, run.updatedAt)), 50)),
        wallP95: round(percentile(runs.map((run) => seconds(run.startedAt, run.updatedAt)), 95)),
        minutes: sum(own.map((job) => job.cost.minutes)),
        listUsd: round(sum(own.map((job) => job.cost.list)), 2),
        billedUsd: round(sum(own.map((job) => job.cost.billed)), 2),
      };
    })
    .sort((a, b) => b.minutes - a.minutes);

  // Jobs and their steps.
  const jobRows = [...groupBy(jobs, (job) => job.key).entries()]
    .map(([key, list]) => {
      const done = list.filter((job) => job.seconds != null);
      const steps = new Map();
      for (const job of done.filter((job) => job.conclusion === 'success')) {
        for (const step of job.steps) {
          if (step.conclusion !== 'success') continue;
          const s = seconds(step.startedAt, step.completedAt);
          if (s == null) continue;
          if (!steps.has(step.name)) steps.set(step.name, []);
          steps.get(step.name).push(s);
        }
      }
      const p50 = percentile(done.map((job) => job.seconds), 50);
      return {
        key,
        path: list[0].path,
        workflow: list[0].workflow,
        name: list[0].name,
        runner: list[0].cost.runner,
        runs: list.length,
        success: list.filter((job) => job.conclusion === 'success').length,
        failure: list.filter((job) => job.conclusion === 'failure').length,
        cancelled: list.filter((job) => job.conclusion === 'cancelled').length,
        p50: round(p50),
        p95: round(percentile(done.map((job) => job.seconds), 95)),
        max: round(Math.max(0, ...done.map((job) => job.seconds))),
        queueP50: round(percentile(list.map((job) => job.queue), 50)),
        queueP95: round(percentile(list.map((job) => job.queue), 95)),
        minutes: sum(list.map((job) => job.cost.minutes)),
        listUsd: round(sum(list.map((job) => job.cost.list)), 2),
        billedUsd: round(sum(list.map((job) => job.cost.billed)), 2),
        perMinute: list[0].cost.perMinute,
        freeForPublic: list[0].cost.freeForPublic,
        steps: [...steps.entries()]
          .map(([name, values]) => ({
            name,
            runs: values.length,
            p50: round(percentile(values, 50)),
            p95: round(percentile(values, 95)),
            share: p50 ? round(percentile(values, 50) / p50, 2) : null,
          }))
          .sort((a, b) => b.p50 - a.p50),
      };
    })
    .sort((a, b) => b.minutes - a.minutes);

  const flaky = findFlaky(jobs, data.logs ?? {});
  const tests = testTimes(jobs, data.logs ?? {});
  const cache = cacheStats(jobs, data.logs ?? {}, data.caches ?? {});

  const context = { data, scale, workflows, jobs, runsById, jobRows, workflowRows, runners, flaky, tests, cache, moveTo };
  const findings = [
    ...failingWorkflows(context),
    ...paidRunners(context),
    ...supersededRuns(context),
    ...duplicateTriggers(context),
    ...flakyFindings(context),
    ...missingTimeouts(context),
    ...missingToolchainCache(context),
    ...cacheFindings(context),
    ...matrixLegs(context),
    ...splitJobs(context),
    ...queueFindings(context),
    ...slowTests(context),
  ]
    .map((finding) => ({ ...finding, id: findingId(data.repo, finding), weekly: withUsd(context, finding) }))
    .sort((a, b) => score(b) - score(a));
  findings.forEach((finding, index) => {
    finding.rank = index + 1;
  });

  const totalMinutes = sum(jobs.map((job) => job.cost.minutes));
  return {
    repo: data.repo,
    visibility: data.visibility,
    defaultBranch: data.defaultBranch,
    generatedAt: data.generatedAt,
    window: data.window,
    totals: {
      runs: data.runs.length,
      jobs: jobs.length,
      minutes: totalMinutes,
      minutesPerWeek: round(totalMinutes * scale),
      listUsd: round(sum(jobs.map((job) => job.cost.list)), 2),
      billedUsd: round(sum(jobs.map((job) => job.cost.billed)), 2),
      queueP50: round(percentile(jobs.map((job) => job.queue), 50)),
      queueP95: round(percentile(jobs.map((job) => job.queue), 95)),
      logsRead: Object.keys(data.logs ?? {}).length,
    },
    runners,
    workflows: workflowRows,
    jobs: jobRows,
    flaky,
    tests,
    cache,
    findings,
  };
}

function groupBy(list, keyOf) {
  const out = new Map();
  for (const item of list) {
    const key = keyOf(item);
    if (!out.has(key)) out.set(key, []);
    out.get(key).push(item);
  }
  return out;
}

/**
 * Ranks findings by what they save a week: a billed dollar counts as 100 points, a minute someone waits as 1, a runner
 * minute nobody is billed for (a public repository's free runners, a self-hosted one) as 0.2. A measured saving
 * outranks an estimate of the same size.
 */
export function score(finding) {
  const weekly = finding.weekly ?? {};
  const weight = finding.confidence === 'measured' ? 1 : 0.6;
  const points = 100 * (weekly.usd ?? 0) + (weekly.wallMinutes ?? 0) + 0.2 * (weekly.runnerMinutes ?? 0);
  return weight * points + (finding.mechanical ? 0.5 : 0);
}

/** The finding's weekly saving in billed dollars, at the billed price per minute of the jobs (or workflow) it is about. */
function withUsd({ jobRows, workflows }, finding) {
  const weekly = { ...(finding.weekly ?? {}) };
  if (weekly.usd != null) return weekly;
  let rows = jobRows.filter((row) => finding.path && row.path === finding.path);
  if (finding.job) {
    const workflow = workflows[finding.path];
    const job = workflow?.jobs.find((j) => j.id === finding.job);
    const own = rows.filter((row) => row.name === finding.job || (job && jobFor(workflow, row.name) === job));
    if (own.length) rows = own;
  }
  const minutes = sum(rows.map((row) => row.minutes));
  const billed = sum(rows.map((row) => row.billedUsd ?? 0));
  weekly.usd = minutes ? round(((weekly.runnerMinutes ?? 0) * billed) / minutes, 2) : 0;
  return weekly;
}

/** A finding's id stays the same night after night while it holds, so a PR and its finding can be matched. */
function findingId(repo, finding) {
  const text = [repo, finding.kind, finding.path ?? '', finding.job ?? '', finding.subject ?? ''].join('\0');
  return `${finding.kind}-${createHash('sha256').update(text).digest('hex').slice(0, 8)}`;
}

// ---------------------------------------------------------------------------------------------------------------
// Flaky jobs and tests: a job that failed and then passed on the same commit.

function findFlaky(jobs, logs) {
  const groups = groupBy(
    jobs.filter((job) => DONE.has(job.conclusion)),
    (job) => `${job.path}\0${job.headSha}\0${job.name}`,
  );
  const events = [];
  for (const list of groups.values()) {
    const sorted = [...list].sort((a, b) => (a.completedAt ?? '').localeCompare(b.completedAt ?? ''));
    const firstPass = sorted.findIndex((job) => job.conclusion === 'success');
    if (firstPass <= 0) continue;
    const failures = sorted.slice(0, firstPass).filter((job) => job.conclusion === 'failure');
    if (failures.length === 0) continue;
    const pass = sorted[firstPass];
    events.push({
      key: pass.key,
      path: pass.path,
      name: pass.name,
      headSha: pass.headSha,
      how: failures.every((job) => job.runId === pass.runId) ? 'rerun' : 'another run',
      failedJobs: failures.map((job) => ({ id: job.id, runId: job.runId, attempt: job.attempt })),
      passedJob: { id: pass.id, runId: pass.runId, attempt: pass.attempt },
      wastedMinutes: sum(failures.map((job) => job.cost.minutes)),
      tests: [...new Set(failures.flatMap((job) => logs[job.id]?.tests?.failed ?? []))].filter((name) => !name.endsWith(":")),
    });
  }
  const byJob = [...groupBy(events, (event) => event.key).entries()]
    .map(([key, list]) => {
      const runs = new Set(jobs.filter((job) => job.key === key).map((job) => job.headSha)).size;
      return {
        key,
        path: list[0].path,
        name: list[0].name,
        flakes: list.length,
        commits: runs,
        rate: round(list.length / Math.max(1, runs), 3),
        wastedMinutes: sum(list.map((event) => event.wastedMinutes)),
        examples: list.slice(0, 5).map((event) => ({
          headSha: event.headSha,
          how: event.how,
          failed: event.failedJobs[0],
          passed: event.passedJob,
        })),
        tests: [...new Set(list.flatMap((event) => event.tests))].slice(0, 20),
      };
    })
    .sort((a, b) => b.flakes - a.flakes);
  const testCounts = new Map();
  for (const event of events) {
    for (const test of event.tests) {
      const entry = testCounts.get(test) ?? { name: test, flakes: 0, jobs: new Set() };
      entry.flakes += 1;
      entry.jobs.add(event.name);
      testCounts.set(test, entry);
    }
  }
  return {
    events: events.length,
    jobs: byJob,
    tests: [...testCounts.values()]
      .map((entry) => ({ name: entry.name, flakes: entry.flakes, jobs: [...entry.jobs] }))
      .sort((a, b) => b.flakes - a.flakes)
      .slice(0, 30),
  };
}

// ---------------------------------------------------------------------------------------------------------------
// Test times from the logs read.

function testTimes(jobs, logs) {
  const byId = new Map(jobs.map((job) => [job.id, job]));
  const failedAnywhere = new Set();
  for (const parsed of Object.values(logs)) for (const name of parsed.tests?.failed ?? []) failedAnywhere.add(name);
  const slow = new Map();
  const where = new Map();
  for (const [id, parsed] of Object.entries(logs)) {
    const job = byId.get(Number(id));
    if (!job) continue;
    for (const test of parsed.tests?.slow ?? []) {
      const key = `${job.key}\0${test.name}`;
      if (!slow.has(key)) slow.set(key, { job: job.name, path: job.path, name: test.name, times: [] });
      slow.get(key).times.push(test.ms);
      if (!where.has(test.name)) where.set(test.name, new Map());
      const shas = where.get(test.name);
      if (!shas.has(job.headSha)) shas.set(job.headSha, new Set());
      shas.get(job.headSha).add(job.name);
    }
  }
  const rows = [...slow.values()]
    .map((row) => ({
      job: row.job,
      path: row.path,
      name: row.name,
      samples: row.times.length,
      p50Ms: round(percentile(row.times, 50)),
      maxMs: round(Math.max(...row.times)),
      failedInLogsRead: failedAnywhere.has(row.name),
      // The jobs that ran this same test on one commit: more than one is the same work done twice.
      sameCommitJobs: Math.max(...[...where.get(row.name).values()].map((names) => names.size)),
      otherJobs: [...new Set([...where.get(row.name).values()].flatMap((names) => [...names]))].filter((name) => name !== row.job),
    }))
    .sort((a, b) => b.p50Ms - a.p50Ms);
  const failureLogs = Object.values(logs).filter((parsed) => parsed.why !== 'sample').length;
  return { slow: rows.slice(0, 40), failureLogsRead: failureLogs };
}

// ---------------------------------------------------------------------------------------------------------------
// Caches.

function cacheStats(jobs, logs, caches) {
  const byId = new Map(jobs.map((job) => [job.id, job]));
  const restores = [];
  for (const [id, parsed] of Object.entries(logs)) {
    const job = byId.get(Number(id));
    for (const entry of parsed.cache ?? []) restores.push({ ...entry, job: job?.name, path: job?.path });
  }
  const steps = [...groupBy(restores, (entry) => `${entry.path}\0${entry.job}\0${(entry.step ?? '').split('@')[0]}`).values()]
    .map((list) => ({
      path: list[0].path,
      job: list[0].job,
      step: list[0].step,
      restores: list.length,
      hits: list.filter((entry) => entry.outcome === 'hit').length,
      partial: list.filter((entry) => entry.outcome === 'partial').length,
      misses: list.filter((entry) => entry.outcome === 'miss').length,
      hitRate: round(list.filter((entry) => entry.outcome === 'hit').length / list.length, 2),
      keys: [...new Set(list.map((entry) => entry.key).filter(Boolean))].slice(0, 5),
      gzip: list.filter((entry) => entry.compression === 'gzip').length,
      zstd: list.filter((entry) => entry.compression === 'zstd').length,
    }))
    .sort((a, b) => a.hitRate - b.hitRate || b.restores - a.restores);
  return {
    restores: restores.length,
    hits: restores.filter((entry) => entry.outcome === 'hit').length,
    partial: restores.filter((entry) => entry.outcome === 'partial').length,
    misses: restores.filter((entry) => entry.outcome === 'miss').length,
    hitRate: restores.length ? round(restores.filter((entry) => entry.outcome === 'hit').length / restores.length, 2) : null,
    gzip: restores.filter((entry) => entry.compression === 'gzip').length,
    zstd: restores.filter((entry) => entry.compression === 'zstd').length,
    activeBytes: caches.activeBytes ?? null,
    activeCount: caches.activeCount ?? null,
    steps,
  };
}

// ---------------------------------------------------------------------------------------------------------------
// The rules. Each returns findings: { kind, title, path, job?, subject?, evidence[], weekly{runnerMinutes,
// wallMinutes, listUsd}, confidence, mechanical, change{kind, ...}, suggestion }.

const minutesText = (s) => (s == null ? '-' : s >= 120 ? `${round(s / 60, 1)} min` : `${round(s)} s`);
const sha = (s) => (s ?? '').slice(0, 7);

/** A workflow that fails most of its runs: every failed run's minutes are spent for nothing. */
function failingWorkflows({ data, scale, workflowRows, jobs }) {
  const out = [];
  for (const row of workflowRows) {
    const finished = row.success + row.failure;
    if (finished < 5 || row.failure / finished < 0.5) continue;
    const failedRuns = new Set(data.runs.filter((run) => run.path === row.path && run.conclusion === 'failure').map((run) => run.id));
    const spent = sum(jobs.filter((job) => failedRuns.has(job.runId)).map((job) => job.cost.minutes));
    if (spent < 10) continue;
    const steps = new Map();
    for (const job of jobs.filter((j) => failedRuns.has(j.runId) && j.conclusion === 'failure')) {
      const step = job.steps.find((s) => s.conclusion === 'failure');
      const key = `${job.name} › ${step?.name ?? '(no step failed)'}`;
      steps.set(key, (steps.get(key) ?? 0) + 1);
    }
    const top = [...steps.entries()].sort((a, b) => b[1] - a[1]).slice(0, 3);
    out.push({
      kind: 'failing-workflow',
      title: `${row.name} failed ${row.failure} of ${finished} runs`,
      path: row.path,
      evidence: [
        `${row.failure} of ${finished} finished runs failed in ${data.window.days} days, and their jobs took ${round(spent)} runner-min`,
        ...top.map(([step, count]) => `failed at ${step}: ${count} times`),
      ],
      weekly: { runnerMinutes: round(spent * scale), wallMinutes: 0 },
      confidence: 'measured',
      mechanical: false,
      change: { kind: 'fix-workflow', path: row.path, step: top[0]?.[0] ?? null },
      suggestion: top.length
        ? `Fix ${top[0][0]}, or stop the trigger that starts this workflow until it is fixed.`
        : 'Find why this workflow fails, or stop its trigger until it is fixed.',
    });
  }
  return out;
}

/**
 * Short jobs on a billed runner that an unbilled one would run as well: the free GitHub-hosted runner of a public
 * repository, or, when `moveTo` names one, the label of the repository owner's own runner pool (whatever the
 * repository's visibility). In a public repository anyone can open a pull request from a fork, and a fork pull request
 * runs its own copy of the workflow, so it can name the pool. Every move in a public repository says so in its
 * evidence; where pull requests start the workflow, `forkSafe` keeps the ordinary ones on ubuntu-latest.
 */
function paidRunners({ data, scale, workflows, jobRows, moveTo }) {
  if (!moveTo && data.visibility !== 'public') return [];
  const why = moveTo
    ? `${moveTo} is the owner's own runner pool, billed by neither GitHub nor a provider`
    : 'this repository is public, so ubuntu-latest (4 vCPU for public repositories) is not billed';
  const out = [];
  for (const [path, workflow] of Object.entries(workflows)) {
    if (!workflow) continue;
    const exposed = Boolean(moveTo && data.visibility === 'public');
    const forks = exposed && PR_EVENTS.some((event) => workflow.on[event]);
    const target = !moveTo ? 'ubuntu-latest' : forks ? forkSafe(moveTo) : moveTo;
    const moves = [];
    for (const job of workflow.jobs) {
      if (typeof job.runsOn !== 'string' || job.runsOn.includes('${{') || job.uses) continue;
      const rows = jobRows.filter((row) => row.path === path && jobFor(workflow, row.name) === job);
      if (rows.length === 0 || rows.some((row) => row.freeForPublic || !row.perMinute || row.runner.startsWith('self-hosted'))) continue;
      const p50 = Math.max(...rows.map((row) => row.p50 ?? 0));
      const p95 = Math.max(...rows.map((row) => row.p95 ?? 0));
      if (p50 > 90 || p95 > 180) continue;
      const usd = sum(rows.map((row) => row.billedUsd));
      if (usd <= 0) continue;
      const linux = /ubuntu|linux/.test(job.runsOn) && !/arm/.test(job.runsOn);
      if (!linux) continue;
      moves.push({ id: job.id, from: job.runsOn, to: target, p50, p95, runs: sum(rows.map((row) => row.runs)), usd });
    }
    if (moves.length === 0) continue;
    const usd = sum(moves.map((move) => move.usd));
    out.push({
      kind: 'paid-runner',
      title: `Run ${moves.length} short job(s) of ${workflow.name ?? path} on ${moveTo ? moveTo : 'the free GitHub runner'}`,
      path,
      evidence: [
        `${why}; these jobs run on ${[...new Set(moves.map((m) => m.from))].join(', ')} and cost $${round(usd, 2)} in ${data.window.days} days`,
        ...moves.map((m) => `${m.id}: p50 ${minutesText(m.p50)}, p95 ${minutesText(m.p95)} over ${m.runs} runs, $${round(m.usd, 2)}`),
        'a short job gains little from a faster runner; a job that needs the provider\'s own cache or hardware should stay',
        ...(moveTo ? hostedLooking(moveTo) : []),
        ...(exposed ? publicPool(moveTo, forks) : []),
      ],
      weekly: { runnerMinutes: 0, wallMinutes: 0, usd: round(usd * scale, 2) },
      confidence: 'measured',
      mechanical: true,
      change: {
        kind: 'set-runner',
        path,
        jobs: moves.map((m) => ({ id: m.id, from: m.from, to: m.to })),
        ...(exposed ? { pool: moveTo, public: true } : {}),
        ...(forks ? { forks: 'ubuntu-latest' } : {}),
      },
      suggestion: forks
        ? `Run ${moves.map((m) => m.id).join(', ')} on ${moveTo}, and send ordinary pull requests from forks to ubuntu-latest; a fork pull request can still name ${moveTo} itself, so read the evidence about public repositories first.`
        : exposed
          ? `Set runs-on: ${target} for ${moves.map((m) => m.id).join(', ')}; this repository is public, so read the evidence about public repositories first.`
          : `Set runs-on: ${target} for ${moves.map((m) => m.id).join(', ')}.`,
    });
  }
  return out;
}

/** What moving a public repository's jobs to the owner's pool exposes, in the evidence of every such move. */
function publicPool(pool, forks) {
  return [
    `this repository is public: its jobs reach ${pool} only if the pool's runner group admits this public repository, and Arca's pool warns when it does, because a job on the pool can reach the pool's GitHub credential and the other slots ("Who can reach what" in Arca's docs/OPERATING.md)`,
    forks
      ? `the runs-on expression sends ordinary pull requests from forks to ubuntu-latest, but it is not a wall: a fork pull request runs its own copy of the workflow and can name ${pool} itself`
      : `nothing in this workflow steers pull requests from forks, and any fork pull request can add a workflow of its own that names ${pool}`,
    'if you still want this, choose "Require approval for all external contributors" (Settings, Actions, General), and read every workflow change before you approve a run',
  ];
}

/** Runs that kept going after a newer commit was pushed to the same branch or pull request. */
function supersededRuns({ data, scale, workflows, jobs }) {
  const out = [];
  for (const [path, runs] of groupBy(data.runs, (run) => run.path)) {
    const workflow = workflows[path];
    if (!workflow) continue;
    if (cancelsInProgress(workflow) === true) continue;
    const events = new Set();
    let wasted = 0;
    const examples = [];
    for (const list of groupBy(
      runs.filter((run) => ['pull_request', 'push', 'pull_request_target'].includes(run.event)),
      (run) => `${run.event}\0${run.headBranch}`,
    ).values()) {
      const sorted = [...list].sort((a, b) => a.createdAt.localeCompare(b.createdAt));
      for (let k = 0; k + 1 < sorted.length; k += 1) {
        const old = sorted[k];
        const next = sorted.slice(k + 1).find((run) => run.headSha !== old.headSha);
        if (!next || next.createdAt >= old.updatedAt || old.conclusion === 'cancelled') continue;
        const after = Date.parse(next.createdAt);
        const spent = sum(
          jobs
            .filter((job) => job.runId === old.id && job.startedAt && job.completedAt)
            .map((job) => Math.max(0, Date.parse(job.completedAt) - Math.max(after, Date.parse(job.startedAt))) / 60000),
        );
        if (spent <= 0) continue;
        if (cancelsInProgress(workflow) === 'expression' && old.event === 'pull_request') continue;
        wasted += spent;
        events.add(old.event);
        if (examples.length < 3) examples.push(`run ${old.id} (${old.event}, ${sha(old.headSha)}) ran ${round(spent, 1)} runner-min after ${sha(next.headSha)} was pushed to ${old.headBranch}`);
      }
    }
    if (wasted < 1) continue;
    const group = '${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}';
    out.push({
      kind: 'cancel-superseded',
      title: `Cancel superseded runs of ${workflow.name ?? path}`,
      path,
      evidence: [
        `${round(wasted)} runner-min in ${data.window.days} days went to runs that kept going after a newer commit reached the same branch`,
        ...examples,
        workflow.concurrency ? `the workflow's concurrency does not cancel these (${JSON.stringify(workflow.concurrency)})` : 'the workflow sets no concurrency group',
      ],
      weekly: { runnerMinutes: round(wasted * scale), wallMinutes: 0 },
      confidence: 'measured',
      mechanical: !workflow.concurrency,
      change: {
        kind: 'add-concurrency',
        path,
        group,
        cancel: events.has('push') ? true : "${{ github.event_name == 'pull_request' }}",
      },
      suggestion: `Add a top-level concurrency group (${group}) with cancel-in-progress, so a new push stops the run it replaces.`,
    });
  }
  return out;
}

/** The same commit tested twice by one workflow: once for the push, once for the pull request. */
function duplicateTriggers({ data, scale, workflows, jobs }) {
  const out = [];
  for (const [path, runs] of groupBy(data.runs, (run) => run.path)) {
    const workflow = workflows[path];
    if (!workflow?.on?.push || !workflow.on.pull_request) continue;
    // A push to a branch the workflow names on purpose (the default branch, a release branch) is the run after the
    // merge, wanted even when its commit was a pull request's head.
    const named = asList(workflow.on.push.branches).filter((branch) => !/[*?[!]/.test(branch));
    const pushShas = new Map(
      runs.filter((run) => run.event === 'push' && !named.includes(run.headBranch)).map((run) => [run.headSha, run]),
    );
    const doubles = runs.filter((run) => run.event === 'pull_request' && pushShas.has(run.headSha));
    if (doubles.length === 0) continue;
    const pushRuns = [...new Set(doubles.map((run) => pushShas.get(run.headSha)))];
    const wasted = sum(pushRuns.map((run) => sum(jobs.filter((job) => job.runId === run.id).map((job) => job.cost.minutes))));
    if (wasted < 1) continue;
    const branches = [...new Set(pushRuns.map((run) => run.headBranch))];
    const keep = [...new Set([data.defaultBranch, ...asList(workflow.on.push.branches).filter((b) => !b.includes('*'))])];
    out.push({
      kind: 'duplicate-trigger',
      title: `${workflow.name ?? path} runs twice on the same commit (push and pull request)`,
      path,
      evidence: [
        `${doubles.length} commits ran this workflow for both a push and a pull request; the push runs took ${round(wasted)} runner-min`,
        `branches pushed: ${branches.slice(0, 6).join(', ')}${branches.length > 6 ? ', ...' : ''}`,
        `push is limited to: ${asList(workflow.on.push.branches).join(', ') || 'every branch'}`,
      ],
      weekly: { runnerMinutes: round(wasted * scale), wallMinutes: 0 },
      confidence: 'measured',
      mechanical: !workflow.on.push.branches,
      change: { kind: 'restrict-push', path, branches: keep },
      suggestion: `Run push only for ${keep.join(', ')}: a branch with a pull request is tested by the pull_request run.`,
    });
  }
  return out;
}

function asList(value) {
  if (value == null) return [];
  return Array.isArray(value) ? value.map(String) : [String(value)];
}

function flakyFindings({ data, scale, flaky }) {
  return flaky.jobs
    .filter((row) => row.flakes >= 2 || row.tests.length > 0)
    .map((row) => ({
      kind: 'flaky-job',
      title: `Flaky: ${row.name} failed then passed on the same commit ${row.flakes} times`,
      path: row.path,
      job: row.name,
      evidence: [
        `${row.flakes} of ${row.commits} commits (${round(row.rate * 100)}%) failed this job and then passed it without a code change; ${round(row.wastedMinutes)} runner-min were spent on the failed attempts`,
        ...row.examples.slice(0, 3).map((e) => `${sha(e.headSha)}: job ${e.failed.id} (run ${e.failed.runId} attempt ${e.failed.attempt}) failed, job ${e.passed.id} (${e.how}) passed`),
        row.tests.length ? `tests that failed in those attempts: ${row.tests.slice(0, 5).join('; ')}` : 'no failing test name could be read from the failed attempts\' logs',
      ],
      weekly: { runnerMinutes: round(row.wastedMinutes * scale), wallMinutes: round(row.wastedMinutes * scale) },
      confidence: 'measured',
      mechanical: false,
      change: { kind: 'quarantine', path: row.path, job: row.name, tests: row.tests.slice(0, 5) },
      suggestion: row.tests.length
        ? `Fix or quarantine ${row.tests[0]}: it failed and passed on the same commit.`
        : `Read the failed attempts' logs and fix the step that fails intermittently.`,
    }));
}

/** Jobs without timeout-minutes run up to 6 hours when they hang. */
function missingTimeouts({ data, scale, workflows, jobRows }) {
  const out = [];
  for (const [path, workflow] of Object.entries(workflows)) {
    if (!workflow) continue;
    const missing = [];
    let excess = 0;
    for (const job of workflow.jobs) {
      if (job.timeout != null || job.uses) continue;
      const rows = jobRows.filter((row) => row.path === path && jobFor(workflow, row.name) === job);
      if (rows.length === 0) continue;
      const p95 = Math.max(...rows.map((row) => row.p95 ?? 0));
      const max = Math.max(...rows.map((row) => row.max ?? 0));
      const minutes = Math.max(10, Math.ceil(Math.max(p95 * 3, max * 1.5) / 60 / 5) * 5);
      excess += sum(
        data.jobs
          .filter((j) => rows.some((row) => row.name === j.name) && j.path === path && j.startedAt && j.completedAt)
          .map((j) => Math.max(0, (Date.parse(j.completedAt) - Date.parse(j.startedAt)) / 60000 - minutes)),
      );
      missing.push({ id: job.id, line: job.line, p95, max, minutes, runs: sum(rows.map((row) => row.runs)) });
    }
    if (missing.length === 0) continue;
    out.push({
      kind: 'missing-timeout',
      title: `Set timeout-minutes on ${missing.length} job(s) of ${workflow.name ?? path}`,
      path,
      evidence: [
        'a job without timeout-minutes may run for 6 hours when it hangs, at full price',
        ...missing.map((job) => `${job.id}: p95 ${minutesText(job.p95)}, max ${minutesText(job.max)} over ${job.runs} runs; proposed ${job.minutes} min`),
      ],
      weekly: { runnerMinutes: round(excess * scale), wallMinutes: 0 },
      confidence: 'measured',
      mechanical: true,
      change: { kind: 'add-timeout', path, jobs: missing.map((job) => ({ id: job.id, minutes: job.minutes })) },
      suggestion: `Add timeout-minutes (3 x p95, at least 10) to ${missing.map((job) => job.id).join(', ')}.`,
    });
  }
  return out;
}

const RUST_STEP = /\bcargo (build|test|clippy|nextest|check|doc)\b/;
const NODE_INSTALL = /\b(npm ci|npm install|pnpm install|yarn install|yarn)\b/;
const PIP_INSTALL = /\bpip3? install\b|\buv sync\b|\bpoetry install\b/;

/** A toolchain step that builds from nothing each run, because no cache is kept for it. */
function missingToolchainCache({ data, scale, workflows, jobRows }) {
  const out = [];
  for (const [path, workflow] of Object.entries(workflows)) {
    if (!workflow) continue;
    for (const job of workflow.jobs) {
      const rows = jobRows.filter((row) => row.path === path && jobFor(workflow, row.name) === job);
      if (rows.length === 0) continue;
      const selfHosted = rows.every((row) => row.runner.startsWith('self-hosted'));
      const caching = cachingSteps(job);
      const usesText = caching.map((step) => step.uses ?? '').join(' ');
      const stepTime = (pattern) => {
        const names = job.steps.filter((step) => pattern.test(step.run ?? '')).map((step) => step.name ?? `Run ${(step.run ?? '').split('\n')[0]}`);
        const times = rows.flatMap((row) =>
          row.steps.filter((s) => names.some((name) => s.name === name || s.name.startsWith(name.slice(0, 40)))).map((s) => ({ ...s, runs: s.runs })),
        );
        return { names, p50: sum(times.map((s) => s.p50)), runs: Math.max(0, ...times.map((s) => s.runs)) };
      };
      const runs = sum(rows.map((row) => row.success));
      // Rust: no rust-cache, sccache or actions/cache of target or ~/.cargo.
      if (!/rust-cache|sccache/.test(usesText) && !caching.some((step) => /target|\.cargo/.test(JSON.stringify(step.with))) && !selfHosted) {
        const rust = stepTime(RUST_STEP);
        if (rust.names.length && rust.p50 >= 120) {
          const toolchain = job.steps.find((step) => /rust-toolchain|setup-rust|rustup/.test((step.uses ?? '') + (step.run ?? '')));
          const saved = (rust.p50 * 0.4 * runs) / 60;
          const release = /release|publish|deploy|sign/i.test(`${path} ${workflow.name ?? ''} ${job.id}`);
          out.push({
            kind: 'add-rust-cache',
            title: `Cache Rust builds in ${job.id} (${workflow.name ?? path})`,
            path,
            job: job.id,
            evidence: [
              `cargo steps take ${minutesText(rust.p50)} at p50 in ${job.id}, over ${runs} successful runs, and the job keeps no cargo cache (no rust-cache, sccache, or actions/cache of target/ or ~/.cargo)`,
              ...rust.names.slice(0, 3).map((name) => `step: ${name}`),
              'saving estimated at 40% of the cargo steps (dependencies are restored, the workspace is rebuilt)',
              ...(release ? ['this is a release job: projects often keep caches out of release builds on purpose (a poisoned cache would ship), so this one is for the maintainers to decide'] : []),
            ],
            weekly: { runnerMinutes: round(saved * scale), wallMinutes: round(saved * scale) },
            confidence: 'estimated',
            mechanical: !release,
            change: { kind: 'add-step', path, job: job.id, after: toolchain ? toolchain.index : 0, action: 'Swatinem/rust-cache', tag: 'v2' },
            suggestion: `Add Swatinem/rust-cache after the toolchain step of ${job.id}.`,
          });
        }
      }
      // Node and Python: a setup action without its cache input, and a slow install.
      for (const [lang, pattern, input] of [
        ['node', NODE_INSTALL, (step) => /^actions\/setup-node@/.test(step.uses ?? '')],
        ['python', PIP_INSTALL, (step) => /^actions\/setup-python@/.test(step.uses ?? '')],
      ]) {
        const setup = job.steps.find(input);
        if (!setup || setup.with?.cache || caching.some((step) => step !== setup && /\/cache@/.test(step.uses ?? ''))) continue;
        const install = stepTime(pattern);
        if (!install.names.length || install.p50 < 30) continue;
        const run = job.steps.find((step) => pattern.test(step.run ?? ''))?.run ?? '';
        const manager = lang === 'python' ? 'pip' : /pnpm/.test(run) ? 'pnpm' : /yarn/.test(run) ? 'yarn' : 'npm';
        const saved = (install.p50 * 0.5 * runs) / 60;
        const release = /release|publish|deploy|sign/i.test(`${path} ${workflow.name ?? ''} ${job.id}`);
        out.push({
          kind: 'add-setup-cache',
          title: `Cache ${manager} downloads in ${job.id} (${workflow.name ?? path})`,
          path,
          job: job.id,
          evidence: [
            `the install step takes ${minutesText(install.p50)} at p50 over ${runs} successful runs, and ${setup.uses.split('@')[0]} has no cache input`,
            'saving estimated at half of the install step',
            ...(release ? ['this is a release job: projects often keep caches out of release builds on purpose (a poisoned cache would ship), so this one is for the maintainers to decide'] : []),
          ],
          weekly: { runnerMinutes: round(saved * scale), wallMinutes: round(saved * scale) },
          confidence: 'estimated',
          mechanical: !release,
          change: { kind: 'add-input', path, job: job.id, step: setup.index, input: 'cache', value: manager },
          suggestion: `Set cache: ${manager} on ${setup.uses.split('@')[0]} in ${job.id}.`,
        });
      }
    }
  }
  return out;
}

function cacheFindings({ data, scale, cache }) {
  const out = [];
  for (const step of cache.steps) {
    if (step.restores < 3 || step.hitRate >= 0.5) continue;
    out.push({
      kind: 'cache-miss',
      title: `Cache in ${step.job} misses ${round((1 - step.hitRate) * 100)}% of the time`,
      path: step.path,
      job: step.job,
      subject: step.step,
      evidence: [
        `${step.step}: ${step.hits} exact hits, ${step.partial} partial, ${step.misses} misses in ${step.restores} logs read`,
        step.keys.length ? `keys seen: ${step.keys.slice(0, 3).join(', ')}` : 'no key was printed',
      ],
      weekly: { runnerMinutes: 0, wallMinutes: 0 },
      confidence: 'measured',
      mechanical: false,
      change: { kind: 'cache-key', path: step.path, job: step.job, step: step.step },
      suggestion: 'Key the cache on the lockfile hash (hashFiles) and add restore-keys with its prefix, so a run finds the newest cache.',
    });
  }
  if (cache.gzip > 0) {
    out.push({
      kind: 'cache-gzip',
      title: 'Caches are compressed with gzip: the runner has no zstd',
      path: null,
      evidence: [`${cache.gzip} of ${cache.gzip + cache.zstd} cache restores read used gzip`, 'a cache saved with zstd (as on GitHub-hosted runners) is not found by a runner without it, and gzip is slower to pack and unpack'],
      weekly: { runnerMinutes: 0, wallMinutes: 0 },
      confidence: 'measured',
      mechanical: false,
      change: { kind: 'runner-image', package: 'zstd' },
      suggestion: 'Install zstd on the self-hosted runner image.',
    });
  }
  if (cache.activeBytes != null && cache.activeBytes > 8 * 1024 ** 3) {
    out.push({
      kind: 'cache-full',
      title: `The repository's caches hold ${round(cache.activeBytes / 1024 ** 3, 1)} GiB of the 10 GiB limit`,
      path: null,
      evidence: [`${cache.activeCount} caches, ${round(cache.activeBytes / 1024 ** 3, 1)} GiB: GitHub evicts the oldest past 10 GiB, and evicted caches are misses`],
      weekly: { runnerMinutes: 0, wallMinutes: 0 },
      confidence: 'measured',
      mechanical: false,
      change: { kind: 'cache-scope' },
      suggestion: 'Save caches only from the default branch (actions/cache/restore on pull requests) and drop per-commit keys.',
    });
  }
  return out;
}

/** Matrix legs that always agree run the same tests twice. */
function matrixLegs({ data, scale, workflows, jobs, jobRows }) {
  const out = [];
  for (const [path, workflow] of Object.entries(workflows)) {
    if (!workflow) continue;
    for (const job of workflow.jobs.filter((j) => j.matrix)) {
      const legs = jobRows.filter((row) => row.path === path && jobFor(workflow, row.name) === job);
      if (legs.length < 2) continue;
      const runs = groupBy(
        jobs.filter((j) => j.path === path && legs.some((leg) => leg.name === j.name) && DONE.has(j.conclusion)),
        (j) => `${j.runId}\0${j.attempt}`,
      );
      let together = 0;
      let disagree = 0;
      for (const list of runs.values()) {
        if (new Set(list.map((j) => j.name)).size !== legs.length) continue;
        together += 1;
        if (new Set(list.map((j) => j.conclusion)).size > 1) disagree += 1;
      }
      if (together < 10 || disagree > 0) continue;
      const cheapest = [...legs].sort((a, b) => a.minutes - b.minutes);
      const dropped = cheapest[cheapest.length - 1];
      const saved = dropped.minutes;
      out.push({
        kind: 'matrix-agree',
        title: `${legs.length} legs of ${job.id} never disagreed in ${together} runs`,
        path,
        job: job.id,
        evidence: [
          `legs: ${legs.map((leg) => `${leg.name} (p50 ${minutesText(leg.p50)}, ${leg.minutes} min)`).join('; ')}`,
          `in ${together} runs where every leg finished, all legs passed or all failed together`,
          `the costliest leg took ${saved} runner-min in ${data.window.days} days`,
        ],
        weekly: { runnerMinutes: round(saved * scale * 0.5), wallMinutes: 0 },
        confidence: 'estimated',
        mechanical: false,
        change: { kind: 'matrix-leg', path, job: job.id, leg: dropped.name },
        suggestion: `Run ${dropped.name} on a nightly schedule or on the default branch only, and keep the other legs on pull requests.`,
      });
    }
  }
  return out;
}

/** The job every run waits for, made of two or more long steps that could run side by side. */
function splitJobs({ data, scale, workflowRows, jobRows }) {
  const out = [];
  for (const workflow of workflowRows) {
    const rows = jobRows.filter((row) => row.path === workflow.path && row.p50 != null).sort((a, b) => b.p50 - a.p50);
    if (rows.length === 0) continue;
    const [first, second] = rows;
    if (first.p50 < 480 || (second && first.p50 < 2 * second.p50)) continue;
    const long = first.steps.filter((step) => step.share >= 0.25 && !/^(Set up job|Post |Complete job|Run actions\/checkout)/.test(step.name));
    if (long.length < 2) continue;
    const savedWall = Math.min(...long.map((step) => step.p50));
    const weeklyRuns = first.success * scale;
    out.push({
      kind: 'split-job',
      title: `Split ${first.name}: it holds ${long.length} long steps one after another`,
      path: first.path,
      job: first.name,
      evidence: [
        `${first.name} takes ${minutesText(first.p50)} at p50 and sets the pace of ${workflow.name}${second ? ` (next longest job: ${minutesText(second.p50)})` : ''}`,
        ...long.map((step) => `step ${step.name}: ${minutesText(step.p50)} p50, ${round(step.share * 100)}% of the job`),
      ],
      weekly: { runnerMinutes: 0, wallMinutes: round((savedWall / 60) * weeklyRuns) },
      confidence: 'estimated',
      mechanical: false,
      change: { kind: 'split', path: first.path, job: first.name, steps: long.map((step) => step.name) },
      suggestion: `Run ${long.map((step) => step.name).join(' and ')} as parallel jobs: each run finishes about ${minutesText(savedWall)} sooner, for one more job's setup time.`,
    });
  }
  return out;
}

function queueFindings({ data, scale, runners, jobs }) {
  return runners
    .filter((row) => row.jobs >= 20 && row.queueP95 != null && row.queueP95 > 120)
    .map((row) => {
      const waited = sum(jobs.filter((job) => job.cost.runner === row.runner).map((job) => Math.max(0, (job.queue ?? 0) - 30))) / 60;
      return {
        kind: 'queue',
        title: `Jobs on ${row.runner} wait ${minutesText(row.queueP95)} at p95 for a runner`,
        path: null,
        subject: row.runner,
        evidence: [`${row.jobs} jobs: queue p50 ${minutesText(row.queueP50)}, p95 ${minutesText(row.queueP95)}`, `${round(waited)} minutes of waiting past the first 30 s in ${data.window.days} days`],
        weekly: { runnerMinutes: 0, wallMinutes: round(waited * scale) },
        confidence: 'measured',
        mechanical: false,
        change: { kind: 'capacity', runner: row.runner },
        suggestion: `Add runners (or slots) for ${row.runner} at the hours jobs queue.`,
      };
    });
}

function slowTests({ data, scale, tests, jobRows }) {
  const seen = new Set();
  return tests.slow
    .filter((row) => row.p50Ms >= 10_000 && !row.failedInLogsRead)
    .filter((row) => !seen.has(row.name) && seen.add(row.name))
    .slice(0, 5)
    .map((row) => {
      const job = jobRows.find((r) => r.path === row.path && r.name === row.job);
      const saved = ((row.p50Ms / 1000) * (job?.success ?? 0)) / 60;
      return {
        kind: 'slow-test',
        title: `Slow test, never seen failing: ${row.name.slice(0, 80)}`,
        path: row.path,
        job: row.job,
        subject: row.name,
        evidence: [
          `${row.name} takes ${minutesText(row.p50Ms / 1000)} at p50 (max ${minutesText(row.maxMs / 1000)}) in ${row.samples} logs of ${row.job}`,
          `it failed in none of the ${tests.failureLogsRead} failed-job logs read`,
          row.otherJobs.length ? `the same test also ran on the same commit in: ${row.otherJobs.join(', ')}` : 'it ran in one job per commit',
          'saving estimated at half its time (tests in one process often run side by side)',
        ],
        weekly: { runnerMinutes: round(saved * scale * 0.5), wallMinutes: 0 },
        confidence: 'estimated',
        mechanical: false,
        change: { kind: 'test', path: row.path, job: row.job, test: row.name },
        suggestion: `Find what this test waits on (a fixed sleep or a timeout is common) and shorten it${row.otherJobs.length ? ', or keep it in one of the jobs that run it on each commit' : ', or run it nightly'}.`,
      };
    });
}
