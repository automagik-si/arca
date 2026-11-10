# Arca

Arca is a Rust kernel for a node's own infrastructure. The kernel reads what it owns, plans each change, and applies a
change only after the node's owner has approved it and the change is written to a journal. Each node signs its own
desired state with its own key. It talks to Incus over the local unix socket.

**Status: `0.1.0-alpha.5`, for private single-host use.**

- **Reads** run against a real Incus host (7.3, now 7.5.1): every project and instance is classed by owner, and drift is reported.
- **Writes** are tested live, one object kind at a time, in a scratch project: network, project, ACL, profile, volume,
  forward, instance and bucket are created, marked, and removed again, leaving the host as it was. Network zones are
  not yet created in a kernel project (Incus refuses them in a restricted project without a zone grant).
- **Deletes of class B objects** (networks, projects, volumes) need a signed owner approval. The owner verifier for
  hardware-backed (`-sk`) SSH signatures and a WebAuthn ES256 passkey verifier both ship, but no owner key is installed
  on a new node, so these deletes fail closed with `NoVerifier`.
- **As a service:** `deploy/systemd/` holds two user units that run `arca serve` read-only, reaching Incus only through
  the GET-only shim, with restart and memory limits. See "Running as a user service" in
  [`docs/OPERATING.md`](docs/OPERATING.md).
- **Running operations:** an intent stays open while an Incus operation still names its target, so a restart in the middle
  of a create no longer leaves a marked object the journal does not know about (Rule 44 in
  [`docs/OWNERSHIP.md`](docs/OWNERSHIP.md)).
- **Adoption:** `arca adopt` takes one listed container from the default project into a kernel project of its own and
  back, keeping its MAC and IPv4 by pinning them on its NIC. It resumes from its record after a crash at any step, and
  `arca adopt verify` checks that the service answers. Done live on Incus 7.5.1 for three real services, two running and one stopped, where a project
  move runs as a copy, sometimes with a new uuid; adoption accepts that copy only when its pin and adoption snapshot match.
  The plan lists what the container already uses (host paths, devices, nesting, low-level keys, another bridge) as
  allowances, the digest covers them, and the kernel project allows exactly those. Privileged containers and a NIC other
  than eth0 are refused. A stopped container is adopted with its saved MAC and stays stopped; `adopt verify --url`
  checks a service through an http address, for a container whose firewall admits only its proxy. See "Adopting a container" in
  [`docs/OPERATING.md`](docs/OPERATING.md).

Hardening for shared or distributed hosts comes later.

## Layout

| Crate | What it holds |
|-------|---------------|
| `arca` | the node binary: `watch`, `metrics`, `serve` (the MCP server), `call`, `token mint`, `shim`, `adopt` |
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

A plain `cargo build --release` needs the build machine's glibc or newer on the host that runs the binary.
`scripts/build-portable.sh` builds `arca` against glibc 2.34 (Enterprise Linux 9 and newer) with cargo-zigbuild and zig; both are pinned, checked by SHA-256 and kept under `~/.cache/arca-portable-tools` (or `ARCA_PORTABLE_TOOLS`), with no root. It runs on x86_64 Linux and needs `curl`, `xz`, `sha256sum` and `objdump`.
It prints the zig it used, the binary's path, its SHA-256 and the highest GLIBC symbol version the binary needs, and fails when that version is above 2.34 or cannot be read.
