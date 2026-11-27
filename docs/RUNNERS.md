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
a machine of the pool. No credential reaches the machine, the machine is destroyed after the job, and its network card
reaches the internet only (see "The job machine's network" below): it sits on the pool's own bridge, and reaches no
service, tenant or other job machine, no private network and none of the host's addresses. It is still anyone's code using the node's internet connection and
its CPU. So, in GitHub's settings:

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
approval page (the one-link GitHub connect). A device-flow or push connection cannot serve a pool. A device-flow one
mints nothing. A push one does hand out the configurations its owner pushed (`github_runner_mint` answers them), but it
has no Actions service connection: the signer answers the pool's sign-in with `Empty`, so the pool gets no scale set,
logs the refusal, tries again, and never starts a machine. The pool asks the kernel's signer for the Actions service connection and for every
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
        "image": "arca-runner-image-ubuntu24-20261111-370/image",
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
`name` is at most 52 characters: a machine's name is the pool's name, `-<class>-` and 8 hex digits, and Incus allows 63.
`image` is the runner image [`images/github-runner/build.sh`](../images/github-runner/README.md) made for a tag:
`arca-runner-image-<tag>/image` when the build kept the stopped machine `arca-runner-image-<tag>` with its snapshot
`image` (it must be in the pool's project; never start or change it, every job machine is a copy of that snapshot), or
`arca-runner-image-<tag>` when the build published it as an image. Any other local image alias or fingerprint, or
`<instance>/<snapshot>` of a stopped VM in the pool's project, works the same way.
`network` (optional) names an existing bridge for the machines' card; absent, the pool gets its own bridge (see "The
job machine's network"). A named bridge must be visible from the pool's project (a project with `features.networks`
sees its own networks only) and have an IPv4 address, its gateway: the pool's access rules are built from it. A
`network` Incus does not have (a misspelling) stops the pool, and the kernel's log says so. Any `eth0` a profile of
`profiles` gives is replaced by the card, and a profile or an image machine that gives any other device but disks (a
second card, a proxy, a passed-through device) makes every create fail. `name` makes the pool's ACL `arca-runner-<name>`, which Incus allows up to 63
characters.
Records and logs go to `<stateDir>/runners/<pool>/`.

## The job machine

1. **Clone.** `POST /1.0/instances` with `type: virtual-machine`, the runner image as source, the class's
   `limits.cpu`, `limits.memory` and root disk size, and the card below as `eth0`, in the pool's own Incus project,
   marked `user.arcakernel.runner=<pool>`. On btrfs, ZFS and Ceph RBD the root disk is a copy-on-write clone of the image's
   volume, and it costs only the blocks the job writes. Measured on btrfs (below, "How it is proven"): the copy and the
   start take one to two and a half seconds, the boot to a ready agent about 25 seconds.
2. **Boot and wait for the Incus agent.** The launcher never uses a network path into the VM.
3. **Hand over.** The controller asks the signer for one JIT configuration (`workFolder` `/home/runner/work`) and the
   launcher writes it into the VM through the agent's file API, on `/run` (memory), mode 0600, owned by root (`arca-job`
   reads it as root). It is never in the instance's config, on a command line or in a log.
4. **Run.** The launcher starts `arca-job` (a short script this repository ships and the launcher writes into the VM)
   through the agent. It stops the image's own `arca-runner.service` (which waits for a configuration handed over
   another way), reads the JIT file, deletes it, starts the runner as `runner` in a login session (so the limits apply)
   with `/etc/environment` loaded (as on GitHub), and writes the runner's output to `/var/log/arca-job/`. The runner
   takes exactly one job and exits. `arca-job` runs the runner program itself (`bin/Runner.Listener run`), not its
   `run.sh`: `run.sh` starts the runner again for ever after a "retryable" exit, and a configuration the runner cannot
   use is one, so a refused configuration would keep the machine forever. `arca-job` starts it again after an update
   exit (3 or 4, as `run.sh` does, at most three times) and once after a retryable exit (2), and then keeps the
   runner's exit code.
5. **Collect.** When the runner exits, `arca-job` packs its output and `_diag` into one archive. The launcher reads it
   through the agent and stores it on the host as `<state>/runners/<pool>/logs/<runner>/logs.tar.gz`, directory 0700
   and file 0600, owned by root. Logs older than seven days are deleted.
6. **Destroy.** Force stop, delete. Nothing is reused. A destroy that fails (Incus refused it or timed out) is tried
   again, waiting longer each time, and the machine keeps its share of the budget until it is gone.

**The job machine's network.** By default each pool has a bridge of its own, `arcarun-` and seven hex digits of the
pool's name hashed. Arca makes it when the pool starts (or finds it and reuses it), marked
`user.arcakernel.runner=<pool>`: a /24 with NAT, DHCP and DNS, no IPv6, and `dns.include_hosts` off, so its DNS does
not hand a job the names in the host's `/etc/hosts` (it still forwards every other name to the host's resolver). Its
subnet is the first /24 of `172.16.0.0/12`, tried from a point the pool's name fixes, that overlaps none of the node's
`kernel.serviceCidr`, its `kernel.reservedCidrs`, a route of the host's main routing table (`/proc/net/route`, read
the way `arca init` reads it), any Incus network's subnet in any project and the subnet of any address on an
interface Incus lists; so a restart, or a rebuilt node with the same networks, gives the pool the same subnet. Routes
the main table does not show (a VPN that keeps its routes in a table of its own, as Tailscale does in table 52) are
not seen: name those ranges in `kernel.reservedCidrs`. At each start an existing bridge is checked again against the
same list: when a route or a network that came since now meets its subnet, the pool does not start and the kernel's
log names the bridge, its subnet and what it meets. Arca never renumbers it; remove what overlaps, or delete the
bridge once no machine uses it and Arca makes a new one clear of it at the next start.

A pool that is gone from the config has its bridge deleted at the next kernel start, after its ACL, and so does a
pool that now names a `network`, or one renamed (the old name's bridge has no pool). Incus refuses either while a
machine still uses it; the kernel says so and tries again at the next start. A network of the bridge's name without
the pool's mark stops the pool and is left alone.

Its own bridge keeps a job off the node's service bridge: there, a job's DHCP requests could take every free address
of the service subnet (Incus accepts the DHCP broadcast before any rule, and checks only the frame's own MAC), so no
new service, tenant or job machine would start until the leases lapse; every job machine would take an address that
may sit inside a tenant's block; and the bridge's DNS would tell a job every instance name on it. A pool whose
`network` names a shared bridge accepts all of that, and Incus also lets its machines reach port 53 of any
`dns.nameservers` that bridge names.

A known limit: on its own bridge a job can still take every address of that bridge's DHCP range (the `0.0.0.0`
broadcast passes before the ACL). Services and tenants are not touched, but the pool's next machines get no address
until those leases lapse (Incus's default lease is an hour), so one hostile pull request can stall that pool's jobs
for about that long.

**The job machine's card.** Arca builds it; no profile gives it, and it is the machine's only card. `eth0` is on the
pool's bridge with `security.mac_filtering`, `security.ipv4_filtering` and `security.ipv6_filtering` on (the address
is the one Incus's DHCP gives it; a bridge without IPv6 drops every IPv6 frame), both default actions `reject`, and
the pool's network ACL `arca-runner-<pool>` in `security.acls`. The machine is made stopped and read back: one with
any device but its disks and that card (another card, a proxy, an InfiniBand or passed-through device, from a profile
or copied from the image machine) is deleted before it ever starts, and the create fails naming the device. Only then
is it started. The ACL is made when the pool starts and found and reused at the next start; before each machine its rules
are built again from the host's addresses as they are then and replaced when they changed (a new host address is
rejected from the next machine on). It carries `user.arcakernel.runner=<pool>`; an ACL of that name without the mark
stops the pool from starting and is left alone. A pool that is gone from the config has its ACL deleted at the next
kernel start. The rules are a tenant service's, in the same order and shape, with the job's whole bridge subnet (at its
real prefix) and the node's service subnet rejected instead of the subnet outside a block, since a job machine has no
block of its own:

- egress: reject the bridge's whole subnet and the service subnet; allow DHCP (udp 67) and DNS (udp and tcp 53) on the
  gateway; reject the private ranges whole (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), the tailnet range
  (`100.64.0.0/10`), link-local and loopback; reject all IPv6; reject multicast, broadcast, `0.0.0.0/8` and
  `240.0.0.0/4`; reject the host's own addresses, the gateway among them; allow the rest (the internet);
- ingress: none. The default `reject` refuses every connection into the machine, from the host, a neighbour or another
  job machine; answers to what it opened pass.

Incus does not keep the rules' order: it puts every `drop`, then every `reject`, then every `allow`, then the card's
default action (`FirewallACLRules` in `internal/server/network/acl/acl_firewall.go`), so a reject always outweighs an
allow. The gateway's DNS still answers although the gateway is in a rejected range: Incus's filter for a card that has
an ACL accepts DNS (udp and tcp 53) to the bridge's own addresses, and a DHCP request sent from `0.0.0.0` to the
broadcast address, before it reaches the ACL's rules (`nftablesInstanceBridgeFilter` in
`internal/server/firewall/drivers/drivers_nftables_templates.go`). The allows for DHCP and DNS on the gateway state
that intent; they open nothing themselves. Any other DHCP packet a machine sends from its own address to the gateway or
the broadcast address meets a reject; whether a client's renewal needs one depends on the client, and it was not
measured. The card's rules sit in the bridge's own filter, per port (`InstanceSetupBridgeFilter` in
`drivers_nftables.go`, chains `in`, `fwd` and `out`), so they hold between two machines on the same bridge as much as
towards the host; in the forward path Incus turns a reject into a drop, so a refused neighbour times out.

**An existing pool after this change.** It needs nothing in its config: at its next start it gets its own bridge and
its machines leave the bridge their project's profile gave them. The pool's project must be allowed to make a network
(an unrestricted project, as a runner project is; with `features.networks` off the bridge lives in `default`), and
`172.16.0.0/12` must have a free /24. A pool that should stay on a bridge it was on names it in `network`. A profile in
`profiles` that gives a device other than a disk or `eth0` must lose it, or every create fails.

**A runner that refuses its configuration.** A runner that exits with an error before it took a job (its
configuration was refused, or the image cannot run it) is a failure, as a machine that could not boot is: its record
carries the runner's exit code, its logs and the note `the runner exited <code> before it took a job`, its runner is
removed from GitHub, and the class waits before it makes the next machine, longer after each failure in a row (five
minutes at most).

The pool never ends a runner that has a job: the job runs as long as it runs, and a failed read of the runner (Incus
restarting or stalled) is not an exit, so the pool reads again five seconds later. A runner without a job whose reads
fail sixty times in a row (five minutes or more) is ended, its runner removed from GitHub first; if the runner stays
on GitHub (GitHub has just handed it a job, or the removal call failed), it is watched on and its count starts again. A runner GitHub reports finished that has
not exited ten minutes later is stuck, and its machine is ended. An idle runner is replaced by a fresh machine after an
hour without a job, and an idle runner above what the class wants goes after five minutes; either way its runner is
removed from GitHub first (call 8), and the machine stays if the runner stays on GitHub (GitHub has just handed it
a job, or the removal call failed); the pool asks again thirty seconds later. A runner that exited without a job is removed from GitHub after its machine is destroyed.

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
- the runner program in `/home/runner/actions-runner` (the pool setting `runnerDir` changes it; Arca's build makes it a
  link to `/home/runner/runners/<version>`), started as `bin/Runner.Listener run`;
- the Incus agent (Incus VM images from the image server carry it; a built image adds the `incus-agent` setup);
- `systemctl`, when the image has its own `arca-runner.service` (Arca's build has; `arca-job` stops it);
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
  class's size with the card above, and only the pool's marked machines are listed and destroyed. The pool's own
  bridge is made marked, with NAT and no IPv6, in a subnet clear of the service subnet, the reserved ranges and the
  host's networks, the same each time, and reused without a write; a taken first choice moves it; a hand-made network
  of its name stops the pool untouched; `network` puts the card on that bridge and closes its real prefix. A machine
  that a profile gives a second card is deleted and its create fails. The ACL holds no ingress rule and the job rules,
  is reused without a write, and is patched before a machine when the host gained an address. A hand-made ACL of its
  name is never taken over, and a gone pool's ACL and bridge are deleted, and nothing else. The rules themselves are
  checked against a model of Incus's weighing (`crates/arca-netproject/src/tenant.rs`): no address of the job's subnet
  or the service subnet on any protocol, no private, tailnet, link-local, loopback, multicast or host address, the
  internet, and no /24 assumed for a smaller bridge.
- **Real Incus** (`crates/arca-runners/tests/real_incus_runner.rs`, ignored, run by hand with
  `ARCA_REAL_RUNNER_PROOF=1`): on a real Incus with the runner image, one machine at a time, with a JIT configuration
  of GitHub's shape that no runner can use (no GitHub credential and no call to GitHub). One test drives the launcher
  step by step: clone, agent ready, hand-over, the runner's refusal, the configuration gone from the machine, the logs
  (`runner.log` and `_diag`), destroy, nothing of the pool left and the image unchanged. The other runs a whole pool
  (fake GitHub, real launcher) and finds the refusal recorded as above, the logs stored 0600 in a 0700 folder, the
  runner removed and no machine left. Measured on a btrfs pool (Ubuntu 24.04 image, 2 vCPU, 4 GiB, two runs on
  2026-11-27):

  | Step | Time |
  | --- | --- |
  | Clone (copy-on-write copy of the image's snapshot, and the start) | 1.0 to 2.5 s |
  | Boot to a ready Incus agent | 24.2 to 24.3 s |
  | Hand-over (JIT file and `arca-job` written, the exec started) | 0.03 to 0.04 s |
  | Runner refuses the configuration (exit 2, tried twice, 5 s apart) | 5.9 to 6.2 s |
  | Destroy (force stop and delete) | 0.5 s |
  | Pool: from the class asking for a machine to the job record | 31.6 s |

  A third test proves the network from inside the pool's job machines on the pool's own bridge (made for the test and
  deleted after it, as the ACL is), in a project without `features.networks`. Besides two job machines it makes two
  throwaway machines with no rules of their own: the target, with a card on the pool's bridge and a card on the service
  bridge and a listener the host reaches on both addresses, and the relay, with one card on the pool's bridge, which
  must reach the target's service-bridge address through the host (the path a job would take) first. The gateway's
  8443 and a private address outside both bridges are checked from the host too. So each refusal below is the job's
  rules and nothing else's, and each must be a connect that timed out (exit 124); any other code fails the test. Run on
  2026-11-27 (two job machines of 2 vCPU and 2 GiB): DNS on the gateway answered both (udp and tcp) and a public https
  site answered; refused, each with exit 124: the target on the same bridge, the target into the job machine, the
  second job machine into the first, the target's service-bridge address (which the relay reached), the gateway's 8443
  and the private address. The machines, the bridge and the ACL are deleted on every exit path; the guard that deletes
  them is made before the first of them. The two tests above pass with the pool on its own bridge and the machine made
  stopped, checked, then started.

  The runner refuses the made-up configuration at its own key check, before it calls any server.
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
