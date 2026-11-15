# Architecture

## Layout

| Crate | What it holds |
|-------|---------------|
| `arca` | the node binary: `serve` (the MCP server, the approval page and the canvas routes), `call`, `watch`, `metrics`, `token`, `link`, `adopt`, `service import`, `probe`, `templates`, `store`, `shim`, `update` (the kernel update), `version` |
| `arca-kernel` | canonical JSON, digests, node identity, signed desired state, the ownership model |
| `arca-approval` | signed owner approvals, spent once |
| `arca-incus` | the Incus client over the unix socket, a fake Incus, a GET-only recording shim, and the writer (feature `write`) |
| `arca-providers` | the Incus provider plugins are served through |
| `arca-reconcile` | the single reconciler: applies desired state, reverts removed entries, deletes only under an approved plan |
| `arca-runners` | GitHub Actions runner pools: the scale set protocol, the controller that starts one fresh VM per job, the launchers and a fake GitHub ([`RUNNERS.md`](RUNNERS.md)) |
| `arca-watch` | the watch-only plugin |
| `arca-netproject` | the shared service project and its bridge, and the tenant projects |
| `arca-postgres` | database instances, their ingress ACL and consumer links |
| `arca-metrics` | the host sampler |
| `arca-alerts` | the alert rules, the evaluator on the sampler's tick, and the Prometheus text export |
| `arca-store` | the node's own SQLite store: metric points and the alert event log |
| `arca-link` | the node link: one endpoint, one-time pairing and byte streams to local sockets |
| `arca-api`, `arca-mcp` | the verb table, the OpenRPC document, and the MCP server |
| `arca-wit` | the WIT world `arca:plugin@0.1.0` |
| `arca-janitor` | a polite, job-based cleanup runner (`arca-janitor`): space reports, growth, clean plans applied by id with every protection checked again, and rule-based insights on where the trash comes from; it depends on no other Arca crate yet ([`README`](../crates/arca-janitor/README.md)) |

Those are the 18 crates under `crates/`. Beside them: `packages/arca-canvas-dsh` (the canvas plugin, a Node package),
`templates/` (the shipped service templates), `schema/` (the OpenRPC document, `arca.openrpc.json`, and the released
`0.1.0` one it is checked against), `ontology/`, `wit/`, `deploy/systemd/` and `scripts/`.

The API version, `0.1.0`, is the version of the verb contract in `schema/`. It moves on its own, not with each release.

## The kernel update

Arca updates its own programs with nobody at a terminal (`crates/arca/src/self_update.rs`; operating notes in
[`OPERATING.md`](OPERATING.md), "Keeping Arca up to date: the kernel update"). The parts and why they are split:

- **The schedule is in `serve`; the run is not.** A run restarts the very units that run `serve`, so `serve` only
  decides that a look is due and starts `arca update run` as a transient unit of its own systemd manager. The run is
  one process from the look to the rollback, and nothing it does depends on the node it restarts staying up.
- **The machine, not the node, is the unit of an update.** One machine can run several Arca programs: the installer's
  `arca.service`, the guide's hand-made shim and node, links, a second node with its own copy of the program. The run
  finds them all from the manager's `ExecStart` lines and an `Automagik Arca` version answer, groups them by the file
  their paths resolve to, and replaces each file behind the release. A lock in the manager's runtime directory keeps
  it to one run at a time, and its result is written into every node's state directory it found.
- **Order and proof.** Shims restart first, then nodes, then links, one at a time; a restart counts only when the unit's
  main process is up, stays up, and its own `/proc/<pid>/exe` says the release's version. Anything else puts every
  changed program back and restarts the units again. The proof is the running process, not the file on disk.
- **The owner's attention wins.** A node with an owner approval card waiting is not restarted: each `serve` writes its
  count of waiting cards each minute, and the run waits for zero. A release is checked against its `SHA256SUMS` (the
  binary and the release's own `install.sh`) and must answer `--version` on this machine before any program is touched.
- **A program that cannot be written is staged.** Arca OS runs Arca from its read-only image. There the run stages the
  release in the node's state directory with a manifest and its SHA-256, and `serve` hands over to it (`exec`) at its
  start when it is newer than itself. The OS update and the kernel update stay independent: whichever program is newer
  runs, so neither can move the node backwards.

To build, test and release Arca, read [`docs/BUILDING.md`](BUILDING.md).
