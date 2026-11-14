// What the analysis reads in a workflow file: its triggers, its concurrency, and for each job its timeout, its steps and
// the API names its runs carry. Every lookup is by the parsed YAML of the file at the default branch.

import { LINES, parse } from './yaml.js';

export function readWorkflow(path, text) {
  let doc;
  try {
    doc = parse(text);
  } catch {
    return null;
  }
  if (!doc || typeof doc !== 'object') return null;
  const on = normaliseOn(doc.on ?? doc.true);
  const jobs = [];
  for (const [id, job] of Object.entries(doc.jobs ?? {})) {
    if (!job || typeof job !== 'object') continue;
    const steps = Array.isArray(job.steps) ? job.steps.filter((step) => step && typeof step === 'object') : [];
    jobs.push({
      id,
      line: doc.jobs[LINES]?.[id] ?? null,
      name: typeof job.name === 'string' ? job.name : null,
      matrix: Boolean(job.strategy?.matrix),
      timeout: job['timeout-minutes'] ?? null,
      concurrency: job.concurrency ?? null,
      uses: typeof job.uses === 'string' ? job.uses : null,
      runsOn: job['runs-on'] ?? null,
      steps: steps.map((step, index) => ({
        index,
        line: job.steps[LINES]?.[job.steps.indexOf(step)] ?? null,
        name: step.name ?? null,
        uses: typeof step.uses === 'string' ? step.uses : null,
        run: typeof step.run === 'string' ? step.run : null,
        with: step.with && typeof step.with === 'object' ? step.with : {},
      })),
      matcher: matcherFor(id, job),
    });
  }
  return {
    path,
    name: doc.name ?? null,
    on,
    concurrency: doc.concurrency ?? null,
    jobsLine: doc[LINES]?.jobs ?? null,
    onLine: doc[LINES]?.on ?? doc[LINES]?.true ?? null,
    jobs,
  };
}

function normaliseOn(on) {
  if (typeof on === 'string') return { [on]: {} };
  if (Array.isArray(on)) return Object.fromEntries(on.map((event) => [event, {}]));
  if (on && typeof on === 'object') {
    return Object.fromEntries(Object.entries(on).map(([event, value]) => [event, value ?? {}]));
  }
  return {};
}

/** A test for the names GitHub gives this job's runs: its `name:` with each expression as a wildcard, or its id. */
function matcherFor(id, job) {
  if (typeof job.name === 'string') {
    const pattern = job.name
      .split(/\$\{\{.*?\}\}/)
      .map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
      .join('.*?');
    const re = new RegExp(`^(?:.* / )?${pattern}$`);
    return (name) => re.test(name);
  }
  return (name) => name === id || name.startsWith(`${id} (`) || name.endsWith(` / ${id}`);
}

/** The workflow job a run's job name belongs to, or null. */
export function jobFor(workflow, name) {
  if (!workflow) return null;
  const exact = workflow.jobs.find((job) => job.name === name || (!job.name && job.id === name));
  return exact ?? workflow.jobs.find((job) => job.matcher(name)) ?? null;
}

/** Whether runs of this workflow on `event` are cancelled when a newer one starts for the same ref. */
export function cancelsInProgress(workflow, job) {
  const value = (job?.concurrency ?? workflow?.concurrency)?.['cancel-in-progress'];
  if (value === true) return true;
  if (typeof value === 'string' && value.includes('${{')) return 'expression';
  return false;
}

/** The step ids that hold a cache for a toolchain: actions/cache and friends, or a setup action's `cache` input. */
export function cachingSteps(job) {
  return job.steps.filter((step) => {
    const uses = (step.uses ?? '').toLowerCase();
    return (
      /(^|\/)(cache|rust-cache|sccache-action|cache-apt-pkgs-action|caching-for-turbo)(@|$)/.test(uses.split('@')[0] + '@') ||
      /rust-cache|sccache|\/cache@|cache\/restore@|setup-bun/.test(uses) ||
      (/^actions\/setup-(node|python|go|java|dotnet)@/.test(uses) && step.with?.cache)
    );
  });
}
