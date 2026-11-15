#!/usr/bin/env node
// arca-ci-pr: one finding of a report into one small change on a branch, with the pull request's text.
//
//   arca-ci-pr --report DIR/acme__app/report.json --workdir WORK [--finding ID] [--out DIR] [--open]
//
// Without --open (the default) nothing leaves this machine: the repository is cloned read-only into WORK, the
// change is committed on a local branch arca-ci/<finding>, and DIR gets <finding>.patch and <finding>.pr.md to show a
// person first. With --open the branch is pushed to a FORK of the repository (`gh repo fork`) and a DRAFT pull
// request is opened from it; the step never merges and never pushes to the repository itself.

import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { GitHub } from './github.js';
import { applyChange, pickFinding, prText, verifyChange } from './pr.js';

function parseArgs(argv) {
  const args = { open: false };
  for (let k = 0; k < argv.length; k += 1) {
    const flag = argv[k];
    const value = () => {
      k += 1;
      if (k >= argv.length) throw new Error(`${flag} needs a value`);
      return argv[k];
    };
    if (flag === '--report') args.report = value();
    else if (flag === '--workdir') args.workdir = value();
    else if (flag === '--finding') args.finding = value();
    else if (flag === '--out') args.out = value();
    else if (flag === '--open') args.open = true;
    else throw new Error(`unknown argument ${flag}`);
  }
  if (!args.report || !args.workdir) throw new Error('usage: arca-ci-pr --report report.json --workdir DIR [--finding ID] [--out DIR] [--open]');
  return args;
}

const git = (cwd, ...args) => execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

/** The newest release of an action, as a commit sha and its tag, so the new step is pinned as the file's others are. */
async function pin(action) {
  const gh = new GitHub({ token: process.env.GITHUB_TOKEN || undefined });
  const latest = await gh.get(action, `/repos/${action}/releases/latest`);
  if (!latest?.tag_name) return null;
  let ref = await gh.get(action, `/repos/${action}/git/ref/tags/${latest.tag_name}`);
  let object = ref?.object;
  while (object?.type === 'tag') {
    const tag = await gh.get(action, `/repos/${action}/git/tags/${object.sha}`);
    object = tag?.object;
  }
  return object?.sha ? { sha: object.sha, tag: latest.tag_name } : null;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const report = JSON.parse(await readFile(args.report, 'utf8'));
  const { finding, skipped } = pickFinding(report, args.finding);
  const out = resolve(args.out ?? join(args.workdir, 'drafts'));
  await mkdir(out, { recursive: true });
  if (!finding) {
    process.stdout.write(`${JSON.stringify({ ok: true, repo: report.repo, finding: null, why: 'no finding this step can write; the top ones need a person', top: skipped.map((f) => f.id) })}\n`);
    return;
  }

  const dir = join(resolve(args.workdir), report.repo.replace('/', '__'));
  if (!existsSync(dir)) {
    execFileSync('git', ['clone', '--quiet', '--depth', '1', '--branch', report.defaultBranch, `https://github.com/${report.repo}.git`, dir], {
      stdio: ['ignore', 'ignore', 'inherit'],
    });
  }
  const branch = `arca-ci/${finding.id}`;
  git(dir, 'checkout', '--quiet', '-B', branch, `origin/${report.defaultBranch}`);

  const change = { ...finding.change };
  if (change.kind === 'add-step') change.pin = await pin(change.action);
  const file = join(dir, change.path);
  const before = await readFile(file, 'utf8');
  const after = applyChange(before, change);
  const problem = verifyChange(before, after, change);
  if (problem) throw new Error(`the change to ${change.path} did not check out: ${problem}`);
  await writeFile(file, after);
  const diffStat = git(dir, 'diff', '--shortstat');
  const { title, body } = prText(report, finding, { skipped, diffStat });
  git(dir, 'add', change.path);
  git(dir, '-c', 'user.name=Arca CI', '-c', 'user.email=ci@example.com', 'commit', '--quiet', '-m', title, '-m', finding.suggestion);
  const patch = git(dir, 'format-patch', '--stdout', `origin/${report.defaultBranch}..HEAD`);
  const base = join(out, `${report.repo.replace('/', '__')}--${finding.id}`);
  await writeFile(`${base}.patch`, patch);
  await writeFile(`${base}.pr.md`, `# ${title}\n\n${body}`);
  await writeFile(`${base}.body.md`, body);

  let url = null;
  if (args.open) {
    // A fork under the signed-in account; the pull request is a draft from it. Never a push to the repository.
    execFileSync('gh', ['repo', 'fork', report.repo, '--clone=false', '--default-branch-only'], { stdio: 'ignore' });
    const login = execFileSync('gh', ['api', 'user', '--jq', '.login'], { encoding: 'utf8' }).trim();
    const fork = `https://github.com/${login}/${report.repo.split('/')[1]}.git`;
    git(dir, 'push', '--quiet', '--force', fork, `${branch}:${branch}`);
    url = execFileSync(
      'gh',
      ['pr', 'create', '--draft', '--repo', report.repo, '--base', report.defaultBranch, '--head', `${login}:${branch}`, '--title', title, '--body-file', `${base}.body.md`],
      { encoding: 'utf8' },
    ).trim();
  }
  process.stdout.write(
    `${JSON.stringify({ ok: true, repo: report.repo, finding: finding.id, branch, patch: `${base}.patch`, pr: `${base}.pr.md`, opened: url, skipped: skipped.map((f) => f.id) })}\n`,
  );
}

main().catch((error) => {
  process.stdout.write(`${JSON.stringify({ ok: false, error: String(error.message ?? error) })}\n`);
  process.exitCode = 1;
});
