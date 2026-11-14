#!/usr/bin/env node
// arca-ci-analyze: the nightly CI analysis of one or more GitHub repositories.
//
//   arca-ci-analyze --repo acme/app [--repo acme/lib] [--days 7] [--out DIR]
//   arca-ci-analyze --dataset DIR/acme__app/dataset.json [--out DIR]     (analyse a saved dataset again, offline)
//
// Access, read-only, from the environment (never printed):
//   GITHUB_TOKEN                               a token that can read the repositories, or
//   GITHUB_APP_ID and GITHUB_APP_KEY_B64       a GitHub App installed on them (the PEM key, base64 on one line), or
//   GITHUB_APP_ID and GITHUB_APP_KEY           the same with the PEM key itself
// REPOS (comma-separated), DAYS and MOVE_TO stand for --repo, --days and --move-to, as an Arca function's plain
// settings. --move-to LABEL names the owner's own runner pool: short jobs on billed runners are proposed to move there.
//
// With --out, each repository gets DIR/<owner>__<name>/{dataset.json,report.json,report.md}. Without it (an Arca
// function run), the Markdown report of each repository is printed, then one JSON result line, the run's result.

import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { analyze, checkLabel } from './analyze.js';
import { collect } from './collect.js';
import { GitHub } from './github.js';
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
    else if (flag === '--help' || flag === '-h') args.help = true;
    else throw new Error(`unknown argument ${flag}`);
  }
  if (args.repos.length === 0 && process.env.REPOS) args.repos = process.env.REPOS.split(',').map((r) => r.trim()).filter(Boolean);
  if (args.days == null && process.env.DAYS) args.days = Number(process.env.DAYS);
  if (!args.moveTo && process.env.MOVE_TO) args.moveTo = process.env.MOVE_TO;
  checkLabel(args.moveTo ?? null); // before any collecting: a bad label fails the run, not each repository
  return args;
}

function client() {
  const appKey = process.env.GITHUB_APP_KEY_B64
    ? Buffer.from(process.env.GITHUB_APP_KEY_B64, 'base64').toString('utf8')
    : process.env.GITHUB_APP_KEY;
  return new GitHub({
    token: process.env.GITHUB_TOKEN || undefined,
    appId: process.env.GITHUB_APP_ID || undefined,
    appKey: appKey || undefined,
    log: (line) => process.stderr.write(`${line}\n`),
  });
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help || (args.repos.length === 0 && args.datasets.length === 0)) {
    process.stdout.write('usage: arca-ci-analyze --repo owner/name [--repo ...] [--days 7] [--out DIR] | --dataset FILE\n');
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
    // The repositories are read side by side, through one client that keeps a few requests in flight in all: a
    // function run has 15 minutes for every one of them.
    const gh = client();
    const done = await Promise.all(
      args.repos.map(async (repo) => {
        try {
          const data = await collect(gh, repo, {
            days: args.days ?? undefined,
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
