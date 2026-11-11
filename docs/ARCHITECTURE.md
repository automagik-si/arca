# Architecture

## Layout

| Crate | What it holds |
|-------|---------------|
| `arca` | the node binary: `serve` (the MCP server, the approval page and the canvas routes), `call`, `watch`, `metrics`, `token`, `link`, `adopt`, `service import`, `probe`, `templates`, `store`, `shim`, `version` |
| `arca-kernel` | canonical JSON, digests, node identity, signed desired state, the ownership model |
| `arca-approval` | signed owner approvals, spent once |
| `arca-incus` | the Incus client over the unix socket, a fake Incus, a GET-only recording shim, and the writer (feature `write`) |
| `arca-providers` | the Incus provider plugins are served through |
| `arca-reconcile` | the single reconciler: applies desired state, reverts removed entries, deletes only under an approved plan |
| `arca-watch` | the watch-only plugin |
| `arca-netproject` | the shared service project and its bridge, and the tenant projects |
| `arca-postgres` | database instances, their ingress ACL and consumer links |
| `arca-metrics` | the host sampler |
| `arca-alerts` | the alert rules, the evaluator on the sampler's tick, and the Prometheus text export |
| `arca-store` | the node's own SQLite store: metric points and the alert event log |
| `arca-link` | the node link: one endpoint, one-time pairing and byte streams to local sockets |
| `arca-api`, `arca-mcp` | the verb table, the OpenRPC document, and the MCP server |
| `arca-wit` | the WIT world `arca:plugin@0.1.0` |

Those are the 16 crates under `crates/`. Beside them: `packages/arca-canvas-dsh` (the canvas plugin, a Node package),
`templates/` (the shipped service templates), `schema/` (the OpenRPC document, `arca.openrpc.json`, and the released
`0.1.0` one it is checked against), `ontology/`, `wit/`, `deploy/systemd/` and `scripts/`.

The API version, `0.1.0`, is the version of the verb contract in `schema/`. It moves on its own, not with each release.

To build, test and release Arca, read [`docs/BUILDING.md`](BUILDING.md).
