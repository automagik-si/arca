# Changelog

What changed in each release of Automagik Arca, in plain words. The newest release is first.

## Unreleased

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
