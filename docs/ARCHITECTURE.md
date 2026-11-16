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
- **Order and proof.** Shims restart first, then nodes, then links, one at a time. A restart counts only when the
  unit is up within `update.healthSeconds`, a node answers on its own socket, the unit then stays the same process with
  no restart for `update.healthSeconds` more, and its own `/proc/<pid>/exe` says the release's version. Anything else
  puts every changed program back and restarts the units again. The proof is the running process, not the file on disk.
- **A run can be cut off.** Before the first rename the run writes `pending.json` into every node's record, and removes
  it when every unit is confirmed or rolled back. A run that finds one settles it first: it finishes the update when
  every unit it names is healthy on the release, and rolls it back otherwise. A release that was rolled back is held
  back until a newer one is published, so a bad release is not retried every six hours.
- **Only a signed release installs.** `SHA256SUMS.sig` must be an Ed25519 signature of `SHA256SUMS` by a key the program
  holds (`RELEASE_KEYS`, verified with ring); the binary and the release's own `install.sh` must match `SHA256SUMS`, and
  the binary must answer `--version` on this machine, before any program is touched. The release workflow signs with a
  key from an environment secret. No key exists yet, so today a node only reports what is available.
- **Only release builds are replaced.** A program older than the release is replaced only when its bytes are the
  published binary of the version it says. A build from a branch is kept, so a node running a branch is never moved
  onto a release it did not ask for.
- **The owner's attention wins.** A node with an owner approval card waiting is not restarted: each `serve` writes its
  count of waiting cards each minute, and the run waits for zero.
- **Where the program cannot be written.** A program on a read-only file system is the OS image's: Arca OS brings its
  `arca` with the OS update, and the kernel update leaves it alone (`os-managed`). A program in a directory only its
  user may not write (a user unit running a program in a system path) gets the release staged in that user's own
  state, with a manifest and its SHA-256; `serve`, `shim` and `link serve` hand over to it (`exec` of the very file
  they hashed) at their start when it is newer than themselves. Staging is refused when another unit runs the program
  and would not hand over.

To build, test and release Arca, read [`docs/BUILDING.md`](BUILDING.md).
