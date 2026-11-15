# arca-janitor

A polite, job-based cleanup runner for the trash agents leave behind: build outputs in idle projects and worktrees,
tool caches, stale scratch folders, old tool builds, docker leftovers. It also measures where space goes and,
from its own history, says what keeps making the trash and what would stop it.

It is one static binary with no host dependencies. It runs as an unprivileged user on Linux or macOS, in a container
or on bare metal, with or without systemd. It never uses sudo and refuses to run as root.

## How it runs

```sh
arca-janitor serve                 # start the daemon in the background (serve --foreground under a supervisor)
arca-janitor submit space-report   # prints a job id at once
arca-janitor status <id>           # state, phase, progress, and why it is paused if it is
arca-janitor wait <id> --timeout 60
arca-janitor result <id> [--json]
arca-janitor cancel <id>
arca-janitor list
arca-janitor report --since 7d     # the last clean runs, a few lines each
arca-janitor schema [<kind>]       # JSON Schema of each kind's params and result
```

The daemon keeps one queue and runs one job at a time. It answers JSON lines on `$XDG_RUNTIME_DIR/janitor.sock` and
keeps every job as a JSON file under `~/.local/state/janitor/jobs/`. Exit codes: 0 done, 1 failed or cancelled,
2 usage or daemon error, 3 `wait` timed out.

**Polite by construction.** The whole daemon runs at nice 19 with idle IO. The systemd unit sets that up. Without
the unit, `serve` re-executes itself once through `ionice -c 3 nice -n 19` (on macOS, `taskpolicy -b nice -n 19`).
At start it reads its priority back from `/proc/self/stat` and `ionice -p`. If the priority did not drop, it still
answers, but it refuses every heavy job and says why. While a job runs, it reads `/proc/pressure/io` (on macOS, the
load per CPU). Above `PSI_THRESHOLD` (default 10%), it pauses, waiting 2 s and doubling up to 60 s, and its status
shows the pause. Each job has a time budget (`JOB_BUDGET_MIN`) and can be cancelled.

## Job kinds

| Kind | What it does |
|------|--------------|
| `space-report [path]` | sizes of every folder down to `--depth` levels: one filesystem, links not followed, mount points skipped; stored as a snapshot |
| `growth [path]` | ranks the folders that grew between the newest snapshot and an older one; scans again only when the newest is older than `--max-age-hours` |
| `big-files [path]` | files over `--min-mb`, optionally only those changed in the last `--changed-hours` |
| `clean-plan` | a dry run of the clean categories (`--profile full\|daily`, `--only a,b`); writes a plan with every item's path, category, bytes, reason and origin |
| `clean-apply <plan-id>` | applies exactly that plan, once; every protection is worked out again and every item checked again first |
| `trash-purge` | deletes quarantine day folders older than `TRASH_DAYS` |
| `nightly` | plans the daily profile and applies it, unless the plan frees more than `NIGHTLY_MAX_GB`, trashes more than `NIGHTLY_MAX_TRASH_GB`, or names a `STOP_WORDS` word |
| `insights` | ranks repeat offenders from the history, each with its evidence and a concrete change at the source |
| `probe` | a harmless job: shows the daemon's priority and the current pressure |

## Results you can trust

- An apply first decides every item's verdict, then changes anything. Removing one build output touches its
  project folder, so a sibling checked afterwards would look fresh. Right before each action, only the disk is
  checked again: the path is still there, still this user's, not a link, not a mount.
- Every action is verified on disk afterwards. A removed path must be gone. A quarantined folder must be gone from
  its place and present in the trash. An emptied folder must be empty. A command must exit 0.
- Each item ends as `done`, `skipped` (with the protection or precondition) or `failed` (with the OS error or
  exit status). Only `done` items count as freed.
- A run with any failed item, or one that cannot write its plan or history, ends as `failed`. `wait` and `result`
  then exit 1, and the result still lists every item's outcome.
- A dry run reports only what would happen (`would`, `would_free_bytes`). A plan's totals are named the same way.
- A socket or state folder that cannot be created stops `serve` with the OS error and exit code 2. It is never
  reported as "already running".

## Categories

Linux: `caches` (npm, pnpm and uv through their own commands; go-build, node-gyp and cargo downloads emptied),
`claude-versions`, `arca-backups`, `docker` (untagged images, build cache older than 7 days), `runner-caches`
(Sundays only in the daily profile), `build-outputs` (`target/` beside a Cargo.toml; `node_modules`, `.next`,
`.turbo` and `.parcel-cache` beside a package.json; only in projects idle for `BUILD_IDLE_DAYS`), and `stale-scratch`
(a scratch root's top-level folder untouched for `STALE_DAYS` is moved to `<root>/.cleanup-trash/<date>/`).

macOS: `caches` (brew, npm, pnpm, yarn, pip, uv, go, CocoaPods, cargo, node-gyp), `xcode` (DerivedData, unavailable
simulators), `docker`, `claude-versions`, `app-caches` (`~/Library/Caches` folders untouched for 30 days, Apple's
own left alone), `logs` (`~/Library/Logs` files older than 30 days), `build-outputs`, `stale-scratch`. The Trash
and Time Machine's local snapshots are left to a person.

In the daily profile, a cache stays warm while it is under its cap (`NPM_MAX_MB`, `GOBUILD_MAX_MB`, `CARGO_MAX_MB`,
`NODEGYP_MAX_MB`).

## Never touched

These are worked out again for every plan and every apply:

- anything inside a top-level folder (under the deepest root) that one of these uses: a user service (systemd
  `WorkingDirectory`, `ExecStart`, `ExecStartPre` and `EnvironmentFile`; on macOS, LaunchAgents'
  `WorkingDirectory` and `ProgramArguments`), a pm2 app (asked only if a pm2 daemon already runs), a container's
  bind mount (docker or OrbStack), an Incus disk device, a Lima mount, or a live process of this user (its working
  folder, its executable, an open file);
- `PROTECT` paths, and any folder holding a `.cleanup-keep` file;
- the home's tool dot-folders (`~/.cargo`, `~/.bun`, `~/.local`...), whose `node_modules` are global installs;
- links, mount points, files not owned by this user, and odd paths;
- a cache while its tool runs, and a build an active session runs.

## Config

`cleanup.conf` (every host) and then `cleanup.<hostname>.conf` (one host), both in `$JANITOR_CONFIG_DIR`
(default `~/.config/janitor`). Each line is `KEY=value` or `KEY+=value`. A list given with `=` becomes that one
value; `+=` adds one. A leading `~` is the home. An unknown key is an error.

```sh
SCRATCH_ROOTS+=/srv/scratch
PROJECT_ROOTS+=~/work
PROTECT+=~/work/acme-release
STOP_WORDS+=/srv/acme-prod/
PSI_THRESHOLD=15
```

Keys: `SCRATCH_ROOTS`, `PROJECT_ROOTS`, `PROTECT`, `CACHE_KEEP`, `STOP_WORDS` (lists); `STALE_DAYS`, `TRASH_DAYS`,
`BUILD_IDLE_DAYS`, `SCRATCH_BUILD_IDLE_DAYS`, `NPM_MAX_MB`, `GOBUILD_MAX_MB`, `CARGO_MAX_MB`, `NODEGYP_MAX_MB`,
`MAX_DEPTH`, `PSI_THRESHOLD`, `JOB_BUDGET_MIN`, `NIGHTLY_MAX_GB`, `NIGHTLY_MAX_TRASH_GB` (numbers); `PSI_PATH`.
`arca-janitor config` prints the config in force and the files it came from.

## Undo

The `log` of a clean-apply job names every quarantined folder (`quarantined A -> B`). Move B back to A within
`TRASH_DAYS`. Build outputs and caches are rebuilt by their tools. To keep a folder forever, add a `.cleanup-keep`
file to it or list it in `PROTECT`.

## Packaging

`dist/` holds systemd user units: `janitor.service` (the daemon) and `janitor-nightly.timer` with its service. The
timer queues `nightly` at 04:30 local time, with up to 20 minutes of random delay. Nothing installs or enables them.
