# Changelog

What changed in each release of Automagik Arca, in plain words. The newest release is first.

## Unreleased

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
- An installer, `scripts/install.sh`: one command that leaves what the README's quickstart leaves, a read-only node
  for the user who runs it. It checks the machine first (Linux, x86_64, glibc 2.34 or newer, `curl`, `sha256sum`, a
  runtime directory that is the user's alone, an Incus socket this user may open) and, when something is missing, says
  what and how to fix it and changes nothing. It refuses to run as root. It downloads one named release
  (`0.1.0-alpha.7`, or the one `--version` names; never GitHub's "latest release" address, which answers 404 while
  every release is a pre-release), checks the program against the release's `SHA256SUMS` and puts it at
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
  (`arca-watch.service`, `arca-shim.service`). The script is not a release asset yet: the README reads it from the
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

**Not in this release.** There is no `arca init` and no stdio MCP door; Linux x86_64 is the only published binary.

## 0.1.0-alpha.2

The last release published before this one, on 2026-10-30.
