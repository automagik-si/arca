# Automagik Arca

<a href="https://weunfold.ai/"><picture><source media="(prefers-color-scheme: dark)" srcset="assets/open-intelligence/open-intelligence-badge-ivory.svg"><img src="assets/open-intelligence/open-intelligence-badge-ink.svg" alt="Open Intelligence" width="170" height="32"></picture></a>

**The infrastructure layer for agentic AI.** Agents need infrastructure. Own *yours*.

Automagik Arca runs your services on machines you own. Set up the way the quickstart shows, an assistant can only look and propose. You approve with your passkey.

- Arca is one program. Arca is open source under the Apache-2.0 licence.
- A fresh install only looks: it changes nothing until you tell it which parts are its own.
- Arca is self-hosted: it runs on machines you own, and Arca itself needs no outside service to work.
- Arca keeps an append-only journal of the changes it applies. Starts, stops and restarts are logged separately.

**Status: alpha.** Arca is an alpha, for one machine at a time. Today it needs a Linux machine (x86_64) with Incus, and a person comfortable with a terminal. The current version is on the [releases page](https://github.com/automagik-si/arca/releases).

| Works today | Not yet |
|---|---|
| Ready to run today: Postgres, NATS, Omni, Automagik, a Cloudflare tunnel and a workspace virtual machine. | There is no installer yet: the first setup is written by hand. |
| Arca can save snapshots of a service onto a disk you plug in. | Arca cannot yet restore a snapshot for you: restoring is done by hand with Incus. |
| Arca speaks MCP, so an assistant can ask it questions and propose changes. | Arca cannot yet run any app or container image you name: a service comes from a ready template, or Arca takes over one that already runs on the machine. |
| Two machines can be paired with a one-time code. | The ChatGPT and Claude apps cannot connect to Arca in one click yet. |
| Arca checks passkeys itself; no outside sign-in service is involved. | Arca does not repair things by itself. |

[Start in five minutes](#quickstart-a-read-only-node-in-five-minutes) · [The full guide](docs/OPERATING.md) · [What changed](CHANGELOG.md)

## Install with one command

[`scripts/install.sh`](scripts/install.sh) does the quickstart below for you, for the user who runs it. It needs what
the quickstart needs (x86_64 Linux with glibc 2.34 or newer, a running Incus that this user can reach) and `curl` and
`sha256sum`.

**Download it, read it, run it.**

```sh
curl -fsSLO https://raw.githubusercontent.com/automagik-si/arca/dev/scripts/install.sh
less install.sh
sh install.sh
```

**Or in one line.** The script runs as soon as it has arrived, and until a release carries it this address follows
the `dev` branch, so what runs is whatever that branch holds at that moment. Use this only when you trust the address:

```sh
curl -fsSL https://raw.githubusercontent.com/automagik-si/arca/dev/scripts/install.sh | sh
```

To hand it to a coding agent, paste this: *Download
`https://raw.githubusercontent.com/automagik-si/arca/dev/scripts/install.sh`, read it, run it with `sh`, and tell me
what it printed.*

- **It checks first.** Linux, x86_64, glibc, `curl`, `sha256sum`, a runtime directory that is yours alone, and an Incus
  socket this user may open. When one is missing it says which and how to fix it, and stops before it has changed
  anything. It never runs `sudo`, it refuses to run as root, and it writes only in your home directory.
- **It installs one named release**, the one the quickstart names, at `~/.local/bin/arca`.
  `sh install.sh --version <version>` names another; without it, an older release is never put over a newer program.
  The program is checked against that release's `SHA256SUMS`. That catches a download that was damaged or cut short. It
  does not catch a release that was tampered with, because the checksums come from the same release.
- **It writes the quickstart's config and mints one `viewer` token**, only where there is no config. A config that
  exists is not changed.
- **It runs the node as a service**, `arca.service`, a systemd user unit, and only for a config it wrote. For a config
  it did not write, and where `systemctl --user` does not answer, it starts nothing, prints the one command that starts
  the node, and ends with exit code 3: the program is installed and no node was started.
- **It ends with the node's own answer** to `arca_status`: the mode (`watch-only`), where the token's file is (never the
  token), and the two ways to connect an assistant from step 4 below. Exit code 0 means the node runs, answered, and
  reaches Incus.
- **A second run** replaces the program in place and restarts the service. `sh install.sh --uninstall` removes the
  service and the program, while the program is still the file it installed, and keeps the config, the tokens and the
  state.

**Not yet:** the script is read from the `dev` branch, so no release's checksum covers the script itself. It is tested
against stand-ins for the download, the program and systemd, and it was run for real on one machine that already had
Incus and Arca; it has not been run on a machine that never had either. There is no installer for a Mac.

## Quickstart: a read-only node in five minutes

You need an x86_64 Linux host with glibc 2.34 or newer (Enterprise Linux 9, Ubuntu 22.04, Debian 12, or newer), a running
Incus, and a user who can read the Incus socket (on most installs, a member of the `incus-admin` group). This quickstart
names no kernel project, so the node only reads: it sends Incus nothing but GET.

**1. Download the binary and check it.**

```sh
v=0.1.0-alpha.7
base=https://github.com/automagik-si/arca/releases/download/v$v
curl -fL -O "$base/arca-$v-linux-x86_64" -O "$base/SHA256SUMS"
sha256sum --check SHA256SUMS
install -D -m 0755 "arca-$v-linux-x86_64" ~/.local/bin/arca
arca --version
```

`sha256sum` must answer `arca-0.1.0-alpha.7-linux-x86_64: OK`. `arca --version` prints
`Automagik Arca 0.1.0-alpha.7 (<commit>, x86_64-unknown-linux-gnu)`; if the shell does not find `arca`, put
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

The answer is one JSON object with `"mode":"watch-only"`, `"incusReachable":true` and `"version":"0.1.0-alpha.7"`.
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

`scripts/release.sh` builds the release assets and publishes nothing. From a clean tree whose HEAD is the tag `v<version>`
(the version is the one under `[workspace.package]` in `Cargo.toml`), it runs `scripts/build-portable.sh`, checks that the
binary's `--version` names that version and commit, and writes `dist/arca-<version>-linux-x86_64`, `dist/SHA256SUMS` and
`dist/RELEASE-NOTES.md` (that version's section of `CHANGELOG.md`). It then prints the `gh release create` command for
those files and the glibc the binary needs; it does not run it. `scripts/release.sh --rehearse` builds and checks the
same files without the tag, to try the script before the tag exists, and prints no publish command.
