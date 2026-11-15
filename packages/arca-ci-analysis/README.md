# arca-ci-analysis

The nightly CI analysis of a GitHub repository, and the step that turns its top finding into one small pull request.
Node 22 or later, no dependencies.

## What it reads

Read-only, from the GitHub Actions API, for a window (7 days by default): the completed workflow runs, the newest 300
of them at most (`--max-runs`), every job of every attempt of those runs with its steps and timings, the workflow files
at the default branch (20 at most), the repository's cache usage, and the logs of a bounded sample of jobs (the failed
attempts of flaky jobs, the newest other failures, the newest successful runs of the costliest jobs). A log is parsed
as it is read and only what was parsed is kept: cache restores (hit, partial, miss, gzip or zstd) and tests (failed
ones, and those over 2 s with their times; bun, node, cargo, nextest, go, pytest, jest/vitest/playwright marks, TAP).

A busy repository has thousands of runs a week, and reading the jobs of each costs one request. So before its first
request a run prints its plan to stderr: the most requests it can make. `--budget` (600 by default, `BUDGET`) is a hard
limit on GitHub REST requests for the whole run. When the plan is over it, the runs and logs read per repository
shrink to fit. When a run is sampled, the report says so, and its window is the span the sampled runs cover, so weekly
figures stay right. When reading stops part-way (the budget is spent, a rate limit will not reset in time, GitHub keeps
failing), the dataset keeps what was read and the report says what is missing. A rate limit waits for its reset (70
min at most in all). A server error is tried again after 2, 4, 8 and 16 s.

Access, never printed, in this order:

| Where | What |
|---|---|
| `--installation-token-file FILE` or `GITHUB_INSTALLATION_TOKEN` | A GitHub App installation token (`ghs_…`). Its rate limit is the App's on that installation. |
| `GITHUB_APP_ID`, `GITHUB_APP_KEY_B64` | A GitHub App installed on the selected repositories, its PEM key as base64 on one line. The App needs repository permissions **Actions: read**, **Contents: read** and **Metadata: read**, and nothing else. Each run mints an installation token narrowed to one repository and those three permissions. |
| `GITHUB_TOKEN` | An installation token. A person's token (`gho_…` from `gh auth token`, `ghp_…`, `github_pat_…`) is used only with `--allow-user-token` (`ALLOW_USER_TOKEN=1`): its rate limit is shared with everything that person runs, merges and CI included. |

## What it reports

Per repository: runs, jobs, runner minutes (rounded up per job, as billed) and cost at list price and as billed (free
minutes of public repositories taken off; self-hosted minutes priced at 0; prices in `src/cost.js`, dated); p50/p95/max
per workflow, job and step; queue time per job and runner; flaky jobs (failed, then passed on the same commit; not a
scheduled run, whose result depends on more than the code, and not a gate job with `needs:` that failed because a job
of its attempt failed or was cancelled) with the tests that failed; the slowest tests and whether any log read showed them failing; cache hit rate; and the findings,
ranked by what they save a week (a billed dollar 100 points, a waiting minute 1, an unbilled runner minute 0.2;
estimates weigh 0.6).

| Finding | Change it proposes | PR step writes it |
|---|---|---|
| `paid-runner` | short jobs of a public repository moved off a billed runner to `ubuntu-latest`; with `--move-to`, any job on a provider's or GitHub's larger billed Linux x64 runner moved to your own pool, whatever its length (not yet a job on GitHub's standard runners, such as `ubuntu-latest`, even in a private repository where they are billed) | yes |
| `cancel-superseded` | a concurrency group with `cancel-in-progress` | yes, when the workflow has none |
| `duplicate-trigger` | `push` limited to named branches when a branch's push and pull request run the same commit | yes |
| `missing-timeout` | `timeout-minutes` of 3 × p95 (at least 10) | yes |
| `add-rust-cache`, `add-setup-cache` | `Swatinem/rust-cache` (pinned to its newest release's commit) or a setup action's `cache` input | yes, except in release jobs |
| `failing-workflow`, `flaky-job`, `slow-test`, `cache-miss`, `cache-gzip`, `cache-full`, `matrix-agree`, `split-job`, `queue` | in words, with the evidence | no: a person or an agent |

## Use

```
node src/cli.js --installation-token-file token.txt --repo acme/app --repo acme/lib --out out/
node src/cli.js --dataset out/acme__app/dataset.json --out out/      # analyse a saved dataset again, offline
node src/cli.js --installation-token-file token.txt --repo acme/app --move-to acme-4vcpu-ubuntu-2404 --out out/  # billed Linux jobs go to your own pool
node src/cli.js --installation-token-file token.txt --repo acme/app --days 2 --max-runs 300 --budget 600  # a busy repository, sampled
node src/pr-cli.js --report out/acme__app/report.json --workdir work/  # draft the top writable finding
node src/pr-cli.js --report out/acme__app/report.json --workdir work/ --open   # push to a fork, open a DRAFT PR
```

`pr-cli.js` without `--open` changes nothing outside `work/`: it clones the repository, commits one change on the branch
`arca-ci/<finding id>`, and writes the patch and the pull request's text next to it. With `--open` it forks the
repository under the signed-in account, pushes the branch there and opens a draft pull request. It never pushes to the
repository itself and never merges.

`pr-cli.js` assumes a public repository: it clones over anonymous https. For a private one, let git use your GitHub
sign-in first (`gh auth setup-git`), and know that `--open` works only where the organisation allows forks of its
private repositories.

`--move-to` (or `MOVE_TO`) takes one runner label of your own pool: a letter, then letters, digits, `.`, `_` and `-`.
Jobs that already carry it are counted as your own pool, at no charge; when the label reads by its name like a GitHub
or Blacksmith runner (`linux`, `ubuntu-…`), each move adds one evidence line saying so.

A public repository is a risk for your pool. Its jobs reach the pool only if the pool's runner group admits the
repository, and Arca's pool warns when it does: a job on the pool can reach the pool's GitHub credential and the other
slots (see "Who can reach what" in `docs/OPERATING.md`). Anyone can open a pull request from a fork, and a fork pull
request runs its own copy of the workflow, so it can name your pool itself. Where pull requests start a workflow, the
change writes
`runs-on: ${{ github.event.pull_request && github.event.pull_request.head.repo.full_name != github.repository && 'ubuntu-latest' || '<label>' }}`.
That sends ordinary pull requests from forks to GitHub's free runner, but it is not a wall. If you still want your pool
for a public repository, choose "Require approval for all external contributors" (Settings, Actions, General), and read
every workflow change before you approve a run. The evidence and the pull request text say all this for every move in a
public repository.

## As an Arca function

Without `--out` the CLI prints each repository's Markdown report, shortened to fit the 64 KiB of output Arca keeps,
then one JSON result line (`ok`, and per repository its totals and top three finding ids). The image is built by
`.github/workflows/ci-analysis-image.yml` from `Dockerfile`; its job summary names the digest. The owner approves:

```
action_propose { "action": "function_create", "args": {
  "name": "ci-analysis",
  "image": "ghcr.io/automagik-si/arca-ci-analysis",
  "digest": "sha256:<from the image workflow>",
  "command": ["/usr/local/bin/node", "/app/src/cli.js"],
  "env": { "REPOS": "acme/app,acme/lib", "DAYS": "7", "GITHUB_APP_ID": "<app id>" },
  "secrets": ["GITHUB_APP_KEY_B64"],
  "internet": true,
  "triggers": [{ "timer": { "everySeconds": 86400 } }],
  "limits": { "seconds": 900, "memory": "512MiB", "cpu": 1 }
} }
```

`internet: true` is what lets a run reach `api.github.com` and the log storage it redirects to. Then
`function_secret_set {name: "ci-analysis", key: "GITHUB_APP_KEY_B64", value: <base64 of the PEM>}`. `function_runs
{name, run}` shows a night's report. Read one at a time from a workstation, a repository of 630 runs a week took 3
minutes (711 requests) and one of 1072 runs about 4 minutes (1178 requests); the function reads its repositories side by
side within its 15 minutes.

## Tests

`node --test`
