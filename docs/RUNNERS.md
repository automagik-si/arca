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

**Until the signer holds the App.** A pool can take its credential from a file instead: the node config's
`runners.credentialsDir` (default `<stateDir>/runner-credentials`) holds one file per binding, named after it, with a
token that has the same organisation permission (a fine-grained personal access token whose resource owner is the
organisation, "Self-hosted runners: Read and write"). It is read at each sign-in, so a new token takes effect without
a restart. The file is readable by the node's user only.

## Configuring a pool

The node config's `runners` key (a kernel-mode node only):

```json
{
  "runners": {
    "pools": [
      {
        "name": "acme",
        "binding": "acme-org",
        "url": "https://github.com/acme",
        "runnerGroupId": 3,
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

`image` is a local image alias or fingerprint, or `<instance>/<snapshot>` of a stopped template VM in the same project.
Records and logs go to `<stateDir>/runners/<pool>/`.

## The job machine

1. **Clone.** `POST /1.0/instances` with `type: virtual-machine`, the runner image as source, the class's
   `limits.cpu`, `limits.memory` and root disk size, in the pool's own Incus project and network, marked
   `user.arca.runner=<pool>`. On ZFS and Ceph RBD the root disk is a copy-on-write clone of the image's volume: about a
   second, and it costs only the blocks the job writes.
2. **Boot and wait for the Incus agent.** The launcher never uses a network path into the VM.
3. **Hand over.** The controller asks the signer for one JIT configuration (`workFolder` `/home/runner/work`) and the
   launcher writes it into the VM through the agent's file API, on `/run` (memory), mode 0600, owned by `runner`. It is
   never in the instance's config, on a command line or in a log.
4. **Run.** The launcher starts `arca-job` (a short script this repository ships and the launcher writes into the VM)
   through the agent. It reads the JIT file, deletes it, starts the runner as `runner` in a login session (so
   `/etc/environment` and limits apply, as on GitHub), and writes the runner's output to `/var/log/arca-job/`. The
   runner takes exactly one job and exits.
5. **Collect.** When the runner exits, `arca-job` packs its output and `_diag` into one archive. The launcher reads it
   through the agent and stores it on the host under `<state>/runner-logs/<pool>/<runner>/`, directory 0700 and files
   0600, owned by root. Logs older than seven days are deleted.
6. **Destroy.** Force stop, delete. Nothing is reused.

A machine whose runner never took a job (the pool shrank, the kernel restarted) is destroyed too, and its runner is
removed from GitHub (call 8). At start the controller deletes every instance in its project that carries its mark: a
kernel that stopped mid-job leaves nothing behind for long.

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
- **Warm runners.** `warm` (default 1 for the dogfood class, 0 otherwise) keeps that many booted machines with a
  registered, idle runner. A job then starts in the time GitHub takes to hand it over, instead of a clone and a boot.
  Targets: queued to started, median 10 s or less, 95th percentile 30 s or less (a cold clone and boot when the warm
  machines are taken). Measured per job: queue time is `runnerAssignTime − queueTime` from the job messages.
- **Nothing when idle.** With `warm: 0` and an empty queue, the pool is one HTTPS long poll per class inside the
  kernel.

## Sign-in alert

Each pool reports its sign-in state to Arca's alerts on every listener pass. The rule `runner-signin` fires (critical)
when a pool's last GitHub exchange failed for a credential reason and has done so for 60 seconds:

- `401` from the token, registration or Actions service calls (bad key, revoked token);
- `404` from the installation lookup (the App was uninstalled or never installed on the organisation);
- `403` for a missing permission;
- the credential missing from the kernel.

Its label is `pool`. It clears after 120 seconds of successful calls. Network errors and GitHub's own 5xx are not
sign-in failures: they keep the last state. `alerts_list` names the pool and the reason (`bad-credentials`,
`app-not-installed`, `forbidden`, `no-credential`), never a token.

## What is recorded per job

One line per job in `<state>/runner-logs/<pool>/jobs.jsonl`: runner name, class, repository, workflow reference, run id,
job id, job name, event, result, queue time, assign time, start, finish, machine create and boot seconds, and where its
logs are. From these: jobs per day, pass rate, queue time (median, p95), boot time.

## How it is proven

- **Fake GitHub** (tests): an in-process server that speaks the calls above, holds a queue of jobs with labels, and
  plays GitHub's part: offers jobs, assigns them once acquired, starts them on a runner that registered with a JIT
  config it minted, completes them. It can answer 401, 404 and 403 to drive the alert.
- **Fake launcher** (tests): an in-memory machine per runner that "runs" a job when told.
- **Incus launcher** (tests): the request bodies and the order of calls against the repository's fake Incus.
- **Dogfood** (our own organisation, one pool): a test workflow and one real CI job. Counted: jobs, pass rate, queue
  time, boot time. And the **state-leak test**: job N writes a marker file in `$HOME`, `/tmp`, `/opt` and the workspace,
  and builds a Docker image; job N+1 on the same class must find none of them.
- **Fingerprint test**: one job that prints the environment (`env`, `id`, groups, `ulimit -a`, sysctls, mounts, free
  space, `docker info`, services, tool versions), run on GitHub's `ubuntu-24.04` and on ours, diffed.

## Order of work

1. This note; the scale set client and listener, the controller, the launcher trait with the Incus VM launcher, the
   sign-in alert, all against the fake GitHub.
2. The kernel task: pools from the node's config, the signer's connection, logs and job records.
3. Dogfood on one class with the image from the image build: the first real job, the state-leak test, the fingerprint.
4. Next slices: one-click onboarding (the App's installation creates the runner group and scale sets), per-customer
   project and network isolation, a Docker Hub pull-through mirror, the Cloud Hypervisor launcher with snapshot-restore,
   placement across hosts.

## Not in this design

- Windows, macOS, arm64 and GPU jobs.
- Release and publish jobs that need GitHub's own runners (npm provenance refuses self-hosted runners; GitHub tells
  every job whether its runner is self-hosted).
- Containers as the job machine.
