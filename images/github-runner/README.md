# GitHub runner image

Arca's runner image is GitHub's `ubuntu-latest` image, built by GitHub's own scripts, so a workflow that runs on
GitHub's runners runs the same here. It is an Incus virtual-machine image: one job, one fresh machine made from it,
deleted when the job ends.

- **The same software.** Every script of GitHub's Packer template in [actions/runner-images](https://github.com/actions/runner-images)
  runs, in GitHub's order, with GitHub's toolset file, pinned to one GitHub release (`releases/<line>.env`). Docker is
  GitHub's version from Docker's own repository. GitHub's image tests run during the build; GitHub's software report is
  made from the result.
- **The same machine for a job.** The `runner` user (uid 1001, passwordless sudo, in the `docker` group), the
  workspace under `/home/runner/work`, `/etc/environment` with `ImageOS`, `ImageVersion`, `RUNNER_TOOL_CACHE`,
  `AGENT_TOOLSDIRECTORY`, `ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE` and GitHub's `PATH`, the kernel settings and limits,
  Docker, MySQL and PostgreSQL as systemd services, swap.
- **The toolcache comes filled.** `/opt/hostedtoolcache` holds what GitHub's holds (Python, PyPy, Node, Go, Ruby,
  CodeQL), so `setup-node`, `setup-python` and `setup-go` find their versions without a download, and
  `/opt/actionarchivecache` holds GitHub's cache of popular actions. The image never changes: a job writes to its own
  copy-on-write copy, which goes with the job, so nothing one job writes reaches another.
- **A published difference per release.** `docs/runner-image/<tag>/README.md`: what we change and why, the image
  tests that failed, and our software report against GitHub's for the same release, made with GitHub's own comparer.

## Building

On a host with Incus, `jq`, `curl`, `git`, `python3` and `unzip`:

```sh
images/github-runner/build.sh ubuntu24        # or ubuntu26
```

What it does is written at the top of `build.sh`. In short: wait for the host to be quiet enough (CPU idle, free
memory, room on the storage pool), start a capped virtual machine (4 CPUs, 8 GiB, an 80 GiB root throttled to
150 MB/s read and 80 MB/s write) in the Incus project `ci`, run GitHub's template against it with Packer's `null`
builder, compare the software reports, add the runner, clean up, publish the image as `arca/github-runner/<tag>`,
delete the machine, and write `docs/runner-image/<tag>/`. Settings (`PROJECT`, `CPU`, `MEMORY`, `DISK`, the gate's
limits) are environment variables, listed at the top of the script. One build per host at a time.

The changes to GitHub's build are listed in [CHANGES.md](CHANGES.md), which every published difference includes.
`patch-template.py` makes them, and stops the build when GitHub's template no longer looks as it expects.

## New GitHub releases

GitHub releases each image line about weekly (tags `ubuntu24/<date>.<n>`, `ubuntu26/<date>.<n>`).

```sh
images/github-runner/check-release.sh            # exit 10 when a pin moved to a newer release
images/github-runner/check-release.sh --build    # and build it
```

Then commit `releases/<line>.env` with `docs/runner-image/<new tag>/`, run the compatibility test on the new image,
and move the job pool to it only when that passes. Keep the previous image for a one-step rollback. The runner program
is pinned in `releases/tools.env`; GitHub stops giving jobs to a runner more than 63 days behind, so the weekly rebuild
is also what keeps it current.

`ubuntu-latest` moves from Ubuntu 24.04 to 26.04 in November 2026 (runner-images issue 14748); both lines build the
same way.

## A job machine

`arca-runner.service` starts at boot and waits for the machine's one just-in-time runner configuration (GitHub's
`encoded_jit_config`), from the file `/run/arca-runner/jitconfig` or from the instance setting
`user.arca-runner.jitconfig` (read through Incus's guest API). It runs that runner once as `runner`, with the work
folder at `/home/runner/work`, and powers the machine off when the job is over. The configuration never appears on a
command line.

```sh
incus init arca/github-runner/ubuntu24/20261111.370 job-1 --project ci -c limits.cpu=2 -c limits.memory=8GiB
incus config set job-1 user.arca-runner.jitconfig="$ENCODED_JIT_CONFIG" --project ci
incus start job-1 --project ci          # runs one job, then powers off: delete it then
# When the build kept the stopped machine instead of publishing it (too little room under /var/lib/incus):
incus copy arca-runner-build-ubuntu24/image job-1 --project ci -c limits.cpu=2 -c limits.memory=8GiB
```

## The compatibility test

`compat/compat.yml` runs the same jobs on GitHub's runner and on ours: what the job sees (`fingerprint.sh`: tools,
environment, system, toolcache), the setup actions finding their toolcache, Docker (bind mounts outside the workspace,
published ports, Buildx, Compose, privileged containers), service containers, a container job, the preinstalled
databases, Playwright and Chrome without installing system libraries, and a cache saved on GitHub's runner restored on
ours. `compat/compare.sh <run id> <owner/repo>` writes the two sides as a table of what differs. Run it from a private
repository only; self-hosted runners in a public one take jobs from anyone's pull request.

## Size and time

GitHub's image is about 70-90 GB on their 75 GB disk; each build's measured size and time are in its
`docs/runner-image/<tag>/README.md`.
