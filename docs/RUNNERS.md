# Runners: a fresh machine for every job

Design note for Arca's GitHub Actions runners, the replacement for the fixed-slot `github-runner-pool` template. It says
what is built, in what order, and why. The operating guide follows in [`OPERATING.md`](OPERATING.md) once the first
slice ships.

## What changes

| | The `github-runner-pool` template (today) | Runners (this design) |
|---|---|---|
| Where a job runs | a slot inside one long-lived pool VM: the runner in one container, Docker in a sidecar | its own virtual machine, cloned copy-on-write from an image snapshot, destroyed when the job ends |
| What a job finds from earlier jobs | the slot's toolcache, Docker images and build cache | nothing: the clone is deleted, not cleaned |
| What a job sees | the whole pool VM (`nproc` 16, 31 GiB) whatever its limit | exactly its size: a 4-CPU, 16 GiB job is a 4-CPU, 16 GiB machine |
| Docker | a sidecar daemon; a bind mount outside the workspace is empty | a normal systemd service on the job's machine, as on GitHub |
| Workspace | `/home/runner/_work/<repo>/<repo>` | `/home/runner/work/<repo>/<repo>`, as on GitHub |
| How many runners | a fixed number of slots, always on | as many as GitHub has jobs for, inside a reserved budget; none when the queue is empty |
| Where the GitHub credential lives | inside the VM that runs the jobs | in the Arca kernel only; a job machine receives one single-use runner configuration |
| Size per job | per slot class, fixed at create | per label: `runs-on` picks a size class |
| When sign-in fails | the pool retries silently | an Arca alert fires |
| Logs of a failed job | gone with the slot's container | the runner's output and `_diag` kept on the host for 7 days, readable by root only |

## The parts

```
GitHub (scale set API)                     Arca kernel (host side)                         Incus
──────────────────────                     ───────────────────────                         ─────
runner group ── scale set per label set ◄── listener: one long poll per scale set
                                             │  statistics, job messages
                                             ▼
                                           controller: demand = warm + assigned jobs,
                                             inside the pool's CPU/memory budget
                                             │                    ▲
                     generatejitconfig ◄──── signer (holds the    │ job ended, logs
                                             App key; mints one   │
                                             JIT config per VM)   │
                                             │                    │
                                             ▼                    │
                                           launcher (trait) ──────┴──► VM clone of the runner image
                                             today: Incus VM          runner + Docker on the VM
                                             later: Cloud Hypervisor  one job, then deleted
```

- **Listener.** A Rust port of GitHub's own scale set client (`actions/scaleset`, the protocol
  actions-runner-controller's listener speaks). One message session per scale set; a long poll (up to two minutes)
  answers with the scale set's statistics and its job messages. No webhook, no public address.
- **Controller.** Keeps, per size class, `min(max, warm + TotalAssignedJobs)` runners alive, as long as the pool's
  budget allows. It acquires the jobs GitHub offers, starts a machine for each runner it wants, records each job, and
  destroys the machine when its runner exits.
- **Signer.** The one place that holds the GitHub App's private key and its installation tokens. It derives the
  Actions service connection (a one-hour token) the listener uses, and mints every just-in-time (JIT) runner
  configuration, with an audit record per mint. The key never leaves it, and never enters a job's machine.
- **Launcher.** A trait with one implementation today: an Incus virtual machine cloned from the runner image. The
  controller, the listener and the tests do not know which launcher they drive. A Cloud Hypervisor microVM launcher
  with snapshot-restore replaces it later behind the same trait.
- **Image.** A real Ubuntu VM image built from GitHub's own `actions/runner-images` scripts, with the runner program
  installed for user `runner` and Docker as a systemd service. Built and versioned separately; the launcher only names
  it.

## GitHub's side

Scale sets are GitHub's mechanism for runners that come and go with demand.

- The person picks the label names. A pool is configured with one or more **size classes**, each with the label set
  its jobs use in `runs-on` (for example `acme-4vcpu-ubuntu-2404`) and its machine size. Arca never rewrites
  workflows and never adds its own name to a label.
- Each size class is one scale set, named after its first label, in the pool's runner group. A scale set receives
  only the jobs whose `runs-on` matches its labels, so the label a job asks for decides the size of its machine.
- The runner group is GitHub's list of repositories that may send jobs here. It is the access control; a label is not.

**Public repositories and fork pull requests.** A runner group serves a public repository only when its "Allow public
repositories" setting is on. Then a pull request from a fork can carry its own workflow file naming the pool's label, and
GitHub runs the pull request's copy of the workflow for `pull_request` events: anyone's code, with passwordless sudo, on
a machine of the pool. No credential reaches the machine and the machine is destroyed after the job, but until
per-person network isolation lands it sits on the host's network beside other jobs' machines. So, in GitHub's settings:

- leave the runner group's "Allow public repositories" off for a pool that serves private repositories only;
- before a pool serves a public repository, set that repository's (or the organisation's) Actions setting "Approval for
  running fork pull request workflows from contributors" to "Require approval for all external contributors", so a
  maintainer reads every outside pull request before it runs on the pool;
- the proof workflow below runs only by hand (`workflow_dispatch`), so it adds no trigger a fork can use; serving it
  from a public repository still needs the group's public setting on, with the approval setting above.

The calls the listener and the signer make (ported from `actions/scaleset`; Actions service paths take
`api-version=6.0-preview`):

| | Call | Credential |
|---|---|---|
| 1 | `POST /orgs/{org}/actions/runners/registration-token` | installation token (signer) |
| 2 | `POST /actions/runner-registration` `{url, runner_event: "register"}`: the Actions service URL and an admin token of about one hour | `RemoteAuth <registration token>` (signer) |
| 3 | `GET _apis/runtime/runnerscalesets?runnerGroupId=&name=`; `POST`, `PATCH`, `DELETE _apis/runtime/runnerscalesets[/{id}]` | admin token |
| 4 | `POST …/runnerscalesets/{id}/sessions`; `PATCH` (refresh) and `DELETE …/sessions/{sessionId}` | admin token |
| 5 | `GET {messageQueueUrl}?lastMessageId=` with `X-ScaleSetMaxCapacity` (long poll; 202 means nothing new); `DELETE {messageQueueUrl}/{messageId}` (ack) | message queue token |
| 6 | `POST …/runnerscalesets/{id}/acquirejobs` `[runnerRequestId…]` | message queue token |
| 7 | `POST …/runnerscalesets/{id}/generatejitconfig` `{name, workFolder}` | admin token (signer, audited) |
| 8 | `GET` and `DELETE _apis/distributedtask/pools/0/agents[/{id}]`: remove a runner whose machine died before its job | admin token |

**Permission.** A GitHub App installed on the organisation with the organisation permission "Self-hosted runners:
Read and write", and nothing else. No repository permission and no Actions read: the job messages already carry the
repository, the workflow run id, the job id, the workflow reference, the labels and the queue, assignment and finish
times. GitHub cannot narrow this permission to one runner group; the restraint is ours (the signer only acts in the
binding's runner group with the labels the person chose), and the key never leaves the kernel.

**The pool signs in through the signer.** A pool's `binding` is a GitHub App connection the person made on the
approval page (the one-link GitHub connect). A device-flow or push connection cannot mint runners: the signer refuses
it, the pool only logs the refusal and tries again, and no runner ever starts. The pool asks the kernel's signer for the Actions service connection and for every
runner configuration; the signer refuses a scale set outside the connection's runner group and labels outside the
connection's label sets, and writes every mint to its audit log. The pool holds no key and no token of its own, and
there is no token file to place.

## Configuring a pool

The node config's `runners` key (a kernel-mode node only):

```json
{
  "runners": {
    "pools": [
      {
        "name": "acme",
        "binding": "gh-acme",
        "budget": { "cpu": 16, "memory": "64GiB" },
        "project": "arcak-runners",
        "image": "runner-ubuntu-2404",
        "storagePool": "default",
        "classes": [
          { "labels": ["acme-4vcpu-ubuntu-2404"], "cpu": 4, "memory": "16GiB", "diskGiB": 80, "warm": 1, "max": 4 }
        ]
      }
    ]
  }
}
```

`binding` is the signer connection's id. The scale sets go into the runner group the connection made for the
repositories the person picked. Leave `runnerGroupId` unset: a pool that names any other group makes its scale sets
there, and the signer then refuses every runner configuration, since it mints only into the connection's own group.
Each class's labels must be one of the connection's label sets. A class whose labels are not boots a machine, is
refused at the mint and waits; it tries again every five minutes at most, one machine boot each time, and never runs a
job.
`name` is at most 52 characters: a machine's name is the pool's name, `-<class>-` and 8 hex digits, and Incus allows 63. `image` is a local image alias or fingerprint, or
`<instance>/<snapshot>` of a stopped template VM in the same project.
Records and logs go to `<stateDir>/runners/<pool>/`.

## The job machine

1. **Clone.** `POST /1.0/instances` with `type: virtual-machine`, the runner image as source, the class's
   `limits.cpu`, `limits.memory` and root disk size, in the pool's own Incus project and network, marked
   `user.arcakernel.runner=<pool>`. On ZFS and Ceph RBD the root disk is a copy-on-write clone of the image's volume: about a
   second, and it costs only the blocks the job writes.
2. **Boot and wait for the Incus agent.** The launcher never uses a network path into the VM.
3. **Hand over.** The controller asks the signer for one JIT configuration (`workFolder` `/home/runner/work`) and the
   launcher writes it into the VM through the agent's file API, on `/run` (memory), mode 0600, owned by root (`arca-job`
   reads it as root). It is never in the instance's config, on a command line or in a log.
4. **Run.** The launcher starts `arca-job` (a short script this repository ships and the launcher writes into the VM)
   through the agent. It reads the JIT file, deletes it, starts the runner as `runner` in a login session (so
   `/etc/environment` and limits apply, as on GitHub), and writes the runner's output to `/var/log/arca-job/`. The
   runner takes exactly one job and exits.
5. **Collect.** When the runner exits, `arca-job` packs its output and `_diag` into one archive. The launcher reads it
   through the agent and stores it on the host as `<state>/runners/<pool>/logs/<runner>/logs.tar.gz`, directory 0700
   and file 0600, owned by root. Logs older than seven days are deleted.
6. **Destroy.** Force stop, delete. Nothing is reused. A destroy that fails (Incus refused it or timed out) is tried
   again, waiting longer each time, and the machine keeps its share of the budget until it is gone.

The pool never ends a runner that has a job: the job runs as long as it runs, and a failed read of the runner (Incus
restarting or stalled) is not an exit, so the pool reads again five seconds later. A runner without a job whose reads
fail sixty times in a row (five minutes or more) is ended, its runner removed from GitHub first; if GitHub keeps it
because it has just handed it a job, it is watched on. A runner GitHub reports finished that has
not exited ten minutes later is stuck, and its machine is ended. An idle runner is replaced by a fresh machine after an
hour without a job, and an idle runner above what the class wants goes after five minutes; either way its runner is
removed from GitHub first (call 8), and the machine stays if GitHub refuses because it has just handed the runner a
job. A runner that exited without a job is removed from GitHub after its machine is destroyed.

The controller deletes every instance in its project that carries its mark and that it does not know: all of them at
start, and any it finds every five minutes after (a machine an earlier destroy or listing could not reach).

**A kernel restart ends the pool's running jobs.** Machines are not adopted across a restart: the kernel that starts
again destroys every machine of the pool, busy or not, and the jobs on them fail on GitHub and write no record here.
Until adoption lands, deploy a node with a pool when its queue is quiet.

**Why a VM and not a container.** Docker inside an LXC container depends on the host's LXC and runc settings, and a
runc security fix in November 2025 broke it on Incus and other LXC hosts until their configuration changed. A CI
service cannot have that class of outage. A VM also runs systemd services, sets its own sysctls, can offer `/dev/kvm`
(nested virtualisation, a separate size class), and is a hardware boundary between jobs.

**What the guest image must provide** (the contract with the image build):

- user `runner` with passwordless sudo and membership of `docker`; Docker enabled as a systemd service;
- the runner program in `/home/runner/actions-runner` (the pool setting `runnerDir` changes it);
- the Incus agent (Incus VM images from the image server carry it; a built image adds the `incus-agent` setup);
- `bash`, `tar`, `gzip`, `runuser` (util-linux).

## Size, budget and warm runners

- A pool reserves a **budget**: total CPUs and memory it may hold at once on this host. A runner of a class counts its
  full size from the moment its machine is created until it is deleted.
- **Demand.** Per class, `wanted = min(max, warm + TotalAssignedJobs)`. GitHub counts a job as assigned from the moment
  the scale set acquires it until it completes, so running jobs are in it. Each finished job's machine is deleted, so
  the count falls by itself.
- **Capacity.** The controller tells GitHub how many runners each class can still have (`X-ScaleSetMaxCapacity`), from
  the budget that is free, so GitHub does not hand a scale set more jobs than the pool can start.
- **Warm runners.** `warm` (default 0) keeps that many booted machines with a
  registered, idle runner. A job then starts in the time GitHub takes to hand it over, instead of a clone and a boot.
  Targets: queued to started, median 10 s or less, 95th percentile 30 s or less (a cold clone and boot when the warm
  machines are taken). Measured per job: queue time is `runnerAssignTime − queueTime` from the job messages.
- **Nothing when idle.** With `warm: 0` and an empty queue, the pool is one HTTPS long poll per class inside the
  kernel.

## Sign-in alert

Each pool reports its sign-in state to Arca's alerts on every listener pass. The rule `runner-signin` fires (critical)
when a pool's last GitHub exchange failed for a credential reason and has done so for 60 seconds:

- `401` from the token, registration or Actions service calls (bad key, revoked token): `bad-credentials`;
- the App not installed yet, uninstalled or deleted on GitHub, which the signer records as a revoked connection:
  `app-not-installed`;
- `403` for a missing permission, or an installation GitHub suspended: `forbidden`;
- the pool's `binding` naming no connection of the signer: `no-credential`.

A refusal of the request itself (a scale set outside the connection's runner group, labels outside its label sets) is
the pool's settings, not its sign-in: the machine's record carries the signer's words, and the class waits before it
tries again.

Its label is `pool`. It clears after 120 seconds of successful calls. A failure found at a runner configuration (the
App uninstalled or suspended after the pool connected) stays until a configuration is minted again, since the
listener's polls run on the earlier connection and keep working for up to an hour. Network errors and GitHub's own 5xx are not
sign-in failures: they keep the last state. `alerts_list` names the pool, never a token. The reason
(`bad-credentials`, `app-not-installed`, `forbidden`, `no-credential`) is in the kernel's log line for the pool; the
pool's status verb, a next slice, will show it.

## What is recorded per job

One line per machine in `<state>/runners/<pool>/jobs.jsonl`: runner name, class, repository, workflow reference, run
id, job id, job name, event, result, queue time, assign time, start, finish, machine create and boot seconds, where its
logs are, and a note when something failed. A machine that never ran a job (a warm runner that was replaced, a failed
boot) writes a line too, with the job fields empty. From these: jobs per day, pass rate, queue time (median, p95), boot
time.

## How it is proven

- **Fake GitHub** (tests): an in-process server that speaks the calls above, holds a queue of jobs with labels, and
  plays GitHub's part: offers jobs, assigns them once acquired, starts them on a runner that registered with a JIT
  config it minted, completes them. It can answer 401, 404 and 403 to drive the alert.
- **Fake launcher** (tests): an in-memory machine per runner that "runs" a job when told.
- **Incus launcher** (tests): against the repository's fake Incus, a machine is a marked virtual machine of its
  class's size, and only the pool's marked machines are listed and destroyed. The runner hand-over (file pushes and
  the exec) is a next test.
- **Dogfood** (our own organisation, one pool): a test workflow and one real CI job. Counted: jobs, pass rate, queue
  time, boot time. And the **state-leak test**: job N writes a marker file in `$HOME`, `/tmp`, `/opt` and the workspace,
  and builds a Docker image; job N+1 on the same class must find none of them.
- **Fingerprint test**: one job that prints the environment (`env`, `id`, groups, `ulimit -a`, sysctls, mounts, free
  space, `docker info`, services, tool versions), run on GitHub's `ubuntu-24.04` and on ours, diffed.

## Order of work

1. This note; the scale set client and listener, the controller, the launcher trait with the Incus VM launcher, the
   sign-in alert, all against the fake GitHub.
2. The kernel task: pools from the node's config, signing in through the signer's connection (no token of their own),
   logs and job records.
3. Dogfood on one class with the image from the image build: the first real job, the state-leak test, the fingerprint.
4. Next slices: one-click onboarding (the App's installation creates the runner group and scale sets), per-person
   project and network isolation, a Docker Hub pull-through mirror, the Cloud Hypervisor launcher with snapshot-restore,
   placement across hosts.

## Not in this design

- Windows, macOS, arm64 and GPU jobs.
- Release and publish jobs that need GitHub's own runners (npm provenance refuses self-hosted runners; GitHub tells
  every job whether its runner is self-hosted).
- Containers as the job machine.
