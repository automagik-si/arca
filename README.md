# Automagik Arca

Automagik Arca is a Rust kernel for a node's own infrastructure. The kernel reads what it owns, plans each change, and
applies a change only after the node's owner has approved it and the change is written to a journal. Each node signs its
own desired state with its own key. It talks to Incus over the local unix socket. The program is one binary, `arca`.

**Status: an alpha for single hosts.** Hardening for shared or distributed hosts comes later. The newest release is
on the [releases page](https://github.com/automagik-si/arca/releases/latest), and [`CHANGELOG.md`](CHANGELOG.md) says
what changed in each one.

## Quickstart: a read-only node in five minutes

You need an x86_64 Linux host with glibc 2.34 or newer (Enterprise Linux 9, Ubuntu 22.04, Debian 12, or newer), a running
Incus, and a user who can read the Incus socket (on most installs, a member of the `incus-admin` group). This quickstart
names no kernel project, so the node only reads: it sends Incus nothing but GET.

**1. Download the binary and check it.**

```sh
v=$(curl -fsSL -o /dev/null -w '%{url_effective}' https://github.com/automagik-si/arca/releases/latest | sed -n 's#.*/releases/tag/v##p')
echo "the newest release is ${v:?none is marked Latest; pick a version on the releases page and set v to it}"
base=https://github.com/automagik-si/arca/releases/download/v$v
curl -fL -O "$base/arca-$v-linux-x86_64" -O "$base/SHA256SUMS"
sha256sum --check SHA256SUMS
install -D -m 0755 "arca-$v-linux-x86_64" ~/.local/bin/arca
arca --version
```

The first two lines ask GitHub which release is the newest. `sha256sum` must answer
`arca-<version>-linux-x86_64: OK`. `arca --version` prints
`Automagik Arca <version> (<commit>, x86_64-unknown-linux-gnu)`; if the shell does not find `arca`, put
`~/.local/bin` on your `PATH`.

**2. Write the smallest config.** These five keys are the required ones. The two directories must be mode 0700.

```sh
mkdir -p ~/.config/arca/tokens ~/.local/state/arca-node
chmod 0700 ~/.config/arca/tokens ~/.local/state/arca-node
cat > ~/.config/arca/node.json <<EOF
{
  "stateDir": "$HOME/.local/state/arca-node",
  "incus": { "socket": "/var/lib/incus/unix.socket" },
  "listen": { "unix": "$XDG_RUNTIME_DIR/arca.sock" },
  "tokens": { "hashDir": "$HOME/.config/arca/tokens" },
  "kernel": { "droiddProjects": [] }
}
EOF
```

`kernel.droiddProjects` names the Incus projects that belong to droidd, another manager Arca stays out of; leave it empty
when the host has none. `listen.unix` is a unix socket path, so it must stay under about 100 characters; a login
session sets `XDG_RUNTIME_DIR`.

**3. Mint a token and start the node.** `serve` reads its tokens when it starts, so mint first.

```sh
arca token mint --scope viewer --id assistant-1 --config ~/.config/arca/node.json --out ~/.config/arca/assistant-1.secret
arca serve --config ~/.config/arca/node.json
```

`serve` runs until Ctrl-C. From a second terminal, ask it for its status:

```sh
arca call arca_status --config ~/.config/arca/node.json --token-file ~/.config/arca/assistant-1.secret
```

The answer is one JSON object with `"mode":"watch-only"`, `"incusReachable":true` and the release's `"version"`.
`arca call watch_report ...` lists every project and instance with its owner.

**4. Connect an assistant over MCP.** The node is an MCP server over streamable HTTP: `POST /mcp`, JSON answers, protocol
version `2025-11-25`, and a bearer token on every request. With the `viewer` token from step 3, this read-only node lists
18 tools, among them `arca_status`, `node_metrics`, `watch_report`, `alerts_list` and `service_list`. Two ways to reach it
work today:

- **An assistant with a shell on the node** runs `arca call <verb>` as in step 3. `arca call` is an MCP client over the
  unix socket, and the socket answers only the user who runs `serve`.
- **An assistant that takes an MCP server URL** (any client with an HTTP transport and a custom header) needs a TCP
  address, and the node opens one only when the config names it: `listen.tailnet`, one address on the host's tailnet, or
  `listen.proxy`, one address for a reverse proxy on the same host that adds TLS and a host name. The client's entry is
  then the URL and the token:

  ```json
  {
    "mcpServers": {
      "arca": {
        "type": "http",
        "url": "http://<tailnet address>:<port>/mcp",
        "headers": { "Authorization": "Bearer <content of assistant-1.secret>" }
      }
    }
  }
  ```

  Behind a reverse proxy the URL is `https://<host name>/mcp`. Both listeners are off in the config above;
  ["The config file"](docs/OPERATING.md#the-config-file) and
  ["Publishing through a reverse proxy"](docs/OPERATING.md#publishing-through-a-reverse-proxy-each-step-approved-by-the-owner-at-that-moment)
  in the operating guide say what each address may be and how to turn it on. A request that carries an `Origin` header
  is refused, so a web page cannot call the node.

**Not yet:** there is no `arca init`, so the config and its directories are written by hand as above; there is no stdio
MCP door, so a client that can only start a local command cannot connect; and Linux x86_64 is the only published binary.

To let the node change things (kernel projects, the owner's passkey and the approval page), and to run it as a service,
read [`docs/OPERATING.md`](docs/OPERATING.md).

## What it does today

- **Reads.** Every project and instance is classed by owner and drift is reported (`watch_report`); host, pool, instance
  and per-card GPU readings (`node_metrics`); alerts with a store of their own and a Prometheus export (`alerts_list`,
  `GET /metrics`, `GET /alerts`).
- **Writes.** A node whose config names kernel projects runs services from templates, Postgres databases, virtual
  machines on bridged and mixed nodes, tenants in projects of their own, GPU assignment, instance start, stop and
  restart, and snapshots onto a disk the owner plugs in. A `viewer` token only reads and proposes: a change an assistant
  proposes waits for the owner, who approves it with a passkey on the approval page that `arca serve` serves. A delete of
  a network, a project or a volume needs a signed owner approval. A node with no kernel project is watch-only and
  writes nothing.
- **Passkeys.** Arca is its own passkey verifier (WebAuthn ES256). The first passkey is enrolled with a one-time code
  shown on the node's own screen, and a trusted passkey adds or removes another. Until a passkey is trusted, every
  approval fails closed.
- **Adoption and import.** `arca adopt` takes one listed container from the default project into a kernel project of its
  own and back, keeping its MAC and IPv4. `service_import` takes over, in place, a service another manager created.
- **The link.** `arca link` pairs two nodes with a one-time code and carries MCP calls between them
  (`arca call --peer <name>`).
- **The canvas.** [`packages/arca-canvas-dsh`](packages/arca-canvas-dsh) is a DSH plugin that draws the node's machines,
  projects and services, with a chat and in-canvas approvals.
- **As a service.** `deploy/systemd/` holds two user units that run `arca serve` read-only, reaching Incus only through
  the GET-only shim, with restart and memory limits.

[`docs/OPERATING.md`](docs/OPERATING.md) is the reference for all of it, and [`docs/OWNERSHIP.md`](docs/OWNERSHIP.md)
states what the kernel may touch.

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

## Releasing

Nobody writes a version or types a tag. Every push to `dev` starts the release workflow
([`.github/workflows/release.yml`](.github/workflows/release.yml)), which cuts a release by itself when something that
ships has changed:

1. It runs the gates: `cargo fmt`, `cargo clippy`, the tests, and the canvas package's tests.
2. It works out the version from the release tags (`scripts/version.sh`). The whole repository carries one version,
   `0.N.P`. A change to anything the binary is built from is a full release, `0.N.0`, with `N` one above the last. A
   change that ships without changing the binary (the canvas package, the units, the schema) is a patch,
   `0.N.(P+1)`. A change to documents, tests or the workflows alone releases nothing. The lists are at the top of
   `scripts/version.sh`.
3. It writes the version into the manifests and the lock files, writes that version's section of `CHANGELOG.md`,
   commits both as `chore(release): v<version>` and tags the commit `v<version>`.
4. It builds the release files at the tag with `scripts/release.sh`, pushes `dev` and the tag together, and publishes
   the GitHub release with `arca-<version>-linux-x86_64` and `SHA256SUMS`.
5. `main` moves to the release commit. `main` holds released commits and nothing else.

If any step before the push fails, no version is taken. If the push landed and the publishing failed, the next run
publishes that same tag before it does anything else; start one by hand with `gh workflow run release.yml --ref dev`.
Add `--field prerelease=true` to publish a release that is not shown as Latest.

**What a contributor writes.** Commit subjects in the form `type(scope): what changed`, in plain words: the release
lists them in `CHANGELOG.md`, one line per change that landed on `dev`. A change that needs more than its line gets a
few sentences under `## Unreleased` in `CHANGELOG.md`; the release moves them to the top of its section. Nothing else:
no version, no tag, no heading.

After each release `dev` on GitHub is one commit ahead of a local `dev`, by the release commit: fast-forward before
the next push.

The release commit has no CI run of its own: it is pushed with the workflow's own token, and GitHub starts no workflow
for such a push. The cut's own gate covers it. It ran on the commit's parent, and the release commit adds only the
version and the changelog section. For the same reason the push stays on the workflow's token: with a personal or an
app token the release commit would start the workflows again. The release workflow always runs on GitHub's hosted
runners, never on a self-hosted one, whose kept cache beside a write token would give a pull request a way to write
`dev`.

`scripts/release.sh` is the one way the release files are built, and it publishes nothing. From a clean tree whose
HEAD is the tag `v<version>` it runs `scripts/build-portable.sh`, checks that the binary's `--version` names that
version and commit, and writes `dist/arca-<version>-linux-x86_64`, `dist/SHA256SUMS` and `dist/RELEASE-NOTES.md` (that
version's section of `CHANGELOG.md`). `scripts/release.sh --rehearse` builds and checks the same files without the
tag, to try a build by hand.
