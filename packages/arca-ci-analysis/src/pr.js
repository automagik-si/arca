// The PR step: one finding in, one small change to one workflow file out, with the pull request's text carrying the
// measured evidence. The change is written into the file as text, at the lines the YAML reader found, so the rest of
// the file (comments, order, quoting) is left as it was. Pure: the caller does git and GitHub (pr-cli.js).

import { LINES, parse } from './yaml.js';
import { readWorkflow } from './workflow.js';

/** The change kinds this step can write. Any other finding is for a person (or an agent) to change by hand. */
export const WRITABLE = new Set(['add-timeout', 'add-concurrency', 'restrict-push', 'add-step', 'add-input', 'set-runner']);

/**
 * The finding to turn into a pull request: `id` if given, else the best-ranked one whose change this step can write.
 * Also the better-ranked findings it passed over, so the PR text can say what it did not do.
 */
export function pickFinding(report, id = null) {
  const findings = report.findings ?? [];
  if (id) {
    const found = findings.find((f) => f.id === id);
    if (!found) throw new Error(`no finding ${id} in the report of ${report.repo}`);
    return { finding: found, skipped: [] };
  }
  const index = findings.findIndex((f) => f.mechanical && WRITABLE.has(f.change?.kind));
  if (index < 0) return { finding: null, skipped: findings.slice(0, 5) };
  return { finding: findings[index], skipped: findings.slice(0, index) };
}

const indentOf = (line) => line.length - line.trimStart().length;

/** The last line (0-based) of the block that starts at `start`: every following line more indented, or blank. */
function blockEnd(lines, start) {
  const base = indentOf(lines[start]);
  let end = start;
  for (let k = start + 1; k < lines.length; k += 1) {
    if (lines[k].trim() === '' || lines[k].trimStart().startsWith('#')) continue;
    if (indentOf(lines[k]) <= base) break;
    end = k;
  }
  return end;
}

/** The indentation one level in, as the file writes it (2 unless the file shows otherwise). */
function step(lines, from) {
  const base = indentOf(lines[from]);
  for (let k = from + 1; k < lines.length; k += 1) {
    if (lines[k].trim() === '' || lines[k].trimStart().startsWith('#')) continue;
    const here = indentOf(lines[k]);
    return here > base ? here - base : 2;
  }
  return 2;
}

/** The new text of the workflow file with `change` written in. Throws when the file is not shaped as expected. */
export function applyChange(text, change) {
  const lines = text.split('\n');
  const doc = parse(text);
  if (!doc || !doc.jobs) throw new Error('not a workflow file: no jobs');
  switch (change.kind) {
    case 'add-timeout':
      return addTimeouts(lines, doc, change).join('\n');
    case 'add-concurrency':
      return addConcurrency(lines, doc, change).join('\n');
    case 'restrict-push':
      return restrictPush(lines, doc, change).join('\n');
    case 'add-step':
      return addStep(lines, doc, change).join('\n');
    case 'add-input':
      return addInput(lines, doc, change).join('\n');
    case 'set-runner':
      return setRunner(lines, doc, change).join('\n');
    default:
      throw new Error(`this step does not write a ${change.kind} change`);
  }
}

function jobLine(doc, id) {
  const line = doc.jobs[LINES]?.[id];
  if (!line) throw new Error(`no job ${id}`);
  return line - 1;
}

function addTimeouts(lines, doc, change) {
  // From the bottom up, so the line numbers of the jobs above stay right.
  const jobs = [...change.jobs].sort((a, b) => jobLine(doc, b.id) - jobLine(doc, a.id));
  for (const { id, minutes } of jobs) {
    const job = doc.jobs[id];
    if (job['timeout-minutes'] != null) continue;
    const start = jobLine(doc, id);
    const pad = ' '.repeat(indentOf(lines[start]) + step(lines, start));
    const runsOn = job[LINES]?.['runs-on'];
    const at = runsOn ? blockEnd(lines, runsOn - 1) + 1 : start + 1;
    lines.splice(at, 0, `${pad}timeout-minutes: ${minutes}`);
  }
  return lines;
}

function addConcurrency(lines, doc, change) {
  if (doc.concurrency) throw new Error('the workflow has a concurrency group already');
  const jobs = doc[LINES].jobs - 1;
  const pad = ' '.repeat(step(lines, jobs));
  const cancel = change.cancel === true ? 'true' : change.cancel;
  const block = [
    '# A newer push to the same pull request or branch cancels the run it replaces.',
    'concurrency:',
    `${pad}group: ${change.group}`,
    `${pad}cancel-in-progress: ${cancel}`,
    '',
  ];
  lines.splice(jobs, 0, ...block);
  return lines;
}

function restrictPush(lines, doc, change) {
  const onKey = doc[LINES].on ? 'on' : 'true';
  const on = doc[onKey];
  const onLine = doc[LINES][onKey] - 1;
  const list = `[${change.branches.join(', ')}]`;
  if (Array.isArray(on) || typeof on === 'string') {
    // `on: [push, pull_request]` becomes a block, with push limited.
    const events = Array.isArray(on) ? on : [on];
    const pad = '  ';
    const block = [`on:`];
    for (const event of events) {
      block.push(event === 'push' ? `${pad}push:` : `${pad}${event}:`);
      if (event === 'push') block.push(`${pad}${pad}branches: ${list}`);
    }
    lines.splice(onLine, 1, ...block);
    return lines;
  }
  const pushLine = on?.[LINES]?.push;
  if (!pushLine) throw new Error('the workflow has no push trigger');
  const push = on.push;
  if (push && push.branches) throw new Error('push is limited to branches already');
  const at = pushLine - 1;
  const pad = ' '.repeat(indentOf(lines[at]) + step(lines, onLine));
  // `push:` with no value (or `push: {}`) gets the branches under it; a push with other keys keeps them.
  lines[at] = lines[at].replace(/push:\s*(\{\s*\})?\s*(#.*)?$/, (m, _empty, comment) => `push:${comment ? ` ${comment}` : ''}`);
  lines.splice(at + 1, 0, `${pad}branches: ${list}`);
  return lines;
}

function stepLines(doc, id) {
  const steps = doc.jobs[id]?.steps;
  if (!Array.isArray(steps)) throw new Error(`job ${id} has no steps`);
  return steps[LINES].map((line) => line - 1);
}

function addStep(lines, doc, change) {
  const starts = stepLines(doc, change.job);
  const after = starts[change.after];
  if (after == null) throw new Error(`job ${change.job} has no step ${change.after}`);
  // The item's dash sits at the step's own indentation; its block ends where the next item (or the job) starts.
  const next = starts[change.after + 1];
  let end = next != null ? next - 1 : blockEnd(lines, after);
  while (end > after && (lines[end].trim() === '' || lines[end].trimStart().startsWith('#'))) end -= 1;
  const pad = ' '.repeat(indentOf(lines[after]));
  const ref = change.pin ? `${change.action}@${change.pin.sha} # ${change.pin.tag}` : `${change.action}@${change.tag}`;
  lines.splice(end + 1, 0, `${pad}- uses: ${ref}`);
  return lines;
}

function addInput(lines, doc, change) {
  const starts = stepLines(doc, change.job);
  const at = starts[change.step];
  if (at == null) throw new Error(`job ${change.job} has no step ${change.step}`);
  const stepDoc = doc.jobs[change.job].steps[change.step];
  // The keys of a "- key:" item start two columns after its dash.
  const keyPad = ' '.repeat(indentOf(lines[at]) + 2);
  if (stepDoc.with && typeof stepDoc.with === 'object') {
    if (stepDoc.with[change.input] != null) throw new Error(`${change.input} is set already`);
    const withLine = stepDoc[LINES].with - 1;
    const pad = ' '.repeat(indentOf(lines[withLine]) + step(lines, withLine));
    lines.splice(withLine + 1, 0, `${pad}${change.input}: ${change.value}`);
    return lines;
  }
  const usesLine = stepDoc[LINES].uses - 1;
  lines.splice(usesLine + 1, 0, `${keyPad}with:`, `${keyPad}  ${change.input}: ${change.value}`);
  return lines;
}

function setRunner(lines, doc, change) {
  for (const { id, from, to } of change.jobs) {
    const job = doc.jobs[id];
    if (job?.['runs-on'] !== from) throw new Error(`job ${id} does not run on ${from}`);
    const at = job[LINES]['runs-on'] - 1;
    // An expression holds single quotes: inside single quotes it would end the string, so it goes in double ones.
    const around = (quote) => (quote === "'" && to.includes("'") ? '"' : quote);
    const replaced = lines[at].replace(/(runs-on:\s*)(["']?)([^"'#\s]+)\2/, (m, head, quote, value) =>
      value === from ? `${head}${around(quote)}${to}${around(quote)}` : m,
    );
    if (replaced === lines[at]) throw new Error(`the runs-on line of ${id} is not one plain value`);
    lines[at] = replaced;
  }
  return lines;
}

/** Reads the changed file back and says whether it holds the change and nothing else that the reader can see. */
export function verifyChange(before, after, change) {
  const old = readWorkflow('before', before);
  const now = readWorkflow('after', after);
  if (!now) return 'the changed file does not parse';
  const docNow = parse(after);
  switch (change.kind) {
    case 'add-timeout':
      for (const { id, minutes } of change.jobs) {
        if (docNow.jobs[id]?.['timeout-minutes'] !== minutes) return `job ${id} has no timeout-minutes: ${minutes}`;
      }
      break;
    case 'add-concurrency':
      if (!docNow.concurrency?.group) return 'no concurrency group';
      break;
    case 'restrict-push':
      if (JSON.stringify(docNow.on?.push?.branches) !== JSON.stringify(change.branches)) return 'push is not limited';
      break;
    case 'add-step':
      if ((docNow.jobs[change.job]?.steps ?? []).length !== (parse(before).jobs[change.job]?.steps ?? []).length + 1) {
        return 'the step was not added';
      }
      break;
    case 'add-input': {
      const value = docNow.jobs[change.job]?.steps?.[change.step]?.with?.[change.input];
      if (String(value) !== String(change.value)) return `${change.input} is not ${change.value}`;
      break;
    }
    case 'set-runner':
      for (const { id, to } of change.jobs) {
        if (docNow.jobs[id]?.['runs-on'] !== to) return `job ${id} does not run on ${to}`;
      }
      break;
    default:
      return `unknown change ${change.kind}`;
  }
  if (old.jobs.length !== now.jobs.length) return 'the number of jobs changed';
  return null;
}

/** The pull request's title and body: what changes, the measured evidence, and how to check it. */
export function prText(report, finding, { skipped = [], diffStat = '' } = {}) {
  const title = `ci: ${finding.title.charAt(0).toLowerCase()}${finding.title.slice(1)}`.slice(0, 100);
  const w = finding.weekly ?? {};
  const body = [
    `## What`,
    '',
    finding.suggestion,
    '',
    `## Why: measured on this repository`,
    '',
    `From the GitHub Actions history of \`${report.repo}\`, ${report.window.since} to ${report.window.until} (${report.window.days} days, ${report.totals.runs} runs, ${report.totals.jobs} jobs):`,
    '',
    ...finding.evidence.map((line) => `- ${line}`),
    '',
    `Expected effect a week: ${effect(w)} (${finding.confidence}).`,
    '',
    `## How to check`,
    '',
    checkText(finding.change),
    '',
    `## Scope`,
    '',
    `One change to \`${finding.change.path}\`${diffStat ? ` (${diffStat.trim()})` : ''}. Nothing else is touched.`,
    ...(skipped.length
      ? [
          '',
          'Findings ranked above this one need a person\'s judgement, so they are in the report and not in this pull request:',
          ...skipped.map((f) => `- ${f.title}`),
        ]
      : []),
    '',
    '---',
    `Opened by the Arca CI analysis (finding \`${finding.id}\`). It has read access to Actions and contents only, never merges, and opens one small pull request per finding.`,
  ];
  return { title, body: body.join('\n') + '\n' };
}

function effect(w) {
  const parts = [];
  if (w.usd) parts.push(`$${Number(w.usd).toFixed(2)} less billed`);
  if (w.runnerMinutes) parts.push(`${Math.round(w.runnerMinutes)} fewer runner-minutes`);
  if (w.wallMinutes) parts.push(`${Math.round(w.wallMinutes)} fewer minutes waiting for CI`);
  return parts.length ? parts.join(', ') : 'no measured saving: this change bounds what a failure can cost';
}

function checkText(change) {
  switch (change.kind) {
    case 'add-timeout':
      return `The jobs run as before; a hung run now stops at ${change.jobs.map((job) => `${job.minutes} min (${job.id})`).join(', ')} instead of 6 hours. Each limit is at least three times the job's measured p95.`;
    case 'add-concurrency':
      return 'Push twice to a pull request in quick succession: the first run shows as cancelled, the second runs to the end.';
    case 'restrict-push':
      return `A push to a feature branch with an open pull request starts one run (pull_request), not two. Pushes to ${change.branches.join(', ')} run as before.`;
    case 'add-step':
      return `The second run of the job after this change shows "${change.action}" restoring a cache, and its build steps take less time than the p50 above.`;
    case 'add-input':
      return `From the second run on, ${change.input}: ${change.value} restores the package cache ("Cache restored from key" in the log) and the install step is shorter.`;
    case 'set-runner':
      if (change.public) {
        return [
          `The moved jobs run on ${change.pool} and pass as before; their time stays within the p95 above, since each is short. Nothing else changes runner.`,
          '',
          `**This repository is public. Read this before merging.** The jobs reach ${change.pool} only if the pool's runner group admits this public repository, and Arca's pool warns when it does: a job on the pool can reach the pool's GitHub credential and the other slots ("Who can reach what" in Arca's docs/OPERATING.md).`,
          change.forks
            ? `The runs-on expression sends ordinary pull requests from forks to ${change.forks}; open one from a fork to see it there. It is not a wall: a fork pull request runs its own copy of the workflow and can name ${change.pool} itself.`
            : `Nothing in this workflow steers pull requests from forks, and any fork pull request can add a workflow of its own that names ${change.pool}.`,
          'If you still want this, choose "Require approval for all external contributors" (Settings, Actions, General), and read every workflow change before you approve a run.',
        ].join('\n');
      }
      return `The moved jobs run on ${[...new Set(change.jobs.map((job) => job.to))].join(', ')} and pass as before; their time stays within the p95 above, since each is short. Nothing else changes runner.`;
    default:
      return '';
  }
}
