#!/usr/bin/env node
// arca-ci-analyze: the nightly CI analysis of one or more GitHub repositories.
//
//   arca-ci-analyze --repo acme/app [--repo acme/lib] [--days 7] [--max-runs 300] [--budget 600] [--out DIR]
//   arca-ci-analyze --dataset DIR/acme__app/dataset.json [--out DIR]     (analyse a saved dataset again, offline)
//
// Access, read-only, never printed, in this order:
//   --installation-token-file FILE, or GITHUB_INSTALLATION_TOKEN   a GitHub App installation token (ghs_...)
//   GITHUB_APP_ID and GITHUB_APP_KEY_B64 (or GITHUB_APP_KEY)       the App itself: it mints one per repository
//   GITHUB_TOKEN                                                   an installation token; a person's token (gho_ from
//                                                                  `gh auth token`, ghp_, github_pat_) only with
//                                                                  --allow-user-token, since its rate limit is shared
//                                                                  with everything that person runs
// Requests: before the first one the run prints its plan (an estimate of the requests it makes) to stderr. --budget N
// (600 by default, BUDGET) is the limit: the most GitHub REST requests the run makes in all; the runs and logs read shrink to fit it, and
// past it the collector stops and keeps what it read. --max-runs N (300, MAX_RUNS): per repository, only the newest N
// completed runs of the window are read.
// REPOS (comma-separated), DAYS and MOVE_TO stand for --repo, --days and --move-to, as an Arca function's plain
// settings. --move-to LABEL names the owner's own runner pool: jobs on a provider's or GitHub's larger billed Linux
// runners are proposed to move there (not yet those on GitHub's standard runners, even in a private repository).
//
// With --out, each repository gets DIR/<owner>__<name>/{dataset.json,report.json,report.md}. Without it (an Arca
// function run), the Markdown report of each repository is printed, then one JSON result line, the run's result.

import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { analyze, checkLabel } from './analyze.js';
import { collect, fitBudget, requestPlan } from './collect.js';
import { access, GitHub } from './github.js';
import { markdown, resultLine } from './report.js';

/** What a function run may print: Arca keeps the last 64 KiB of a run's output. */
const PRINT_BUDGET = 56 * 1024;

function parseArgs(argv) {
  const args = { repos: [], days: null, out: null, datasets: [] };
  for (let k = 0; k < argv.length; k += 1) {
    const flag = argv[k];
    const value = () => {
      k += 1;
      if (k >= argv.length) throw new Error(`${flag} needs a value`);
      return argv[k];
    };
    if (flag === '--repo') args.repos.push(value());
    else if (flag === '--days') args.days = Number(value());
    else if (flag === '--move-to') args.moveTo = value();
    else if (flag === '--out') args.out = value();
    else if (flag === '--dataset') args.datasets.push(value());
    else if (flag === '--budget') args.budget = Number(value());
    else if (flag === '--max-runs') args.maxRuns = Number(value());
    else if (flag === '--installation-token-file') args.tokenFile = value();
    else if (flag === '--allow-user-token') args.allowUserToken = true;
    else if (flag === '--help' || flag === '-h') args.help = true;
    else throw new Error(`unknown argument ${flag}`);
  }
  if (args.repos.length === 0 && process.env.REPOS) args.repos = process.env.REPOS.split(',').map((r) => r.trim()).filter(Boolean);
  if (args.days == null && process.env.DAYS) args.days = Number(process.env.DAYS);
  if (!args.moveTo && process.env.MOVE_TO) args.moveTo = process.env.MOVE_TO;
  if (args.budget == null) args.budget = process.env.BUDGET ? Number(process.env.BUDGET) : DEFAULT_BUDGET;
  if (args.maxRuns == null && process.env.MAX_RUNS) args.maxRuns = Number(process.env.MAX_RUNS);
  if (!args.allowUserToken && process.env.ALLOW_USER_TOKEN === '1') args.allowUserToken = true;
  for (const [flag, number] of [['--budget', args.budget], ['--max-runs', args.maxRuns ?? 1], ['--days', args.days ?? 1]]) {
    if (!Number.isFinite(number) || number <= 0) throw new Error(`${flag} takes a positive number`);
  }
  checkLabel(args.moveTo ?? null); // before any collecting: a bad label fails the run, not each repository
  return args;
}

/** The most GitHub REST requests a run makes when --budget does not say: about a tenth of a token's hourly limit. */
const DEFAULT_BUDGET = 600;

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help || (args.repos.length === 0 && args.datasets.length === 0)) {
    process.stdout.write('usage: arca-ci-analyze --repo owner/name [--repo ...] [--days 7] [--max-runs 300] [--budget 600] [--installation-token-file FILE] [--allow-user-token] [--move-to LABEL] [--out DIR] | --dataset FILE\n');
    process.stdout.write(`${JSON.stringify({ ok: false, errors: ['no repository named'] })}\n`);
    return;
  }
  const reports = [];
  const errors = [];
  const finish = (ok) => {
    if (!args.out) {
      const budget = Math.floor(PRINT_BUDGET / Math.max(1, reports.length));
      for (const report of reports) process.stdout.write(fit(report, budget));
    }
    process.stdout.write(`${resultLine(reports, { ok, errors })}\n`);
  };
  process.on('SIGTERM', () => {
    errors.push('stopped at its time limit');
    finish(false);
    process.exit(0);
  });

  for (const path of args.datasets) {
    const data = JSON.parse(await readFile(path, 'utf8'));
    reports.push(await save(args.out, data, analyze(data, { moveTo: args.moveTo })));
  }
  if (args.repos.length) {
    // Before any request: who signs in, and the most requests the run can make, fitted to the budget.
    const how = await access(args);
    const options = fitBudget(args.repos.length, args.budget, {
      days: args.days ?? undefined,
      maxRuns: args.maxRuns ?? undefined,
      app: Boolean(how.appId),
    });
    // An estimate (retries, jobs lists over 100 entries and runs still in progress cost more); the budget is the limit.
    const about = requestPlan(options) * args.repos.length;
    process.stderr.write(
      `plan: ${args.repos.length} repositories, ${options.days} days, the newest ${options.maxRuns} runs and ` +
        `${options.flakeLogs + options.failureLogs + options.sampleLogs} logs each: about ${about} GitHub requests, ` +
        `never more than the budget of ${args.budget}; signed in with ${how.how}\n`,
    );
    // The repositories are read side by side, through one client that keeps a few requests in flight in all and
    // stops at the budget: a function run has 15 minutes for every one of them.
    const gh = new GitHub({
      token: how.token,
      appId: how.appId,
      appKey: how.appKey,
      budget: args.budget,
      log: (line) => process.stderr.write(`${line}\n`),
    });
    const done = await Promise.all(
      args.repos.map(async (repo) => {
        try {
          const data = await collect(gh, repo, {
            ...options,
            log: (line) => process.stderr.write(`${line}\n`),
          });
          return await save(args.out, data, analyze(data, { moveTo: args.moveTo }));
        } catch (error) {
          errors.push(`${repo}: ${error.message}`);
          return null;
        }
      }),
    );
    reports.push(...done.filter(Boolean));
  }
  finish(errors.length === 0);
}

async function save(out, data, report) {
  if (!out) return report;
  const dir = join(out, data.repo.replace('/', '__'));
  await mkdir(dir, { recursive: true });
  await writeFile(join(dir, 'dataset.json'), JSON.stringify(data));
  await writeFile(join(dir, 'report.json'), JSON.stringify(report, null, 2));
  await writeFile(join(dir, 'report.md'), markdown(report));
  process.stderr.write(`${data.repo}: ${join(dir, 'report.md')}\n`);
  return report;
}

/** The Markdown report, made shorter until it fits the budget. */
function fit(report, budget) {
  for (const [maxJobs, maxFindings] of [[25, 15], [15, 10], [10, 6], [5, 3]]) {
    const text = markdown(report, { maxJobs, maxFindings });
    if (Buffer.byteLength(text) <= budget) return text;
  }
  return markdown(report, { maxJobs: 3, maxFindings: 3 }).slice(0, budget);
}

main().catch((error) => {
  process.stdout.write(`${JSON.stringify({ ok: false, errors: [String(error.message ?? error)] })}\n`);
  process.exitCode = 1;
});
