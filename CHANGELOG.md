# Changelog

What changed in each release of Automagik Arca, in plain words. The newest release is first.

## Unreleased

<!-- The release writes the next section here by itself: one line per change that landed, from its commit subject.
Write under this comment only what a reader needs beyond those lines; the release puts its heading above it. -->

Releases are now cut by themselves: every change that lands on `dev` and ships is released, numbered and published
without anyone writing a version or a tag. The numbers changed with it. A release is `0.N.P`, with no `alpha` word:
`0.8.0` follows `0.1.0-alpha.7`, a change to what the binary is built from takes the next `N`, and a change that ships
without changing the binary takes the next `P`. The canvas package carries the same version as the binary. Arca is
still an alpha for single hosts; the number no longer says so, the README does.

- Automagik Arca carries the Open Intelligence tag of the [we unfold](https://weunfold.ai/) movement, in its
  one-colour cut with the we unfold mark: in the two READMEs, and at the foot of every page `arca serve` returns (the
  approval page, the canvas, its sign-in page and its error page). On a page it is an inline picture 170 px wide, in
  the page's ink on the page's ground in light and dark, loaded from nowhere and never a link. The files and where they
  come from are in `assets/open-intelligence/`.

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
