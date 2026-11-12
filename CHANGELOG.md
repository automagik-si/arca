# Changelog

What changed in each release of Automagik Arca, in plain words. The newest release is first.

## Unreleased

<!-- The release writes the next section here by itself: one line per change that landed, from its commit subject.
Write under this comment only what a reader needs beyond those lines; the release puts its heading above it. -->

A card that waits while the node is upgraded to the version that binds a template's content (a create, or a database
made from the template) is now refused when its approval is submitted, and the approval is not used; propose it
again. Before, the approval was accepted and the create then refused.
A `service_create` the node has no room for is now refused when it is proposed, in the words the create uses (what is
in use, what is asked, what is allowed, and the config key), so the owner is never shown a card for it. Before, the
card was shown, the approval was accepted and used up, and only then did the create refuse. This covers the node's
caps on virtual machines (`kernel.vmLimits`, `kernel.bridged.limits`) and a tenant's caps. The approved create still
checks them, because the node can fill up while a card waits. One refusal for room can still follow an approval: no
free address in the service subnet.

A template whose image is `local:<alias>` and that already carried a `digest` was accepted before, and the digest was
ignored. It is enforced now: a create from such a template is refused unless the image under the alias has that
fingerprint, and a digest that is not `sha256:` and 64 lowercase hex digits is refused when the template is read. Check
a local template's `digest` against `incus image list` before you upgrade, or take the digest out.

Every run of a function now starts from its image's own root. Before a run starts, its container's root is rebuilt
from the function's image: from the copy the node already holds, the one the function's record took when it was
created, or from the registry by the approved digest when that copy is gone. So no run finds what an earlier one
wrote, which replaces the limit 0.13.0 named. A root that cannot be rebuilt is a run that does not start, and its
line says why. A run's line gains `root`, `rootImage` and `rootMs`. A function's first run rebuilds nothing, and its
line says `root` is `new`: a container that has never started still has the root its create made from the image, and
Incus cannot start one that was rebuilt before its first start.

What the owner approves on a card is now what is created. A proposed `service_create` or `postgres_create` is given,
by the node, the image its template names and a fingerprint of the template's content (its file and the scripts it
names). The card shows an `Image` row and ends with a `Template content` row, and the approved create runs only while
the template on the node is still exactly that one: an edit, or a template installed again by an upgrade, refuses the
create with "propose it again". An image named without a digest is shown with words that say so. Two new optional
arguments, `image` and `templateDigest`, are filled in by the node; a caller that states one must state the node's
own. A proposal for a template that is not installed, or whose script is missing, is now refused when it is made.

**If your node has templates from 0.13.0 or older:** upgrading with the installer now brings them up to the release.
A shipped template that nobody edited is replaced with this release's and named in the report; one you edited is
kept and named, with the command that replaces it. For a node the installer does not run as a service, run `arca
templates install --config <file>` once after the upgrade (or `arca init --enable-changes` again). 0.13.0 said to
use `--replace` for this, which also overwrites your edits: it is no longer needed. A service that already exists is
not changed.

A commit to the node's store that works but takes over one second is no longer called a failed write. It has its own
counter, `arca_store_slow_commits_total`, and its own alert, the warning `store-commit-slow`.
`arca_store_write_failures_total` and the critical alert `store-write-failed` now mean only a write the store refused. A
dashboard or an outside alert built on `arca_store_write_failures_total`, or on `store-write-failed`, stops firing for a slow
disk: watch the new counter or the new alert for that. Both counters start at 0 when the server starts, as before.

## 0.13.0 (2026-11-12)

On a node that only looks, the refusals of an instance start, stop or restart, of a storage change and of a GPU
assignment no longer hold the word `watch-only`, which they did in 0.12.0: they carry the same sentence as every other
refusal and every answer of such a node, `this node only looks: nothing here can be changed until the node is running
with changes turned on (arca init --enable-changes, then restart the node if init did not)`. Their codes are unchanged
(`InstanceLifecycleDisabled`, `StorageDisabled`, `GpuDisabled`): match on the code, not on the words.

**If you installed the templates with 0.12.0 or older:** the shipped `postgres-autopg` template now makes the
database's password itself, so a database you approve on the page starts by itself. Your node keeps the template it
already has. To get the new one, run `arca templates install --replace --config <file>`; it overwrites shipped
templates you edited. A `postgres-autopg` service that already exists is not changed, and keeps its password.
Arca now runs functions: a program in an OCI image by digest, run on a timer, each run one start of a container of its
own that is stopped again when the program exits. A function is created and removed only from a proposal the owner
approves, and its card shows the image and digest, the command, the timer, the limits of a run (time, memory, CPU), the
names of its secrets and everything it may reach: the services of this node its links name, each on one tcp port, the
private addresses the node's config names for it, and no connection to any public address. One way out stays, and
the card says it: names are looked up through the node's DNS, which asks the internet's name servers, so a program
can send a little data out in the names it asks for. Secrets are set by name
(`function_secret_set`) and a run is given only the ones its function names. Each run writes one line to a run log
that is also the meter (start, duration, CPU time, memory peak, outcome, the program's result line), read with
`function_runs`; a run writes nothing to the desired state or the effect journal. A program's last output line is its
result, one JSON object with `"ok"`. Six new verbs: `function_create`, `function_remove`, `function_secret_set`,
`function_list`, `function_run`, `function_runs`. Known limit: a run can write its container's root file system, and
the next run finds what it wrote (Incus refuses to start an OCI container with a read-only root). `docs/OPERATING.md`,
"Functions", has the whole of it.

`postgres_create` now works on a node set up from a clean install. On a node whose config sets no
`kernel.postgresImage`, it makes a service from the node's `postgres-autopg` template, with the image fetched by
digest and the password the template generates: give the name only. A database made this way is a service: connect
to it with `service_connect`, remove it with `service_remove`; `postgres_list` shows it with `service: true`. A proposal's card
shows the image and binds it, and a template changed before the approval refuses the create.

No call chooses a database's image any more. A `doctor` or `operator` token's own `postgres_create` used to run any
digest-pinned image it named; it now uses the node's template, or `kernel.postgresImage` where the config sets it, and
a call that states another image is refused. A node whose config sets `kernel.postgresImage` keeps the old kind of
database whole, and its records and lists are unchanged; `image` is now optional in the call.

- a database from a clean install is the shipped Postgres service, made through the owner's card, and no token names its own image
- functions, a program that runs on a timer in a container of its own, created only from the owner's card, with limits and a run log that is the meter
- after arca init a person sees one next action, the shipped Postgres service makes its own password so it starts through the card alone, and the README says what 0.12 does
- a node that only looks still shows its services, their status and their logs, and refuses every write first, in one sentence
- a runner pool obeys a stop at once, in every loop and during a request

## 0.12.0 (2026-11-12)

- the installer shows the one line that connects an assistant, arca status says who approves and which tokens do not wait, and a machine with no screen is told what to run
- fix: token reload, link pair and link join never let a stale ARCA_CONFIG pick the node
- one rule for finding the config on every command, a stale ARCA_CONFIG never picks the node for a command that hands out or removes, and an unknown field is named
- fix: arca init calls the node's address the way token reload left it, so the merged set compiles (diff read by the Orchestrator)
- arca init --enable-changes, one guided step from a node that only looks to one that can make changes, and it names every token that could change things without the passkey
- arca token reload, a checked reload that never signals a stranger or a node still starting, and every hint names it

## 0.11.0 (2026-11-12)

The MCP server now speaks protocol version `2026-07-28` beside `2025-11-25`. For a client of `2025-11-25`, or one that
sends no version header, `initialize`, `ping`, `tools/list` and `tools/call` answer as before. The server used to read
no version from a request and now does, so four answers changed for such a client:

- `server/discover` answers; it used to be an unknown method.
- A `MCP-Protocol-Version` header naming a version other than `2026-07-28`, `2025-11-25`, `2025-06-18` or `2025-03-26`
  is refused with `400` and the list of supported versions.
- A `MCP-Protocol-Version` header sent twice, or one that is not visible ASCII, is refused with `400`.
- A version in `params._meta` is read even with no header: an unknown one, one that is not a string, and one that
  differs from the header are refused with `400`.
A service of the node's own project can now be let through to one private address on one tcp port, for example a
GitLab on another bridge of the node or on its tailnet. It takes two keys, and neither alone: the node's config names
it (`kernel.serviceDestinations: [{ "service", "address", "port" }]`, one IPv4 address, never a range or a name), and
the owner approves the service's create, whose card carries a `Private network` row with the address. A direct create
of a named service is refused with `NotApproved`. The service's own Incus ACL is the only thing that changes, and only
tcp to that address on that port becomes reachable: every other tcp port, udp, icmp and every other IP protocol to it
stay refused. Nothing of the host's firewall or routing is touched, so the node must already forward and masquerade
that path, and the destination then sees the node's own address: whatever trusts that address there trusts everything
in the service. A tenant's service, an address of the node's own and a virtual machine on a host bridge never get one;
`serve` refuses to start when the config loses an entry a service's record carries; `service_list` and
`service_status` show `destinations`, and status says for each whether it stands. An existing service is not changed
by an entry: remove it and create it again. Known limit: taking an entry out of the config does not close the address;
remove the service first. Not yet run on a live node. See "A service that may reach one private address" in
`docs/OPERATING.md`.

- a service of the node's own project may reach one private address on one tcp port, named in the node's config and shown on the owner's card
- the approval page on one machine with no domain, at http://localhost, and the first passkey's setup code on the terminal
- a loopback HTTP MCP door by default, the installer prints the line that connects an assistant, and arca status says whether the door is open
- every route of the approval page answers with or without a trailing slash, and the canvas page loads its assets from both
- a node never set up for services answers an empty service and tenant list, and one that still holds service records keeps refusing
- the common commands need no flag, arca status says the node in words, and a refusal says the next step
- the release build's path check reads a path whole, stops when it cannot search, and judges by what is committed
- the MCP door speaks protocol version 2026-07-28 beside 2025-11-25, and answers server/discover

## 0.10.0 (2026-11-12)

`service_status` now answers `update` for a service whose version is known: `pin`, the version it was created with,
`templatePin`, the one the template installed on the node names now, and `state`, `same` or `differs` (`unknown`
with a `problem` when the installed template cannot say). `service_list` carries `update` only on a service that
differs, and the canvas marks it "Other version". Both versions come from the node's own records and files: no
registry is asked, nothing is written, and it never says which of the two is newer. A virtual-machine template says
where its version is with a new key, `"update": { "pin": "<a key of settings>" }`; the shipped `automagik-workspace`
names `serverImage`. A container's version is its `digest`. It only tells you: the way to the template's version is
still `service_remove` and `service_create`. See "Whether the installed template names another version" in
`docs/OPERATING.md`.

- a service's status says whether the installed template names the same version as the one running, another one, or cannot tell

## 0.9.0 (2026-11-12)

- The README and `docs/QUICKSTART.md` now fetch the installer from the newest release
  (`https://github.com/automagik-si/arca/releases/latest/download/install.sh`) and no longer from the `dev` branch:
  what runs is the released `install.sh`, which that release's `SHA256SUMS` lists.
- A new template, `gitlab-runner-pool`: one virtual machine that runs the CI jobs of one GitLab, self-hosted or
  gitlab.com, with GitLab Runner 19.5.3 (by digest) and the docker executor. One pool serves up to twelve runners
  created in GitLab, each with its own token (`GITLAB_RUNNER_TOKEN_1` to `GITLAB_RUNNER_TOKEN_12`), limit and size of
  job, under one cap of jobs at once; `url`, `concurrent`, `jobCpu`, `jobMemory`, `serviceMemory`, `jobServices`,
  `runners`, `memoryOvercommitPercent` and `reach` are given at create. Every job runs in fresh containers on a network of its own, never
  privileged and never with the Docker daemon's socket; the virtual machine, not the container, is the boundary. The
  pool checks the jobs' memory against what the machine reports and does not start when they do not fit, with the
  numbers (`memoryOvercommitPercent`, 100 to 200, lets the caps add up to more than the machine has, for a mixed
  load whose jobs cannot be told apart); it says in one sentence each why a GitLab is not reached (the name, a timeout, the certificate, a token);
  and its status block has the jobs that run now and each finished job's result. With `reach` `tailnet` the machine
  joins a tailnet (`TAILSCALE_AUTHKEY`) to reach a GitLab that is only there, found by its full tailnet name, by
  ordinary DNS, or by the node `gitlabNode` names. `gitlab-runner-pool-large` is the same
  pool in a machine of 32 CPUs and 64GiB. It has not run against a real GitLab yet: see "A pool of GitLab runners"
  and "The GitLab pool" in `docs/OPERATING.md`.

- a pool of GitLab runners as a template, in two sizes: one machine, up to twelve runner registrations under one limit of jobs, each job capped and on its own network

## 0.8.0 (2026-11-11)

Releases are now cut by themselves: every change that lands on `dev` and ships is released, numbered and published
without anyone writing a version or a tag. The numbers changed with it. A release is `0.N.P`, with no `alpha` word:
`0.8.0` follows `0.1.0-alpha.7`, a change to what the binary is built from takes the next `N`, and a change that ships
without changing the binary takes the next `P`. The canvas package carries the same version as the binary. Arca is
still an alpha for single hosts; the number no longer says so, `docs/OPERATING.md` does.

Each release now carries `install.sh`, the installer, beside the binary. It installs the release it comes with, and
`SHA256SUMS` lists it with the binary: check a download with `sha256sum --check --ignore-missing SHA256SUMS`, and
stop unless it prints `arca-<version>-linux-x86_64: OK`.

- A workspace's own settings are given when it is created: the `automagik-workspace` template lists `reach`,
  `publicUrl` and the three Cloudflare Access settings in `createSettings`, so `service_create` takes them in
  `settings` and no copy of the template is made. `serverImage` is not listed: the image a workspace runs stays the
  template's to pin. `publicUrl` now ships empty, which means "not set".
- The `automagik-workspace` template can be reached on a tailnet instead of through a Cloudflare Tunnel:
  `settings.reach` is `cloudflare` (as before, and when it is left out) or `tailnet`. With `tailnet` the machine
  runs a Tailscale node (Tailscale's own image, by digest) in a network namespace of its own, which serves HTTPS on
  port 443 of the machine's tailnet name, never Funnel, and reaches the gate through a relay on a private network
  that leads to the gate's port alone. The node never shares the server's namespace, where it would hand every
  loopback port of the workspace to the tailnet. Its one secret is `TAILSCALE_AUTHKEY`, handled like the tunnel's token, and the node's identity is kept
  on the machine's disk. The gate then signs its owner in itself, with passkeys, after a first claim; the keeper
  says whether the node is logged in, under which name, and whether the gate is claimed, and never reads the claim
  code. Arca does not yet hand that code to the owner. Both reaches now pin the Automagik server build that has
  the passkey sign-in (`5fe0be6`). With the Cloudflare reach the connector is no longer started beside a gate that
  has no Access settings, where nobody could sign in through it. See "On a tailnet instead" in `docs/OPERATING.md`.
- An installer, `scripts/install.sh`: one command that leaves what `docs/QUICKSTART.md` leaves, a read-only node
  for the user who runs it. It checks the machine first (Linux, x86_64, glibc 2.34 or newer, `curl`, `sha256sum`, a
  runtime directory that is the user's alone, an Incus socket this user may open) and, when something is missing, says
  what and how to fix it and changes nothing. It refuses to run as root. It downloads one named release (the one its
  own `ARCA_VERSION` line names, which is the release it was published with, or the one `--version` names; it never
  asks GitHub which release is the latest), checks the program against the release's `SHA256SUMS` and puts it at
  `~/.local/bin/arca`; an older release is never put over a newer program unless `--version` names it. Where there is
  no config it writes the quickstart's and mints one `viewer` token; a config that exists is not changed. It runs the
  node as `arca.service`, a systemd user unit, and ends with the node's answer to `arca_status` and the two ways to
  connect an assistant. It starts a node only for a config it wrote, or through its own service from an earlier run,
  and only where it can ask the node afterwards: for any other config, and where `systemctl --user` does not answer,
  it starts nothing, prints the one command that starts the node and ends with exit code 3. A node that answers and
  cannot reach Incus is a failure. A second run replaces the program and restarts the service; `--uninstall` removes
  the unit and the program, while the program is still the file it installed, and keeps the config, the tokens and
  the state. It never runs `sudo`, writes only in the user's home, never prints the token, and works from a pipe and in
  a shell with no `XDG_RUNTIME_DIR`. It refuses a user who already has the operating guide's units
  (`arca-watch.service`, `arca-shim.service`). The addresses in the quickstart still read the script from the
  `dev` branch. It is tested against stand-ins, and was run for real only on a machine that already had Incus and
  Arca.
- Two families of refusal have their own codes, so a caller no longer reads the sentence to tell them from any other
  refusal. `capacity`: the call would pass a cap of a tenant or of the node (instances, containers, virtual machines,
  CPUs, memory, disk, tenants, addresses). `rate`: the caller asked too often or already has too much waiting
  (`kernel.tenants.createRate`, the 5 and 20 pending approval requests, the Omni pairing hold-off). Until now all of
  these came back as `refused`. **A caller that matched `refused` for them must match the new codes.** Every sentence
  is unchanged. Fields stand beside the code in the error's `data`. For `capacity`: `scope` (`tenant` or `node`:
  whose cap it is, since a tenant under its own caps can be refused because the node is full), `limit`, and when
  known `have`, `want` and `max` (bytes for memory and disk, a count otherwise). For `rate`: `retryAfterSeconds` when
  the wait is known. Either gains `proposeAgain: true` when it came from running an approved action, whose approval
  is spent. `RateLimited` and `TooManyProposals`, the codes of the two proposal refusals, keep their names;
  `RateLimited` gains `retryAfterSeconds`. The OpenRPC document lists the four codes under `x-arca-refusals`, each
  with its `family` (`capacity` or `rate`) and the schema of its `data`, and dropping a listed code, family or value
  is a break against a released document. The approval page's 403 answers carry the same fields, and its 429 answers
  carry `refusal: "rate"` beside `message`. The table is in `docs/OPERATING.md`, "Refusals for want of room and for
  asking too often".
- The README is now one short page: what Arca is, how to get it, how it works and what it runs, with the Arca logo and
  one diagram (the files are in `assets/readme/`, each with one file for a light page and one for a dark page). Its
  long sections moved into `docs/`: the quickstart is `docs/QUICKSTART.md`, with "Install with one
  command" above its steps by hand; the status table and "What it does today" open `docs/OPERATING.md`; the crate
  table is `docs/ARCHITECTURE.md`; "Build and test" and "Releasing" are `docs/BUILDING.md`. A link to
  `README.md#quickstart-a-read-only-node-in-five-minutes` now stops at the top of the README: the quickstart's address
  is `docs/QUICKSTART.md`.
- A new shipped template, `automagik-workspace`: one Automagik workspace in its own virtual machine (2 CPUs, 4GiB,
  20GiB), published through a Cloudflare Tunnel whose connector runs inside the same machine. A keeper runs the
  Automagik server, its gate (the same image, the only door to the server) and the connector in one network
  namespace, in the shape of Automagik's own compose file; nothing listens on the machine's addresses, and the
  template has no port, no link and no host listener. Its one secret is `TUNNEL_TOKEN`. Its settings carry the
  public address and the owner's Cloudflare Access application (team domain, audience tag, allowed e-mail
  addresses), all three or none. The first boot says on the console what it installs and what failed, and the keeper
  says whether each part is healthy (HTTP 401) and whether the tunnel has registered. The pinned server image is the
  first build with the gate and its Cloudflare Access sign-in. No workspace was brought up on
  a real machine in this work: see "An Automagik workspace in its own virtual machine" in `docs/OPERATING.md` for
  the steps and for what stays unproven.
- Automagik Arca carries the Open Intelligence tag of the [we unfold](https://weunfold.ai/) movement, in its
  one-colour cut with the we unfold mark: in the two READMEs, and at the foot of every page `arca serve` returns (the
  approval page, the canvas, its sign-in page and its error page). On a page it is an inline picture 170 px wide, in
  the page's ink on the page's ground in light and dark, loaded from nowhere and never a link. The files and where they
  come from are in `assets/open-intelligence/`.
- A service may talk to the kernel. A container template declares `kernelAccess: { "port": <n> }`, and its service, in
  the node's own project, finds the kernel's MCP at `http://127.0.0.1:<n>/mcp` inside its instance: one Incus proxy
  device that listens inside the instance and connects to the node's new services socket, `listen.services`. That
  socket serves the MCP routes alone and never the approval page, to the server's own user and to root, so no other
  local user reaches it and it has no port. Nothing crosses the bridge and no ACL rule changes. The token the service
  is given still decides everything: without one every call is 401. Such a service is created only from a proposal the
  owner approved, whose card says the access in words; it is refused in a tenant and on a node that sets no
  `listen.services`. `serve` refuses to start while a service has the access through a socket the config no longer
  serves. `service_list` and `service_status` show it. See "A service that may talk to the kernel" in
  `docs/OPERATING.md` and Rule 69 in `docs/OWNERSHIP.md`.
- `GET /metrics` and `GET /alerts` are held to a token's tool list, as the verbs they read are: a token minted with
  `--tools` needs `node_metrics` for `/metrics` and `alerts_list` for `/alerts`, or the route answers 403. A token
  with no tool list reads both, as before. **If a scraper's token has a tool list, add the verb to it.**
- `serve` binds the services socket only where every directory above it, too, belongs to the server's user or to root
  and cannot be written to by anyone else (a sticky directory such as `/tmp` may be above it).

- the release build no longer takes the runner-pool template's /home/runner paths for a leak, and names any path it does refuse
- versions are cut automatically: a push to dev that changes the product publishes a release with its program, its installer and their checksums
- a short README with the logo and the diagram; the quickstart, the layout and the build notes move into docs (owner's wording, safety lines exact)
- the workspace template takes its address, reach and sign-in settings at create, on the owner's card
- a tenant's block filled by something else is the node's capacity, not the tenant's
- the workspace template can be reached on a tailnet only, through a node in its own network and a relay to the gate
- out-of-room and rate refusals have their own codes, say whose limit it was and when to try again
- the approval card shows every element of a list setting; sizes, numbers and characters in settings are checked before approval
- settings at create for templates that list them, the runner pool's follow-ups, and default sizes that fit
- scripts/install.sh, one command that sets up a read-only node
- a finished folder copy leaves the list before it is journalled; the tests that depended on a machine's speed or on downloaded crates no longer do
- the runner pool's first boot installs the docker command and says what is missing; pool tests can no longer hang; every CI job has a limit
- the read-only web routes follow a token's tool list; the services socket checks the directories above it; a rebuilt record follows the device (security re-check SHIP)
- a template may give its service access to the kernel, through a socket that serves the token-protected API alone
- automagik-workspace, one Automagik workspace in its own virtual machine behind its gate and a tunnel
- the README opens with what Arca is, what works today and what does not yet (wording from the page's fact table)
- a link to a project that does not exist always lands on the default project
- Merge pull request #1 from automagik-si/fix/ci-timing-tests
- service_create takes start: false and creates the service stopped
- the Open Intelligence tag of we unfold, one colour, in the READMEs and in the pages' footers
- a virtual machine in a tenant has its stored secrets written into its guest
- the repository's own CI on Arca's runner pool for trusted events, hosted runners for the rest; a bench against GitHub's runners
- a virtual machine takes a first-boot script, settings and secrets; github-runner-pool template

## 0.1.0-alpha.7

The first release under the name Automagik Arca, and the first with a binary to download since `0.1.0-alpha.2`. The
tags `v0.1.0-alpha.3` to `v0.1.0-alpha.6` were never published as releases, so this section covers everything since
`alpha.2`.

**The name.** The product is now called Automagik Arca. The binary is still `arca`, and nothing a running node reads
has changed: the config keys, the paths, the verb names, the marks on Incus objects and the project names are the same.
The repository is `github.com/automagik-si/arca`.

**New in the binary.**

- `arca --version` (also `arca version` and `arca -V`) prints `Automagik Arca <version> (<short commit>, <target triple>)`.
  `arca_status` now reports this release's version, `0.1.0-alpha.7`, where it used to say `0.1.0`.
- A portable Linux x86_64 binary, linked against glibc 2.34, so it starts on Enterprise Linux 9, Ubuntu 22.04,
  Debian 12 and newer. `scripts/release.sh` builds it with its `SHA256SUMS`.

**Owner approvals.**

- `arca serve` serves an approval page. The owner approves with a passkey, one tap per change. The first passkey is
  enrolled with a one-time code shown on the node's own screen, with no terminal step, and a trusted passkey adds or
  removes another.
- An assistant can propose an action (`action_propose`) or ask for an approval (`owner_approval_request`); the owner
  approves or declines it on the page. The cards say what will happen and, afterwards, exactly what happened, also when
  an approved action then fails.
- One page can approve changes for another kernel on the same machine or on another machine.
- Every destructive verb takes an optional `confirm`.

**Services.**

- Services from templates: create, list, status, logs, removal, connections between services, secrets, and publishing.
  The shipped templates are installed in one step with `arca templates install`, and include a template that publishes
  a service through a Cloudflare Tunnel.
- Services have limits, autostart, data volumes of their own and a LAN port.
- Postgres databases over MCP, with consumer links.
- A service another manager created can be imported in place, and released again.
- The Omni verbs, built on the service model.

**Machines.**

- Virtual machines, on bridged nodes (the host's own bridges, with required caps) and on mixed nodes (containers on the
  managed network beside virtual machines).
- Tenants: isolated service projects on one node, each with its own caps and addresses, with provisional deploys.
- `instance_start`, `instance_stop` and `instance_restart` for Arca's own instances.
- Per-card GPU inventory and readings, and assignment approved by passkey.
- Adoption takes stopped containers too, lists what a container already uses as allowances, and `adopt verify` can
  check a service through an http address.

**Snapshots.**

- The owner plugs in a disk, picks it as snapshot storage, and takes snapshots of apps and data volumes onto it, each
  step approved. Folders are snapshotted through an unprivileged copy helper.

**Health.**

- Alerts: rules in `alerts.json`, `alerts_list`, a HEALTH line, and a Prometheus export (`GET /metrics`, `GET /alerts`).
  Readings cover disk pools, instances and crash loops, connection latency, SMART drive health, and incus-osd updates,
  Secure Boot and TPM.
- The node keeps its metric points and alert events in a SQLite store of its own, with rollups and retention.
- Host metrics read the Incus host's own totals and choose local or remote at runtime. The watcher sweeps on a schedule
  instead of on each call.

**Nodes talking to each other.**

- `arca link`: two nodes pair with a one-time code, keep their membership in sync, and carry MCP calls to each other
  (`arca call --peer <name>`).

**The canvas.**

- `packages/arca-canvas-dsh`, a DSH plugin: machines, projects, services, GPUs, phones and desktops on one canvas, every
  Arca host in one view, a Doctor chat that answers in plain language, and approvals inside the canvas. A remote canvas
  sits behind the owner's passkey.

**Tokens and the MCP door.**

- A token can be limited to a list of tools, and revoked; `SIGHUP` reloads the tokens.
- `journal_list` and `ontology_query` are new reads. The OpenRPC document carries each verb's `readOnlyHint`.

**Not in this release.** There is no `arca init`; Linux x86_64 is the only published binary.

## 0.1.0-alpha.2

The last release published before this one, on 2026-10-30.
