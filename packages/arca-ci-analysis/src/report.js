// The report a person reads (Markdown), and the one-line result an Arca function run ends with.

import { PRICES_AS_OF } from './cost.js';

const n = (value, digits = 0) => (value == null ? '-' : Number(value).toFixed(digits));
const usd = (value) => (value == null ? '-' : `$${Number(value).toFixed(2)}`);
const dur = (s) => (s == null ? '-' : s >= 120 ? `${(s / 60).toFixed(1)}m` : `${Math.round(s)}s`);
const cell = (text) => String(text ?? '').replace(/\|/g, '\\|').replace(/\n/g, ' ');

export function markdown(report, { maxJobs = 25, maxFindings = 15 } = {}) {
  const t = report.totals;
  const out = [];
  out.push(`# CI report: ${report.repo}`);
  out.push('');
  out.push(
    `Window: ${report.window.since} to ${report.window.until} (${report.window.days} days). Generated ${report.generatedAt}. ` +
      `${t.runs} runs, ${t.jobs} jobs, ${t.logsRead} job logs read. Repository is ${report.visibility}.`,
  );
  out.push('');
  out.push('## Summary');
  out.push('');
  out.push(`- Runner time: **${t.minutes} min** billed-rounded (${n(t.minutesPerWeek)} min a week).`);
  out.push(
    `- Cost at list price: **${usd(t.listUsd)}**; billed after the free minutes of a ${report.visibility} repository: **${usd(t.billedUsd)}** ` +
      `(prices as of ${PRICES_AS_OF}, an estimate; self-hosted minutes priced at 0).`,
  );
  out.push(`- Queue time (job created to started): p50 ${dur(t.queueP50)}, p95 ${dur(t.queueP95)}.`);
  out.push(
    `- Flaky: ${report.flaky.events} fail-then-pass events on the same commit, in ${report.flaky.jobs.length} jobs.`,
  );
  const c = report.cache;
  out.push(
    `- Cache restores in the logs read: ${c.restores} (${c.hits} exact hits, ${c.partial} partial, ${c.misses} misses; hit rate ${
      c.hitRate == null ? '-' : `${Math.round(c.hitRate * 100)}%`
    })` + (c.activeBytes != null ? `; ${(c.activeBytes / 1024 ** 3).toFixed(2)} GiB in ${c.activeCount} caches.` : '.'),
  );
  out.push('');

  out.push('## Findings, ranked');
  out.push('');
  if (report.findings.length === 0) out.push('No finding this window.');
  for (const f of report.findings.slice(0, maxFindings)) {
    const w = f.weekly ?? {};
    out.push(`### ${f.rank}. ${f.title}`);
    out.push('');
    out.push(
      `\`${f.id}\` · ${f.kind} · ${f.confidence} · saves about ${n(w.runnerMinutes)} runner-min, ${n(w.wallMinutes)} waiting-min and ${usd(w.usd)} billed a week` +
        (f.mechanical ? ' · a mechanical change the PR step can write' : ''),
    );
    out.push('');
    if (f.path) out.push(`File: \`${f.path}\`${f.job ? `, job \`${f.job}\`` : ''}`);
    out.push('');
    for (const line of f.evidence) out.push(`- ${line}`);
    out.push('');
    out.push(`**Change:** ${f.suggestion}`);
    out.push('');
  }

  out.push('## Workflows');
  out.push('');
  out.push('| workflow | runs | ok | failed | cancelled | wall p50 | wall p95 | runner min | list $ | billed $ |');
  out.push('|---|---|---|---|---|---|---|---|---|---|');
  for (const w of report.workflows) {
    out.push(
      `| ${cell(w.name)} (\`${cell(w.path)}\`) | ${w.runs} | ${w.success} | ${w.failure} | ${w.cancelled} | ${dur(w.wallP50)} | ${dur(w.wallP95)} | ${w.minutes} | ${usd(w.listUsd)} | ${usd(w.billedUsd)} |`,
    );
  }
  out.push('');

  out.push(`## Jobs (the ${maxJobs} costliest)`);
  out.push('');
  out.push('| job | workflow | runner | runs | failed | p50 | p95 | max | queue p50 | queue p95 | min | list $ |');
  out.push('|---|---|---|---|---|---|---|---|---|---|---|---|');
  for (const j of report.jobs.slice(0, maxJobs)) {
    out.push(
      `| ${cell(j.name)} | ${cell(j.workflow)} | ${cell(j.runner)} | ${j.runs} | ${j.failure} | ${dur(j.p50)} | ${dur(j.p95)} | ${dur(j.max)} | ${dur(j.queueP50)} | ${dur(j.queueP95)} | ${j.minutes} | ${usd(j.listUsd)} |`,
    );
  }
  out.push('');

  out.push('## Slowest steps (p50, in the 10 costliest jobs)');
  out.push('');
  out.push('| job | step | runs | p50 | p95 | share of job |');
  out.push('|---|---|---|---|---|---|');
  for (const j of report.jobs.slice(0, 10)) {
    for (const s of j.steps.slice(0, 3)) {
      out.push(`| ${cell(j.name)} | ${cell(s.name)} | ${s.runs} | ${dur(s.p50)} | ${dur(s.p95)} | ${s.share == null ? '-' : `${Math.round(s.share * 100)}%`} |`);
    }
  }
  out.push('');

  out.push('## Runners');
  out.push('');
  out.push('| runner | kind | jobs | min | queue p50 | queue p95 | list $ | billed $ |');
  out.push('|---|---|---|---|---|---|---|---|');
  for (const r of report.runners) {
    out.push(`| ${cell(r.runner)} | ${r.kind} | ${r.jobs} | ${r.minutes} | ${dur(r.queueP50)} | ${dur(r.queueP95)} | ${usd(r.listUsd)} | ${usd(r.billedUsd)} |`);
  }
  out.push('');

  if (report.flaky.jobs.length) {
    out.push('## Flaky jobs (failed, then passed on the same commit)');
    out.push('');
    out.push('| job | workflow file | events | commits | wasted min | tests seen failing |');
    out.push('|---|---|---|---|---|---|');
    for (const f of report.flaky.jobs.slice(0, 15)) {
      out.push(`| ${cell(f.name)} | \`${cell(f.path)}\` | ${f.flakes} | ${f.commits} | ${f.wastedMinutes} | ${cell(f.tests.slice(0, 3).join('; ') || '-')} |`);
    }
    out.push('');
  }

  if (report.tests.slow.length) {
    out.push('## Slowest tests (from the logs read)');
    out.push('');
    out.push('| test | job | samples | p50 | max | failed in logs read | jobs on one commit |');
    out.push('|---|---|---|---|---|---|---|');
    for (const t2 of report.tests.slow.slice(0, 15)) {
      out.push(
        `| ${cell(t2.name.slice(0, 100))} | ${cell(t2.job)} | ${t2.samples} | ${dur(t2.p50Ms / 1000)} | ${dur(t2.maxMs / 1000)} | ${t2.failedInLogsRead ? 'yes' : 'no'} | ${t2.sameCommitJobs} |`,
      );
    }
    out.push('');
  }

  if (report.cache.steps.length) {
    out.push('## Cache restores (from the logs read)');
    out.push('');
    out.push('| job | step | restores | exact | partial | miss | hit rate | gzip | zstd |');
    out.push('|---|---|---|---|---|---|---|---|---|');
    for (const s of report.cache.steps.slice(0, 15)) {
      out.push(
        `| ${cell(s.job)} | ${cell((s.step ?? '').split('@')[0])} | ${s.restores} | ${s.hits} | ${s.partial} | ${s.misses} | ${Math.round(s.hitRate * 100)}% | ${s.gzip} | ${s.zstd} |`,
      );
    }
    out.push('');
  }

  out.push('## How this was measured');
  out.push('');
  out.push(
    '- Durations are job and step start-to-finish from the GitHub Actions API, for jobs that succeeded or failed. p50/p95 are nearest-rank.',
  );
  out.push('- Queue is a job\'s created-to-started time. Runner minutes are rounded up per job, as GitHub bills them.');
  out.push(
    '- Flaky means one job failed and the same job passed later on the same commit (a re-run, or another run of the same workflow).',
  );
  out.push(
    '- Cache hits and test times come only from the job logs read: the failed attempts of flaky jobs, the newest other failures, and the newest successful runs of the costliest jobs.',
  );
  out.push(
    '- Savings marked "estimated" use a stated rule (e.g. 40% of cargo steps for a Rust cache); "measured" ones are minutes that were spent in the window.',
  );
  return out.join('\n') + '\n';
}

/** The run's result line: one JSON object under 4096 bytes, as an Arca function must end. */
export function resultLine(reports, { ok = true, errors = [] } = {}) {
  const repos = reports.map((report) => ({
    repo: report.repo,
    runs: report.totals.runs,
    min: report.totals.minutes,
    usd: report.totals.billedUsd,
    flaky: report.flaky.events,
    hit: report.cache.hitRate,
    top: report.findings.slice(0, 3).map((f) => ({ id: f.id, min: Math.round((f.weekly?.runnerMinutes ?? 0) + (f.weekly?.wallMinutes ?? 0)) })),
  }));
  let line = JSON.stringify({ ok, repos, errors: errors.slice(0, 5) });
  while (Buffer.byteLength(line) > 4000 && repos.some((repo) => repo.top.length)) {
    for (const repo of repos) repo.top.pop();
    line = JSON.stringify({ ok, repos, errors: errors.slice(0, 5) });
  }
  return line;
}
