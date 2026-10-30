# Arca

Arca is a Rust kernel for a node's own infrastructure. The kernel reads what it owns, plans each change, and applies a
change only after the node's owner has approved it and the change is written to a journal. Each node signs its own
desired state with its own key. It talks to Incus over the local unix socket.

**Status: `0.1.0-alpha.1`, for private single-host use.**

- **Reads** run against a real Incus 7.3 host: every project and instance is classed by owner, and drift is reported.
- **Writes** are tested live, one object kind at a time, in a scratch project: network, project, ACL, profile, volume,
  forward, instance and bucket are created, marked, and removed again, leaving the host as it was. Network zones are
  not yet created in a kernel project (Incus refuses them in a restricted project without a zone grant).
- **Deletes of class B objects** (networks, projects, volumes) need a signed owner approval. The owner verifier for
  hardware-backed (`-sk`) SSH signatures ships, but no owner key is installed on a new node, so these deletes fail closed
  with `NoVerifier`.
- **As a service:** `deploy/systemd/` holds two user units that run `arca serve` read-only, reaching Incus only through
  the GET-only shim, with restart and memory limits. See "Running as a user service" in
  [`docs/OPERATING.md`](docs/OPERATING.md).
- **Known gap:** a restart while an Incus operation is still running can leave a marked object the journal does not
  know about.

Hardening for shared or distributed hosts comes later.

## Layout

| Crate | What it holds |
|-------|---------------|
| `arca` | the node binary: `watch`, `metrics`, `serve` (the MCP server), `call`, `token mint`, `shim` |
| `arca-kernel` | canonical JSON, digests, node identity, signed desired state, the ownership model |
| `arca-approval` | signed owner approvals, spent once |
| `arca-incus` | the Incus client over the unix socket, a fake Incus, a GET-only recording shim, and the writer (feature `write`) |
| `arca-providers` | the Incus provider plugins are served through |
| `arca-reconcile` | the single reconciler: applies desired state, reverts removed entries, deletes only under an approved plan |
| `arca-watch` | the watch-only plugin |
| `arca-metrics` | the host sampler |
| `arca-api`, `arca-mcp` | the verb table, the OpenRPC document, and the MCP server |
| `arca-wit` | the WIT world `arca:plugin@0.1.0` |

Specs: [`docs/OWNERSHIP.md`](docs/OWNERSHIP.md) (what the kernel may touch) and [`docs/OPERATING.md`](docs/OPERATING.md)
(the binary).

## Build and test

The toolchain is pinned in `rust-toolchain.toml`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace
cargo clippy --workspace -- -D warnings
```
