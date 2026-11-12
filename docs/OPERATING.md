# Operating the `arca` binary

The `arca` binary is Automagik Arca's one program. It runs one node's one-shot host reads, mints scoped bearer tokens, runs
the recording shim, and serves the Arca MCP. Every command that needs the node reads one JSON config file, given with
`--config`.

## Status

Automagik Arca runs your services on machines you own. Set up the way the quickstart shows, an assistant can only look and propose. You approve with your passkey.

- Arca is one program. Arca is open source under the Apache-2.0 licence.
- A fresh install only looks: it changes nothing until you tell it which parts are its own.
- Arca is self-hosted: it runs on machines you own, and Arca itself needs no outside service to work.
- Arca keeps an append-only journal of the changes it applies. Starts, stops and restarts are logged separately.

**Status: alpha.** Arca is an alpha, for one machine at a time. Today it needs a Linux machine (x86_64) with Incus, and a person comfortable with a terminal. The current version is on the [releases page](https://github.com/automagik-si/arca/releases).

| Works today | Not yet |
|---|---|
| Ready to run today: Postgres, NATS, Omni, Automagik, a Cloudflare tunnel and a workspace virtual machine. | The installer sets up a read-only node only: letting Arca change things is still set up by hand. |
| Arca can save snapshots of a service onto a disk you plug in. | Arca cannot yet restore a snapshot for you: restoring is done by hand with Incus. |
| Arca speaks MCP, so an assistant can ask it questions and propose changes. | Arca cannot yet run any app or container image you name: a service comes from a ready template, or Arca takes over one that already runs on the machine. |
| Two machines can be paired with a one-time code. | The ChatGPT and Claude apps cannot connect to Arca in one click yet. |
| Arca checks passkeys itself; no outside sign-in service is involved. | Arca does not repair things by itself. |

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
- **Functions.** A program in an OCI image by digest, run on a timer in a container of its own that is started for
  each run and stopped after it, with a time, a memory and a CPU limit, secrets given by name, and a network card that
  reaches only what the owner's card names ([Functions](#functions)).
- **Passkeys.** Arca is its own passkey verifier (WebAuthn ES256). The first passkey is enrolled with a one-time code
  shown on the node's own screen, and a trusted passkey adds or removes another. Until a passkey is trusted, every
  approval fails closed.
- **Adoption and import.** `arca adopt` takes one listed container from the default project into a kernel project of its
  own and back, keeping its MAC and IPv4. `service_import` takes over, in place, a service another manager created.
- **The link.** `arca link` pairs two nodes with a one-time code and carries MCP calls between them
  (`arca call --peer <name>`).
- **The canvas.** [`packages/arca-canvas-dsh`](../packages/arca-canvas-dsh) is a DSH plugin that draws the node's machines,
  projects and services, with a chat and in-canvas approvals.
- **As a service.** `deploy/systemd/` holds two user units that run `arca serve` read-only, reaching Incus only through
  the GET-only shim, with restart and memory limits.

`docs/OPERATING.md` is the reference for all of it, and [`docs/OWNERSHIP.md`](OWNERSHIP.md)
states what the kernel may touch.

## Commands

`--config` and `--token-file` have defaults, the files the installer writes: `~/.config/arca/node.json` and
`~/.config/arca/assistant-1.secret`. The order is the flag, then the variable (`ARCA_CONFIG`, `ARCA_TOKEN_FILE`), then
the default, and every command finds its config this way. A command that writes to a node or acts on it, and was not
given `--config`, first says on stderr which config it uses and how it was found (`arca: using the config <path> (the
default)` or `(from ARCA_CONFIG)`). A command that removes or cuts something off (`token revoke`, `link remove`, `probe
consumer remove`, `adopt apply`, `adopt release`, `init --watch-only`, `templates install --replace`) also names the node's
state directory in that line. A command that
only reads says nothing, and a config named with `--config` is not said back. One thing has no default: a call with
`--peer` sends its token to another node, so its `--token-file` is always named. `token mint` and `token revoke` with
`--hash-dir` alone read no config at all, not even the variable's.

**The variable alone never picks the node for a command that hands out, removes or rewrites something.** `token mint`,
`token revoke`, `token reload`, `call --peer`, `link pair`, `link join`, `link remove`, `probe consumer remove`, `adopt
apply`, `adopt release` and both modes of `init` would succeed on whatever node `ARCA_CONFIG` was left set for, and the installer mints the same token id on every
node. So with the
variable set and no `--config`, they go on only when the variable names the same file as the default. When it names
another file they refuse with exit 2, do nothing, and say the flag to run them with:
`ARCA_CONFIG names <path>, not the default config (<default>). ... run the same command with: --config <path>`. With
`--config`, or with no variable set, nothing changes. `templates install --replace` overwrites templates that were
edited, so it names the node's state directory in its first line as the removing commands do.

A writing command also refuses a config it found by itself that belongs to another user, and says whose it is. This is
root with a user's home directory kept (`sudo` without `-H`): the default path would lead to that user's node. Run the
command as that user, or name the config with `--config`.

`token mint` writes the hash and sends no signal: a node reads its tokens when it starts and on SIGHUP, so a running
node takes a new token after `arca token reload` or a restart.

| Command | Reads | Prints |
|---------|-------|--------|
| `arca status [--json \| --security] [--config <file>] [--token-file <file>]` | the running node, with the same call as `arca call arca_status` | the node in words: version, mode, uptime, whether Incus answers, the unix socket it answered on, the loopback door as the node itself reports it (open with its URL, or CLOSED with the reason), the other listeners the config file names (not checked); then approvals, from the node's own answer: whether changes are on, the page that approves and whether it is open, whether a passkey is trusted yet, and then two last lines: `Next:`, the ONE thing to do (a node that only looks names `arca init`; with no passkey, the page and its button at a terminal with a screen, and `arca setup-code` with the `ssh` line at one without), and `More:`, which names `arca status --security`. With `--json`, the `arca_status` result as it came. With `--security`, everything that is not the next step, from the config and the token directory alone, so the node need not be running: the cautions for each door this config opens, what a node that may change things may touch, which tokens do not wait for the page and what they may do, where the templates and the config `arca init` kept are, the way to the page from another computer, what a phone needs, and what a node set back to watch-only keeps |
| `arca watch --once --json [--config <file>]` | every instance in every project, through Incus GET | the `watch_report` result |
| `arca metrics --json [--sample-for <s>] [--config <file>]` | the host and the configured pools | the `node_metrics` result |
| `arca store check [--config <file>]` | nothing from the node: it opens a scratch store in the state directory and removes it | the SQLite version and the pragmas the store reads back, as JSON; see "The store" |
| `arca templates install [--config <file>] [--replace]` | nothing from Incus: it writes the shipped service templates, each with its entrypoint or first-boot script, into `<stateDir>/templates` | `{ "dir", "installed", "unchanged", "kept", "replaced" }` as JSON. A file that is an earlier release's own, byte for byte, is brought up to this release's and named under `replaced`; any other file that differs is the owner's edit and is kept unless `--replace` is given |
| `arca probe consumer add --name <n> --image <fingerprint> [--config <file>]` / `remove --name <n> [--config <file>]` | locks desired state; add reads the default profile root pool with GET, then proposes a consumer entry | entry id and sequence |
| `arca serve [--config <file>]` | runs until SIGTERM or SIGINT; SIGHUP reloads the token store only | nothing |
| `arca init --enable-changes [--config <file>] [--yes] [--service-cidr <a.b.c.0/24>]` | the config, the machine's routes and addresses, and Incus through GET | the plan and two questions; it writes the config's missing keys after a yes, and ends in five lines: what changed, who does not wait for the page, the one next step, where a database comes from, and one pointer to the rest. See "Turning on changes" |
| `arca init --watch-only [--config <file>]` | the config | it empties `kernel.projects` and nothing else, and says in a few lines whether the running node took that up; until it has, the first line says the node still makes changes |
| `arca setup-code [--config <file>]` | asks the running node, on its unix socket, for the setup code of the first passkey; for a machine with no screen | `{ "code", "expiresInSeconds", "page" }` as JSON on stdout, and one sentence on stderr; see "The page on this machine alone" |
| `arca link serve [--config <file>]` | runs the node's link until SIGTERM or SIGINT | nothing on a clean run |
| `arca link pair [--config <file>] [--minutes <n>]` | the running link's status | `{ "link", "expires" }` as JSON on stdout; a terminal QR on stderr |
| `arca link join [--] <link> [--config <file>]` | pairs with the endpoint in the link | `{ "id", "name" }` as JSON |
| `arca link peers [--config <file>]` | the paired peers file | the peers array, as JSON |
| `arca link remove [--] <name> [--config <file>]` | removes every peer with that name through the running link, or from disk when stopped | `{ "removed": <count> }` as JSON |
| `arca call <verb> [--config <file>] [--token-file <file>]`, with `--args <json>` or `--args-file <file>`; `arca call` alone lists the verbs | one tool call against the local node | the verb's result, as JSON |
| `arca call --peer <name> <verb> [--config <file>] --token-file <file>`, with `--args <json>` or `--args-file <file>` | one tool call through a paired peer. The token is sent to the peer, so it is one the peer minted and `--token-file` is always named: this node's own token is never sent by default | the verb's result, as JSON |
| `arca token mint --scope <s> --id <id> --out <file> [--tools <comma list>]` with `--config <file>`, `--hash-dir <dir>` or neither (the config every command finds) | nothing | the id, scope and tool list |
| `arca token revoke <id>` with `--config <file>`, `--hash-dir <dir>` or neither (the config every command finds) | the token's hash file | the id and `revoked: true` |
| `arca token reload [--config <file>]` | the pid file and the reload record in the state directory, and `/proc` | one line: `reloaded: the node on this config (pid <n>) read its tokens again and holds <k> token(s)`. It exits non-zero, with the reason on stderr, when no node runs on the config, when the node could not read its token directory, or when it cannot tell |
| `arca shim --upstream <socket> --listen <socket> --log <file> [--log-max-bytes <n>]` | forwards GET, refuses the rest | the shim summary, on SIGTERM; the summary so far on stderr, on SIGUSR1 |
| `arca adopt plan\|apply\|release\|verify\|status --instance <name> ... [--config <file>]` | one listed container, and its adoption record | the plan, the steps run, or the check made; see "Adopting a container" |
| `arca --version`, `arca version` or `arca -V` | nothing | one line: `Automagik Arca <version> (<short commit>, <target triple>)`; the commit reads `unknown` when the binary was built with no git checkout |
| `arca service import plan --name <service> [--config <file>]` | one instance `kernel.importServices` lists, and the instance listing, through Incus GET | the import's dry run as JSON: every check, the one write and the pre-image digest; see "Importing a service another manager created" |

`watch` and `metrics` print the same JSON as the MCP verb of the same name, so a command's output can be compared with the
server's answer.

Exit codes: `0` success; `1` a runtime failure (a socket, a read or a write failed); `2` a usage or config failure (a bad
command line, a refused config, or a token directory that is unsafe or missing); `3` a tool error from `call`, whose JSON-RPC
error object is printed on stdout; `4` a token the node refuses, HTTP 401 or 403. Other messages go to stderr. A message never
carries a token, a secret or a key.

`metrics` samples once at start and then every 5 seconds for `--sample-for` seconds (default 5, from 0 to 60). Its `mean`
covers every sample of the run, and the `node_metrics` window it prints is one minute.

## The config file

One JSON document with camelCase keys. Every unknown key, at any level, is refused. A value that is not the right type is
refused without the value being echoed back.

```json
{
  "stateDir": "/var/lib/arca",
  "incus": { "socket": "/var/lib/incus/unix.socket", "pools": ["default"] },
  "identityPhones": ["phone1"],
  "listen": { "unix": "/run/arca/arca.sock", "tailnet": null, "proxy": null },
  "tokens": { "hashDir": "/etc/arca/tokens" },
  "kernel": {
    "projects": [],
    "scratch": false,
    "droiddProjects": ["app1"],
    "reservedCidrs": []
  }
}
```

| Key | Required | Meaning |
|-----|----------|---------|
| `stateDir` | yes | An existing directory, mode 0700. The node key and the desired-state files live in it. See the state section below. |
| `incus.socket` | yes | The Incus unix socket. Reads only. |
| `incus.pools` | no | An allow-filter on the storage pools Incus lists. Absent, or `[]`, means every pool Incus lists. A non-empty list samples and alerts only the listed pools that Incus also lists. A listed name that Incus does not list has no row, so its firing alert resolves with `subject-gone`. A pool entry with no name makes the pool reading unknown for that tick. See "Alerts". |
| `incus.local` | no | Whether `incus.socket` reaches the local Incus daemon. When absent, the node asks Incus `GET /1.0` for `environment.server_name` and compares it with the local host name: a match is local, a mismatch is remote. The socket path rule (the standard local daemon sockets) is the fallback only when `/1.0` or the host name cannot be read. An explicit value wins. See "Host metrics". |
| `identityPhones` | no | Instance names the watch flags with `identity: true`. |
| `listen.unix` | yes | The MCP socket. Created with mode 0600. |
| `listen.loopback` | no | One loopback address and port, such as `"127.0.0.1:7471"`, or `null`: the HTTP MCP door for a client on the same machine. It serves the MCP routes alone, never the approval page, to loopback peers that name it by its own address or as `localhost`. `null` is the owner's word for no door: `scripts/install.sh` writes it for `--no-door` and never opens a door in a config that says it. See below. |
| `listen.tailnet` | no | One tailnet address, or `null`. See below. |
| `listen.proxy` | no | `{ "addr": "<ip>:<port>", "peer": "<ip>" }`, or `null`: one address for a reverse proxy on the same host, served to the single peer address `peer`. See below. |
| `listen.services` | no | The path of a second unix socket, or `null`: the services socket. It serves the MCP routes alone, never the approval page, to the server's own user and to root. A service with kernel access connects to it. See below, and "A service that may talk to the kernel". |
| `tokens.hashDir` | yes | The directory of hash files. See below. |
| `kernel.projects` | no | The project family the kernel may write in. With projects named, the writer serves only those projects. With none, the kernel is watch-only: the reconciler's provider reads only, service writes are refused, no GPU device writer exists, and a removal cannot reach Incus. |
| `kernel.scratch` | no | When `true`, every kernel project must start with `arcak-scratch-`. |
| `kernel.droiddProjects` | yes | droidd's projects. Required, and it must not be empty while `kernel.projects` is not. |
| `kernel.serviceCidr` | no | Service bridge `<ipv4-network>/24`, outside every `kernel.reservedCidrs` range. Required for Postgres creation and probe consumer add; no default. |
| `kernel.postgresImage` | no | The old way to name a database's image; leave it unset on a new node. A digest-pinned reference (`name@sha256:<64 hex>`) to an image Incus already holds under that fingerprint: Arca does not fetch it. With it, `postgres_create` makes a database of the old kind from exactly this image, proposed or not. Without it, `postgres_create` makes a service from the node's `postgres-autopg` template, whose image Incus fetches by digest. See "A database". |
| `kernel.reservedCidrs` | no | Subnets reserved for droidd. Read by later steps only. |
| `kernel.adopt` | no | `[{ "instance": "<name>", "to": "arcak-adopt-<name>" }]`: the containers `arca adopt` may take from the default project, each with its adoption project. A config that lists one is for `arca adopt` alone, and `serve` refuses it. |
| `kernel.attachNetworks` | no | The bridges in the default project an adoption project's grant may name, though they carry no kernel mark. Nothing writes them. An adoption whose `eth0` is on a bridge this list leaves out plans that bridge's grant as an allowance. |
| `kernel.bridged` | no | Bridged mode: `{ "bridges": ["<host bridge>", ...], "pool": "<Incus storage pool>", "limits": { "virtualMachines": n, "cpu": n, "memory": "<size>", "disk": "<size>" } }`, every key required. The host bridges, which already exist, that a virtual machine's NIC may join. Without `kernel.serviceCidr` the node is bridged: it runs virtual machines only and Arca creates no Incus network on it. With `kernel.serviceCidr` the node is mixed. See "Bridged nodes and virtual machines" and "Mixed nodes". |
| `kernel.tenants` | no | `{ "capacity", "maxTenants", "defaultCaps", "createRate", "front"? }`: the node may hold tenants. It needs `kernel.serviceCidr` and is refused beside `kernel.adopt`. See "Tenants". |
| `kernel.vmLimits` | no | `{ "virtualMachines": n, "cpu": n, "memory": "<size>", "disk": "<size>" }`, every key required: the virtual-machine caps of a node that has `kernel.serviceCidr` and no `kernel.bridged`. Without it such a node creates no virtual machine. It is refused beside `kernel.bridged`, which states the same caps in its `limits`. See "Mixed nodes". |
| `kernel.importServices` | no | `[{ "name": "<service>", "project": "<project>", "cidr": "<ipv4-network>/24" }]`: the services another manager created that `service_import` may take over where they stand. Each `project` must be listed in `kernel.droiddProjects`, and `cidr` is that project's service subnet. Empty by default: nothing is imported and nothing is written in another manager's project. See "Importing a service another manager created". |
| `kernel.serviceDestinations` | no | `[{ "service": "<service>", "address": "<one IPv4 address>", "port": n }]`: the private addresses named services of the node's own project may reach through the node, each on one tcp port. It needs `kernel.serviceCidr`. Empty by default: a service reaches no private address. An entry alone opens nothing. See "A service that may reach one private address". |
| `link` | no | The node link: `name`, `forward` and `pairMinutes`. See "The link". |
| `approvalHost` | no | The host name every owner approval statement names. When it is not set, the system's host name is used. |
| `approvalOrigin` | no | The page origin the owner approval page is served at: `https://` plus a lowercase host name and at most a non-443 port, with no path (for example `https://arca.example.com`); or `http://localhost:<port>` with the port of `listen.loopback`, the page of a node on its own machine with no domain and no certificate. Its host is the WebAuthn rp id; it is not the host an approval statement names. |
| `canvasOrigin` | no | The loopback DSH origin the remote canvas proxies: an `http://` literal loopback IP and port, with no path. Default `http://127.0.0.1:3081`; any other origin refuses the config before `serve` listens. |
| `approval.trustOwnerKeysFrom` | no | Absolute paths of other kernels' owner passkeys files on this machine, which this kernel also trusts, read-only, for action approvals only (never to change its own passkeys). See "Approving on another kernel of this machine". |
| `approval.origins` | with `trustOwnerKeysFrom` | The page origins (`approvalOrigin` form) a passkey from those files may answer from. A line for any other origin is not trusted. |
| `approval.linkedHosts` | no | `[{ "name": "<host>", "socket": "<path>", "tokenFile": "<path>" }]`: the kernels on this machine whose proposals this kernel's approval page lists and relays approvals to. |

The config is refused, with exit 2, when:

- a key is unknown, a required key is missing, or a value has the wrong form;
- `approvalHost` is not a host name: one letter or digit, then up to 252 letters, digits, dots or hyphens;
- `approvalOrigin` is not `https://`, a lowercase host name, and at most a non-443 port, with no path, nor
  `http://localhost:<port>`; or it is `http://localhost:<port>` and `listen.loopback` is not set to `127.0.0.1` or
  `[::1]` on that port;
- `canvasOrigin` is not `http://`, a literal loopback IP, and a nonzero port, with no path;
- `listen.loopback` is not a loopback address (`127.0.0.0/8` or `::1`; a wildcard, a tailnet address, any other address
  of the machine and an IPv4-mapped address are refused), or its port is 0, 80, 443, 7443 or 9443;
- `listen.tailnet` is not a tailnet address: it must be a fixed port, not a wildcard, inside `100.64.0.0/10` or
  `fd7a:115c:a1e0::/48`, and not on port 80, 443, 7443 or 9443;
- `listen.proxy.addr` is a wildcard, a tailnet address (IPv4-mapped included), port 0, or port 80, 443, 7443 or 9443;
  `listen.proxy.peer` is missing or a wildcard; or `listen.proxy` carries any other key;
- `listen.services` is not an absolute file path of at most 100 bytes made of letters, digits, `/`, `.`, `_` and `-`
  with no `..` part, or it is `listen.unix`'s path;
- a kernel project is `bench`, `default`, or a name in `kernel.droiddProjects`;
- `kernel.projects` is not empty and `kernel.droiddProjects` is empty;
- `kernel.scratch` is `true` and a kernel project does not start with `arcak-scratch-`.
- `approval.trustOwnerKeysFrom` names a relative path, or names a file while `approval.origins` is empty; an
  `approval.origins` entry is not an `approvalOrigin`; or a linked host's name is not a host name or is listed twice, or its
  `socket` or `tokenFile` is a relative path.
- an adopted instance is not a plain name (`^[a-z][a-z0-9-]{0,56}$`), its `to` does not start with `arcak-adopt-` or is
  not in `kernel.projects`, an instance or a `to` is listed twice, an attach network is not a plain name, or
  `kernel.scratch` is `true` while `kernel.adopt` is set.
- a `kernel.importServices` entry carries a key other than `name`, `project` and `cidr`, its name is not a service name, its
  project is not listed in `kernel.droiddProjects`, its cidr is not an IPv4 /24, or a name is listed twice; or the list is
  not empty on a node with no `kernel.serviceCidr` (a bridged node among them) or with `kernel.scratch`. A mixed node,
  which sets `kernel.bridged` beside `kernel.serviceCidr`, may list imports.
- a `kernel.serviceDestinations` entry carries a key other than `service`, `address` and `port`, its service is not a
  service name, its address is not one IPv4 address (a range and a name are refused), the address is outside the
  private ranges of RFC 1918 and `100.64.0.0/10` or inside `kernel.serviceCidr`, it is `100.100.100.100` or
  `100.100.100.200`, its port is 0,
  an entry is listed twice, one service has more than 4 entries or the list more than 16; or the list is not empty on
  a node with no `kernel.serviceCidr`.

No command reads Incus before the config passes these checks.

### Loopback listener

`listen.loopback` is the HTTP MCP door for a client on the same machine: an assistant that takes an MCP server URL and
cannot open a unix socket. `"listen": { "unix": "...", "loopback": "127.0.0.1:7471" }` gives the URL
`http://127.0.0.1:7471/mcp`. `scripts/install.sh` writes this key, on port 7471 or the next free port up to 7480. The
key takes a loopback address and nothing else. No value of it reaches another machine: for that there is
`listen.tailnet` or `listen.proxy`, each under its own rule.

What the listener serves and refuses:

- **The MCP routes, and the page of this machine alone.** `POST /mcp` and the read-only GET routes, each behind a
  bearer token: 401 without one. An approval page with an `https://` origin is never on this listener: `/approve`
  and everything under it is 404. The one page it carries is the page whose origin is `http://localhost:<port>`
  with this listener's port; see "The page on this machine alone".
- **Loopback peers only.** A peer that is not a loopback address is closed at accept with no bytes sent.
- **Only a request that names it.** The `Host` must be the listener's own address and port (`127.0.0.1:7471`) or
  `localhost` and that port (`localhost:7471`), compared without regard to case. Every `Host` header a request carries
  must be one of the two, and so must the authority of an absolute request target. Any other name, the same name with
  another port or with none, and a request with no `Host` are 403 on every route, before the token is looked at and
  before the body is read. This is what stops DNS rebinding: a web page loaded from a foreign name that is then pointed
  at `127.0.0.1` sends that foreign name as its `Host`.
- **No `Origin`.** As on every listener, a request that carries an `Origin` is 403, so a web page cannot call the node
  from a browser.

- **A bounded wait for each request.** A peer has 30 seconds to send a request's head, the first of a connection and
  each next one on a connection that is kept open. A peer that sends nothing in that time is closed, so idle connections
  cannot hold the listener's 64 slots.

Use the address, `http://127.0.0.1:7471/mcp`, in a client, not the name `localhost`. The name can also mean `::1`,
where this listener does not listen and another program of the machine could.

What it does not do. The connection is plain HTTP, which on loopback never leaves the machine. Every user and every
process of the machine can connect to the port; without a token each gets 401, and the token's file is readable by the
node's user alone. A client that is given the token keeps it in its own config.

**The port can be taken by someone else.** A TCP port belongs to whoever binds it first. On a machine shared with
other accounts, another account can take the port while the node is not running (before it starts, after a crash,
during an upgrade) and would then receive what a client sends there, the bearer token included. The unix socket and
`arca call` are not exposed to this: the socket lives in a directory only the node's user can open. On such a machine,
leave the door off (`"loopback": null`, or `sh install.sh --no-door`) or accept that the token a client holds is a
`viewer` token another account could come to read.

A port that is in use when `serve` starts does not stop the node. The unix socket and every other listener serve as
usual, `serve` says once on stderr `arca: the loopback door <addr> is closed: address in use: another program holds
this port. The node runs without it and tries the address again`, and it tries the bind again after 1 second, then
after twice as long each time, up to 30 seconds, for as long as it runs. When a try succeeds it says `arca: the
loopback door <addr> is open`. `arca_status` reports the door of a node whose config names one, as
`"loopback": { "addr": "<addr>", "open": true | false, "reason": "<why it is closed>" | null }`, with a `"page"`
object beside them when the node's approval page is on this listener (see "The page on this machine alone"). A node
that serves an approval page also reports `"approvalPage": { "url", "open", "reason", "passkeys" }`: the page's
address, whether it is open (for the page of this machine alone, whether the node holds both loopback addresses),
the reason while it is not, and how many owner passkeys the node trusts (`null` while the kernel is busy with a
change). `arca status` says these in words. Any token may call `arca_status`, a `viewer` token among them, so an
assistant given one sees the page's address and the count. `arca status` also reads the node's token directory, to
say beside "Changes: on" whether any `doctor` or `operator` token changes things without the page. While it reads
`"open": false`, whatever answers on that port is not this node: do not send the token there.

### Tailnet listener

Serving on a tailnet address is a separate step. The owner approves it before `serve` is started with a tailnet address, and
until then the node runs on its unix socket alone. A peer outside the tailnet ranges is closed with no bytes sent.

### Proxy listener

`listen.proxy` is for a reverse proxy on the same host, such as Caddy in a container on the host's bridge, which connects
from its bridge address rather than from a tailnet address. The listener binds `addr` and serves exactly one peer address,
`peer`; an IPv4-mapped peer is compared as its IPv4 address. Every other peer is closed at accept with no bytes sent. The
bearer and `Origin` rules are the same as on the unix socket, so the proxy's peer gets the same 401, 403 and 200 answers.

The bridge may not be up yet when the server starts at boot. While `addr` is not assigned to the host, the bind is tried again
for up to 120 seconds, after a pause of 1 second that doubles each time up to 16 seconds, and each try is logged on stderr
(`arca: the proxy address <addr> cannot be bound yet ...`, then `arca: the proxy address <addr> is bound`). The unix socket
and the tailnet listener serve meanwhile. An address still not assigned after 120 seconds stops the server with exit 1, and
any other bind error, such as a port in use, stops it at once.

The connection is plain HTTP on the bridge, and a host that can take the peer's address can reach the listener; the bearer
token is still required. Serving on a proxy address is a separate step, approved by the owner like the tailnet listener.

### Services socket

`listen.services` is the path of a second unix socket, for example `/run/arca/services.sock` beside `listen.unix`. It
is bound as the first one is (mode 0600, renamed into place) and serves the server's own user and root, and nobody else.
It serves the MCP routes and nothing else: `POST /mcp` and the read-only GET routes, each behind a bearer token. The
approval page is never on it, on any node and whatever the config becomes: a request for `/approve` or anything under it
is 404, and a request that carries an `Origin` is 403. A node may serve its approval page and have this socket too; the
two sockets are separate, and one path cannot be both.

Incus connects to this socket for a service with kernel access (see "A service that may talk to the kernel"), and
Incus's proxy helper runs as uid 0: that is why uid 0 is served. Serving uid 0 means every process of the host that runs
as uid 0, and Incus's proxy helper acting for any instance whose device names this path. Arca gives such a device to a
service of the node's own project alone, a tenant's project blocks proxy devices, and whoever can add one by hand
already administers Incus. None of them gets anything from the socket without a token. No other local user reaches the
socket, and it has no port: nothing on the host can take a port in its place while the server is down.

The directory that holds the socket must belong to the server's user or to root and be writable by its owner alone, and
so must every directory above it, except that a directory above it may be sticky instead, as `/tmp` is. `serve` refuses
to bind the socket anywhere else, naming the directory: a user who could write there, or rename a directory on the way,
could put a socket of their own at the path and be sent a service's token. The socket has its own limit of 64
connections, apart from every other listener's. A clean stop removes it.

## Turning on changes

A node the installer leaves only looks. One step lets it change things, and one step takes it back. Neither needs a
second machine, a domain or a certificate.

```sh
arca init --enable-changes --config ~/.config/arca/node.json
```

or, at install time, `sh install.sh --enable-changes`, which installs and then runs the same step with `--yes`.

**What waits for the owner, and what does not.** What an assistant proposes waits: nothing of it runs until the
owner approves it with a passkey on the approval page, and until the first passkey is enrolled nothing can be
approved at all. The installer's token is a `viewer` token, which can look and propose and nothing more, so on a
node as the installer leaves it every change waits for the passkey.

A `doctor` or `operator` token does not wait. On a node that may change things, whoever holds one may, with no card
and no passkey, create a service from a template (the shipped ones among them), connect and disconnect services, set
a service's secret, unpublish one, start an instance, link and unlink a database, create a database from an image
the token names, remove a database's record, password and links, and use the Omni verbs; on a node that holds
tenants, also create and remove a tenant. Such a
token gains that reach the moment the node may change things, so the step reads the token directory first and
**names every `doctor` and `operator` token before it asks**. With none it says so, and then "every change waits
for your passkey" is true as written. `arca token revoke <id> --config <file>` takes one away. `--yes` does not
answer for such a token: where one exists, or the token directory cannot be read, the step under `--yes` stops,
names them and changes nothing, and so does `sh install.sh --enable-changes`. Run it without `--yes` to answer
yourself.

**What it asks**, each with a default:

1. `Let Arca make changes on this machine, each only after you approve it? [y/N]`, or, where a `doctor` or
   `operator` token exists, `... what is proposed after your approval, and directly for the tokens named above?`.
   The default is no. `--yes` answers it on the command line.
2. `Service network <a.b.c.0/24> is free. Use it? [Y/n]`. The default is yes. `--service-cidr <a.b.c.0/24>` names
   another, which is checked the same way.

**What it writes.** Only keys that are missing; a key that is set is never changed.

| Key | Written when | Value |
|-----|--------------|-------|
| `kernel.projects` | it is empty | `["arcak-svc"]`, the one Incus project Arca's services live in |
| `kernel.droiddProjects` | it is empty | `["default"]`. Arca never writes in `default`; the entry only names it as not Arca's, which the config requires of a node that names a project of its own |
| `kernel.serviceCidr` | it is absent | the /24 for the bridge `arcak-svcbr`, which Arca makes for its services |
| `approvalOrigin` | no approval page is named | `http://localhost:<port>`, on the loopback door the config already opens |

An approval page the config already names, `https://` or `http://localhost`, is kept as it is, and the step says
which page approves. The config as it was is kept beside it as `<config>.before-init`, and both files are the
user's alone. The new config is checked as `serve` checks it before it is written, so the step never leaves a config
the node refuses.

Then it installs the shipped templates into the state directory (`arca templates install`), and restarts the node
only when the node is the installer's own service for this config (`arca.service`, written by `scripts/install.sh`).
A node started any other way is the owner's to restart, and the step says so.

**What it says when it is done** is five lines, so the one thing to do is not lost among the rest:

1. what changed, and whether the running node took it up. "Arca may now make changes" is said only when the
   restarted node answered so itself;
2. who waits for the page: either that no token of this node may do more than look and propose, or every `doctor`
   and `operator` token by name, with the line that revokes one. This line never leaves the report;
3. `Next:`, the ONE thing to do. A node that was not restarted: restart it, with the command. A node that was
   restarted while init had no token to ask it with: ask it, with `arca status`, whether its page is open, before
   a setup code is typed or anything is approved there. A page the node does not hold: type no code there.
   Otherwise the passkey: at a terminal with a screen (`DISPLAY` or `WAYLAND_DISPLAY` is set), the page and its
   "Get a setup code" button; at one without, `arca setup-code`, the page to type the code on, and for the page of
   this machine alone the `ssh` line that brings the page to your own computer and the sentence that a phone
   cannot open it. With a passkey already trusted: ask your assistant for something;
4. `A database: ask your assistant for the postgres-autopg service.`;
5. `More:`, one pointer: `arca status` for what is open and what comes next, `arca status --security` for
   everything else, and `arca init --watch-only` to go back.

Nothing the longer report used to say is gone. What Arca may touch, that it never writes in `default`, what a
`doctor` or `operator` token may do, the cautions of the page of one machine, the `ssh` line, the phone, the
templates' directory and where the config as it was is kept are all said by `arca status --security --config
<file>`, from the config, with the node running or not.

**How the service network is chosen.** It is read from the machine, never guessed. The step takes the first of
`10.47.0.0/24` to `10.47.255.0/24` that meets none of these, and names each source in what it prints:

- a route in the kernel's routing table (`/proc/net/route`);
- an address of the machine itself (`/proc/net/fib_trie`);
- a network Incus manages, and an address on any interface Incus lists;
- a range `kernel.reservedCidrs` names;
- the tailnet range `100.64.0.0/10`, which is never used whatever the tables say.

When one of the sources cannot be read, the step asks whether to go on with what it could read (default no), and
under `--yes` it stops. A network named with `--service-cidr` must be an IPv4 /24 and pass the same checks.

**What it refuses, changing nothing:**

- a config with no approval page and no loopback door for one. The step opens no door: a missing or `null`
  `listen.loopback` may be the owner's no. Open it (`sh install.sh --door`) or set an `https://` `approvalOrigin`
  first;
- a loopback door that is not on `127.0.0.1` or `[::1]`, which the name `localhost` does not reach;
- a machine where an instance carries another manager's marks. It names what it saw; list that manager's projects
  in `kernel.droiddProjects` by hand, and the step fills that key only on a machine that has none;
- a config set up by hand for something else: `kernel.scratch`, `kernel.bridged`, `kernel.adopt`, or
  `kernel.projects` that names projects without `arcak-svc`;
- a service network that is not free, or no free one among its own; and a network named with `--service-cidr`
  that is not inside the private ranges of RFC 1918, since a public, multicast or link-local one would pass every
  other check and then hide those addresses from the machine;
- a config that is a link (name the file it points to), and a `<config>.before-init` that is a link or a directory.
  The copy and the new config are written as new files of the user's own, no link is followed, and the directory is
  synced after the rename;
- an Incus that does not answer on `incus.socket`;
- root. It runs as the node's user and never runs `sudo`. The one program it starts is `systemctl --user`, to
  restart the installer's unit.

It takes `--config` as a flag and reads no default, like every command that writes.

**Going back.**

```sh
arca init --watch-only --config ~/.config/arca/node.json
```

empties `kernel.projects` and changes nothing else, then restarts the installer's service as above. Its report is
three or four lines. It says "Arca now only looks" only when the restarted node answered so itself; otherwise its
first line says that the node that is running still makes changes until it is restarted, and its `Next:` line is
that restart. Then that nothing was removed, and one pointer: `arca status`, `arca status --security` for what was
kept, and `arca init --enable-changes` to turn changes on again. Once restarted, the node only looks: it starts, stops, creates and removes nothing, and a proposal that waits can no longer run. Nothing is
removed: the Incus project `arcak-svc`, its bridge, the services in it and the owner's passkeys stay as they are.
Running `--enable-changes` again adds the project back and keeps the network and the page.

**A node that only looks still shows what is there.** After `--watch-only` the reading tools answer as they did:
`service_list`, `service_status`, `service_logs`, `postgres_list`, `tenant_list`, `tenant_status` and `tenant_usage` read
the node's own records and Incus, exactly as on a node that makes changes. What differs:

- Every answer says what the node is. Its text, and its structured content under `watchOnly`, carry `this node only
  looks: nothing here can be changed until the node is running with changes turned on (arca init --enable-changes, then
  restart the node if init did not)`. The sentence is about the node that is running: one whose config already turns
  changes on, but that was not restarted, still only looks, and the restart is the step that is left.
- Nothing is adopted. An instance in `arcak-svc` that the node holds no record of is shown as it is, with the state
  `unknown` and the note `not recorded by this node`; a node that makes changes would have written a record for a
  marked one.
- `service_logs` reads the instance's console log, and its `note` says so: the journal is read by running a command
  inside the instance, and a node that only looks runs none. A container's console log holds only what its init system
  wrote to the console, which is often much less than its journal, and may be nearly empty.
- `omni_status` is refused, with that reason: it answers by running a command inside the service.
- Every tool that would change an instance, a service, a database, a tenant, storage or a GPU assignment is refused
  with that same sentence, before an approval is spent and before anything is saved: a tenant's own calls, the
  `deploy_*` tools, `tenant_create` and `tenant_remove` included. A refusal of an instance start or stop, of a
  storage change or of a GPU assignment keeps its own code (`InstanceLifecycleDisabled`, `StorageDisabled`,
  `GpuDisabled`) and carries the same sentence. The tools that only keep or settle a note for the owner
  (`action_propose`, `owner_approval_request`, `owner_approval_status`, `approval_submit`, `enroll_confirm`,
  `plan_decline`, `peer_keys_sync`) still record it: what they record cannot change the node until changes are turned
  on.

A node that only looks and holds records of services it ran, but whose config lost `kernel.serviceCidr` too, shows them
the same way: the records are enough to tell which instances are its services. A node that never had a service config
and holds no record answers an empty list.

**What this step does not do:**

- It does not make every shipped template start from the page alone. A template that makes every secret it needs
  starts once its card is approved: `postgres-autopg`, `nats`, `omni-api` and `automagik-server`. A template that
  needs a secret of the owner's is created stopped until it is set: `cloudflare-tunnel` (`TUNNEL_TOKEN`), the
  runner pools and `automagik-workspace` (their tokens and keys). Omni starts, and its Telegram and WhatsApp tools
  need `telegram_bot_token` and `whatsapp_phone_number`. `automagik` needs an image built from a checkout before it
  can be created, and its `tailscale_authkey` before it can be published. `service_secret_set` cannot be proposed, so it needs a `doctor` token, which changes
  things with no card and no passkey: mint one for that step and revoke it after, or leave those templates until
  setting a secret can go through the page.
- It does not enrol a passkey. The owner does, on the page, with a setup code from the node's own screen or from
  `arca setup-code`; see "The page on this machine alone".
- The page it names is the page of one machine, for a machine where every account is trusted. Approving from a
  phone or from another machine needs an `https://` origin, set by hand.
- It sets no `kernel.postgresImage`, and none is needed. **A database after this step is the shipped
  `postgres-autopg` service:** ask the assistant for it, or propose `postgres_create` with a name, or
  `service_create` with `"template": "postgres-autopg"`. Both make the same service and wait for the owner's card
  like any change (see "A database"). The template fetches its own pinned image and makes the database's password
  itself at create (32 letters and digits, stored as the service's `POSTGRES_PASSWORD` secret and never shown), so
  the approved create starts by itself, and a proposed `service_connect` gives the linked service its
  `DATABASE_URL`. No `service_secret_set` is needed, which matters because that verb cannot be proposed and needs a
  `doctor` token. See "Templates an earlier release installed" below for an older node. No call
  chooses the image: a `doctor` or `operator` token's own `postgres_create` runs without a card, as its
  `service_create` of that template does. It sets no `kernel.vmLimits`, so no virtual machine is created.
  Tenants, adoption and import are not turned on. A node that may change things also runs its snapshot writer
  (snapshot pools and the project `arcak-snapshots`) and its GPU device writer: each acts only from a card the owner
  approves.
- It does not check that Incus can fetch a template's image; a create that cannot answers in Incus's own words.
- It grants no access to Incus. The node's user must already be able to open the Incus socket, which is the
  machine's administrator's to grant, as is keeping the service running without a login session.

## The link

`arca link serve --config <file>` runs one iroh endpoint with a persistent key. The optional `link` config section has
camelCase keys and refuses unknown keys:

- `name`: the node's name, from 1 to 60 characters, without U+0000..U+001F control characters. When absent, the name is the
  first 10 characters of its endpoint id. Incoming device names have those controls stripped, are capped at 60 characters,
  and fall back to `device` when empty.
- `forward`: an ALPN-to-unix-socket-path map. Each admitted stream is forwarded unchanged in both directions to its socket.
  `arca link serve` adds `arca/mcp/1` pointing to `listen.unix` unless the map already has an explicit entry for that ALPN.
- `pairMinutes`: the default code lifetime in minutes, overridden by `link pair --minutes`. The default is 10, and the
  selected value is clamped to 1 through 60.
- `localOnly`: when true, binds only loopback and disables discovery and relays. The default is false (N0 discovery and
  public relays). It also disables mesh reachability: the node neither reaches peers on other hosts for membership or
  forwarding, nor is reached by them. This also permits testing `link serve` without external network access.

The endpoint accepts `arca/pair/1` for pairing, `arca/peers/1` for membership, `arca/mcp/1` for peer calls, plus the ALPNs
in `forward`. Both membership and forwarded connections require a live entry in `peers.json`; a forwarded connection checks
that entry before its local socket is opened. `arca/pair/1` and `arca/peers/1` cannot be forward entries.
Each forwarded connection runs at most 16 streams concurrently; further streams wait until a running stream finishes.
Handshakes have a 10-second deadline, with at most 16 pending and 4 pending per address. The 4-per-address cap applies
only to pending handshakes; open forwardings are capped only in total, at 64 open forwarded connections.
`link remove` uses the running server's control socket, closes that peer's live connections, and triggers mesh sync
immediately. Without a running server it writes the removal to disk; the next start propagates it. External disk removals
are detected within 2 seconds while serving.

`arca/peers/1` exchanges one full JSON snapshot on one bidirectional stream: `{"peers":[...],"removed":[{"id","at"}]}`.
The receiver merges the request and replies with its merged snapshot, so both ends learn membership changes. Each JSON
line is bounded at 64 KiB including its newline; pairing retains its 4 KiB bound. Sync runs after successful pairing,
after removal, at startup, and every 30 seconds. Offline members do not make these operations fail; they catch up on a
later exchange. Any admitted member can introduce or remove peers received from another member; membership changes
rely on that mesh trust.

A removal tombstone wins when its `at` timestamp is at least the peer's `paired` timestamp. A newer pairing survives and
clears the tombstone; older snapshots cannot resurrect a removed device while its tombstone is retained. Existing peer
records change only when an incoming `paired` timestamp is newer. Tombstones are retained for offline members for up to
30 days, keeping at most the 64 newest (ties broken by endpoint id). Their timestamps are capped at 5 minutes ahead of
the local clock. An older snapshot can reintroduce a removed device whose tombstone has expired or been displaced by the cap.

All link state lives directly in the top-level `stateDir`:

| File | Contents |
|------|----------|
| `secret.key` | The endpoint's 32 raw secret bytes, generated once. |
| `peers.json` | An array of `{ "id", "name", "paired", "seen", "ticket"? }`; timestamps are epoch milliseconds. The optional endpoint ticket supplies transport addresses; older records without one use N0 lookup. Admission reads this file on every connection. |
| `removed.json` | A tombstone map `{ "<endpoint id>": <epoch milliseconds> }`, propagated in membership snapshots. |
| `pairing.json` | The pending single-use code, expiry, node name and wrong-attempt counters. Deleted after successful pairing. |
| `status.json` | The running server's `{ "pid", "id", "ticket", "name" }`. Removed on clean stop; stale or absent status prevents `pair`. |
| `.link-control.sock` | The mode 0600 local socket for joining and dialing through the running endpoint. Removed on clean stop. |

The link creates a missing `stateDir` with mode 0700 and refuses an existing directory with any group or other permission
bit, reporting `link state directory must have mode 0700` as a config failure (exit 2).
For an older state directory, run `chmod 0700 <stateDir>` before starting; the link does not change existing directory
permissions automatically because the top-level directory may be shared with other services.
These files are written atomically with mode 0600. `pair` requires a running `link serve`. It creates a code and a link of
the form `arca://pair?v=1&n=<percent-encoded name>&c=<code>&t=<endpoint ticket>`. A code works once, until its expiry. A peer
is refused after three wrong tries, and 1,000 wrong tries overall exhaust the code. `join` consumes that link and stores
the host peer locally; when a local server is running, it reuses that server's endpoint.

`pair`, `join`, `peers` and `remove` print JSON on stdout. `pair` draws its terminal QR on stderr, so redirecting stdout
captures only JSON. The link and QR contain the pairing code; keep them private until the code is used or expires.

## Tokens

A token is a 256-bit secret of the form `arca_` followed by 43 base64url characters. The server never stores the secret. It
reads one hash file per token from `tokens.hashDir`:

```
scope=viewer
sha256=<64 lowercase hex digits>
tools=service_list,service_logs,journal_list
```

The file name is `<id>.hash`, and the id is 1 to 64 lowercase letters, digits or hyphens. The scopes are `viewer`, `doctor`
and `operator`. The `tools=` line is optional; without it the token reaches every verb its scope admits, and with it the token
reaches only those named tools. A tool above the scope is refused at mint, so the list only narrows.

One id is no token's: `owner-approval`, the name an action runs under once the owner approved it. `arca token mint`
refuses it. A release before this one did not, so a file `owner-approval.hash` may exist: the node starts and reads
every other token, leaves that file out, and writes one line each time it reads its tokens ("token file
owner-approval.hash is ignored: ..."). Its secret is refused as any unknown token is. Remove the file with `arca
token revoke owner-approval`.

### Modes and the umask

**The token directory must be mode 0700 or 0500.** The server refuses any directory with a group or other write bit, and it
refuses a directory it does not own. A host umask of 002 makes a new directory 0775, which the server refuses, so set the
mode explicitly after creating it:

```sh
mkdir /etc/arca/tokens
chmod 0700 /etc/arca/tokens
```

Each hash file must be a regular file, not a symbolic link, mode exactly 0400, and owned by the server's user. A file of any
other mode, owner or kind makes the whole load fail, and the message names the file, never its content.

`arca token mint --tools <comma list>` writes the optional allowlist; an unknown name or one above the scope is refused with
exit 2 before anything is written. `arca token mint` writes both files at mode 0400, refuses to overwrite an existing id or
output file, and removes its hash file if it cannot write the secret. It needs the token directory to exist and to have no group or other write bit. Create
the directory and mint into it before the mode is tightened to 0500, because a 0500 directory cannot take new files.

```sh
arca token mint --scope viewer --id viewer-1 --hash-dir /etc/arca/tokens --out "$HOME/.config/arca/viewer-1.secret"
```

`--config <file>` may name the token directory instead, as `tokens.hashDir` of the node config, and then `--hash-dir` is not
needed. The config is loaded and checked first. When both are given, `--hash-dir` must name that same directory; a different
one is refused with exit 2 and nothing is written. Giving neither is refused with exit 2.

**Run `arca token mint` as the user that runs `serve`.** A mint run as another user writes hash files owned by that user, and
the server refuses every file it does not own, so the load fails until those files are minted again as the server's user.

`arca token reload --config <file>` makes the node that runs on that config read its tokens again: the step after a
`token mint` on a running node, which sends no signal itself. It finds the node as `token revoke` does, and signals only a
process it has checked to be the node: one that belongs to this user and whose command line is `arca serve` on that very
config, with `serve` as the command itself. The node's own link (`arca link serve` on the same config) is never taken
for it, and neither is a pid file left by a node that crashed, whose pid is gone or is another program's. It then waits for the node's own record of the reload, `<stateDir>/serve.reload`,
and answers in words: `reloaded`, with the pid and the count of tokens; `no node runs on this config`; `not reloaded`, when
the node could not read its token directory and keeps the tokens it had; or `could not tell`, when the signal was sent and
the node did not say so within 30 seconds (a node older than this command reloads without saying so). Only `reloaded`
exits 0.

A node that is still starting is left alone: `serve` takes `SIGHUP` as the first thing it does, and until it has, the signal
would end the process. `token reload` then answers `the node on this config (pid <n>) is still starting; nothing was
signalled. Run this again in a moment`, and `token revoke` says the same on stderr and reports `reloadSignalled: false`.
Run both as the node's own user: a node that another user runs is never signalled, and the answer says so (`no node of
yours runs on this config`). The node is found by the config path its `serve` was started with, so a config file that
was moved or renamed after the node started is not found, and the answer is that no node runs on it; give the path the
node was started with, or restart the node on the new path.

`arca token revoke <id>` removes the token's hash file. With `--config`, it sends `SIGHUP` to a live `arca serve` for that
config: it checks `<stateDir>/serve.pid` only when its pid is a live `arca serve`, and otherwise finds the same process in
`/proc`. The server writes that pid file as soon as its own unix socket is bound, before it serves a request, and removes
it only when it still holds its own pid; a write that fails is said once on stderr and does not stop the node. A second start that fails to bind leaves the running server's pid file unchanged. The answer carries
`reloadSignalled`; `token_list` drops the token and a call with it is 401 without a restart. With `--hash-dir` alone no state
directory or serve is known, so no signal is sent (`reloadSignalled: false`) and a running server keeps the token until it is
restarted or signalled. The hash files stay mode 0400. The signal goes through a pid descriptor (Linux 5.3 or later), never the host's `kill` command: the process is held, checked again and signalled through the descriptor, so no other process can receive it.

The secret file holds the secret and one newline. A client reads that file, strips the newline, and sends the secret as
`Authorization: Bearer <secret>`. Keep the secret out of command lines, logs and shell history.

A service is one template-derived container in the service project. Its template lives in
`<stateDir>/templates/<name>.json`; `service_create` allocates `.10`-`.239` on the service bridge, creates the
service profile and NIC ACL, records the desired entry, and starts the container when every environment secret it
needs is available; with `start: false` it never starts it (see "Creating a service stopped" below). `service_list` and `service_status` derive state from Incus and report instances that Arca did
not create as `unknown` without touching them. A row's `ports` are the container's own ports;
`lanPort` is the host proxy port when the template sets `lan`, or null. Only `lanPort` is a host
listener. A row carries `kernelAccess` only when the service's template declares it (see "A service that may talk to the
kernel"). `service_logs` returns console lines as untrusted text with stored
secrets redacted and a capped, truncated result. The eleven shipped templates live in `templates/`, with their pinned
images and vendored npm lockfile. Five of them, `workspace-vm`, `github-runner-pool`, `gitlab-runner-pool`,
`gitlab-runner-pool-large` and `automagik-workspace`, are
virtual machines (see "Bridged nodes and virtual machines", "Mixed nodes", "A pool of GitHub Actions runners", "A pool
of GitLab runners" and "An Automagik workspace in its own virtual machine").

**Templates an earlier release installed.** A release may change a shipped template: up to 0.12.0, for one, the
shipped `postgres-autopg` template declared `POSTGRES_PASSWORD` and did not make it, so a create came up stopped
until a `doctor` token set the secret. A node keeps no such template by accident. `arca templates install`, which
`arca init --enable-changes` also runs, compares each installed file with the files earlier releases shipped (the
program holds their SHA-256 hashes):

- a file that is an earlier release's own, byte for byte, is replaced with this release's and named under
  `replaced`; `arca init` names those in one line before its report. Nobody edited it, so nothing is lost;
- a file that matches no release is the owner's edit. It is kept, under `kept`, as before, and only `--replace`
  overwrites it (that flag names the node first);
- a template and the script it reads move together: where one of the two is edited, the other is kept as it is too,
  though it is an earlier release's, so a new template is never paired with a script it was not shipped with.

The installer runs this step on an upgrade (`sh install.sh` on a machine that has Arca), for the node it runs as
a service and only where templates are already installed: a node that only looks has none and gets none. Its
report names what it brought up to this release, and what it kept because it was edited, with the one command that
replaces those (`arca templates install --replace --config <file>`). Where the run puts an older program over a
newer one (`--version`), the files it keeps may be the newer release's own: the report says so and gives no command
to replace them. The step writes every file before it puts any in place; if it stops between two files, its words
name what it had already put in place, and running it again finishes. For a node started any other way, run `arca
templates install --config <file>` once after the upgrade, or `arca init --enable-changes` again; neither needs
`--replace` for a template nobody edited.

What a replaced template changes is the next create. A create that was proposed before the replacement and still
waits is not made from the new file: its card bound the content of the template it showed, so approving it is
refused and nothing is created; propose it again (see "What the owner saw is what is created"). `arca init` and the
installer say so beside the names they replaced. Nothing changes for a service that already exists: a
`postgres-autopg` service keeps the password that is stored, and none is made over it. One that was created
stopped with no password gains none by itself either: set it with `service_secret_set`, or remove the service and
create it again from the new template. A runner pool reads its script when its virtual machine is created, so a
pool that runs keeps the script it has.

`arca templates install --config <file>` installs the shipped templates in one step: each template's JSON and the
script its entrypoint or its first boot reads (`postgres-autopg.sh` for `postgres-autopg`, `github-runner-pool.sh` for
`github-runner-pool`, `gitlab-runner-pool.sh` for both `gitlab-runner-pool` and `gitlab-runner-pool-large`,
`automagik-workspace.sh` for `automagik-workspace`), from the build itself, so no checkout is needed. An entrypoint's `{ "file": "<name>" }` names a plain file in the templates directory: letters, digits, dots,
dashes and underscores, not starting with a dot, so no path. A template whose script is missing is refused by
`service_create` with the file it looked for (`templates/<name>`). A template whose `image` is `local:<alias>`
(`automagik`; `automagik-server` is an OCI image) needs that image in the service project's image store first:
`service_create` refuses with the alias when the project's image listing answers and no image there carries it,
before anything is created. A listing that cannot be read decides nothing, and the create goes on.

A local image may be pinned: a `digest` (`sha256:` and 64 lowercase hex digits, the image's Incus fingerprint) beside
`local:<alias>`, in a container template or a virtual machine's. Without it the alias is trusted for whatever image
stands under it. With it the fingerprint decides, and the alias is only the name the image is looked for under:

- `service_create` refuses, before anything is created, when the image under the alias has another fingerprint. The
  refusal gives both: `its image <alias> is pinned to sha256:<a>, and the image under that alias in the service project
  arcak-svc is sha256:<b>`. An approved create is held to it like any other, and a create in a tenant is checked
  against the tenant's own project.
- It also refuses when the alias is missing, and when the project's images cannot be read: a pin that cannot be checked
  is not met. On a node with no service yet the service project does not exist, so no image can be in it; the refusal
  says so and to create another service first.
- The instance is made from the pinned image: Incus is given the fingerprint, and with a fingerprint Incus never looks
  the alias up.
- The owner's card shows the pinned image in its `Image` row, as `local:<alias>@sha256:<fingerprint>`, and the
  approval binds it with the rest of the template (see "What the owner saw is what is created"). An unpinned local
  image is shown as `local:<alias>` with the words that say it has no digest.
- A digest in upper case, or one that is not 64 hex digits, is refused when the template is read.

**Creating a service stopped.** A secret (`service_secret_set`) and a link (`service_connect`) reach a container's
environment at its next start, not while it runs. A service that is started as it is created therefore boots once
without them, and a program that sets itself up at its first boot (a database with its password, an application with
its admin password or its `DATABASE_URL`) keeps what it saw then. So `service_create` takes `start`:

| `start` | What `service_create` does |
| --- | --- |
| left out, or `true` | As always: the service is started once it is created. Two kinds of template are created stopped whatever `start` says: a container whose template needs a secret that is not set yet, and a virtual machine whose template declares secrets. The answer then carries `stoppedBecause`, with the reason. Their create is the one it always was, with the template's `boot.autostart`. |
| `false` | The service is created stopped and nothing starts it. The answer's `state` is `"stopped"`, `stoppedBecause` says "the call said start: false", and `service_status` shows the same state. Its create carries no `boot.autostart` (see below). |

The order for a deploy that needs secrets or links at its first boot:

1. `service_create` with `"start": false`, for each service.
2. `service_secret_set` and `service_connect`, as often as needed. Both write the stopped service's config, as they do
   for a running one.
3. `instance_start` for each service, the providers first. This is the first start, and the environment it gives a
   container holds every secret and every link variable set in step 2.

**A host restart.** A template may set `autostart`, which a create writes as `boot.autostart`: Incus then starts the
instance whenever the host restarts, also one that has never run. With an explicit `start: false` the create leaves the
key out, and the service's record says `autostart: false`, so a create made again from the record is the same. Incus
(7.5.1) describes the key as "Whether to always start the instance when the daemon starts", and "If unset or set to
`last-state`, restores the last state". So a service created with `start: false` is never booted by a host restart
before its first `instance_start`; after that, a host restart restores the state it was in, and a service stopped
with the owner's approval stays stopped. This holds for an explicit `start: false` only. A create that the template
alone stops (the two kinds in the table) keeps `boot.autostart` as the template sets it, as before: Incus may boot such
a service on a host restart before its first `instance_start`, without its secrets. So for a template with `autostart`,
`start: false` and `start` left out are not the same create.

**A virtual machine.** It takes `start: false` like a container, but its secrets are not environment values: they are
stored, and `instance_start` writes them into the guest as files once the machine has booted (see "Secrets of a
virtual machine"), inside a tenant as in the node's own project. A virtual machine is never the consumer of a link.

Nothing else changes: the address, the firewall rules, the caps and the approval rules are those of any create, and
the effect journal holds the instance's create as it holds any create (an intent, then the applied effect with the
instance's delete as its inverse). The service's record holds `start: false`. On the approval page, the card of a
`service_create` has a `Start` row whenever the service will be created stopped: for `start: false` ("No: it is
created stopped, and instance_start starts it later", and the summary ends "and leave it stopped"), and for a
template that is created stopped whatever the call says ("No: it is created stopped because ...", with the reason).

The `automagik` image is built from a checkout with `bash templates/automagik.build.sh PROJECT PROFILE
local:BASE_FINGERPRINT`, which publishes it in `PROJECT` under the alias the template names. `PROFILE` must carry a
root disk and a NIC with internet access and DNS, because the builder downloads Node, Tailscale and the plugins. The
`arca-service` profile has no NIC (each service gets its own), so it cannot be the build profile: use one that adds a
NIC on a network the project may use, and delete it afterwards if it was made for the build. The script waits up to
60 seconds for the builder to resolve DNS and says so when it never does. The base is an amd64 Debian image already
in the project, prepared like this (Debian 13 here), with the same `PROFILE`:

```bash
incus init images:debian/13 base-prep --project PROJECT --profile PROFILE
incus start base-prep --project PROJECT
incus exec base-prep --project PROJECT --env DEBIAN_FRONTEND=noninteractive -- sh -c \
  'apt-get update && apt-get install -y --no-install-recommends bash ca-certificates curl git xz-utils tar passwd \
   util-linux systemd && apt-get clean && rm -rf /var/lib/apt/lists/*'
incus stop base-prep --project PROJECT
incus publish base-prep --project PROJECT   # prints the BASE_FINGERPRINT
incus delete base-prep --project PROJECT
```

The pull also leaves a cached copy of `images:debian/13` in the project; delete it if it is not wanted. The build
removes its own builder instance and leaves the base and the built image.

When a create fails in the kernel, the refusal carries the recorded reason and says what remains: `nothing was left
behind` only when the cleanup that follows completed and nothing waits for an approval. A template with `data`
leaves its data volume in a delete plan for the owner's approval, and the refusal names it. When the cleanup did not
complete (for example the create's outcome is not known), the refusal says so.

Templates may also set `limits` with an optional decimal-string `cpu` and/or a size-string `memory` (for example,
`{"cpu":"4","memory":"4GiB"}`); at least one is required when `limits` is present. These instance settings override the
shared profile. `cpu`, `memory` and a data `size` must start with a digit 1-9; zero is refused. `autostart` defaults to
false; true sets `boot.autostart=true`.

`data` names an absolute printable mount `path` of at most 255 bytes that is not the root (`/`, `//` and `/.` are
refused) and has no `..` segment, optional numeric `uid` and `gid` (unsigned 32-bit), and an optional `size` such as
`8GiB`. Arca creates a custom volume `<service>-data` in the service project's storage pool before the instance, setting
`initial.uid`/`initial.gid` when supplied. The disk device mounts that volume at `path`; `size`, when supplied, is set on
the volume config only; Incus supports a disk device's `size` only on the rootfs disk, so the data disk carries none. The
volume carries the entry's ownership marker, is reused on reconciliation, and is deleted with the instance and ACL on
approved service removal.

`lan: {"port":3081}` requires that port in `ports` and adds a `web` proxy device listening on `tcp:0.0.0.0:3081`,
connecting to `tcp:127.0.0.1:3081` in the container. The proxy is a forkproxy device, so it connects to the container
from the host; LAN traffic on `lan.port` therefore bypasses the service NIC ingress ACL. Known side effect: the wildcard
bind exposes the port on every host IPv4 interface, including a tailnet interface when present. The service project
remains restricted; Arca sets and reconciles only `restricted.devices.proxy=allow` to permit these proxy devices. Disk
restrictions retain Incus' managed default.
The `automagik-server` template pins the OCI server image, mounts `/data` owned by uid/gid 1000, overrides the profile to
4 CPU / 4GiB, enables autostart, and exposes port 3081 through this proxy.

### A database

`postgres_create` takes a name and nothing else. The image is the node's, never the call's, through both doors: a
call of a `doctor` or `operator` token, and a proposal the owner approves on a card.

On a node whose config sets no `kernel.postgresImage` (every node `arca init --enable-changes` sets up), the database
is the service that `service_create` makes from the node's `postgres-autopg` template:

- Incus fetches the template's image itself, by registry and digest, when it does not hold it. Nothing is put in
  Incus by hand first.
- The shipped template generates the password, and Arca keeps it in a private file; no tool shows it. A node that
  still holds an older `postgres-autopg.json` (one installed before the shipped template generated the password:
  `arca templates install` keeps a file that differs) or the owner's own edit makes the database stopped. The
  owner's card then carries a `Start` row that says so, and the answer's `stoppedBecause` says why. Two ways on:
  `arca templates install --replace` puts the shipped template back for the next database; or, for this one, set
  the secret with `service_secret_set` (key `POSTGRES_PASSWORD`) and start it with `instance_start`.
- The answer carries `service: true`, and `postgres_list` shows the database with `service: true` and, under
  `links`, the services connected to it.
- A database made this way is a service: connect to it with `service_connect`, remove it with `service_remove`.
  The service that connects is given `DATABASE_URL` for the role `app` and the database `app`.
  `postgres_link`, `postgres_unlink` and `postgres_delete` refuse its name and say which verb acts on it.
- Its name must be one no database and no service holds.
- A token with a tool list needs `service_create` in it beside `postgres_create`: the database is a service.

A proposal is given the template's name, the image it pins and the fingerprint of the template's content when it is
made. The card shows the image by digest, the template, a `Made as` row, which says that the template generates the
password only when it does, and last a `Template content` row. The approval binds all of it: if the template on the
node is another one when the owner approves (another image, or an edit of the script that opens the database), the
create is refused and nothing is made; propose it again. See "What the owner saw is what is created".

If the template is not installed, the create and the proposal are refused in words that name `arca templates
install`. An image or a template stated in a call must be exactly the node's own, or the call is refused; the
refusal never repeats what the caller stated.

**The old way.** On a node whose config sets `kernel.postgresImage`, nothing changed: `postgres_create` makes a
database of the old kind from that image, which Incus must hold already under that fingerprint (Arca does not fetch
it, and the fingerprint Incus gives an image it fetched from a registry is not the image's digest). Such a database
is linked to a consumer with `postgres_link` and removed with `postgres_delete`, and its row in `postgres_list` is
the one it always was. A call that states another image than the configured one is refused. One thing stops on
upgrade: on a node with no `kernel.postgresImage`, a token's own `postgres_create` could name any digest-pinned
image; it no longer can. A node that relied on that sets `kernel.postgresImage` to that image. A database of the
old kind is not turned into a service, and stays as it is.

### A service that may reach one private address

A service's NIC rules reject every private range, the tailnet range (`100.64.0.0/10`) and the node's own addresses, and
allow the internet. So a service cannot reach a machine on another bridge of the node, on the node's LAN or on its
tailnet. The node's config may name, for one service, one such address and one tcp port:

```json
"kernel": {
  "serviceDestinations": [
    { "service": "runners", "address": "10.20.30.40", "port": 443 }
  ]
}
```

`service` is the name of a service of the node's own project. `address` is one IPv4 address inside one of the three
private ranges of RFC 1918 or `100.64.0.0/10`, and outside `kernel.serviceCidr`: never a range, and never a name,
which could be pointed elsewhere after the owner approved it. Loopback, link-local (where a cloud's metadata answers)
and every other range are refused, and so are two addresses of the tailnet range: `100.100.100.100`, which the node's
own Tailscale answers on, and `100.100.100.200`, a cloud's metadata address. `port` is one tcp port. One service may
have 4 entries, a node 16.

Two keys open it, and neither alone:

- **The operator's config names it.** No call, template or proposal can state a destination of its own: what a call
  states as `destinations` is only ever compared with the config, and anything else is an invalid call.
- **The owner approves the create.** A service the config names is created only from a proposal: `action_propose` with
  `service_create`, then the owner's approval. A direct `service_create` of that name is refused with `NotApproved`,
  whatever the token's scope. The proposal carries `"destinations": [{ "address", "port" }]`, filled in from the
  config, so the approval binds it, and the owner's card carries a `Private network` row: "Yes: this service, and
  everything that runs in it, can reach 10.20.30.40 on tcp port 443 through this node, and is seen there as this node
  itself: whatever trusts this node's address there trusts everything in this service. Every other private address
  stays closed." For a pool of CI runners "everything that runs in it" is every job. A config that changes after the
  card was shown (an entry added, taken out, or another port) is not created by that approval: propose it again.

The service's record then carries `destinations`, and its NIC rules are built from the record. Incus weighs a reject
above an allow whatever the order of the rules, so an allow alone would open nothing. Each address is cut out of the
reject of the private ranges, four rules are added for it, and the last rule, which allows the internet, changes from
every address to every address but the opened ones:

| Action | Destination | Protocol | Ports |
| --- | --- | --- | --- |
| reject | `10.20.30.40/32` | tcp | `1-442,444-65535` |
| reject | `10.20.30.40/32` | udp | every port |
| reject | `10.20.30.40/32` | icmp4 | |
| allow | `10.20.30.40/32` | tcp | `443` |
| allow | every address but `10.20.30.40` (32 ranges) | every protocol | |

Only tcp to that address on that port is newly reachable. A packet to it that is anything else matches no allow, and
meets the default action of the service's NIC, which is reject: that closes every other tcp port, udp, icmp, and every
IP protocol a rule cannot name (GRE, IPIP, ESP, SCTP), so nothing can be tunnelled to that address around the one
port. Every other address is decided exactly as before, and a service with no destination has the five rules it always
had.

What never gets one:

- **A tenant's service.** The list is not read for a create in a tenant, so a tenant's service that bears a listed name
  is an ordinary tenant service; a call in a tenant that states `destinations` is refused; the plugin refuses a record
  that carries the key with a tenant; and a tenant's rules take no destination at all.
- **An address of the node's own.** The reject of the host's addresses is never cut, so such an address stays closed
  whatever a record says. The network and broadcast addresses of the node's own networks count as the node's own: the
  node itself answers on them. A proposal and a create that name one are refused in words.
- **A virtual machine on a host bridge.** Arca writes no rules there, so the create is refused: the machine reaches
  whatever that network reaches.
- **A service that exists.** An entry added for it changes nothing. Remove the service and create it again, through a
  proposal.

`service_list` and `service_status` carry `destinations: [{ "address", "port" }]` for such a service and no such key
for any other. `service_status` also answers `ready` for each, and `problem` when it is false: the rules Incus holds
for the service are not the ones its record gives, or the address has become the node's own.

`serve` refuses to start (exit 2, naming the service and the address) while a service's record carries a destination
the config no longer names for it. Put the entry back, or remove the service first. Entries the record does not carry
never stop a start. Known limit: taking the entry out of the config does not close the address. The record is what the
rules are built from, so the address stays open on Incus until the service is removed, and `serve` meanwhile does not
start. To close one: remove the service (a proposal, approved), then take the entry out.

Arca changes the service's own Incus ACL and nothing else: no firewall rule, route or sysctl of the host. The packet
must already have a way through the node, which is the node's to give:

- The host forwards IPv4 (`net.ipv4.ip_forward=1`), as it does for the service network to reach the internet.
- The service bridge's own NAT (`ipv4.nat`, which Arca sets when it creates the bridge) rewrites the source, so the
  packet leaves with the node's own address on the interface it leaves by. A destination on another bridge of the node
  sees the node's address on that bridge; one on the tailnet sees the node's tailnet address. A firewall at the
  destination must admit that address. That cuts both ways: anything at the destination that trusts the node's address
  now trusts every process of the service, every CI job among them. A reverse proxy or an allowlist there that takes
  the node's address for the node itself, and so believes a forwarded-for header from it, believes one a job wrote.
- The host's firewall lets a packet be forwarded from the service bridge (`arcak-svcbr`) to the interface the
  destination is behind. Where it does not (a forward policy of drop, or a zone that does not forward), that is the
  operator's rule to add.
- A destination on the tailnet needs the node's Tailscale to have an interface (kernel mode); a node whose Tailscale
  runs in userspace forwards nothing. The tailnet's own access rules must let the node reach it. A service that also
  joins the tailnet itself routes tailnet addresses through its own Tailscale and never uses the destination.
- Inside the service the name its clients use must resolve to that address.

None of this path has been checked on a live node yet, and neither has Incus 7.6 taking a reject and an allow of that
length and a port list with ranges on a bridged NIC; the rules themselves are tested against a model of Incus's
documented behaviour. Should Incus refuse a list, the ACL is not written and nothing opens. To check one on a node:
`service_status` answers `ready: true` for it; `incus network acl show arcak-svc-svc-<service>` shows the five rules
above; from inside the service `curl -sv https://<address>/` connects, and any other port of that address, and any
neighbouring address, is refused at once.

### A service that may talk to the kernel

A service's NIC ACL rejects the bridge gateway, so a service cannot reach the kernel's MCP over the network, and the
unix socket serves the server's own uid alone and may carry the approval page. A container template may declare that its service talks to the kernel:

```json
"kernelAccess": { "port": 7461 }
```

`port` is a port inside the instance, 1024 to 65535, that is none of the template's own `ports` and not its `dashPort`.
The service's instance then carries one more device, named `kernel`, and nothing else changes: its NIC, its ACL and the
unix socket are as they are for every service.

```json
"kernel": { "type": "proxy", "bind": "instance", "listen": "tcp:127.0.0.1:7461", "connect": "unix:<listen.services>" }
```

Incus listens inside the instance, on its loopback, and connects from the host to the kernel's services socket
(`listen.services`). Inside the instance the kernel is `http://127.0.0.1:7461/mcp`: plain HTTP that never leaves the
instance's loopback, so a client that allows plain HTTP on loopback alone works unchanged. Nothing crosses the bridge.

The device carries bytes and no authority. Every process in the instance reaches the socket, and each one gets what the
token it sends allows: a call without a token is 401, and a token is held to its scope and its tool list
(`arca token mint --tools`) on `POST /mcp` and on the GET routes alike. Give the service a token whose tool list names the verbs it needs, as a service secret.

A create of such a template is refused, before anything is written, unless all of these hold:

- **The service is the node's own.** In a tenant the create is refused, and so is its proposal, so the owner is never
  shown a card for it. No verb installs a template: a tenant names a template that is already in
  `<stateDir>/templates`, and one that declares `kernelAccess` is refused there. Beneath that, a tenant's project blocks
  every proxy device (`restricted.devices.proxy=block`).
- **The node has the socket.** `listen.services` must be set. When it is not, the refusal says what to set:
  `"listen": { "services": "/run/arca/services.sock" }` (a path beside `listen.unix`) in the node's config, then restart
  `serve`. No other listener stands in for it: the unix socket, the tailnet listener and `listen.proxy` may carry the
  approval page, whose routes need no token, so no device is ever pointed at one of them, and `listen.loopback` is a
  TCP port, which another program could take while the server is down. A node that serves its
  approval page and has `listen.services` gives kernel access like any other.
- **The owner approved it.** Such a service is created only from a proposal: `action_propose` with `service_create`,
  then the owner's approval. A direct `service_create` of such a template is refused with `NotApproved`, whatever the
  token's scope. The proposal carries the template's key, `"kernelAccess": { "port": 7461 }`, filled in from the
  template, so the approval binds it, and the owner's card carries a `Kernel access` row: "Yes: this service can call
  this node's kernel from inside, at http://127.0.0.1:7461/mcp, with whatever token it is given. The token's scope and
  tool list decide what it may do, and a call without a token is refused." A template that gains the key, or changes
  its port, after its card was shown is not created by that approval. A call that states the key for a template
  without it is an invalid call.

Only a container template takes the key: a virtual machine has no device that listens inside it.

`service_list` and `service_status` carry `kernelAccess: { "port", "connect" }` for such a service and no such key for
any other. `connect` is the socket the device was made for. `service_status` also answers `ready`, and `problem` when
it is false: the instance does not carry the device as Arca made it, or the node's `listen.services` is no longer the
socket the device connects to.

The device keeps the path it was made with. `serve` therefore refuses to start (exit 2, naming the service and the
path) while any service's record names kernel access through a socket the config does not serve as `listen.services`:
one that is no longer set, or one at another path. Set it back, or remove the service first. This holds whatever else
the config has become, so a device is never left connected to a listener that could carry the approval page.

The device is the instance's own. It stays through a restart of the instance and of `serve` (tested against the fake
Incus), and it is deleted with the instance by `service_remove`. That it is there again after a restart of the host is
Incus's to keep, in its own database, and is not yet checked on a live node. A record that is lost and made again from
the instance's marks reads the device back when it connects to this node's own `listen.services`, so `service_list`
still says the service has the access.

A `kernel` device somebody made by hand, with `incus config device add`, is not an access Arca gave. One that names
another path is never read into a record: `service_list` shows no `kernelAccess` for it, `serve` starts as it would
without it, and nothing of Arca listens at that path. One that names this node's own services socket, on an instance
whose record was lost and made again from its marks, is read back as above, and `serve` then refuses to start without
that socket, as for any service with the access. Arca does not remove such a service (it writes nothing to a record
made this way), so the way out is the device: start `serve` with `listen.services` at the path the refusal names, take
the device off (`incus config device remove <instance> kernel --project arcak-svc`), and call `service_list` once. The
record follows the device at every service call, so it no longer names the access, and the config may then change.

The effect journal records the create of the instance, and the digest it keeps (`op_digest`) is the digest of the create
that carries the device; `journal_list` shows the effect, not the body. The service's record in the desired state names
both ends of the access, and a proposal's journal line keeps the card's row.

What a process in the instance can and cannot do through the device:

- With no token, nothing: every MCP route answers 401, and no other route exists on the socket.
- With the service's token, what that token allows. Any process in the instance that can read the token can use it.
- It can hold connections open. The socket's own limit of 64 is then spent, which stops other services with kernel
  access from calling the kernel; the unix socket, the tailnet listener and the proxy listener are not affected.

Checked on a live node (Incus 7.6, SELinux enforcing): a container with a proxy device of this shape (`bind=instance`,
`listen=tcp:127.0.0.1:<port>`, `connect=unix:<path>`, the path a socket of mode 0600 under the server user's runtime
directory) reached the socket on the host, and the host saw the peer as uid 0, which the services socket serves.

Not yet checked on a live Incus: the same device on a container of the restricted service project as Arca itself
creates it, on an application (OCI) container, on a host confined by AppArmor in place of SELinux, and after a restart
of the host.

To check a service on a node without writing anything: `service_status` answers `kernelAccess.ready: true`; `incus
config device show <service> --project arcak-svc` shows the `kernel` device above; `ls -l` shows the socket at mode
0600, owned by the server's user; and the service's own log shows its first call to the kernel answered. If the
service's calls are closed with no answer while `ready` is true, Incus is not connecting as uid 0.

`service_remove` takes `{name, approval}` and returns `{removed: true}` after full success. The owner signs a delete
approval for target `name` and arguments `{name}` through the owner request flow arriving in Group 7. Removal spends
that service-action approval once, then removes the desired entry and runs its pending delete plan under one kernel
lock. Any reconciliation failure or partial plan run is a typed refusal naming whether the instance and ACL remain;
the approval is spent, so another try needs a new one. Secret files are removed only after full success. An adopted
record is refused.

### Who may call what

| Caller | Result |
|--------|--------|
| no `Authorization`, a malformed bearer or an unknown token | 401 |
| a `doctor` calling `token_list` | 403 |
| a `viewer` calling `token_list`, or any verb that writes (`plan_decline`, `approval_submit`) | 403 |
| any request carrying an `Origin` header | 403, before the body is read |
| a call the caller may make | 200, with the verb's result or a JSON-RPC error |

`tools/list` shows a caller only the verbs it may call.

`action_propose` records a durable proposal, so its MCP annotation advertises `readOnlyHint: false` even though it changes no desired state.

#### The OpenRPC method flags

`schema/arca.openrpc.json` gives every method two flags about writing. They are different questions:

- `x-arca-writes`: the verb changes desired state, so a viewer may not call it and `tools/list` hides it from one. A released method may not turn it on.
- `x-arca-read-only-hint`: the `readOnlyHint` the MCP door serves for the verb in `tools/list`. It is `false` for every verb that writes, and also for `action_propose` (it records a durable proposal) and `owner_approval_status` (it hands the requester an approval string), which change no desired state. A client that reads the hint from the OpenRPC document sees exactly what the MCP door serves. A change in either direction against a released document is a break.

The 0.1.0 release predates `x-arca-read-only-hint`, so its methods carry only `x-arca-writes`.

`tools/list` advertises each verb's MCP `outputSchema`: the natural result schema, wrapped under its `output.key` when the result is not an object, with `truncated` and (for an untrusted verb) `untrusted` added and every object level open. The OpenRPC document keeps the natural schemas.

`tools/call` answers a success with a short summary text block and `structuredContent` that carries the result, the untrusted note when the verb is untrusted, and `truncated` only when the result was cut; `_meta` carries `droidd/untrusted` and `droidd/truncated`. A refusal is a JSON-RPC error with `data.refusal`. A tool-level failure that is not a refusal is `isError: true` with `<code>: <message>` and `_meta` `droidd/error`.

Every destructive verb (`destructiveHint: true`: `approval_submit`, `action_approve`, `postgres_delete`, `service_remove`, `service_import`, `service_release`, `gpu_detach`, `storage_add`, `snapshot_delete`, `instance_stop`, `instance_restart`, `function_remove`) takes an optional boolean `confirm`: absent or `true` runs the call as without it, `false` is refused (`NotConfirmed`, "confirm is false: nothing was removed and no approval was spent") before the handler runs, and the door takes it out first, so it never enters a statement's `paramsDigest`, a plan, a proposal, the journal or the desired state.

### Refusals for want of room and for asking too often

A refusal's code is `data.refusal`, and its `message` is the reason in words. Most codes say everything a caller can
act on. Two families of refusal are told apart from all the others, so a caller never has to read the sentence: the
call would pass a cap (`capacity`), or the caller asked too often or already has too much waiting (`rate`). The
sentences are the ones these refusals always carried. Until this change, the sites of `capacity` and `rate` came back
with the plain code `refused`.

| Code | Family | When | Fields beside `refusal` |
| --- | --- | --- | --- |
| `capacity` | `capacity` | The call would pass a cap, and nothing was written. A tenant's caps on `service_create` (`limits.instances`, `limits.containers`, `limits.virtual-machines`, `limits.cpu`, `limits.memory`, `limits.disk`). A tenant's block with no free address. The node's virtual-machine caps on `service_create` (`kernel.vmLimits`, or `kernel.bridged.limits`), and a virtual-machine count set below the machines the node holds. The node's room for tenants on `tenant_create` (`kernel.tenants.maxTenants`, `kernel.tenants.capacity`, the node's virtual-machine cap, and no free block). No free address in the service subnet. | `scope`, always: `tenant` or `node`, whose cap it is. `limit`, always: `instances`, `containers`, `virtualMachines`, `cpu`, `memory`, `disk`, `tenants` or `addresses`. `have`, `want` and `max` when known: what is in use, what the call asked for and what the cap allows. Memory and disk are bytes; every other cap is a count. `proposeAgain: true` when the refusal came from running an approved action. |
| `rate` | `rate` | A wait ends it. `tenant_create` past `kernel.tenants.createRate`. `owner_approval_request` when the token already has 5 requests waiting, or Arca 20. `omni_pair` while a pairing code lasts, or inside the 60 seconds after an attempt. | `retryAfterSeconds` when the wait is known: whole seconds, rounded up, at least 1. `proposeAgain: true` when the refusal came from running an approved action. |
| `RateLimited` | `rate` | `action_propose` when one token has recorded its thirty new proposals of ten minutes. The code it has always had. | `retryAfterSeconds`: the wait until the token's oldest counted proposal leaves the window. The sentence says "ten minutes", the whole window. |
| `TooManyProposals` | `rate` | `action_propose` when eight proposals already wait, from any token. The code it has always had. | None. Room is made when one is approved or declined, or lapses fifteen minutes after it was made. |

The OpenRPC document lists these four codes under `x-arca-refusals`, each with its `family`, a summary and the schema of
its `data`. A caller that wants one check for "asked too often" reads the family from that list instead of knowing
three names. Dropping a listed code, changing its family, or dropping a listed `limit` or `scope` value is a break
against a released document, as a removed method is. The list is not every code a verb answers: a code outside it
carries no field (apart from `untrusted`, for a refusal that quotes a service).

`scope` is the field a billing client needs. A call in a tenant is checked against the node's caps first and against
the tenant's own caps after, and both name the same caps. A tenant that is far under its own caps can be refused
because the node is full: the refusal then says `scope: "node"`, and `have`, `want` and `max` are the node's amounts,
not the tenant's. A client that serves a tenant's owner should say "the service is full", never "your plan is full",
for `scope: "node"`, and should not show the node's amounts to the tenant.

- `scope: "tenant"`: the tenant's own caps on `service_create`.
- `scope: "node"`: the node's virtual-machine caps, the count set below the machines the node holds, everything
  `tenant_create` is refused for, the service subnet with no free address, and a tenant's block with no free address.

A tenant's block with no free address is the node's, although the sentence names the tenant. A block holds an address
for every instance the tenant's caps allow, and those caps are checked first, so the tenant is under its instance cap
when this is answered: the block is full because something else holds addresses in it, such as a lease on the bridge
that is not one of the tenant's services. Nothing the tenant removes makes room, so its plan is not what is full.
`have` and `max` are the block's addresses.

`proposeAgain: true` marks a refusal that came from running an approved action (through `action_approve` or the
approval page; the message begins "The approval was accepted, but"). The approval was spent by the run. Waiting, or
making room, does not make that same call pass: the action is proposed again and approved again. An approved
`tenant_create` that runs past `kernel.tenants.createRate` is the case that happens. Without the field, a `rate`
refusal means the same call is allowed again after the wait.

What the other fields mean in the cases that are not obvious:

- For `tenant_create`, the amounts of `instances`, `cpu`, `memory`, `disk` and `virtualMachines` are caps set aside, not
  use: `have` is what the tenants the node holds were promised (with the node's own machines, for
  `virtualMachines`), and `want` is what the new tenant asks to be promised.
- An amount that could not be read (a template whose size is not a size) counts as more than any cap. The sentence says
  "an unreadable amount", and `want` is left out.
- A virtual-machine count set below the machines the node holds refuses every create that would write the project,
  a container's too. It carries `have` and `max` and no `want`.
- No free block for a new tenant carries `limit: "addresses"` and no amount.
- `retryAfterSeconds` is exact for `kernel.tenants.createRate`, the Omni hold-off and `RateLimited`. For the two caps
  on pending approval requests it is the longest the wait can be: the time until the first of the waiting requests
  expires. A request the owner decides makes room sooner. Those two caps are `rate` and not `capacity` because what
  is full is the caller's own queue to the owner, not the node: no instance, CPU, memory or disk ran out, the remedy
  is to wait, and the sentence has always carried "429".

Not in the list, and unchanged:

- `FolderTooBig` (a folder does not fit the snapshot storage) keeps its code.
- A node that sets no virtual-machine caps at all, a config that disagrees with the desired state, and a template
  whose data volume states no size are still `refused`: nothing ran out, and waiting changes nothing.
- A create that Incus itself refuses for a project limit comes back as the create's failure, in Incus's words.

The approval page answers 403 `{refusal, message}` for a refused action, and for these codes the same fields stand
beside them. The proposals journal keeps the code and the message of a failed run, not the fields. A refusal relayed
from a linked machine carries its code and its message, and not the fields. The page's own 429 answers (a setup code
asked for too soon or while one is live, a setup lock, too many sign-in challenges) carry `refusal: "rate"` beside
`message`, and `retryAfterSeconds` when the wait is known; a setup lock rounds it up to whole minutes, as its sentence
does, so it can be up to 59 seconds over. The third chat's bare 429 has no body.

## The MCP server

`serve` answers `POST /mcp` with JSON responses (streamable HTTP, protocol versions `2026-07-28` and `2025-11-25`) on the
unix socket. The verbs
in this build are `arca_status`, `node_metrics`, `watch_report`, `alerts_list`, `token_list`, `journal_list`, `plan_list`,
`action_propose`, `plan_decline`, `approval_submit`, `action_approve`, `peer_keys_sync`, the Postgres and ontology verbs, `service_create`, `service_list` and
`service_status`, `gpu_attach` and `gpu_detach`, `storage_list`, `storage_add`, `snapshot_create`, `snapshot_list` and
`snapshot_delete`, and `instance_start`, `instance_stop` and `instance_restart`.

The unix socket serves only a peer whose credentials carry the server's own uid. Any other peer is closed before a byte of
its request is read.

### When Incus does not answer, and when the answer is "internal"

Every service verb, a read too, first asks Incus for the node's instances. That listing holds each instance's status,
config and devices and is answered at once. It does not ask for an instance's **live state**, which for a running
virtual machine Incus takes from the machine's guest agent: a machine that runs many containers of its own can take
most of the 10 seconds a read is given, and a listing with every instance's state is as slow as the slowest of them.
Only three things read live state:

- `service_status` of a virtual machine **on a host bridge**, which runs: its address is given by that network and
  is in no record, so this one call reads the state of that one instance. When Incus does not answer in time the
  call still answers, with `ipv4` empty and `ipv4Problem` saying why. `service_list` reads no state: such a machine's
  row has `ipv4` empty and `ipv4In: "state"`, which says the address is there to be read and is not "none".
- `service_create` and `function_create`, and `tenant_create`: a new instance's address, MAC or block must not be one
  that an instance holds live without a record of it.

A read that failed fast (the socket was not reached, or Incus answered with a server error) is made once more after a
fifth of a second, inside the same 10 seconds. One that got no answer by its deadline is not made again. When the
listing still fails, and when one of the reads a create makes for an address, for its project or for its new instance
fails, the call is refused in words, with a code of its own in `data.refusal`:

| Code | What happened | What to do |
|---|---|---|
| `incus_unavailable` | Incus did not answer within the 10 seconds, could not be reached on its socket, or answered with a server error (status 500 and above). | Read the sentence. When it says nothing was changed, nothing is wrong with the request: make it again (an approved action is proposed again, since its approval was used). When it says a create or an import went through, or that an instance may exist, do NOT make it again: look with `service_status`, `service_list` or `function_list`, which the sentence names. |
| `incus_answer` | Incus answered with another status, with more than this node reads, or with something that is not the JSON it expects. | Asking again is likely to answer the same: look at Incus. |
| `state_not_saved` | The kernel could not save the change as the node's next state. The service's record is as it was. | The node's log has the reason; make the request again once it is fixed. |

The sentence says what was being read and for which service, and what the call had changed by then ("Nothing was
changed.", or that a create did go through). It never holds Incus's own text.

`internal: the tool failed` is left for a fault of Arca's own: a handler that panicked, an answer that does not fit
the verb's schema, or a failure a handler keeps to itself. A few failures that could be said in words are still among
them: the kernel refusing to save a create's own state, and the read of the host's addresses a function with
destinations makes. The caller is told no more than "internal". The node's log gets one line for each, with the verb
and a reason the server wrote (it can name a file of the node or a service, and never an argument of the call):

```
arca: service_status was answered "internal: the tool failed": the kernel could not complete the call
```

### Protocol versions

The server keeps no session, so each request is judged alone. The tools, their parameters and their results are the
same in both versions, and so are the tokens and their scopes.

- **`2026-07-28`.** A request says its version itself, with no `initialize` before it: the header
  `MCP-Protocol-Version: 2026-07-28`, the `Mcp-Method` header with the request's method, for `tools/call` the `Mcp-Name`
  header with the tool's name, and in `params._meta` both `io.modelcontextprotocol/protocolVersion` and `io.modelcontextprotocol/clientCapabilities`.
  Every result then carries `resultType` and the server's name in `_meta`, and `tools/list` carries `ttlMs` and
  `cacheScope`. A request that names this version and lacks one of those is answered `400` with a JSON-RPC error whose
  message lists everything that is missing: `-32020` when a header is missing or does not match the body, `-32602` when
  only `_meta` is incomplete. A method the server does not have is `404` with `-32601`; `ping` is one of them, since
  this version dropped it. An unknown tool is `-32602`.
- **`2025-11-25`.** A request with the header `MCP-Protocol-Version: 2025-11-25`, or with no version header at all, is
  a legacy request. `initialize` answers `2025-11-25` whatever version it asks for, `ping` answers, and `tools/list`
  and `tools/call` return what they returned before `2026-07-28` existed: no result gains a field, and a JSON-RPC
  error travels inside a `200`. The header values `2025-06-18` and `2025-03-26` are served the same way.
- **`server/discover`** answers `supportedVersions` (`2026-07-28`, `2025-11-25`), the capabilities and the server's name.
  It answers a request that names no version too, so one `curl` with a token shows what a node speaks.
- **Any other version**, in the header or in `params._meta`, is `400` with the error `-32022`. Its `data` holds
  `supported` and `requested`, the latter cut at 64 characters.
- A `GET` on `/mcp` is `405`. A `Mcp-Session-Id` header is ignored, and the server never issues one.
- The token is checked first, as always: a request without a valid one is `401` whatever version it declares.

Before `2026-07-28` the server read no version from a request. It now does, so four answers changed for a client that
never names `2026-07-28`. A client that follows `2025-11-25` meets none of them except the first, and only if it asks:

- `server/discover` answers. It used to be an unknown method, `-32601`.
- A version header that names a version the server does not know is `400` with `-32022`. It used to be ignored.
- A version header sent twice, or with a value that is not visible ASCII, is `400` with `-32020`. It used to be ignored.
- A version in `params._meta` (the key `io.modelcontextprotocol/protocolVersion`) is read even when no header is sent.
  An unknown one is `400` with `-32022`, one that is not a string is `400` with `-32602`, and one that differs from
  the header, or names `2026-07-28` with no header, is `400` with `-32020`. It used to be ignored.

`arca call` and the canvas are clients of this same route and still speak `2025-11-25`: `initialize`, then `tools/call`.
That is deliberate and needs no action. Both are released together with the server they call, the server answers
`2025-11-25` for as long as it is listed above, and a tool call returns the same result in either version, so moving
them would change nothing a user sees. They move to `2026-07-28` before `2025-11-25` is ever removed here.

- `arca_status` reports the node id, mode (`kernel` when kernel projects are configured, `watch-only` otherwise), uptime,
  whether Incus answers, `appliedSeq`: the applied desired-state document's sequence, or `null` before any document applies,
  and `metricsSource` (`local` or `remote`) with the reason it was chosen. See "Host metrics". A node with an `approval`
  key also reports `approval`: its `linkedHosts` by name, and one `trustedKeyFiles` entry per trusted passkeys file with its
  `index` in `trustOwnerKeysFrom` and its file `name` (never its full path), the `keys` it adds and, when it adds none, the
  plain `reason` (for example `the file is missing` or `the file cannot be read by this kernel`). It reports the files' last
  read, by an approval check or an earlier status call, and reads them again only when that read is more than 10 seconds
  old.
- `node_metrics` takes `minutes` from 1 to 60. The server samples the host every 5 seconds into an hour-long ring, on its own
  thread. A call reads the last samples published and never waits on Incus. Until the first sample is taken, `latest` and
  `mean` are `null`. See "Host metrics" for where its readings come from.
- `watch_report` reads every project and instance through Incus GET, but a call never starts a sweep. The `arca-watcher`
  thread sweeps once when `serve` starts and then every 30 seconds, so the GETs do not depend on how often the report is
  asked for. A call answers from the newest successful sweep, and every answer carries `ageMs`, the milliseconds since that
  sweep started. A sweep that runs longer than about 44 seconds makes `watch` stale: the stamp is the sweep's start, the
  next sweep starts only after the last one ends, so the newest stamp can be more than 90 seconds old before the next sweep
  finishes. A failed sweep leaves the previous report in place. Before the first sweep has finished, a call waits up to
  35 seconds for it. If no sweep has ever succeeded, the call answers the last sweep error, or `no watch sweep has finished
  yet`. Live desired-state entries do not exist yet, so the report has no entries to compare, and every kernel-marked
  object is reported as drift.
- `alerts_list` answers the node's alerts: the firing ones, the unknown ones, the disabled rules, the resolves of the last
  24 hours and the store's health. It reads the newest published tick and never waits on Incus. See "Alerts".
- `token_list` lists each live token's id, scope and optional tool list. It never lists a secret or a digest.
- `plan_list` lists each waiting delete plan and action proposal. A delete plan carries the statement its owner signs; an
  action proposal carries its action, arguments, details, kernel-built summary and `host`, the host its statements name.
  See the next section.
- `action_propose` takes `{action, args}` and records an inert action proposal, returning `kind`, `summary`, `details` and
  the record. It is a viewer verb: it changes no desired state and applies nothing, so MCP does not advertise it as
  read-only. The owner approves it on the approval page with a passkey.
- `plan_decline` retains a waiting delete plan's objects, or declines a waiting action proposal. A decline is final and
  survives a restart.
- `approval_submit` checks an owner's approval for a waiting delete plan and, when it holds, deletes the plan's objects once.
  Action proposals are approved and run by the approval page, which can reach the gated verb's handler.
- `action_approve` takes `{plan: "<act-id>", approval}`, checks the owner's passkey approval for a waiting action proposal
  with this kernel's own verifier, nonce store and sign counts, and when it holds runs the action once, exactly as the
  approval page does, answering `{plan, id, action, result}`. It is a doctor verb that writes. Another kernel's approval
  page on the same machine relays a linked host's approvals here; see "Approving on another kernel of this machine". It is
  served only by a kernel whose `approval` key names at least one `trustOwnerKeysFrom` file or sets `peerKeys`; any other
  kernel neither lists it nor answers it (`not_found`).
- `peer_keys_sync` takes `{lines, statement?, approval?, from?}`: the page host's whole owner passkey list, and, for a
  changed list, the statement this kernel issued with the owner's passkey proof over it (its JSON) in `approval`. `from`
  names the `approval.peerKeys` source whose list it is; absent, it is the first source. It answers
  `{status, keys, statement?}` with `status` `trusted` (first sync), `unchanged`, `needs_approval` (with the statement to
  sign; nothing written) or `changed`. No line is echoed. It is a doctor verb that writes, served only by a kernel whose
  `approval.peerKeys` is set; see "Approving on another machine".

`serve` listens on the unix socket, and on the loopback, tailnet and proxy addresses and the services socket when the
config names them. If any listener
fails, all of them stop and `serve` exits 1. On SIGTERM or SIGINT it stops every listener, removes its socket and exits 0.
SIGHUP reloads the bearer token store only; it does not reread the node config or the owner passkeys.
A socket that another server answers on is left alone.

`journal_list` is a viewer read over the effect journal. It returns the newest record for each effect and pages by effect
id with `after` and `limit` (default 100, maximum 500). Each record exposes only `id`, `entry`, `class`, `target`, `plugin`,
`state`, `at_ms`, `intent` and `object`; it never exposes `pre_image`, `inverse` or `acl_patch`, which can hold rendered
secrets. The read decodes the whole append-only file without the journal's open-time integrity checks, so a listing can show a
sequence `serve` would refuse at open; its cost grows with history.

## Host metrics

`node_metrics`, and `arca metrics --json`, describe the machine the node's Incus serves, never the machine Arca happens to run
on when the two differ. The mode is chosen at startup, in this order:

1. An explicit `incus.local` wins: `true` means the local daemon, `false` a remote one.
2. Otherwise the node reads the Incus server document, `GET /1.0`, and compares `environment.server_name` with the local
   host name, without case. A match is local; a mismatch is remote. A name of `localhost`, `localhost.localdomain` or an
   empty name identifies no machine and is treated as unknown.
3. If `/1.0` cannot be read, or it does not carry a usable server name, or the local host name cannot be read, the socket
   path decides: the standard local daemon sockets (`/var/lib/incus/unix.socket` and `/var/lib/incus/unix.socket.user`) are
   local and every other path is remote. This choice is provisional: each later sample retries the `/1.0` check, under the
   same 2 s cap and with no added startup delay, until the server names itself, then keeps that answer. The provisional
   choice stands for at most 10 minutes after the first provisional sample; then the sampler settles on the socket-path
   fallback for alerts and keeps probing `/1.0` every 60 seconds. The first real answer switches to the mode it decides,
   clears the sample ring, and ends the probing. The window and the cadence use a monotonic clock, so a wall-clock step
   does not end the window early.

The name comparison cannot tell two machines apart when both keep the same default name, such as two stock images still
named `ubuntu`: a match reads as local. The sampler has no way to resolve that ambiguity from names alone. Set
`incus.local` in `node.json` on that node; an explicit `true` or `false` always wins and ends the question.

The chosen mode and a value-free reason are logged at startup, and again if a provisional choice is replaced or settles on
the fallback, and returned by `arca_status` as `metricsSource` (`local` or `remote`) and `metricsSourceReason`. The reason
never carries a host name. A proxy whose upstream is the local daemon, such as the recording shim, therefore needs no
`"local": true`: its server name matches the local host name. A container that mounts the host's Incus socket at the
standard path is still remote when its own host name differs from the Incus server name, so the sampler never reports the
container's `/proc` as the managed host's. When a later answer resolves the mode to the other one, the sampler clears its
sample ring and its rate counters, so the window mean and the next rate cover only samples from the new mode.

- **Local daemon.** The sample is this machine's: `cpu.cores` and `cpu.usedPct`, the `load` averages,
  `memory.totalBytes`, `memory.availableBytes` and `memory.usedBytes`, PSI, CPU temperature, GPU busy, swap and NIC rates come
  from the node's `/proc` and `/sys` trees. This is the reading the sampler has always taken.
- **Remote Incus.** The host totals come from the node's own Incus, `GET /1.0/resources`: `cpu.cores` is `cpu.total`
  (threads), `memory.totalBytes` is `memory.total`, and `memory.usedBytes` is `memory.used`.
  `memory.availableBytes` is `memory.total - memory.used`. `cpu.coresKnown` is `true`. No local `/proc` or `/sys` reading is
  reported: `cpu.usedPct`, every `load` average, PSI, CPU temperature, GPU busy, swap and NIC rates are `null`. `cpu.reason`
  and `load.reason` say `not available for a remote host`. Once the mode is settled remote, PSI and CPU temperature are
  reported to the alert evaluator as readings this node can never provide, so a firing `mem-psi-high` or `cpu-hot` resolves
  with `no data` after its clear window instead of staying unknown forever. While the choice is still provisional they are
  not reported that way: the mode may yet resolve local, and a `no data` resolve would only fire the alert again after the
  switch. The provisional window is at most 10 minutes, so a fallback that never resolves local settles and the normal
  no-data handling starts then; a later `/1.0` answer can still switch the mode and end the probing. The pools, instance
  counts, incus-osd reading and SMART state still come from the same Incus, so they describe the remote machine too.
- **The resources read fails.** On a remote node the totals are `null` and `cpu.coresKnown` is `false`; `cpu.cores` stays an
  integer in the schema, so its `0` is a placeholder a consumer must not show as a count. `cpu.reason` and `memory.reason`
  say `the Incus resources read failed`. The node never falls back to its own `/proc`: a failed read is unknown, not the wrong
  machine. `load.reason` still says `not available for a remote host`.

### GPUs

Every sample carries a `gpu` object: the cards the node's Incus reports in `GET /1.0/resources`, `gpu.cards[]`. The PCI
address is the key. The NVIDIA UUID is extra identity when the card reports one, never the key, so two identical RTX 4060
Ti cards are two rows from the first sample and are never merged by product or UUID. The card fields are:

| Field | Meaning |
|-------|---------|
| `pci` | the PCI address, normalized to four lowercase domain hex digits, such as `0000:01:00.0` |
| `uuid` | the NVIDIA UUID when the card reports one, else `null` |
| `vendor`, `product` | the PCI database names |
| `driver`, `driverVersion` | the bound kernel driver and its version, `null` when no driver is bound |
| `numaNode` | the card's NUMA node |
| `vramTotalBytes` | the card's total VRAM when that is known; Incus's own document does not carry it, so it is filled from local `nvidia-smi` telemetry and is `null` on a remote host |
| `use` | `driver-bound` (a driver is bound), `vfio-pci` (reserved for passthrough) or `no-driver` |

The inventory is read for a remote host as well as the local one: it describes the machine the node's Incus serves. A
failed resources read is `gpu.reason: "the Incus resources read failed"` with no cards, never the last list and never an
empty "no GPUs" answer. A card the document does not give a PCI address is skipped, and `gpu.skipped` says so while the
other cards still list. A host with no GPU is a known-empty list, not a failure.

Telemetry is local only, like the CPU readings. Every tick, a local node runs `nvidia-smi
--query-gpu=pci.bus_id,uuid,utilization.gpu,memory.used,memory.total,power.draw,power.limit,temperature.gpu,clocks.sm,clocks_event_reasons.active`
for each card's numbers and `nvidia-smi --query-compute-apps=gpu_bus_id,pid,used_memory` for the processes. A query is
killed after 5 seconds and reads at most 1 MiB of output, so a hanging tool never blocks the tick. Each card carries
`telemetry` and a `reason` when it has none: `not measurable from here` on a remote host, `nvidia-smi is not installed`,
`the nvidia-smi read timed out`, `the nvidia-smi read failed`, or `no reading yet` when the tool ran but did not list the
card. One query runs at a time: a second query waits up to five seconds for the running one, then gets
`the previous nvidia-smi read has not finished`, and a killed child that the kernel cannot reap keeps the gate closed until
the child and its reader are gone; the approval path's in-use read uses the same gate. A query that cannot spawn its child
gives its place back. A missing number is `null`, never a zero. The card's own `processes` rows carry `pid`, `vramBytes` and
an `owner`: an Incus `instance` with its `project` and `name` (and a `container` id when the process is in a Docker container
inside that instance), a Docker `container` named by exactly 64 hex digits in its `docker-<id>.scope` cgroup, or `host`.

Attribution reads `/proc/<pid>/cgroup` by the host's PID, so a process in another PID namespace resolves the same way. The
cgroup v2 `0::` line is used when present; on v1 the first path that is not `/` is. `lxc.payload.` counts only as the first
path segment and is checked before Docker, so a Docker scope inside an instance belongs to the instance. A host Docker
container is accepted only as a `docker-<64 hex>.scope` segment below `system.slice`; anywhere else is host attribution.
Every shown name must be printable ASCII within its cap; a cgroup that cannot name an instance or a container is host
attribution.

The cards a successful inventory has seen are kept in memory, per process lifetime. A card that disappears is reported in
`gpu-gone` until it returns; a restart forgets the list, so a card already gone when the node starts is not reported as a
disappearance until it has been seen once.

`node_metrics`' `mean` averages each card's numbers over the window, keyed by PCI address, and carries the newest process
list. A card whose telemetry was never measurable has a `null` telemetry and a reason, not a zero.

A mean over a window that holds both successful and failed reads averages the successful ones and takes its reason from the
newest successful read, so a late failure does not label averaged figures. A window with no successful read is `null` with
the failure reason. The failure and provenance fields are additions: `cpu.coresKnown`, `memory.usedBytes`, `cpu.reason`,
`load.reason` and `memory.reason`. Every field the earlier schema carried keeps its name and meaning, and a local node's
figures do not change.

## Starting, stopping and restarting instances

`instance_start`, `instance_stop` and `instance_restart` take `{project, instance}` and change the power state of one
existing Incus instance. They exist so that Arca can bring its own instances back after a host crash, and stop or restart
one when the owner says so.

They act only on an instance Arca owns or has adopted: one in `kernel.projects` whose own config carries the kernel's
mark (`user.arcakernel.entry`, which an adoption leaves as `adopt-<name>`), the same check `gpu_attach` makes. An instance
outside the family, one with no mark, droidd's, one both marks contest, and one whose delete plan was declined are each
refused with `InstanceNotArca` and a sentence that says why; no write is sent to it (an instance inside the family is read first). A name that does not exist is
`InstanceUnknown`. A watch-only node has no state writer and answers `InstanceLifecycleDisabled`.

| Verb | Who runs it | On a running instance | On a stopped instance |
|------|-------------|-----------------------|-----------------------|
| `instance_start` | a `doctor` token, directly; no approval | nothing changes, `changed: false`, "is already running"; a virtual machine's stored secrets are written into its guest again | starts it, and writes a virtual machine's stored secrets into its guest |
| `instance_stop` | only an approved action proposal | shuts it down | nothing changes, `changed: false`, "is already stopped" |
| `instance_restart` | only an approved action proposal | shuts it down and starts it again | starts it, and says so |

A stop and a restart interrupt whatever the instance serves, so they are destructive verbs (`destructiveHint: true`, with
the optional `confirm`) and their `approval` takes only the one-shot grant that an approved proposal arms: a direct call
with anything else is `NotApproved`. All three can be proposed with `action_propose`, so a viewer (the canvas Doctor) can
ask and the owner approves on the approval page; an approved `instance_start` runs with no grant. The owner approval
requests (`owner_approval_request`) have no word for a stop or a restart, so those two run through a proposal only. An
instance that is not Arca's cannot be proposed either, so the owner is never shown a card for it.

A stop and a restart are graceful: Incus gives the instance 60 seconds to shut down. Arca forces nothing. One call takes
at most 150 seconds, and the approval page waits 180 seconds for a linked kernel's answer to an approval (30 for any other
relayed write), so a slow stop on a linked host still answers as done or with its own refusal. An instance
that does not shut down stays running and the call is refused with `InstanceStopFailed` or `InstanceRestartFailed`, with
the state the instance is in now. A frozen instance, or one in an error state, is left as it is
(`InstanceStateUnsupported`). An instance recreated between the owner check and the change is refused with
`InstanceChanged`, and nothing is sent to it.

The result is `{project, instance, action, status, changed, message}`: `status` is the state after the call and `message`
is the sentence a person reads.

A power state is not a desired-state entry, so these changes are not in `journal.jsonl` and `journal_list` does not show
them. Each call that passes its gate is written to `instance-actions.jsonl` in the state directory instead:

```
{"kind":"begin","id":"ia-<ms>-<n>","at_ms":<ms>,"token_id":"<the caller's token, or owner-approval>","proposal":"<act-… of the approved proposal, or null>","action":"start","project":"<project>","instance":"<instance>"}
{"kind":"end","id":"ia-<ms>-<n>","outcome":"started","code":null,"uuid":"<volatile uuid>","status":"Running"}
```

`outcome` is `started`, `already_running`, `stopped`, `already_stopped`, `restarted`, `started_from_stopped`, `refused` or
`failed`, and `code` is the refusal's code for the last two: `failed` only when a state change was sent and did not finish.
`proposal` is the approved proposal a stop or a restart ran from; a start carries no grant, so it is `null` there. A
start or a restart that wrote a virtual machine's secrets into its guest adds a third line,
`{"kind":"guest_secrets","id":"ia-<ms>-<n>","outcome":"written","secrets":["<name>", ...]}`, or `"outcome":"failed"`
with `"secrets":null`: it names the secrets and never holds a value or a reason (see "Secrets of a virtual machine").
The `begin` line is durable before anything is sent to Incus:
when it cannot be written (the file is a link, or is not mode 0600) the call is refused with `InstanceJournalUnavailable`
and nothing changes. A file past 8 MiB is set aside as `instance-actions.jsonl.1`, replacing the one set aside before.

## Snapshot storage and snapshots

The owner plugs a disk (normally USB) into any Arca machine, picks it as snapshot storage, and copies every app and data
volume onto it. The disk alone is then enough to bring the apps back after the machine is reinstalled with Arca OS.

### Disks and their roles: `storage_list`

`storage_list` (viewer) reads the host's disks with `lsblk --json --bytes --tree` (name, size, model, serial, transport,
removable, type, file system, label, UUID, mount points and the devices on each disk) and the Incus pools with their used
and total space. Loop, optical and zram devices are not listed. Each disk gets one plain role:

| Role | When | Offered as |
|---|---|---|
| `system` | something on it is mounted at `/`, `/boot` or as swap, holds swap, carries a member signature (any signature ending in `_member`, such as `linux_raid_member`, firmware RAID's `isw_raid_member` or `ddf_raid_member`, `LVM2_member` and `zfs_member`, or `crypto_LUKS`, `bcache`, `ceph`, `ceph_bluestore`, `drbd` or `swsuspend`), mounted or assembled or not, so one half of a failing RAID1 is never offered, is held by another device (its `/sys/class/block/<dev>/holders` is not empty), is a member of a multi-device file system that holds `/` or that the kernel lists as mounted, is given to an app (a `unix-block` device, or a `disk` device whose source is under `/dev/`, in any project), or the disk holds an Incus pool (by device, UUID, label, or a pool folder on its file system; a pool whose name Arca would never write is still counted) | never |
| `snapshots` | the disk holds a snapshot pool of this machine (a folder pool is matched by the file system UUID it recorded, so its disk is known even while it is not mounted), or a file system labelled `arca-snapshots-<name>` (a whole disk another machine made snapshot storage) | never: it is not erased |
| `in use` | something on it is mounted elsewhere | `folder`: a pool in a folder on it |
| `free` | nothing on it is mounted or a member of anything | `whole disk`: Incus erases and formats it |

`lsblk` lists a multi-device btrfs or bcachefs mount on one member only, so every device with the same file system type and
UUID, and every device the kernel lists under `/sys/fs/btrfs/<uuid>/devices/` or `/sys/fs/bcachefs/<uuid>/dev-*/block`, is
given the mount points of all of them before roles are decided: a second disk of the root btrfs is `system`.

Each disk carries a `reason` in plain words, and an `in use` disk lists its `folders` (its mount points). A pool has
`snapshots: true` when its config carries `user.arcasnapshot.storage=<its own name>`, which `storage_add` sets when it
creates the pool. The pool's description (`Arca snapshot storage`) is only for people; anyone can edit it, so it is not the
marker.

### Adding storage: `storage_add`

`storage_add` is gated like every action proposal: the Doctor (or any viewer) proposes it, the owner approves it with the
passkey, and it runs once. The arguments are `{name, device}` or `{name, path}`:

- `{name, device}` (a `free` disk, such as `sdb`) creates an Incus `btrfs` pool named `arca-snapshots-<name>` on the
  whole device, mounted with `compress=zstd`. The request sets `source.wipe=true`: Incus wipes the old partition table and
  file system signatures (a backup GPT at the end of the disk included) and then formats the disk, since `mkfs.btrfs`
  refuses a disk that still has a partition table. Incus labels the new file system with the pool's name, so the disk
  shows as `snapshots` on any machine. The kernel process never formats, mounts or writes a disk device.
- `{name, path}` (a folder on an `in use` disk) creates an Incus `dir` pool at `<path>/arca-pool`. Incus makes a
  folder pool only in a folder that is already there and empty, so when the approved add runs the kernel makes
  `<path>/arca-pool` first (mode 0700; any missing part of `<path>` with 0755), walking from `/` one part at a time with
  `openat`/`mkdirat` relative to the part before it and never through a link, then asks Incus for the pool; if Incus refuses, the empty folder it just made is removed again. That folder is
  the only thing the kernel ever writes on a disk, and nothing else on the disk changes. The proposal itself writes
  nothing: it refuses a folder the kernel may not write in (`FolderNotWritable`) and an `arca-pool` that is already
  there but is a link, a file or a folder with files in it (`PoolFolderInUse`). The folder is judged by where it really is: every link in its path is
  followed when the proposal is made, the resolved path is what the owner approves, and a folder that leads onto a system
  disk is refused. A folder that leads elsewhere by the time it runs is `DiskChanged`.

When the proposal is made the kernel reads the disk again and binds its device name, model, serial, transport, size and
current contents (and, for a folder, its mount point and the UUID of that file system, `fsUuid`) into the arguments, so the
owner's digest covers them. The folder pool records that UUID as `user.arcasnapshot.fs-uuid`. The approval card says
in plain words the disk model, size and transport, what is on it now (each partition with its file system and label, for
a folder target too), the pool name, and for a whole disk: **"Everything on this disk will be erased."** The run reads the disk again and refuses
anything that changed (`DiskChanged`), so a different disk plugged in under the same name is never formatted.

Before a whole disk is erased, when the proposal is made and again when it runs, the kernel opens the disk and every
partition on it read-only with `O_EXCL` and closes them at once. The kernel refuses that while a mounted file system (in any
mount namespace), a RAID, a volume group, an encrypted mapping or swap holds the device, whatever `lsblk` showed: that is
`DiskBusy`. When the open is refused for permission (`EACCES` or `EPERM`, as for a service user outside the `disk` group),
and only then, the kernel checks through the system's own lists instead, and every check must pass for the disk and each
partition: no mount in `/proc/self/mountinfo` (by device number or by source), no active swap in `/proc/swaps`, no holders,
no membership of a mounted btrfs or bcachefs (`/sys/fs/btrfs`, `/sys/fs/bcachefs`), no RAID, LVM, ZFS, LUKS or swap
signature, and the classifier's own rules (the file system siblings of a mount, devices given to apps) still find the disk
`free`. A disk in use by any of them is `DiskBusy`; lists or kernel links that cannot be read, or a disk or partition the
system lists no device number for, are `DiskUncheckable`. Which check was used
is bound into the arguments (`inUseCheck`: `exclusive open` or `system lists`; a run that would use the other one is
`DiskChanged`), and a card checked through the lists says
**"In-use check through system lists (Arca can't open the disk directly)."** Any other failure to open the disk (it is
gone, or is not a block device) is `DiskUncheckable`. Folder targets are not opened. While any folder snapshot pool cannot be reached (its folder is missing or its recorded file system is not mounted),
every whole-disk erase is refused (`SnapshotStorageUnreachable`: "plug it in or remove it first"), since the disk about to
be erased could be that one.

Refusals: a system disk, a member disk or a disk holding a pool (`SystemDisk`); a disk that holds snapshots, as a whole
disk or as a folder (`SnapshotDisk`); a mounted disk as a whole-disk target
(`DiskMounted`); a partition instead of the whole disk (`NotWholeDisk`); a device or folder not in `storage_list`
(`DiskNotListed`); a pool name or folder already in use (`PoolExists`); a folder the kernel may not write in
(`FolderNotWritable`); an `arca-pool` that is not an empty plain folder (`PoolFolderInUse`); a folder on a vfat, exFAT or
NTFS file system, which has no Linux owners or permissions and cannot keep app copies (`FolderFileSystem`); a disk held at
the moment of erase (`DiskBusy`) or one that cannot be checked (`DiskUncheckable`); an unreachable folder snapshot pool
(`SnapshotStorageUnreachable`); and every storage action on a watch-only node
(`StorageDisabled`). Snapshots are written only to a pool that carries the `user.arcasnapshot.storage` marker.

### Taking snapshots: `snapshot_create`

`snapshot_create` (gated) takes `{project, instance, pool}` or `{project, volume, pool}`, for any project, or
`{folder, pool}` for a host folder (see [Folder snapshots](#folder-snapshots)). The copy goes into
the project `arcak-snapshots` on the chosen snapshot pool, named `<project>--<name>--<UTC yyyymmddThhmm>` (each part shortened
when needed so the name fits 63 characters). The kernel creates `arcak-snapshots` the first time, with
`features.networks=false` so a copy's network resolves the default project's, and its own profiles, images and volumes.

- An instance is copied stateless and alone (`instance_only`, not its own snapshots), with `boot.autostart=false`, and is
  never started: the request carries no `start` and the writer has no way to start anything. The copy uses no profile:
  the source's expanded config and its root disk (moved to the snapshot pool) and NICs are written into the copy itself.
  Incus fills every key and device a copy request leaves unset from the source's own (`createFromCopy`), so leaving
  something out does not drop it. The create request masks each of the source's own keys it drops with an empty value
  (Incus does not store one) and each of its own devices it drops with a `none` device; once Incus has made the copy, one
  `PUT` of the copy writes its clean config and devices (keeping the volatile keys Incus gave it) and
  `user.arcasnapshot.verified=true`, and the copy is read back. A copy that still runs, may start, uses a profile, carries a live ownership mark, any device of its source, a mask
  or a root disk off the snapshot pool is deleted, and the snapshot is `StorageWriteFailed`. A copy Incus is still making
  when the verb answers is finished the same way in the background once Incus is done; until then it already carries no
  live mark and only `none` devices. A background failure goes to the service log and to the action journal against the
  approved proposal (`SnapshotCleanupFailed`).
  Devices that would tie the copy to the source project or to this host (custom volumes, host folders, GPUs, proxies, USB
  and the like, and a NIC on a network the source project owns) are left out and named in
  `user.arcasnapshot.dropped-devices`. NICs lose their fixed MAC and IP addresses, so the copy never claims its source's.
- A custom volume is copied alone (`volume_only`) into `arcak-snapshots` on the snapshot pool, keeping its config except
  the source pool's driver keys. It is read back too: a volume copy that carries a live ownership mark is deleted and
  refused.

Sources are only read: they are never stopped, changed or deleted, a running app keeps running, and no write ever names the
source's project. Every copy records its source in `user.arcasnapshot.source-project`, `source-name`, `source-kind`,
`source-pool` (volumes) and `created`. A copy carries no live ownership mark: the kernel's `user.arcakernel.*` keys and
droidd's `user.droid.*`, `user.tactu.*` and `user.arca.*` keys are moved under `user.arcasnapshot.mark.<key>`, so neither
owner ever takes a copy for its own, and a restore can still see who owned the source. A copy still running after twenty seconds is reported as `copying`; Incus finishes it
on its own and `snapshot_list` shows it. A copy Incus refuses (for example a running virtual machine, or a full disk) is
`StorageWriteFailed` with Incus's own message. A pool not added by `storage_add` is `NotSnapshotStorage`; a source that is
not there is `SourceUnknown`; a name already taken (two snapshots of one app in the same minute) is `SnapshotExists`.

### Folder snapshots

Before a machine is reformatted, everything on it must be copied off, including big host folders that are not Incus
instances or volumes, such as `/home` or `/mnt/ai`. `snapshot_create` with `{folder, pool}` copies one such folder into
snapshot storage. It is gated like every snapshot: the owner approves it with the passkey and it runs once.

When the proposal is made the kernel resolves the folder and binds into the owner's digest: the folder's path (absolute,
trailing `/` dropped), the UUID of the file system it is on (`fsUuid`, from `lsblk`), its size in bytes of regular files
(`size`), the space its paths take (`allocated`: each file's size or its blocks, whichever is larger, so a compressed
source is not undercounted; the blocks of everything else), its
number of files and folders (`files`), the folders it could not open (`unreadable`), the separate file systems inside it
(`separate`, up to 100 paths, and `separateCount`), the target pool, and the pool's free space (`poolFree`, shown only).
The measurement only reads names and sizes: it does not follow links, stays on the folder's own file system (a disk
mounted inside it, or a btrfs subvolume, counts as one empty folder and is listed in `separate`), and leaves out a
top-level `lost+found`, sockets and device nodes, as the copy does.

The space the copy takes is estimated as `allocated` plus 4 KiB for each path. That estimate plus 10% must fit the pool's
free space. The snapshot is named `folder--<path>--<UTC yyyymmddThhmm>` (`folder--home--20261109T1405`); a path too long
for the name keeps its end and gains 8 hex digits of its SHA-256.

The card says: "Copy the folder /home (177 GB, 1.2 M files) into snapshot storage usb1". Its rows are:

- the folder;
- its size and files;
- the target;
- "Space it takes" (the estimate) and the free space after;
- the snapshot name;
- "Read-only: The folder is not changed".

When some folders could not be opened, the size and the space are marked as estimates ("an estimate: N folder(s) could
not be measured"), and a "Not measured" row says the snapshot gets no size limit. When the folder holds separate file
systems, a "Not copied (separate file system)" row lists up to ten of them ("and N more") and says to snapshot them
separately.

Refused at proposal and again at run, with a plain reason: a path that is not absolute or has `.`/`..` parts (invalid);
`/` itself, or `/proc`, `/sys`, `/dev`, `/run`, `/boot`, `/tmp`, `/var/lib/incus` or anything under them
(`FolderRefused`); a path that does not exist or is not a folder (`FolderNotFound`); a link anywhere in the path
(`FolderLink`: name the real folder); a folder inside a folder snapshot pool, or one that holds the target pool, so the
copy would copy itself (`FolderInSnapshotStorage`); a folder on no disk `lsblk` lists (`FolderNotOnDisk`); a folder whose
space estimate plus 10% does not fit the pool's free space (`FolderTooBig`; `SpaceUnknown` when Incus does not report the
pool's space); no regular pool for the copy helper (`HelperPoolMissing`); and, at run, a folder now on another file system than the one approved (`FolderChanged`).

The kernel runs as an unprivileged user and never copies the bytes. Incus does, in the project `arcak-snapshots`:

1. A custom volume named after the snapshot is made on the pool. Its size limit is 1.2 times the space estimate (at least
   1 GiB); there is no limit when some folders could not be measured, and then only the pool's free space bounds it. It
   has `security.shifted=true` so files keep their real owners on disk, and the marks
   `user.arcasnapshot.source-kind=folder`, `source-path`, `source-name` (the path), `fs-uuid` and `created`.
2. A copy helper container `arca-copy-<16 hex>` is made from a small image Incus fetches once into `arcak-snapshots` and
   keeps under the alias `arca-folder-copy` (`debian/13` from `images.linuxcontainers.org`: BusyBox `tar` in Alpine keeps
   no extended attributes or ACLs, and the Debian image has GNU `tar`, `find` and `awk` without needing a network). Its
   root disk is on a regular pool of the machine, never on snapshot storage (an unprivileged container cannot walk a
   snapshot pool's folder, so it would not start): the first pool in `incus.pools` that exists and is not snapshot
   storage, else the default profile's root pool; with neither, the snapshot is refused (`HelperPoolMissing`). Only the
   volume is on the snapshot pool. The
   folder is a read-only disk at `/src` with `shift=true` (owners kept), the volume is at `/dst` with `limits.read=150MB`
   and `limits.write=80MB`. Incus refuses the caps on some pools, without naming them: one host answered `Invalid block
   device "tmpfs"`, and they were refused on an ext4-backed pool too. So any refusal of the
   helper's create or start with the caps on is logged, and the helper is made once more without them. The copy is then
   not left unthrottled: `tar` pauses one second after every 7800 records (about 80 MB), so it reads at most about
   80 MB/s. The answer's `ioCaps` and `io` (`incus-caps` or `throttled-by-tar`), the manifest's `io`, and a verified
   volume's `user.arcasnapshot.io-caps` say which way it ran. The helper has no NIC, no
   profile, `boot.autostart=false`, `security.privileged=false`, `limits.cpu=2` and `limits.memory=1GiB`. It carries
   `user.arcasnapshot.copy-helper-for=<snapshot>`. Arca stops, deletes or runs a command in an instance only when its
   name starts with `arca-copy-` and it carries that mark. It is read back before it starts; one that is not exactly
   that is deleted.
3. One command runs in it:
   - **The list.** `find /src -mindepth 1 -xdev` lists what is copied: a top-level `lost+found`, the manifest, sockets
     and device nodes are left out.
   - **The copy.** `tar -C /src --null --no-recursion --sparse --ignore-failed-read --numeric-owner --acls --xattrs -T -`
     copies exactly that list (and `/src` itself) into `tar -C /dst --numeric-owner --same-owner --acls --xattrs -xpf -`.
     This keeps owners, modes, times, symlinks, hard links, fifos, sparse files, ACLs and extended attributes.
   - **No `--sparse` when a file takes no blocks.** If any regular file with bytes in it takes no blocks (inline data,
     a FUSE folder), `--sparse` would copy it as zeros. The copy then runs without `--sparse`, and the manifest says
     `sparse: false`. The answer gets `sparse: false` and a note: sparse files take their full size.
   - **Skipped.** Sockets cannot be copied (apps make them again), and an unprivileged helper cannot make device nodes.
     Both are left out of the copy and of both counts, and counted as skipped.
   - **Separate file systems** inside the folder are not copied (`-xdev`). The kernel's list of them is recorded.
   - **Changes and read errors.** With `--ignore-failed-read`, `tar`'s exit code does not tell a vanished file from a
     failing disk, so its messages and the listing's decide.
     - These messages are changes, not failures: `file changed as we read it`, `File removed before we read it`,
       `Cannot stat/open: No such file or directory`, `File shrank by N bytes; padding with zeros`, and `find`'s
       `No such file or directory`. They are matched against an anchored allow-list.
     - Any other message is a read error: for example `Read error at byte ...` (EIO), or `Permission denied`.
     - A failed listing with no vanished folder to explain it is also a read error.
     - A read error makes the copy incomplete, and the reason quotes the first message.
   - **Counts.** Both sides are then counted: every path, and the bytes of regular files.
   - **The manifest.** `/dst/.arca-manifest.json` is written: `{version: 3, snapshot, sourcePath, fsUuid, started,
     finished, source: {paths, bytes}, copy: {paths, bytes}, skipped: {sockets, devices}, changed: {files, head},
     readErrors: {lines, head}, separate: {count, paths}, io, sparse, tar: {list, read, write}}`. Each `head` holds the
     first ten messages.
4. The helper is stopped (forcibly, waiting until it reads back stopped, since a failed start can leave it held) and
   deleted on every exit path; a delete refused as running is stopped and retried once.
   - **Verified.** Only when both sides counted the same does the volume get `user.arcasnapshot.verified=true`, with
     `paths`, `bytes`, `skipped-sockets` and `skipped-devices`.
   - **Changed during copy.** When files changed while they were read, the volume also gets
     `user.arcasnapshot.changed-during-copy=<number of files>`.
   - **Incomplete.** Otherwise the volume is kept with `user.arcasnapshot.incomplete=<reason>`: the counts differ,
     reading or writing failed, the helper did not start, or the copy did not run to its end.

A copy that ends within twenty seconds answers one of two ways:

- `copied`, with:
  - `files` and `bytes`;
  - `ioCaps` and `io`;
  - `changedDuringCopy` and `changedFiles` (the first messages);
  - `skipped: {sockets, devices}`;
  - `separate` and `separateCount`;
  - `notes`, plain sentences for anything the owner should know. Examples: "3 files changed while being copied; take
    another snapshot with the apps stopped", sockets or device nodes left out, separate file systems not copied, or a
    helper that could not be deleted (Arca removes it when it next starts).
- `SnapshotIncomplete`, with the reason.

A longer one answers `copying`, and `snapshot_list` shows it as `copying` while this process runs it. When it ends, its
outcome is journalled against the approved proposal (`SnapshotCopied`, `SnapshotIncomplete` or `StorageWriteFailed`) and
logged with the same notes. A copying folder snapshot cannot be deleted (`SnapshotBusy`). A copy is never resumed: when the kernel starts, it
deletes every copy helper left in `arcak-snapshots` and marks each unfinished folder volume incomplete ("the copy stopped
when Arca restarted"). A file that changes while it is copied is marked as above. A folder whose paths or bytes count
differently afterwards (files added or deleted mid-copy) is incomplete. Stop what writes to it first.

The volume and its manifest hold what a restore needs: the source path, the file system UUID, the counts and the files with
their owners, modes, times, links and attributes. Restoring a folder snapshot is not part of Arca yet.

### Listing and deleting: `snapshot_list`, `snapshot_delete`

`snapshot_list` (viewer) lists every copy in `arcak-snapshots`: its name, kind, source project and name (and a volume's
source pool, or a folder's `sourcePath`), pool, size, when it was made, its state, and the devices the copy left out. A
ready folder copy whose files changed while they were read also has `changedDuringCopy` and a `note` with the sentence
above. The
state is `copying` while Incus makes it, `ready` once done, and `incomplete` for an app copy without
`user.arcasnapshot.verified` (its clean-up never ran: the process stopped first, or it failed and the copy could not be
deleted) or a folder copy whose counts did not match (its `reason` says why); delete an incomplete copy and take it
again. A copy helper is never listed. A volume copy is made with its final config in one request, needs no clean-up and is `ready` once done. `snapshot_delete`
(gated) takes `{name}` and deletes that one copy; only names in `arcak-snapshots` are found, so nothing outside it can be
deleted.

The snapshot writer (`arca_incus::SnapshotWriter`) sends only these writes, and refuses anything else before the socket is
touched: `POST /1.0/storage-pools`, `POST /1.0/projects` (for `arcak-snapshots` only), `POST` and `DELETE` of an
instance or a custom volume with `?project=arcak-snapshots`, and the one clean-up `PUT` of a new instance copy there. For a
folder snapshot, also in `arcak-snapshots` only: `POST /1.0/images` (the helper image), the `PUT` of the folder volume's
marks, and `PUT .../state` and `POST .../exec` of the copy helper, which the writer sends only to an instance that reads
back as a copy helper. No `PATCH`, and a snapshot copy is never started.

### Restoring from the disk alone

After the machine is reinstalled (Arca OS), plug the snapshot disk in and:

1. Initialize Incus as usual, with the default bridge (`incusbr0`) the copies' NICs name.
2. Create the snapshot project with the same features, so the recovery finds it:
   `incus project create arcak-snapshots -c features.networks=false -c features.profiles=true -c features.images=true -c features.storage.volumes=true`.
3. Run `incus admin recover`. Name the pool as it was (for a whole disk, the file system label `arca-snapshots-<name>`
   that `lsblk -o NAME,LABEL` shows), its driver (`btrfs` for a whole disk, `dir` for a folder) and its source (the disk's device, such as `/dev/sdb`, or the folder
   `<path>/arca-pool`). Incus scans the pool and lists the copies and volumes in `arcak-snapshots`; confirm to import them.
4. Copy each app back into its project and onto the machine's own pool, then start it:
   `incus copy <snapshot> <name> --project arcak-snapshots --target-project <project> --storage default`, then
   `incus config unset <name> boot.autostart --project <project>` (the copy kept `false`) and
   `incus start <name> --project <project>`. A data volume comes back with
   `incus storage volume copy <pool>/<snapshot> default/<name> --project arcak-snapshots --target-project <project>`.
5. Add back anything the copy left out (its `user.arcasnapshot.dropped-devices`), such as a data volume's disk device or a
   proxy. The restored app is not owned by the kernel or droidd until it is adopted again; its old marks are readable
   under `user.arcasnapshot.mark.*`.

A copy needs nothing from its source project: no profile, no volume and no project network. The tests check this on every
copy they make.

## Calling the server

`arca call <verb>` makes one call and prints its result. The token is read from `--token-file`, which must be a regular file
with no group or other permission bit, and holds the secret and one newline. Its text is never printed. The arguments are a JSON
object: `--args <json>` for the ones that carry no approval, or `--args-file <file>`, which must be private in the same way.

`--peer <name>` selects a paired peer by name and may appear before or after the verb. It requires a running local
`arca link serve` and reuses that endpoint through its control socket in `stateDir`. Each POST opens a fresh control
connection and bidirectional stream using `arca/mcp/1`. The HTTP request and bearer header reach the peer unchanged;
the peer's MCP server checks the token as usual. Missing or ambiguous peer names fail with exit 1. A saved peer ticket,
when present, must match its endpoint id; without a ticket, the link uses N0 discovery by endpoint id.

`--args` may not carry an `approval` key, and it is refused with exit 2 before anything is sent. An approval is passed only in
`--args-file`, so it never appears in the process list or in the shell history.

```sh
arca call plan_list --config /etc/arca/node.json --token-file "$HOME/.config/arca/viewer-1.secret" --args '{}'
arca call approval_submit --config /etc/arca/node.json --token-file "$HOME/.config/arca/doctor-1.secret" --args-file "$HOME/approve.json"
```

A call sends `initialize` and then `tools/call`, each on its own connection. Its exit codes:

- `0`: the result is printed on stdout as JSON.
- `3`: the node refused the call with a JSON-RPC error. The error object is printed on stdout, with the refusal in
  `data.refusal`, and nothing is printed on stderr.
- `4`: the node answered 401 or 403 to the token.
- `1`: any other failure, such as a socket that does not answer.

## Desired state, delete plans and approvals

`serve` opens the desired-state store before it listens. The state directory holds these files:

| File | Holds |
|------|-------|
| `node.key` | the node's ed25519 key, created on first start |
| `desired.json` | the current signed desired-state document, with the entries that converged and the plans that were declined. Mode 0600 |
| `desired.lock` | the lock held while `serve` runs. A second process over the same directory is refused |
| `journal.jsonl` | the journal of every effect the reconciler sent, as intents and outcomes |
| `action-proposals.jsonl` | the action proposals and their `proposed`, `approved`, `declined` and `result` events. Opening re-checks every proposed record against its action's validation and `kernel.postgresImage`, then compacts it to the waiting, unexpired proposals; an unreadable, invalid or oversized file is renamed `*.bad-<ts>` and the kernel starts empty. An append that takes it past about 1 MiB compacts it too, and a stale compaction temporary file is removed at open |
| `instance-actions.jsonl` | every `instance_start`, `instance_stop` and `instance_restart` that passed its gate: a `begin` line before anything is sent to Incus and an `end` line with what happened. Mode 0600, created on the first action |
| `approval-nonces.jsonl` | the approval nonces that were spent |
| `passkey-counts.jsonl` | the last sign count kept for each passkey. Mode 0600 |
| `secrets/` | the secret files the writer reads, if any |

`serve` refuses a state directory that is not mode 0700, or not owned by the user that runs it, before it creates the node key.
It refuses a state file that is a link, is not mode 0600, does not parse, or is not signed by this node's key. It refuses a lock
file that is a link or is not owned by that user. Each refusal is a config or runtime failure, and nothing is served.

A removal of a desired entry whose objects are class B (deleted by an Incus delete) does not delete them. It waits as a delete
plan until an owner approves it. A plan lapses fifteen minutes after it was made. `plan_list` leaves a lapsed plan out, and an
approval for it is refused as `Expired`. A restart, or a new proposal while the removal still waits, makes a fresh plan with a new
creation time and fifteen new minutes.

Kernel instances start on create unless the body sets `start: false`; removal force-stops a running instance and waits for that operation before DELETE.

The desired-state document expires thirty days after it is issued. A document past its expiry holds the applied state, and the
reconciler makes no plan from it. So a restart after the thirty days drops every waiting plan, and `plan_list` lists none.

`plan_list` returns each waiting plan that has not lapsed, with `plan` (its digest), `entry`, `createdAt`, `expiresAt`,
`objects`, `paramsDigest`, and `statement`. The statement is the exact text the owner signs, built fresh for each listing. Its
keys are sorted, it has no spaces, and it ends without a newline:

```
{"action":"delete","expiresAt":"<ten minutes after issue>","host":"<host>","issuedAt":"<now>","nonce":"<32 hex digits>","paramsDigest":"<digest of {\"plan\":<plan>}>","target":"plan-<plan>"}
```

An action proposal is an inert request from a viewer (normally the canvas Doctor) to run one of seventeen gated service,
database, GPU, storage, snapshot or instance verbs: `service_create`, `service_remove`, `service_import`, `service_release`
(see [Importing a service another manager created](#importing-a-service-another-manager-created)), `service_publish`, `service_unpublish`,
`service_connect`, `service_disconnect`, `postgres_create`, `gpu_attach`, `gpu_detach`, `storage_add`, `snapshot_create`,
`snapshot_delete` (see [Snapshot storage and snapshots](#snapshot-storage-and-snapshots)), `instance_start`, `instance_stop` or
`instance_restart` (see [Starting, stopping and restarting instances](#starting-stopping-and-restarting-instances)). `function_create` and `function_remove` (see [Functions](#functions)) are proposed the same way. `action_propose` takes `{action, args}`; `args` is checked against that verb's own parameter schema, with
the inline `approval` field removed because the proposal replaces it. An unknown action is an invalid call and leaves no
record. A proposal cannot choose a container image: `postgres_create` uses the image the node's `postgres-autopg`
template names, or `kernel.postgresImage` on a node whose config sets it, and an explicit image that differs is
refused (see "A database"). `service_create` names a template from the kernel's catalog, whose name the schema constrains;
its card has a `Start` row whenever the service will be created stopped, with the reason, so the owner approves a stopped service knowingly.

**What the owner saw is what is created.** When a `service_create` or a `postgres_create` is proposed, the node
reads the template and gives the proposal two things, which no caller gives:

- `image`: the image the template names. The card's `Image` row shows it: `<image>@sha256:<digest>` when the template
  gives a digest. When it gives none the row says so: `images:debian/13 (no digest: Incus takes what that name gives
  when the service is created)`. For a virtual machine that names the version it runs, the row names the machine's
  own system image first and then `running <version>`.
- `templateDigest`: a fingerprint (SHA-256) of the template's file and of each script it names. The card's last row,
  `Template content`, shows it whole, with the words "Arca creates this only if the template on the node is still
  exactly this one".

The approved create reads the template once and is made from that read. If the template is no longer exactly the one
the card showed, nothing is created and the answer says "propose it again". That covers an edit of the template's
file, even of one space, an edit of a script it names, and a template installed again: a card that waits while an
upgrade replaces a shipped template is refused, and the next proposal shows the new one. A proposal lives fifteen
minutes, so this is a short wait. A proposal for a template that is not installed, or whose script is missing, is
refused when it is made.

**A proposal the node has no room for is refused when it is made.** A `service_create` is held to two sets of caps:
the node's caps on virtual machines (`kernel.vmLimits`, or `kernel.bridged.limits` on a node that sets
`kernel.bridged`: `virtualMachines`, `cpu`, `memory`, `disk`), and, when it names a tenant, that tenant's caps
(`limits.instances`, `limits.containers`, `limits.virtual-machines`, `limits.cpu`, `limits.memory`, `limits.disk`).
Both are checked when the proposal is made, from the node's config, its own records and the template, without asking
Incus. A proposal that would pass a cap is refused then, with the code `capacity` and the sentence the create itself
uses: `creating <name> would exceed this node's cap on CPUs (kernel.bridged.limits.cpu): 2 in use, 2 asked, 3
allowed`. No card is recorded, so the owner is never asked to approve it. A proposal exactly at a cap is recorded. A
proposal that names a tenant the node does not hold is refused when it is made too.

The approved create checks the same caps again, with the same code. The node can fill up in the fifteen minutes a card
waits: another create, or a config that lowers a cap. The owner's approval is then accepted and used up, the card is
retired, and the answer begins "The approval was accepted, but service_create did not complete:" and carries
`proposeAgain: true`. Proposed again while the node is still full, it is refused when it is made.

A card that waited while the node filled up stays on the owner's page and can still be approved; that approval is
used up and the create then refuses. Proposing the same create again in that time is refused for want of room, and
the old card stands until it is declined or its fifteen minutes pass: decline it. A create that fits only once
something else is removed is refused until that removal has run, because the caps count what the node holds now:
remove first, then propose the create.

One refusal for want of room can still come after an approval on a node where nothing changed: the service subnet, or
a tenant's block, has no free address. Addresses in use are read from Incus, and a proposal is answered without
Incus. `function_create` and `postgres_create` are held to no cap; only the free address applies to them.

**What else a proposed create is refused for when it is made.** The arguments are held to the verb's schema first,
so a name that is not a service name is told as that. Then everything the create would refuse for what the node's
config and its own records hold is refused in the create's words, and no card is recorded:

- a name a service of that tenant, or of the node, already holds (`service <name> exists`), and a name a function
  holds;
- a name whose instance another service on the node already has. A tenant's instance is named `<service>-<tenant
  id>` and the node's own is named as its service, so the node's `x-<tenant id>` and that tenant's `x` are one
  instance name, and Incus takes a name once on the service network. The second is refused: `the name <name> is
  taken on this node: ...`. The words say nothing of the service that has it. A proposed `function_create`, and a
  proposed `postgres_create` on a node whose database is made from its template, are refused the same way;
- a `deploy` named outside a tenant; a deploy that is closed; and a deploy a token opened. An approved create runs
  as the approval itself and not as any token, so a card can open a deploy, and a later card can add to a deploy a
  card opened while it is provisional, but no card adds to a deploy a token opened. The proposal and the create say
  it in one sentence, which is also what a token is told about a deploy another token or a card opened: "deploy
  <id> was opened by another caller: only the caller that opened a deploy adds to it, and an approved action runs
  as the owner's approval, never as the token that proposed it";
- a tenant's virtual machine that names a `bridge`, and a tenant's service from a template that opens a port on
  the host (`lan`) or publishes a dash;
- a `mac` another of the node's records holds;
- a config that leaves the service project out or sets up no service network, on a node that makes changes;
- a virtual-machine count the config sets below the machines the node holds.

What is read from Incus is asked by the approved create alone, so these can still follow an approval: Incus does
not answer; an instance of that name exists that is not Arca's; a local image the template pins is not there; an
instance Arca holds no record of has the MAC; the Incus project exists and this node did not make it; the default
profile names no storage pool; no address is free; and whatever Incus itself refuses while it creates or starts
the instance. An instance that carries Arca's marks and that the node holds no record of is adopted first, and the
create then answers `service <name> exists`. The node's own disk can refuse too: the deploy journal or the
service's secrets cannot be written. So can anything that changed while the card waited: the template (see above), the room, the name, a
deploy that closed, or a restart of the node with another config. On a node that only looks, two of the checks
above are not made when a create is proposed: the config's service project, and the virtual-machine count against
what the node holds. Its create answers that the node only looks, so a proposal those two alone would refuse is
recorded there as before. Everything else in the list, and the room, is refused there as on any other node.

A token's own `service_create` shows no card and binds nothing, as before. It may state `templateDigest` or `image`
itself, and is then refused when the node's template differs. Not part of the fingerprint: why a service is created
stopped (the card's `Start` row is read from the stored secrets), the node's config, and whatever an image name
without a digest points at.

A card made before the node bound a template's content (one that waits while the node is upgraded to this version)
has no `Template content` row. It is refused when its approval is submitted, with "propose it again", and the
approval is not used: nothing is signed away. That holds for a create and for a database made from the template.

The binding covers the create only. What acts on a service afterwards reads the template as it is installed then:
`service_connect` (the variables a link gives, and the secrets the template declares), the Omni verbs, and
`instance_start` when it writes a machine's secrets. Editing a template after a service was created changes what
those do for it.
A GPU proposal names an existing Incus instance and the card's PCI address. The kernel resolves the instance's project,
type and `volatile.uuid` when the proposal is made and puts them in the arguments, so the owner's digest binds them. Only a
kernel-mode node writes, and only an instance in `kernel.projects` whose own config classifies as a kernel object: a
droidd, `bench`, foreign, contested or retained instance is refused even inside the family. The metrics mode is read again
at each call, so a later local/remote change is respected. `gpu_attach`
refuses an address that is not in the inventory, a card bound to `vfio-pci` when the target is a container, and a
driver-bound NVIDIA card another consumer uses when the target is a virtual machine. A virtual machine's attach is refused
too when that use cannot be checked, with `Arca could not check whether the card is in use; try again`: an unknown answer is
never treated as a free card. The check is repeated when an approved attach runs, so a card that becomes unreadable is never
attached. `gpu_detach` still names a card that has left the inventory by the address in the instance's own `gpu-<pci>`
device. At execution the instance is re-read under the bound project: one that moved, changed type or was recreated
(another `volatile.uuid`) is refused. The GPU rows are
`Project`, `Instance` (with its type), `GPU` (the product and the exact PCI address), and `Effect`. A container's effect
says its memory is shared with every other user of the card, never isolated; a virtual machine's effect says the VM takes the
whole card while it runs and the host and every container lose it until it stops.

The record carries `details`, a kernel-built array of `{label,value}` rows that lists every argument the owner is approving;
the approval page renders every row as text. The record is:

```json
{"id":"act-<12 hex>","action":"service_create","summary":"Create the service nats from template nats","details":[{"label":"Name","value":"nats"},{"label":"Template","value":"nats"}],"args":{"name":"nats","template":"nats"},"argsDigest":"<sha256 of canonical {action,args}>","createdAt":<ms>,"expiresAt":<createdAt+15 min>}
```

A GPU proposal's rows name the exact card:

```json
{"id":"act-<12 hex>","action":"gpu_attach","summary":"Attach the GPU at 0000:02:00.0 to instance alpha in project arcak-gpu","details":[{"label":"Project","value":"arcak-gpu"},{"label":"Instance","value":"alpha (container)"},{"label":"GPU","value":"NVIDIA GeForce RTX 4060 Ti (0000:02:00.0)"},{"label":"Effect","value":"Adds the card to alpha as a physical GPU device. Its memory is shared with every other user of the card, not isolated."}],"args":{"project":"arcak-gpu","instance":"alpha","kind":"container","uuid":"<volatile uuid>","pci":"0000:02:00.0"},"argsDigest":"<sha256 of canonical {action,args}>","createdAt":<ms>,"expiresAt":<createdAt+15 min>}
```

The `summary` and `details` are built in the kernel from the checked fields; a caller cannot supply either. The product
in a GPU row comes from the inventory at proposal time; it is kept in the kernel's private proposals journal, so a
restart still shows it after the card is gone. A direct `gpu_attach` or `gpu_detach` call carries no usable approval: the
handler accepts only the one-shot grant an approved proposal arms, and consumes it before the change. At most eight
unexpired proposals wait. A ninth is refused with `TooManyProposals` (family `rate`), and the eight that wait are left alone. The same
`argsDigest` returns the record that already waits instead of adding another; that deduplicated request records nothing.
One token may record thirty new proposals in ten minutes, and a deduplicated or refused proposal counts nothing against
that window; the thirty-first new proposal gets `RateLimited`, with `retryAfterSeconds` beside the code (see "Refusals
for want of room and for asking too often"). A proposal lapses fifteen minutes after it was made. The
journal writes `proposed` when the record is made, `approved` when its proof verifies, `declined` on a decline, and `result`
after the one run. On start every `proposed` record is validated again: the verb's parameter schema without `approval`, its
semantic checks, and the kernel's configured `kernel.postgresImage` (a proposed database made under another setting
of that key does not wait on). A record that
fails is not accepted, and the file is quarantined. The journal is compacted to the waiting, unexpired proposals at start,
and again during an append once it passes about 1 MiB; an unreadable, invalid or larger-than-16-MiB file is quarantined as
`action-proposals.jsonl.bad-<ts>` and the kernel starts empty, so this file never stops a start. A temporary file that a
crashed compaction left behind is removed when the kernel next opens the state directory.

`plan_list` carries an action proposal as
`{"kind":"action","id":"act-…","action":"…","summary":"…","details":[…],"args":{…},"argsDigest":"…","createdAt":<ms>,"expiresAt":<ms>}`.
`GET /approve/plans` carries the same item plus `statement`, the exact text the owner signs:

```
{"action":"<the verb>","expiresAt":"<ten minutes after issue>","host":"<host>","issuedAt":"<now>","nonce":"<32 hex digits>","paramsDigest":"<argsDigest>","target":"act-<12 hex>"}
```

`plan_decline` accepts either a delete plan's digest or an action proposal's `act-…` id. The approval page dispatches
`POST /approve/approve` by the same id: a 64-hex id keeps the delete-plan path, and an `act-…` id verifies the proof against
the proposal, spends the nonce, retires the proposal, and then runs its verb through the same handler the gated verb uses,
exactly once. A failure is journalled as the action's result and returned; the approval is spent either way.

The owner approves the delete statement or the action statement in the namespace `arca-kernel-approve`, with an SSH key or a
passkey. The approval string is the base64 of the statement, a dot, and the base64 of the proof. A proof whose first byte is `{`
is a passkey proof. Any other proof is an armoured SSH signature.

- **An SSH key.** The owner signs the statement with `ssh-keygen -Y sign -n arca-kernel-approve` and an `sk-ssh-ed25519` key.
  The key is read from the signers file `/etc/arca/owner-signers`, which lists the owner's keys for that namespace.
- **A passkey.** The proof is a WebAuthn ES256 assertion. The passkey is read from the passkeys file
  `/etc/arca/owner-passkeys`.

`serve` reads both files at start, and logs only their counts: `arca: <n> owner signing key(s), <m> passkey(s) kept`. A file
that is not there holds no key, so every approval of that kind is refused with `NoVerifier`.

Each line of the passkeys file is one passkey, on one line, with single spaces:

```
owner namespaces="arca-kernel-approve" rp="<rp>" origin="<https origin>" alg="ES256" cred="<base64url>" key="<base64>"[ <comment>]
```

- `rp` is the relying party the passkey was made for, a lowercase DNS name.
- `origin` is the one page the passkey may answer from: `https://`, then `rp` or a name under it, then an optional
  `:<port>` (not 443, which a browser leaves out). It is written out because Arca has no fixed serve port.
- `cred` is the credential id, 16 to 1023 bytes of unpadded base64url.
- `key` is the P-256 public key, 91 bytes of SPKI DER in base64. Its fingerprint is `base64 -d | sha256sum` of that text.

An existing passkey is enrolled by copying its `cred` and `key` into a line of this shape. A blank line or a `#` comment is
skipped. A line whose `namespaces` list does not name `arca-kernel-approve` is read and checked, but not kept.

**The trust rule.** The passkeys file is trusted as the signers file is. It and every directory above it must be root-owned,
must not be group- or other-writable, and must not be a link. The file must also be at most 64 KiB, and every line must be
of the shape above. If a credential or a key repeats, the file is refused too. A passkeys file that breaks the rule refuses
every passkey approval with `Untrusted`, and `serve` logs that refusal. The refusal names the file and the line number,
never a field of the line. The signers file and its SSH keys are not affected.

**The challenge.** A passkey signs `SHA-256("arca-kernel-approval/v1\n" + namespace + "\n" || statement)`, where
`namespace` is `arca-kernel-approve` and `statement` is the exact statement bytes. The prefix is not droidd's
`droid-owner-approval/v1\n`, so an assertion made for droidd never verifies here.

**The proof.** The proof is the canonical JSON of exactly these keys: `alg`, `authenticatorData`, `clientDataJSON`,
`credentialId`, `kind`, `signature` and `v`. The values are `v` 1, `kind` `webauthn` and `alg` `ES256`, and every buffer is
unpadded base64url. The proof is at most 8192 bytes. A proof that is not its own canonical JSON is refused with
`NotCanonical`, and any other bad form is refused with `Malformed`.

**The checks**, in order:

- The credential must be on a line that lists the namespace (`BadSignature`).
- `clientDataJSON` must:
  - be a `webauthn.get` (`Malformed`);
  - carry this statement's challenge (`BadSignature`);
  - come from the line's exact origin, with no `crossOrigin: true` and no `topOrigin` (`WrongOrigin`).
- `authenticatorData` must:
  - carry the line's rpIdHash (`WrongRp`);
  - have user presence set (`NoUserPresence`) and user verification set (`NoUserVerification`);
  - set the backup state only with backup eligibility, carry no attested data and no extensions, and be exactly 37 bytes
    (`Malformed`).
- The ES256 signature over `authenticatorData || SHA-256(clientDataJSON)` must verify (`BadSignature`). It is read as strict
  DER, and a high-S signature is accepted as its low-S twin.

**The sign count.** A count above 0 must be above the last one kept for that passkey. A count of 0 is accepted only while the
kept one is 0 or there is none, because a synced passkey always reports 0. Otherwise the approval is refused with
`SignCountRegressed`. The count is checked and kept in `passkey-counts.jsonl`, fsynced, before the nonce is read. So a replay of a
counting passkey's approval is refused with `SignCountRegressed`. A replay of a synced passkey's approval (count 0) is refused
with `Replayed`.

The count is kept before the host, action, target, arguments and time window below are checked. So a counting passkey's
approval that one of those checks refuses is used up: its nonce is not spent, but sent again, even for the plan it names, it is
refused with `SignCountRegressed`, and the owner signs the statement again. Later approvals are not affected, because the
passkey's next assertion carries a higher count. A synced passkey's approval (count 0) is not used up.

`approval_submit` takes the plan digest and the approval. It refuses, and spends no nonce, when:

- the signature does not verify against a listed key (`BadSignature`), or no key is listed (`NoVerifier`);
- a passkey assertion fails one of the checks above (`WrongOrigin`, `WrongRp`, `NoUserPresence`, `NoUserVerification`,
  `SignCountRegressed`, `Malformed`), or its passkeys file cannot be trusted (`Untrusted`);
- the statement names another host, action, target or arguments (`WrongHost`, `WrongAction`, `WrongTarget`, `WrongParams`);
- the statement is issued more than five minutes ahead of the node's clock (`IssuedAhead`), has expired (`Expired`), or
  runs for more than fifteen minutes (`TtlTooLong`);
- its nonce is spent (`Replayed`).

A plan that waits under no digest is `NoSuchPlan`. A valid approval for such a plan is still checked, so a spent approval reads as
`Replayed`. The approval string is never stored or echoed, and an approval spends its nonce once, before its delete runs.

### The approval page

With `approvalOrigin` set, `serve` serves an approval page and mounts its routes outside the MCP bearer check:

- `GET /approve` returns the page.
- `GET /approve/plans` returns the waiting delete plans and action proposals. A delete plan has the same fields as
  `plan_list`; an action proposal adds `statement`, the exact passkey statement for that proposal.
- `POST /approve/setup-code` issues a one-time 8-digit setup code to the machine's own screen while no passkey is trusted.
  On a machine with no screen it answers 503 `{ "message": "No screen to show the code on", "command": "arca setup-code
  --config <this node's config>" }`, and the page shows that exact line: the code then comes from the terminal.
- `POST /approve/approve` takes `{ "plan": "<digest or act-id>", "approval": "<string>" }`. A delete plan keeps the
  `approval_submit` path; an `act-…` id verifies the passkey against the proposal's statement and runs its action once.
- `POST /approve/decline` takes `{ "plan": "<act-id>" }` and declines only an action proposal. The open page has no
  credential for a delete plan; that decline stays with the doctor-scope `plan_decline`.
- With `approval.linkedHosts` set, `GET /approve/plans` also lists each linked kernel's waiting action proposals, and
  `POST /approve/approve` and `POST /approve/decline` take an optional `"host"`. See the next section.
- With linked hosts that keep this host's passkeys, `GET /approve/plans` adds `peerKeys` cards, `POST /approve/peer-keys`
  approves one and `POST /approve/peer-keys/sync` syncs now. See [Approving on another machine](#approving-on-another-machine).
- `POST /approve/register` takes `{ "credentialId": "<base64url>", "publicKey": "<base64>", "publicKeyAlgorithm": -7 }`,
  plus `"setupCode"` while no passkey is trusted, and returns `{ "trusted": true, "line": "<line>" }`; without `setupCode`
  it returns `{ "line": "<line>" }` as the developer answer.

Every route of the page, the canvas routes below among them, answers the same with one trailing slash as without it
(`/approve/` is `/approve`, `/approve/plans/` is `/approve/plans`): the same handler under the same rules, not a
redirect, so a POST stays a POST and a query string stays with it. Two slashes, or an encoded one, are no route.

The GETs need no token and admit no `Origin` or the configured one. Apart from the canvas chat POST described below, the
POSTs admit only the exact configured `Origin` and refuse any other before reading the body. `/mcp` and every other MCP route
keep refusing every `Origin`.

With `approvalOrigin` set, the same page origin also serves the remote canvas at exactly these URLs, each with or
without one trailing slash: `GET /approve/canvas`
(the canvas, or a passkey sign-in page), `GET /approve/canvas/page.css`, `GET /approve/canvas/page.js`,
`GET /approve/canvas/snapshot`, `POST /approve/canvas/chat` and `GET /approve/canvas/chat/history`. The CSS, script,
snapshot and history are fetched from `canvasOrigin` over literal loopback; no cookie or authorization header is forwarded,
only an HTTP 200 body passes, and upstream redirects, cache headers and cookies are dropped. A down, slow or non-200 DSH
returns an Arca-styled "the canvas is not running" page. The snapshot, history and chat need a valid `__Host-arca-canvas`
session cookie (Secure, Path=/, HttpOnly, SameSite=Strict, twelve hours); only the canvas URLs read it.
`POST /approve/canvas` issues a canonical sign-in statement, and a second POST with `{ "statement", "proof" }` spends the
challenge before verifying the passkey and sets the cookie. The challenge store is in memory, holds 256 entries, lives two
minutes, is single use, and has one global cap of 60 a minute; it never uses the caller address. Failures are logged without
detail. This view is tailnet-only behind the existing `/approve/*` proxy route.

Every page `serve` returns here (the approval page, the canvas, its sign-in page and its error page) closes with the
Open Intelligence tag of the we unfold movement: an inline picture 170 px wide, in one colour (the page's ink on the
page's ground, light and dark), loaded from nowhere and never a link (`assets/open-intelligence/`).

`POST /approve/canvas/chat` is one Doctor chat turn and costs model tokens only for the signed-in owner. The session is
checked before the request body is read, so an unsigned or invalid chat gets 401 with no connection and before any body
limit applies. The POST needs the exact `Origin`, or a browser's `Sec-Fetch-Site: same-origin` when no `Origin` is sent;
anything else, including an `Origin` that is not valid text, is 403 before the upstream. The body must be `application/json`
and at most 64 KiB (415 or 413 otherwise, also before the upstream). At most two chats stream at once; a third gets 429 before
any upstream connect. The kernel forwards the JSON to `POST /arca-canvas/chat` on the same loopback upstream and adds no
write capability. The answer is relayed as a stream with the upstream's `Content-Type` (`text/event-stream`,
`application/x-ndjson` or `application/json`) as each chunk arrives: nothing is buffered whole, one ten-minute total covers
the connect, request write, response head and relay, and an idle gap of 60 seconds or a 2 MiB relay cap also ends the stream.
A cut answer ends with a final `event: arca-cut` whose data names `idle`, `total`, `full`, `cap` or `upstream`
(`{"arcaCut":{"reason":"..."}}` line for the NDJSON/JSON media types); a client disconnect closes the upstream connection
without a cut event. `GET /approve/canvas/chat/history` is the same bounded three-second forward to
`/arca-canvas/chat/history`. Every canvas route carries the CSP with `frame-ancestors 'none'`, `nosniff`, `no-referrer`, and
`Cache-Control: no-store` on every canvas body, including refusals.

#### The page on this machine alone, with no domain and no certificate

A node on one machine can serve its approval page to a browser on that machine, with no domain name, no certificate
and no reverse proxy. Two keys do it:

```json
"listen": { "unix": "...", "loopback": "127.0.0.1:7471" },
"approvalOrigin": "http://localhost:7471"
```

The page is then `http://localhost:7471/approve`. Open it by that name, never by the IP address: a browser runs
passkeys at `http://localhost` because it treats that name as a secure context without a certificate, and it refuses
an IP address as a passkey's relying party, so the same page at `http://127.0.0.1:7471/approve` loads and cannot save
or use a passkey.

**This is for a machine where every account, and every agent or sandbox that shares its network, is trusted.** A
loopback port has no owner: any program of the machine can bind one that is free, and a browser cannot tell which
program answers at `http://localhost:7471`. A program that answered there in the node's place would be the approval
page at the page's exact origin. It could collect the setup code you type and enrol its own passkey, or show a
harmless card while your passkey signs a dangerous one. The node closes what it can (next paragraph); it cannot close
the port to a program that takes it while the node is not running. On a machine shared with accounts or agents you
do not trust with that, use an `https://` origin, where a name and a certificate say who answers.

**Both loopback addresses, or no page.** `localhost` names two addresses, `127.0.0.1` and `::1`, and a browser tries
`::1` first. So a node with this origin binds the port on both, whichever one `listen.loopback` names, and serves
the page only while it holds both. While it does not hold one of them (another program has it), every page route
answers 503 with the reason, `arca setup-code` and the page's own button give no code, and the page approves
nothing. So the node takes nothing from you while the page is closed, and there is no setup code to type anywhere.
**A signature is different.** If you approve with your passkey at another program's copy of the page, that
signature is a valid approval of the card that program chose. It stays valid until its statement expires, and the
program can spend it once the page opens again, or through the node's approval verbs with a token that may call
them. If you approved at a page while `arca_status` said the page was closed, decline that card. `arca_status` says it
under `"loopback"`, as `"page": { "open": true | false, "reason": "<why it is closed>" | null }`. The node keeps
trying the address and opens the page when it has both. A machine with no IPv6 loopback at all (IPv6 turned off, or
a kernel built without it) has no second address for anyone to hold: the bind fails with "address not available" or
"address family not supported", the node says so once on stderr, and the page is open on the one address there is.
The node does not take that failure at its word, because it can be true for the node alone: a service unit that
restricts address families, or a network namespace of the node's own, makes the bind fail while the machine still
has `::1` for any other program to take. So the node reads `/proc/net/if_inet6`. Only when that file is missing, or
lists no `::1`, does the address count as nobody's. When it lists `::1`, or cannot be read, the page stays closed
and the reason says so. Do not run a node with this origin under restricted address families or in a network
namespace of its own. Any other failure of that bind, a port in use first of all, keeps the page closed.

Before you type a setup code or approve for the first time, ask `arca_status` (`arca call arca_status ...`) and read
`"page": { "open": true }`.

What the config must say, and what it refuses with exit 2:

- `approvalOrigin` may be `http://localhost:<port>` and no other `http://` origin: not an IP address, not another
  name, not a name under `localhost`, not `http://localhost` with no port, and nothing after the port;
- `listen.loopback` must be set to `127.0.0.1` or `[::1]` on that same port, because that is where the page is
  served;
- `approval.origins` and `approval.peerKeys.origins` stay `https://` only: another kernel does not trust passkeys
  enrolled at a localhost page.

Where the page is served:

| Page origin | Unix socket | Loopback listener | Tailnet listener | Proxy listener |
|-------------|-------------|-------------------|------------------|----------------|
| `https://<name>` | yes | no | yes | yes |
| `http://localhost:<port>` | yes | yes | no | no |

On the loopback listener the page is behind that listener's `Host` rule: a request must name `localhost:<port>` or
`127.0.0.1:<port>`, so a web page loaded from another name that was pointed at this machine reads nothing, not even
the waiting cards. The page's own rule still holds on top of it: a POST needs the exact origin `http://localhost:<port>`.

The first passkey:

1. Open `http://localhost:7471/approve` and press **Get a setup code**. On a desktop the code appears in a
   notification. On a machine with no screen the page says so; run this in a terminal on that machine, as the node's
   user, and type the 8 digits it prints:

   ```sh
   arca setup-code --config ~/.config/arca/node.json
   ```

   It prints `{ "code", "expiresInSeconds", "page" }` on stdout. The node gives the code only to a caller that has
   just made a file in its state directory, which only the node's user can; nobody else on the machine, and no peer
   on a forwarded socket, gets one. It answers with the code that is live, or a new one, and there is none once a
   passkey is trusted.
2. Type the code and save the passkey. **The browser must have somewhere to save it.** On a Linux desktop that is
   Chrome or Chromium signed in to Google Password Manager, or a password manager extension that keeps passkeys;
   where the browser offers it, a phone nearby over Bluetooth. Firefox on Linux has no passkey manager of its own.
   Arca asks for a passkey that checks the person (a fingerprint, a face or a PIN).
3. If the browser on that machine has nowhere to save a passkey, or the machine has no browser, open the page from
   a computer that has one, through an SSH forward. On that computer:

   ```sh
   ssh -o ExitOnForwardFailure=yes -L localhost:7471:127.0.0.1:7471 <the machine that runs Arca>
   ```

   `ExitOnForwardFailure=yes` makes `ssh` stop when port 7471 is already taken on that computer. Without it `ssh`
   only warns, and the browser would open whatever program holds the port there.

   and open `http://localhost:7471/approve` in its browser. Its origin is the same `http://localhost:7471`, so the
   passkey it saves is the node's. The setup code still comes from the node's own screen or terminal.

What this does not do:

- **A phone cannot open this page**, and neither can another machine without the SSH forward. Approving from a
  phone needs a name and a certificate: an `https://` origin.
- **The passkey belongs to `http://localhost:<port>`.** Change the port, or move to an `https://` origin later, and
  the passkey no longer answers at the new origin. There is no way to enrol a second time from the page: a setup code
  is given only while no passkey is trusted, and adding a passkey needs one that answers at the page's origin. To
  start again, stop the node, remove `<stateDir>/owner-passkeys`, start it, and enrol with a new setup code. Until
  that file is removed, the old passkey is still trusted for the old origin.
- **Other accounts of the machine can reach the page.** They can read the waiting cards and the list of passkeys
  (names, fingerprints and dates), decline a proposal or deny a request (neither needs a passkey), and use up the
  hour's setup codes or lock setup for an hour with wrong ones. Through the node's own page they cannot approve and
  they get no setup code. What the paragraph at the top says still holds: a program that holds the port while the
  node does not is the page, and what you type or sign there is that program's.
- **Cookies are the whole name's.** A browser keeps cookies for `localhost` without regard to the port. The canvas
  session cookie this page sets is therefore sent to every other web server you open at `http://localhost:<any
  port>`, and a program that received it could use it on the canvas routes here. The canvas is not tested on this
  origin; do not sign in to it here on a machine where you open other local web servers you do not trust.
- **Not tested with a real passkey manager.** A browser test drives Chromium's virtual authenticator at
  `http://localhost`. Google Password Manager, a password manager extension and a phone over Bluetooth were not
  tried against a `localhost` page.

#### Approving on another kernel of this machine

One canvas can approve changes on every Arca kernel that runs on the same machine as the page host, with the same passkey.
The page host never approves for another kernel: each kernel records its own proposals, and each verifies the owner's
signed statement with its own verifier, nonce store and sign counts. The page host only lists the other kernel's waiting
proposals and carries the signed bytes to it. A kernel on another machine is linked the same way, with its socket forwarded
to this machine and the owner's passkeys kept in sync there; see [Approving on another machine](#approving-on-another-machine).

On the page host (here `alpha`, page origin `https://alpha.example.com`), list each linked kernel with its MCP socket and
the file holding a token for it:

```json
"approval": {
  "linkedHosts": [
    { "name": "beta", "socket": "/run/arca-beta/arca.sock", "tokenFile": "/etc/arca/alpha/beta-relay.secret" }
  ]
}
```

`name` is the name the canvas knows that machine by (its `hosts[].name` in the canvas config). The token is minted on the
linked kernel with scope `doctor`, limited to the three verbs the page relays:

```sh
arca token mint --scope doctor --id relay-alpha --tools plan_list,action_approve,plan_decline \
  --out /etc/arca/alpha/beta-relay.secret --config /etc/arca/beta.json
```

It is read from its file at each call, so it can be rotated without a restart. It is opened without following a link, and
must be a regular file owned by root or by the page host's user, with no group or other permission bit; any other token
file relays nothing (502 `LinkToken`).

On the linked kernel (`beta`), trust the page host's passkeys file read-only, for the page host's origin only:

```json
"approval": {
  "trustOwnerKeysFrom": ["/var/lib/arca-alpha/owner-passkeys"],
  "origins": ["https://alpha.example.com"]
}
```

The file is opened read-only, without following a link, at every approval check, so a passkey the owner adds or removes on
the page host is trusted or dropped here without a restart. It is never written, copied or cached. It is read only when it is
a regular file owned by root or by this kernel's user, with no group or other write bit, and parses whole; only its lines
whose `origin` is listed are trusted. A file that is missing, unreadable or refused adds no key, and `arca_status` names the
reason. The sign-in route of beta's own canvas never uses these keys, and neither does a passkey change: only beta's own
passkeys (its root file and its state file) can add or remove one of beta's passkeys, and a passkey trusted from another
kernel's file is refused with "only this machine's own passkeys can change its passkeys". The trusted keys approve action
proposals only. With no `approval` key, a kernel trusts only its own owner keys and does not serve `action_approve`, as
before.

Then:

- `GET /approve/plans` adds each linked kernel's waiting action proposals. Each row carries `linkedHost` (the linked
  name), `host` (the host that kernel's statements name) and a fresh `statement` that names that host. The answer adds
  `linkedHosts: [{ "name", "read" }]`; a kernel that does not answer (`read: false`) adds no row. A linked kernel's delete
  plans are not listed.
- `POST /approve/approve` with `"host": "<linked name>"` relays `{plan, approval}` to that kernel's `action_approve` and
  answers what it said: its result with `host` added, or its exact refusal (403 `{refusal, message}`, for example
  `WrongHost`, `BadSignature` or `Replayed`). `POST /approve/decline` with a `host` relays to `plan_decline` the same way.
- A `host` the page does not list is refused 403 `{ "refusal": "NotLinked", "message": "this machine is not linked for
  approvals yet" }`. A linked kernel that refuses the token answers 502 `LinkToken`; one that does not answer, 502
  `Unreachable`. Nothing is relayed in either case.
- Without a `host`, the page approves only on its own kernel, as before. It never calls its own kernel for another
  kernel's id.

In the canvas, the Doctor proposes on the page host and on any machine the page host's `arca_status` lists in
`approval.linkedHosts`; any other machine is refused with "this machine is not linked for approvals yet" before a proposal
is recorded. Every card shows the host its statement names, and a linked machine's card approves and declines through the
page host with that machine's name.

#### Approving on another machine

A kernel on another machine (here `gamma`) is approved from the same canvas. The page host (`alpha`) reaches gamma's MCP
socket through a unix socket forwarded to a path on alpha, for example an SSH local forward kept up by a service:

```sh
ssh -N -o ExitOnForwardFailure=yes -o StreamLocalBindUnlink=yes \
  -L /run/arca-alpha/gamma.sock:/run/arca-gamma/arca.sock arca@gamma.example.com
```

On alpha, gamma is a linked host like any other; its `socket` is the forwarded path:

```json
"approval": {
  "linkedHosts": [
    { "name": "gamma", "socket": "/run/arca-alpha/gamma.sock", "tokenFile": "/etc/arca/alpha/gamma-relay.secret" }
  ]
}
```

The token is minted on gamma with scope `doctor`, limited to the verbs the page uses there:

```sh
arca token mint --scope doctor --id relay-alpha \
  --tools arca_status,plan_list,action_approve,plan_decline,peer_keys_sync \
  --out gamma-relay.secret --config /etc/arca/gamma.json
```

What a second machine cannot do is read alpha's passkeys file. Instead gamma keeps a copy alpha sends it, and trusts it
only because the operator said so. On gamma, name the page host and its page origin:

```json
"approval": {
  "peerKeys": { "from": "alpha", "origins": ["https://alpha.example.com"] }
}
```

`from` is the page host's approval host (its `approvalHost`, or its system host name). Turning `peerKeys` on is the one
step that enables trust: gamma then serves `peer_keys_sync` (scope `doctor`) and `action_approve`; without `peerKeys`
neither is served unless `trustOwnerKeysFrom` names a file.

- **First sync.** Alpha syncs on start, every 10 minutes, and each time the owner opens the approval page. It reads
  gamma's `arca_status`; when `approval.peerKeys.from` names alpha, it sends its own owner passkey lines (its root file
  and its state file, as its verifier holds them, without comments, labels or dates) with `peer_keys_sync`. While gamma's
  `<stateDir>/peer-passkeys` is missing or empty, the first set is kept as is (trust on first use), and gamma's
  `arca_status` says `trusting alpha's passkeys since <time> (first sync)`.
- **No change.** An identical set changes nothing and needs no signature.
- **A changed set.** When the owner adds or removes a passkey on alpha, gamma does not keep the new set on alpha's word.
  It answers with a statement naming gamma's host, action `peer_keys_set`, target `peer-keys-<sha256>` and the digest of
  `{ "sha256": "<sha256>" }`, where `<sha256>` is the SHA-256 of the new set's lines, sorted, each followed by a newline.
  Alpha's approval page shows the card **Trust your new passkey list on gamma**. The owner approves it with a passkey gamma
  already keeps; alpha relays the signed statement with the set, and gamma checks it with its own verifier (host, action,
  digest, window, nonce, sign count). A passkey not already in gamma's copy, gamma's own passkeys and SSH keys cannot
  approve it; a replay is refused. A statement lasts ten minutes; an expired card is refreshed by the next sync.
- **The last key.** An empty set is refused: the last kept key cannot be removed this way.
- **Another page host.** A file whose header names another page host than `from` adds no key, and the next sync from the
  configured page host is a first sync: changing `from` is the same owner-enabled step as turning `peerKeys` on.

Gamma trusts the kept passkeys exactly as `trustOwnerKeysFrom` keys: for action approvals only, only for the listed
`origins`, never for gamma's own sign-in and never to add or remove one of gamma's own passkeys. The file is gamma's own:
written whole and atomically at mode 0600 without following a link, and read under the `trustOwnerKeysFrom` rule (no link,
a regular file owned by root or gamma's user, no group or other write bit, parsed whole). A file that cannot be trusted adds
no key and is never overwritten by a first sync; `arca_status` reports `approval.peerKeys` as
`{ "from", "keys", "since", "reason" }`, with no key, credential, line or path. Alpha never logs or stores the lines it
sends; its log names only the host, the outcome and a count, and only when something changed or failed. A link whose token
may not call `arca_status` is taken as one that does not ask for the list.

**More than one page host.** A node can trust the owner's approval page and, say, a cloud control service at the same
time. `peerKeys` is then a list of sources, each `{from, origins}`:

```json
"approval": {
  "peerKeys": [
    { "from": "alpha", "origins": ["https://alpha.example.com"] },
    { "from": "cloud.example", "origins": ["https://cloud.example.com"] }
  ]
}
```

The single object above stays valid and means exactly what it did: one source, the file `peer-passkeys`, the same
answers and the same status. In a list, each `from` is named once (letter case aside) and no origin is listed by two
sources; a list that breaks either rule, or is empty, stops `arca serve` at the config. The first source keeps
`<stateDir>/peer-passkeys`, as the single object does. Each further source keeps its own `<stateDir>/peer-passkeys-<from>`,
written and read under the same file rules, with its own header, first sync and changes. Moving a source to another place
in the list moves it to another file, so its next sync is a first sync, as a changed `from` is.

`peer_keys_sync` takes an optional `from`, the source whose list is sent. Absent, it is the first source, so a page host
that is the first source calls as it always has; a page host that is a further source finds itself in the target's
`arca_status` and names itself in `from`, matched without regard to letter case. A `from` no source names is refused. A
changed list is kept only with an approval by a passkey already kept for that same source, verified with that source's
own key, so a key of one source cannot change another's list, even when it was sent under the same credential id. The
sources are not wholly apart, though: Arca reads its own passkeys first, then each source in the config's order, and a
line whose credential id or key fingerprint an earlier one already holds is skipped. So an earlier source can shadow a
later source's line by sending one under the same credential id or key. That gives the earlier source nothing (the
shadowed key approves nothing, and cannot approve a change to its own source's list until the earlier line is gone), but
it can take the later key out of use. List the owner's own approval page first, and sources you trust less after it.
A further source's statement binds `{ "sha256": "<sha256>", "from": "<from>" }`; the first source's binds
`{ "sha256": "<sha256>" }` as before. A key is trusted only for its own source's origins: an assertion by one source's key
from another source's origin is refused, and a line a source sends for an origin that is not its own is kept in its file
and trusted for nothing. As before, every kept key approves actions only: never a sign-in, never a change to this node's
own passkeys. Action approvals include owner approval requests, and so `enroll`. Once a service's own passkeys are a
source here, the people who hold them can approve an `enroll` request for that service (or any other request) with their
own passkey, without the node owner's tap. That is intended: a service's existing owners add its next owners. The first
enroll of a service is the node owner's own tap only when the service's source is added to `approval.peerKeys` after
that first enroll. A source that is in the config before it gets its first list on trust on first use, from whichever
doctor token sends it first, and the keys in that list can then approve the first enroll themselves. So add a service's
source after its first enroll, not before. With a list, `arca_status` reports
`approval.peerKeys` for the first source, as it does for the single object, and `approval.peerKeySources`, one
`{ "from", "keys", "since", "reason" }` per source in the config's order, the first included.

The page routes:

- `GET /approve/plans` adds `peerKeys: [{ "linkedHost", "title", "keys", "statement", "expiresAt" }]`, one card per linked
  host whose set waits for the owner.
- `POST /approve/peer-keys` takes `{ "host", "statement", "proof" }`, reads alpha's lines again, and relays them with the
  statement and the proof to that host's `peer_keys_sync`. The answer is the host's own (`status` `changed`, or its exact
  refusal); an unlisted host is refused `NotLinked`.
- `POST /approve/peer-keys/sync` syncs with every linked host now (at most once per 30 seconds; a request inside that
  window answers from the last sync) and answers `{ "hosts": [{ "name", "status" }],
  "peerKeys": [...] }`.

In the canvas, a linked machine is treated alike whether it runs on the page host's machine or another: the Doctor proposes
on it when the page host lists it in `approval.linkedHosts`, and its cards approve through the page host.

To use it:

1. Set `approvalOrigin` to the page's HTTPS origin, for example `https://arca.example.com`, and restart `serve`.
2. Publish the `/approve` routes through the reverse proxy to the same `listen.proxy` upstream as `/mcp`. The page is also
   reachable on the tailnet listener, but WebAuthn needs the HTTPS proxy origin; plain HTTP is not a secure context.
3. Open the page at that origin and press **Get a setup code**. The code appears in a desktop notification on the machine
   running `serve`; it is never shown in the page or a log, and no HTTP answer carries it but the unix socket's
   answer to `arca setup-code`, for a machine with no screen.
4. Type the 8 digits and register the passkey. `serve` writes the trusted line to `<stateDir>/owner-passkeys`, reloads the
   verifier and logs `arca: passkey trusted, 1 passkey(s) kept`; no restart and no terminal step are needed.
5. Choose a waiting plan and approve it. The page shows each plan's target digest and expiry; the kernel's answer reports the
   deleted objects or the refusal.

The one-time setup code has 8 digits and expires after 10 minutes. Only one code can be live at a time, with one request
per 60 seconds and 10 codes per rolling hour. Five wrong tries per caller address per rolling hour lock only that caller;
a locked caller is refused even with the correct code. A global backstop of 50 wrong tries per rolling hour locks new
issuance and all code submissions for everyone, including the correct code. A lock never invalidates the live code:
another unlocked caller, or the same caller after its rolling-hour lock ends, can still use it if it has not expired.
The proxy listener uses the last non-empty `X-Forwarded-For` address (or a shared unknown caller when absent); unix and
tailnet share one direct caller budget.

Once one passkey is trusted, no setup code is issued or accepted. To add a phone or computer, open the page there,
enter a passkey name, and choose **Add passkey**. Save the new passkey when your device asks, then confirm with a passkey
Arca already trusts. The page shows **Passkey trusted** only after that trusted tap succeeds; the new key cannot approve
its own addition. No restart is needed.

The page lists trusted keys by name and added date, without public keys or credential ids in the list. Keys enrolled before
names and dates were recorded have a default name and an unavailable date. Choose **Remove** beside a key and confirm with
a trusted passkey to remove it immediately. Arca refuses removal of the last trusted key or any key held only in the root
file. Keep a second trusted passkey before replacing your only one.

`GET /approve/passkeys` returns that list with opaque targets. Registration while keys exist prepares a `passkey_add`
statement; `POST /approve/passkeys/change` prepares removal by target or submits a statement and WebAuthn proof. Both
changes bind `passkey-<SHA-256 of the full line>` and the digest of `{ "line": <full line> }`, including the name and added
date in the line's comment. The statement lasts ten minutes and its nonce is single use. A replay, changed line, wrong
digest, or proof from an untrusted key is refused. Setup-code budgets apply only to first-key enrolment; later changes
require a trusted tap and never use the shared proxy caller budget.
The accepted crash window between renaming the state passkeys file and `replace_owner_passkeys` leaves the new set on disk and the old set in memory until restart, which loads the set from disk.
`/etc/arca/owner-passkeys` remains readable as a developer option, never the owner path. The state passkeys file is owned by
the `serve` account with mode 0600 in its 0700 state directory; it has no root anchor, so anything running as that account is a
trust root (see Decision 11 in the wish).

**A scratch kernel can take its owner keys from files named in `ARCA_TEST_OWNER_SIGNERS` and `ARCA_TEST_OWNER_PASSKEYS`.**
That is honoured only when:

- `kernel.scratch` is `true`;
- every kernel project starts with `arcak-scratch-`;
- the build has debug assertions.

Any other kernel refuses to start while either variable is set. A named file is then trusted when it and the directory that
holds it are owned by the user that runs the kernel. The variables exist so that the tests can run the real verifiers against
keys they write themselves.

## The recording shim

`arca shim` listens on a unix socket and forwards every GET to the upstream socket verbatim. It answers every other request
without forwarding it. It logs one JSON line per request, with its method, path, status and whether it was forwarded. It
never logs headers or bodies.

Its summary has two counts:

- `get` counts GET requests whose request head reached the upstream.
- `refused` counts requests answered without being forwarded. That includes a non-GET request (405), a malformed or
  body-carrying request (400), and a GET the upstream could not take (502). The `refused` count includes 502.

The shim prints its summary as one JSON line and exits 0 on SIGTERM or SIGINT. A proof that a client sent only GETs is the
shim's log, read after the shim has stopped, together with `refused: 0`.

On SIGUSR1 the shim prints its summary so far to stderr, as one JSON line, and keeps serving. A request still in flight may
not be counted yet. If a log line could not be written, a plain line saying so follows the summary, so a reader that takes
the last line as the summary gets no proof. This is the live proof for a shim that runs as a service.

`--log-max-bytes <n>` caps the log. Once the log holds `n` bytes or more it is renamed to `<log>.1`, replacing the one
before, and a new log is begun, so the log never holds more than `n` bytes plus one line, and one rotated file is kept. A
log already at the cap when the shim starts is rotated first. `n` must be at least 1. Without the flag the log is never
rotated.

## Running as a user service

`deploy/systemd/` holds two systemd user units that run the node read-only for days:

- `arca-shim.service` runs `arca shim` from the Incus socket `/var/lib/incus/unix.socket` to `%t/arca-shim/incus.sock`. It
  logs to `~/.local/state/arca-shim/shim.log`, capped at 16 MiB with one rotated file. The unit counts as started only once
  its socket exists.
- `arca-watch.service` runs `arca serve --config ~/.config/arca/node.json`. It `Requires=` and comes `After=` the shim, so
  it never reads Incus except through the shim, and it stops when the shim stops.

Both units restart on failure after 10 seconds, at most 5 times in 10 minutes (`Restart=on-failure`, `RestartSec=10`,
`StartLimitIntervalSec=600`, `StartLimitBurst=5`). Swap is off for both (`MemorySwapMax=0`). `arca-watch` has
`MemoryHigh=192M` and `MemoryMax=256M`, and `arca-shim` has `48M` and `64M`. The binary is `~/.local/bin/arca`. The units
use systemd specifiers only (`%h` for the home directory, `%t` for the user's runtime directory), so they carry no
host-specific value.

`scripts/install.sh` (`docs/QUICKSTART.md`, "Install with one command") installs a unit of its own instead,
`arca.service`: the node alone, reading the Incus socket its config names, with no shim and no memory limit, and with
the same restart rule. It is for a first read-only node. The installer refuses a user who has either of the two units
above, and its `--uninstall` leaves the program in place for them.

What the host needs:

- The service's user can read the Incus socket, for example through the group that owns it.
- The user manager delegates the `memory` controller, so the memory limits apply, and lingering is on for the user, so
  the units run without a login session (`loginctl enable-linger`, run by the owner).
- `arca` is installed at `~/.local/bin/arca`.

### The service's config

The config must name no kernel project, so the node only reads. Its paths follow the units' directories. `<uid>` is the
service user's uid, and `<home>` its home directory; the config file takes no specifiers.

```json
{
  "stateDir": "<home>/.local/state/arca-node",
  "incus": { "socket": "/run/user/<uid>/arca-shim/incus.sock", "pools": ["default"] },
  "identityPhones": [],
  "listen": { "unix": "/run/user/<uid>/arca/arca.sock", "tailnet": null, "proxy": null },
  "tokens": { "hashDir": "<home>/.config/arca/tokens" },
  "kernel": { "projects": [], "scratch": false, "droiddProjects": ["app1"], "reservedCidrs": [] }
}
```

The shim path is not the daemon's standard socket, but the runtime check compares the Incus server name it reaches with the
local host name, so this node is local without an explicit `incus.local`.

`StateDirectory=arca-node` creates `~/.local/state/arca-node` at mode 0700, which `serve` requires. The state directory is
not named `arca`: systemd 254 and later turn a user unit's `StateDirectory=arca` into a link to `~/.config/arca`, the config's
directory, when that exists and `~/.local/state/arca` does not, and `serve` refuses a state directory that is a link. An
install whose config names `~/.local/state/arca` and holds a real directory there keeps working unchanged; the unit then only
adds an empty `~/.local/state/arca-node`. `RuntimeDirectory=arca` creates
`/run/user/<uid>/arca` for the socket, and removes it when the unit stops. The token directory is minted into at mode 0700
and then set to 0500 (see Tokens).

### Install (the owner approves it first)

```sh
install -m 0644 deploy/systemd/arca-shim.service deploy/systemd/arca-watch.service ~/.config/systemd/user/
systemd-analyze --user verify ~/.config/systemd/user/arca-shim.service ~/.config/systemd/user/arca-watch.service
systemctl --user daemon-reload
systemctl --user enable --now arca-shim.service arca-watch.service
```

**Rollback:** `systemctl --user disable --now arca-watch.service arca-shim.service`, remove both unit files from
`~/.config/systemd/user/`, then `systemctl --user daemon-reload`. The state directory and the tokens are kept.

### Checks

```sh
systemctl --user is-active arca-shim.service arca-watch.service
systemctl --user show arca-watch.service -p MemoryMax -p MemorySwapMax -p StartLimitBurst -p NRestarts -p MemoryPeak
systemctl --user show arca-shim.service -p MemoryMax -p MemorySwapMax -p NRestarts -p MemoryPeak
since=$(date '+%Y-%m-%d %H:%M:%S'); systemctl --user kill -s USR1 arca-shim.service; sleep 1
journalctl --user -u arca-shim.service --since "$since" -o cat | tail -n 1    # {"get":<n>,"refused":0}
arca call watch_report --config ~/.config/arca/node.json --token-file ~/.config/arca/viewer-1.secret --args '{}'
```

`arca-watch` reads back `MemoryMax=268435456`, `MemorySwapMax=0` and `StartLimitBurst=5`, and `arca-shim` reads back
`MemoryMax=67108864`. Once the watcher has finished a sweep, `watch_report` carries an `ageMs` above 0, which is the age of
that sweep. The service is stable after 24 hours active with `NRestarts=0`, a `MemoryPeak` under `MemoryHigh`, and a live
shim summary of `refused: 0`.

### Publishing through a reverse proxy (each step approved by the owner at that moment)

Agents on the tailnet reach the node through a reverse proxy, such as Caddy in a container on the host's bridge, that
serves `/mcp` only, from the tailnet source only, under a host name. Each step below is separate, is taken only after the
owner's yes, and has its own rollback. Take them in order and roll back in reverse order.

1. **Proxy listener.** Set `listen.proxy` in the service's config to the host's bridge address and a free port, and the
   proxy container's bridge address as `peer`, for example `{ "addr": "192.0.2.1:7460", "peer": "192.0.2.20" }`. Then
   `systemctl --user restart arca-watch.service`, and check that the address answers 401 from the peer and nothing from
   any other address.
   **Rollback:** set `listen.proxy` back to `null` and restart `arca-watch.service`.
2. **Proxy site file.** Add one site file for the new host name to the proxy, copied from an existing tailnet-only site on
   the same proxy, with three changes: the host name; an upstream of plain HTTP to `listen.proxy.addr`; and the rule that
   admits only `/mcp` from the tailnet source, with every other request answered 403. In Caddy's terms:

   ```
   <host name> {
       @mcp {
           path /mcp
           remote_ip <tailnet source>
       }
       handle @mcp {
           reverse_proxy <listen.proxy.addr>
       }
       handle {
           respond 403
       }
   }
   ```

   Then reload the proxy (`systemctl reload caddy` in its container).
   **Rollback:** remove the site file and reload the proxy.
3. **DNS record.** Create an `A` record from the host name to the host's tailnet address, not proxied by the DNS
   provider, so the name resolves only to a tailnet address. Keep the provider's API token out of command lines and logs.
   **Rollback:** delete the record.
4. **MCP client entry.** Mint a token for the client as the service's user: set the token directory to 0700, run
   `arca token mint --scope viewer --id <id> --config ~/.config/arca/node.json --out <secret file>`, then set it back
   to 0500. `serve` reads its tokens only at start, so run `systemctl --user restart arca-watch.service`. Add the client
   entry under a name of its own, such as `arca-kernel`, that no other server on the client uses, with the URL
   `https://<host name>/mcp` and the bearer from the secret file. `viewer` is enough to read; `doctor` is needed only for
   `approval_submit`.
   **Rollback:** remove the client entry; set the token directory to 0700, remove `<id>.hash`, set it back to 0500, and
   restart `arca-watch.service`.

Once all four are in place, from a tailnet host: a `POST https://<host name>/mcp` without a bearer gets 401, `initialize`
with the viewer token gets 200, and any other path gets 403 or 404. Send the bearer from a 0600 header file
(`curl -H @<file>`), never on the command line.

## Adopting a container

`arca adopt` moves one existing container, listed in `kernel.adopt`, out of the default project into its own adoption
project, and back. It is the one path that writes outside the kernel's project family, through a closed table of steps
(Rule 45 in [`OWNERSHIP.md`](OWNERSHIP.md)). `serve` refuses a config that lists an adoption, so the adoption config is a
second file that names the service's `stateDir`, the adoption project in `kernel.projects`, and the bridge in
`kernel.attachNetworks`:

```json
"kernel": {
  "projects": ["arcak-adopt-svc1"],
  "scratch": false,
  "droiddProjects": ["app1"],
  "adopt": [{ "instance": "svc1", "to": "arcak-adopt-svc1" }],
  "attachNetworks": ["br1"]
}
```

1. `arca adopt plan --instance svc1 --expect-ip 192.0.2.131 --json --config adopt.json` runs the preflight with GET alone
   and prints the plan, its digest, its allowances and every request. The preflight refuses, naming the reason, any
   container the move would change or that needs what no allowance admits (see `cmd_adopt::preflight`). For a running
   container `--expect-ip` must be its live IPv4; a stopped one shows none, so `--expect-ip` names the IPv4 its adoption
   pins (see "A stopped container" below).
2. `arca adopt apply --instance svc1 --plan-digest <digest> --dry-run --config adopt.json` takes no lock and sends no write.
   It prints the same allowances.
3. Stop the service that holds the state directory (`systemctl --user stop arca-watch.service`), then run `apply` without
   `--dry-run`. It takes `desired.lock` and runs CreateProject, Stop, Snapshot, PinNic, LocalizeRoot (only when the plan
   localizes the root), Move, Mark and Start; a stopped container's plan has no Stop and no Start, and its `apply` needs the
   same `--expect-ip` as its plan. Each step
   writes an intent line before its request and an outcome line after it to `<stateDir>/adoptions.jsonl` (mode 0600,
   synced). A fresh plan's digest must be the one given.
4. `arca adopt verify --instance svc1 --port 8080 [--http-path /] [--wait <s>] --config adopt.json` checks that the instance
   runs under the uuid the record holds, with the pre-image's MAC and IPv4 on `eth0`, then connects to `<ipv4>:<port>`
   and needs an HTTP answer below 500 within 10 s. It tries again every 2 s for up to `--wait` seconds (60 by default, at
   most 3600), so a lease still being given or a service still starting is waited for, and then reports the last failure.
   A container whose `eth0` ACL admits only its reverse proxy never answers the host on its own port: give
   `--url http://<proxy host>[:<port>]/<path>` in place of `--port` (exactly one of the two is required, and `--http-path`
   goes with `--port` only). The instance checks are the same; the port probe is replaced by one GET of the URL, which must
   answer 2xx or 3xx within 10 s, retried within `--wait` as above. A redirect is not followed: a 3xx is the answer. Only
   `http://` is supported, since arca carries no TLS client, so point it at a plain-HTTP listener of the proxy. The URL may
   not carry user info or a fragment, and the host name is resolved by the system resolver, outside the 10 s. The request
   carries the URL's query, but no message and no answer shows it: it is given as `?…`, since it may carry a secret. An
   adoption that left a stopped container stopped takes neither `--port` nor `--url`; see "A stopped container" below.
5. Start the service again. `watch_report` and `arca watch --once --json` count each adoption that is not released in full as
   a live entry, so the instance shows in its adoption project as kernel-owned (`adopt-svc1`) with no drift.

**Allowances.** The adoption project is restricted. It grants `eth0`'s network, allows snapshots and backups, and past
those sets exactly the `restricted.*` keys that what the container already uses needs, as Incus 7.5 checks a moved-in
container. Nothing else is allowed, and the container's own config is not changed. The plan holds them under
`allowances.project`, so the digest the owner approves covers them, and `plan` and the dry run print them under
`allowances` with, for each key, the config keys and devices that need it (`why`):

| The container has | The allowance |
|---|---|
| `security.nesting` true | `restricted.containers.nesting=allow` |
| an interception key Incus admits (`security.syscalls.intercept.{bpf,bpf.devices,mknod,mount,mount.fuse,setxattr,sysinfo}`), not false | `restricted.containers.interception=allow` |
| another `security.syscalls.intercept*` key, any `raw.*` key, `security.bpffs*`, `linux.kernel_modules`, `limits.memory.swap`, `limits.memory.oom_priority`, `security.idmap.*`, `security.selinux.*`, `security.guestapi.images`, `security.devlxd.images` or `boot.host_shutdown_*` | `restricted.containers.lowlevel=allow` |
| a host-path disk | `restricted.devices.disk=allow` and `restricted.devices.disk.paths=<its sources, comma-separated>` |
| a `unix-char`, `unix-block`, `unix-hotplug`, `infiniband`, `gpu`, `usb`, `pci` or `proxy` device | `restricted.devices.<type>=allow` |
| `eth0` on a bridge `kernel.attachNetworks` does not list | `restricted.networks.access=<that bridge>` |
| `eth0` on a listed bridge, its `security.acls*` keys, a pool-backed root, a `none` device | nothing |

An `eth0` ACL needs no allowance: a bridged NIC looks its ACLs up in the project that serves its network, which is `default`
for an adoption project, and no `restricted.*` key governs an ACL. The preflight refuses, naming the reason,
`security.privileged` true (the adoption never grants `restricted.containers.privilege`), a NIC other than `eth0`, an `eth0`
with no managed network, an `eth0` key other than `type`, `name`, `network`, `hwaddr`, `ipv4.address` and the
`security.acls*` keys, a `hwaddr` or `ipv4.address` other than the running MAC and IPv4 (for a stopped container, a `hwaddr`
other than its volatile one, or an `ipv4.address` other than `--expect-ip`), a disk on a pool other than the
root, a host path a comma-separated list cannot hold exactly, a host-path disk with `shift` set (Incus refuses it once
`restricted.devices.disk.paths` is set), any other device type, and any device or config key but
`eth0` and the root that a profile gives.

**The pin.** PinNic sets `devices.eth0` on the instance itself: the network, the pre-image MAC as `hwaddr` and the pre-image
IPv4 as `ipv4.address`, with the `security.acls*` keys the instance's `eth0` sets kept as they are. It is set before Move and
never removed, and Move and MoveBack refuse an unpinned instance. Because
the pin sets `hwaddr`, Incus drops `volatile.eth0.hwaddr` at PinNic (`deviceVolatileReset`), and from then on the MAC is the
device's own, in both projects and after a release. The snapshot `arca-adopt-<digest[0..12]>` is taken after Stop and before
PinNic: restore it only after a release, never in the adoption project, where it would drop `eth0`.

**The localized root.** A root disk the default profile gives would not reach the adoption project, whose own default
profile is empty. The plan then records it under `allowances.localize`, and LocalizeRoot, after PinNic, sets it on the
instance itself, as the profile gives it, with one PATCH under the ETag of the read its checks were made on. It refuses a
root the profile changed since the plan, and Move refuses an instance whose root is not yet its own. The release takes it
back off at RestoreRoot, after MoveBack, only when the instance's own root is still the one LocalizeRoot set and the default
profile still gives the same one: one full PUT of the instance as read, under its ETag, less that device. A root that
changed meanwhile is kept, and the release refuses before ReleaseStop.

A snapshot keeps only the instance's own devices, so every snapshot taken before LocalizeRoot, the adoption's own
`arca-adopt-<digest[0..12]>` and any earlier one, holds no root. When the instance moves back, Incus gives such a snapshot a
bare root on the pool with no `size`, and restoring it later drops the root's size. The adoption does not rewrite
snapshots: after restoring one, set the size on the instance again (`incus config device set <instance> root
size=<size>`). `plan` and the dry run say so under `allowances.why`, keyed `localize.root`, with the size the profile gives.

**A stopped container.** A container that is stopped when it is planned has no running MAC or IPv4 to read. Its plan pins the
MAC Incus starts it with, `eth0`'s own `hwaddr`, else its `volatile.eth0.hwaddr` (the two must agree when both are set), and
the IPv4 that `--expect-ip` names, which `plan` and `apply` both need. That IPv4 must be `eth0`'s `ipv4.address` when it sets
one, and the address of every static lease the network lists for the MAC; a dynamic lease of another address, or no lease,
is no claim, since the pin's `ipv4.address` is what its next start leases. Another MAC's lease or another device's
`ipv4.address` on it is refused, as for a running container. The plan says `stopped` in its pre-image, which the digest
covers, and its steps are CreateProject, Snapshot, PinNic, LocalizeRoot (when it localizes the root), Move and Mark: nothing
stops or starts it, and it is still stopped, in its adoption project, once `apply` completes. Its release is Unmark,
MoveBack, RestoreRoot (when it localizes) and DeleteProject, and leaves it stopped in `default`. A release refuses, before any
step, while it is not stopped where the release finds it: stop it by hand first. `verify` takes neither `--port` nor `--url`
for it and probes nothing: it checks that the instance is in its project under the recorded uuid, carries the adoption's mark
(or, after a release, no kernel mark), has the pin as its own `eth0` and is stopped, and prints `"stopped": true` and
`"probed": false`. A running container's plan is the one it was before: it says nothing of `stopped`, and keeps its digest.

**Resume.** When `apply` stops at any point, whether a kill, a crash or a failed step, run the same `apply` with the same
digest again. It goes on from the plan in the record, never from a fresh preflight, which would refuse an instance that has
moved. A step whose intent is the last line runs again without a second intent, and a step that had landed sends nothing.
The digest must be the recorded plan's, and so must an `--expect-ip` given to it. Only one adoption may be open at a time,
and a complete adoption applied again sends nothing.

**A move is a copy.** Incus 7.5.1 runs a stopped instance's project move as a copy, under a new `volatile.uuid`. The writer
confirms a move under a new uuid only when the new uuid is a plain uuid, the old uuid is in no project, the name is gone from
the project it left, exactly one instance of the name is in the new one, that one is unmarked, carries the pin and holds the
adoption snapshot, and the adoption project still carries this adoption's mark. The record
then holds the new uuid (a `uuid` line before the move's outcome), and every later step, `verify` and `adopt status` follow it.
A Move that failed as landed but unconfirmed is resumed with the same `apply`: the copy is confirmed by the same rule and the
move is not sent again. An instance started by hand in its adoption project meanwhile is marked as it runs, and Start then
sends nothing. Resume it with `apply` before any `release`: a release from that state refuses at MoveBack, because the
instance runs, and records the failure, so only `release` can finish it after that.

**Release.** `arca adopt release --instance svc1 [--dry-run] --config adopt.json` takes the lock, except for `--dry-run`, and
returns the instance to `default`, running and unmarked, from wherever its record stands, then deletes the adoption project.
Before Move, RestoreRoot (when the plan localizes the root), ReleaseStart and DeleteProject are left; after Move, MoveBack
comes first; after Mark, Unmark comes before it; after Start, ReleaseStop, Unmark, MoveBack, RestoreRoot, ReleaseStart and
DeleteProject all run. A stopped container's release has no ReleaseStop and no ReleaseStart. An adopt step left at its intent
is closed as failed first, and each release step reads before it
sends. When the instance is in its adoption project without the pin, or with a localized root that RestoreRoot would not
take off, the release, and its dry run, refuse before any write, since those steps would refuse only after the instance was
stopped and unmarked. The pin and the snapshot are kept. DeleteProject is the adoption's one DELETE: it deletes the adoption
project only when it carries this adoption's mark and holds no instance, and a project already gone is the step done. A
release recorded before releases deleted the project ends at ReleaseStart, and `release` finishes it with DeleteProject.
Once a release has begun, `apply` refuses to resume and `release` finishes it. A release done in full, run again, sends
nothing. `verify` then checks the instance in `default`.

Exit codes: `2` for a refusal before any write: an instance the config does not list, a preflight check, a stale digest, a
digest that is not the recorded plan's, a record that holds no such adoption, another open adoption, a resume after a
release has begun, a release of an instance that lost its pin or whose localized root RestoreRoot would keep, a release of an
adoption planned stopped while the instance is not stopped, a stopped container planned or applied afresh without
`--expect-ip`, a resume whose `--expect-ip` is not the recorded plan's, a verify before the adoption or release completed, or a
bad `--port`, `--http-path`, `--url` or `--wait` (or both or neither of `--port` and `--url`, or either of them for an
adoption planned stopped). `1` for a held
lock, a failed step (the record names it) or a failed verify.

## Importing a service another manager created

`service_import` takes over a service that another manager created, where it stands. It does not move, stop or restart the
instance, and it changes no device, profile, ACL or other key. It writes one key on the instance,
`user.arcakernel.entry = adopt-<name>`, and from then on Arca lists the service: `service_list`, `service_status` and
`service_logs` work on it, the canvas shows it with its state, address and ports, and every verb that would write it
(`service_remove`, `service_connect`, `service_disconnect`, `service_secret_set`, `service_publish`, `service_unpublish`)
refuses with the code `adopted`. `service_release` is the exact inverse. The rules are Rules 52 and 53 in
[`OWNERSHIP.md`](OWNERSHIP.md).

**1. List the service.** Nothing can be imported until the node's config names it. Add it to `kernel.importServices` and
restart the service once:

```json
"kernel": {
  "projects": ["arcak-svc"],
  "droiddProjects": ["app1"],
  "serviceCidr": "192.0.2.0/24",
  "importServices": [{ "name": "db1", "project": "app1", "cidr": "10.0.5.0/24" }]
}
```

`project` is the other manager's project the instance is in, and it must be one of `droiddProjects`. `cidr` is that
project's service subnet: the instance's address is checked against it, not against `kernel.serviceCidr`. An instance that
carries the mark and has no entry is treated as before: outside `kernel.serviceCidr` it lists as `failed` and gets no record.

**2. Dry run.** `arca service import plan --name db1 --config <file>` reads Incus with GET alone, never opens the state
directory, and so runs beside the serving kernel. It prints:

```json
{
  "name": "db1", "project": "app1", "cidr": "10.0.5.0/24", "ready": true,
  "checks": [{ "check": "listed", "ok": true, "detail": "kernel.importServices lists it" }, "..."],
  "uuid": "<the instance's volatile.uuid>",
  "preImage": "<64 hex>",
  "write": {
    "method": "PATCH", "path": "/1.0/instances/db1?project=app1", "ifMatch": true,
    "body": { "config": { "user.arcakernel.entry": "adopt-db1" } }
  },
  "wrote": false
}
```

`write` is the one request the import would send, and `null` when `ready` is `false`. The checks are `listed`, `found`,
`container`, `unmarked` (no key in the kernel's namespace), `serviceKeys` (the other manager's template, address and ports
keys), `address` (inside the listed cidr, and the one `eth0` pins), `record` (the keys read as a service record),
`nameFree` and `identity` (the instance has a uuid). `nameFree` is `null` here: only the running kernel knows Arca's own
services, and it checks the name when the import is proposed and again when it runs. The command exits `0` whether or not
the plan is ready; read `ready`. `preImage` is the SHA-256 of the canonical JSON of the instance's project, name, type, uuid,
profiles, own devices, the names of its own non-volatile config keys, and the values of its `user.arca.*` and
`user.arcakernel.*` keys. No other config value feeds it, so no secret does.

**3. Import.** Propose it and have the owner approve it; the verb runs only with the one-shot grant of an approved
proposal, never with an approval of its own:

```
action_propose { "action": "service_import", "args": { "name": "db1" } }
```

The proposal is refused, with the failed check in plain words, when the plan is not ready, so the owner is never asked to
approve an import that cannot run. The owner approves it on the approval page with the passkey. The import then plans again,
records its intent in `<stateDir>/adoptions.jsonl`, sends the one `PATCH` under the `If-Match` of the read it checked, reads
the instance back and records the outcome. It answers `{ "imported": true, "preImage": "<64 hex>", "service": <the service
row> }`. If the other manager wrote the instance between the read and the write, Incus refuses the write (412), nothing
changes, and the answer says to propose the import again. It is never forced.

**4. Restart.** A restart reads the record `service-db1` back from the desired state. It imports nothing again and sends no
write.

**Rollback.** `service_release` gives the service back:

```
action_propose { "action": "service_release", "args": { "name": "db1" } }
```

Once the owner approves it, the release clears the one key, under `If-Match`, when the instance still carries
`adopt-db1`, then removes Arca's record of the service, and answers `{ "released": true, "restored": <bool>, "preImage":
"<64 hex or null>" }`. `restored` is `true` when the instance's image is again the pre-image the import recorded; it is
`false` when the other manager changed the instance in between, which the release leaves as it is. The instance is not
stopped and nothing else on it is written. The release refuses, and writes nothing, when:

- `kernel.importServices` no longer lists the service (list it again, restart, release, then take the entry out);
- the instance of that name is not the one that was imported (its uuid differs);
- the instance carries another key in the kernel's namespace;
- its mark no longer names this import.

An instance that is already gone, or whose mark was already cleared by hand, takes no write: only the record is removed.
Keep the allowlist entry until the release is done. After it, remove the entry and restart; the instance then carries only
the other manager's keys and that manager owns it alone again.

`watch_report` shows an imported instance in its own project with the class `contested`, since it carries both managers'
marks, and reports no drift for it.

The import and the release are recorded in `<stateDir>/adoptions.jsonl` as lines of kind `import` and `release`. A build
older than this one does not read those lines: before going back to an older build on a state directory that holds them,
release every imported service, and keep the file aside.

## Alerts

On every 5-second tick, `serve` reads the node, evaluates ten rules and writes the points and any alert events to
`arca.db` (see "The store"). The thresholds come from `alerts.json` in the state directory. Alerts never write anything to
Incus or to the desired state.

### alerts.json

The file is optional, and a missing file gives the defaults. It is read once, when `serve` starts, so an edit takes effect at
the next restart. Its keys are camelCase. An unknown key, an unknown or repeated rule id, a value of the wrong type or range,
or a file that is not JSON stops `serve` with exit 2. The error names a key and a position, never a value.

- `rules`: a list of `{id, enabled, threshold, forS, clearForS, clearMargin, severity}`. Only `id` is required. An omitted
  field keeps that rule's default. `severity` is `critical` or `warning`.
- `shimSocket`: the socket of the recording shim. With it, the `shim` health component is checked. Without it, there is no
  `shim` component.
- `probes`: `{gateway, internet, relay}`, each an `ip:port` literal. A named target is probed; an unnamed one is not.

```json
{
  "rules": [
    { "id": "disk-pool-full", "threshold": 85 },
    { "id": "cpu-hot", "enabled": false }
  ],
  "probes": { "gateway": "192.0.2.1:443" }
}
```

Set `"enabled": false` for a sensor the node lacks, such as a CPU without a temperature sensor, or a node with no SMART
drive. A disabled rule is never evaluated or exported, and `alerts_list` lists it under `disabled`. If a rule that has a
firing alert is disabled, that alert resolves at the next start with reason `rule-disabled`. On a node without incus-osd,
also disable `update-failed` and `smart-warning`; see "incus-osd readings".

### The rules

Every rule uses one comparison. It fires when its value stays above `threshold` for `forS` seconds, and it clears when its
value stays at or below `threshold - clearMargin` for `clearForS` seconds. Unless a note below says otherwise, a rule's
default is `forS` 60 and `clearForS` 120, so a condition that lasts 59 seconds never fires.

| Rule | Reads | Default threshold | Clear margin | Severity |
|------|-------|-------------------|--------------|----------|
| `disk-pool-full` | the used space of each pool, in percent of its size | 90 | 5 | critical |
| `mem-psi-high` | memory pressure, the kernel's `some` average over 10 s, in percent | 25 | 5 | warning |
| `cpu-hot` | the CPU temperature, in °C | 85 | 5 | warning |
| `service-down` | each health component: `incus`, `arca`, `watch`, and `shim` when configured | 0 | 0 | critical |
| `drift` | every drift entry of the newest watch sweep, of every kind | 0 | 0 | warning |
| `update-failed` | the incus-osd update status: a failed check, no completed check for 14 days, or a node up 24 hours with no completed check and no usable check time | 0 | 0 | warning |
| `crash-loop` | the starts of one instance in the last 600 seconds | 2 | 2 | critical |
| `smart-warning` | a drive's SMART health check, failed when it reports false | 0 | 0 | critical |
| `gpu-vram-high` | each card's used VRAM, in percent of its total, keyed by PCI address | 95 | 5 | warning |
| `gpu-gone` | 1 for each card that was in the inventory and is gone from it now | 0 | 0 | critical |

A yes-or-no rule reads 1 when it is bad and 0 when it is fine, so its threshold of 0 fires on 1. The thresholds are a
starting point. Tune them after a week of data.

`gpu-vram-high` fires only above 95 percent held for five minutes, so exactly 95 is not above it. `gpu-gone` fires after a
card that a successful inventory held has been missing for 15 seconds, and clears when the card returns. A failed
inventory read is unknown, so it never resolves either alert. `gpu-vram-high` is marked unavailable only when every card's
telemetry reason is `not measurable from here` or `nvidia-smi is not installed`, and only when the metrics mode is not
provisional: a firing alert then resolves with `no data` after its clear window rather than hanging forever. A failed or
timed-out read, or a card with no reading yet, is an ordinary unknown that keeps a firing alert. The GPU labels are the
card's PCI address, so two identical cards have two alert rows.

The pools that `disk-pool-full` reads come from Incus's own pool list on every tick. The `incus.pools` key of the config
file filters that list (see the config table), so a pool that is left out is never evaluated.

`store-write-failed` is built in. It is not in `alerts.json`, it is critical, and it lives only in memory. It fires at the
first failed commit, and it clears after 120 seconds of commits that work. It shows in `alerts_list`, in the HEALTH line and
as `arca_alert_firing`.

`store-commit-slow` is built in the same way, and it is a warning. It fires at the first commit that works and takes over one
second, and it clears after 120 seconds of commits that take less. Such a commit is on disk and nothing waits: the alert
says the disk under the store is slow, not that a write failed. (Up to 0.13 a slow commit fired `store-write-failed`,
critical, and counted as a failed write.)

### Unknown data

A reading is unknown when its source gave none or it is too old. The maximum ages are: the host sample and the health
components, 15 seconds; the watch sweep and the latency probes, 90 seconds; the incus-osd update status, 15 minutes; the SMART
result, 2 hours.

- Unknown data never starts an alert. A pending alert keeps its state. A firing alert stays firing and is marked
  `unknown: true` while its input is missing, and no event is written, even through a long outage.
- The one exception is a reading the node can never provide, such as PSI or CPU temperature on a remote metrics source. A
  firing alert for one resolves once with reason `no data` after `clearForS`; a fresh reading before then keeps it firing.
- `alerts_list.unknown` lists each unknown rule and label set, with reason `missing` (no reading yet), `stale` (too old) or
  `unavailable` (a reading this node can never provide).
- The one exception is `subject-gone`. A fresh, known reading that no longer has a firing alert's label set (a deleted pool
  or instance, or a drive that left the drive list) resolves that alert once.
- When Incus does not answer, the instance and pool readings are unknown, and no alert ends. `node_metrics` still lists
  `projects` and `pools` as empty arrays, because its released schema has no null, so those lists alone do not show an
  outage.
- A drive listed with an ID but no usable SMART result (its SMART block is missing, null or disabled) is unknown, not passed.
  A firing `smart-warning` for it is held as unknown. A drive that was OK and then loses its SMART result stays listed as
  unknown, and it resolves only when it leaves the drive list. A SMART block with a non-empty `error` makes the whole SMART
  reading unknown.
- When incus-osd hangs, one tick can take about 20 seconds. The HEALTH line then reads `alerts stale`, and it recovers at
  the next tick that finishes.

### incus-osd readings

Every 300 seconds, `serve` reads two incus-osd documents with GETs: the security state and the update status. Every 600
seconds it reads the storage document for SMART. A node without incus-osd gives no reading for these, so their rules are
unknown there, and the HEALTH line shows `· N unknown` for them. On such a node, disable both rules in `alerts.json` and
restart `serve`, which reads that file only at start:

```json
{
  "rules": [
    { "id": "update-failed", "enabled": false },
    { "id": "smart-warning", "enabled": false }
  ]
}
```

- **Secure Boot.** `arca_secureboot_enabled` is 1 when Secure Boot is on.
- **TPM.** `arca_tpm_present` is 1 only when incus-osd reports the TPM status `ok`, a physical TPM whose measured boot state
  matches. It is 0 for these other statuses:
  - `pcr mismatch`: PCR 7, the Secure Boot measurement, differs from the value incus-osd computes from its event log.
    The mismatch does not by itself stop TPM unlock. incus-osd reports the encrypted volumes as `unlocked (TPM; PCR update
    pending)` and assumes TPM unlock still works at the next reboot.
  - `swtpm`: no physical TPM was found, so incus-osd runs on a software TPM. incus-osd reports this as a degraded security
    state.
  - Any other status is also 0.
- **Update.** The update status is read by its text. A status that starts with `Failed` is a failed check. A status that ends
  with ` update check completed`, or contains ` has been updated to version `, is a completed check. Any other status, such
  as a check in progress, a skipped check outside the maintenance window, or an error text, keeps the previous result.
  `update-failed` is 1 after a failed check and until a later completed one, and it is 1 when no check has completed for 14
  days. A node with no usable check time and no known completed check also fires once its uptime reaches 24 hours.
  Uptime comes from `/proc/uptime`, so restarting arca does not restart this grace period. Missing or unparseable uptime
  leaves this condition unknown. A completed status with an unusable time does not count as never checked.
  `arca_update_last_success_timestamp_seconds` is the start time of the last completed check, because incus-osd records
  a check's time when the check starts. It is absent until one completes. `arca_update_failed` is absent while the rule
  has no reading.
- **Recovery keys.** The security document holds recovery keys. Each read takes only the Secure Boot and TPM fields, and it
  drops the body when the read returns. The keys are therefore held in memory, as a JSON value, for the length of each read.
  They are never stored, logged, put in an error or exported.

### Latency probes

`probes` names up to three TCP targets. Every 30 seconds `serve` opens a TCP connection to each named target and times it,
with a 1-second timeout. `arca_latency_seconds` carries that connect time, which is not a ping. A failed or timed-out connect
has no series, not a zero. A target that is not named has no series.

### Instances and crash loops

Each tick makes one listing of every instance, with its state, CPU, memory and last-use time. A start is counted when an
instance's last-use time changes. Three starts in 10 minutes fire `crash-loop`. Two starts inside one 5-second tick count as
one, which only delays the alert, because the rule needs three. A stopped instance has no CPU or memory series, and an
instance with no `type` in the listing counts as a container. The listing loads full state, so a slow Incus can stretch a
tick.

### Drift

`drift` is the total of all drift entries in the newest watch sweep. Its value includes entries of a kind that the export
does not name (they are counted as `other`), so the sum of `arca_drift_entries` can be lower than the alert's value.

### SMART drive labels

`arca_smart_passed` and the SMART alerts name a drive by its `device` label. The label is `drive-` and 12 hex digits. It is the
low 48 bits of a 64-bit FNV-1a hash of the drive's incus-osd ID, which is its first `/dev/disk/by-id/` link. The model and
serial are never in a label, a log, an error or a metric.

The label is a pseudonym, not protection. Anyone who has the ID can compute it, and it changes when the first by-id link that
incus-osd records for the drive changes. To map a label to a physical drive:

1. Read the storage document with one GET on the Incus socket, which is read-only:
   `curl --unix-socket <incus.socket> http://localhost/os/1.0/system/storage`. Its output holds serial numbers, so keep it in
   your terminal.
2. For each entry in `state.drives`, compute its label from its `id`:

   ```sh
   python3 -I -c 'import sys
   h = 0xcbf29ce484222325
   for b in sys.argv[1].encode():
       h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
   print("drive-%012x" % (h & 0xFFFFFFFFFFFF))' "<the drive's id>"
   ```

3. The entry whose label matches the `device` label of the alert, or of `arca_smart_passed`, is the drive. Its `id` names it:
   `readlink -f "<the drive's id>"` gives the device node.

### The HEALTH line

`alerts_list` returns `healthLine`, which the console footer shows too. It reads one of these:

- `alerts none`
- `alerts <n> firing: <rule>, <rule>`, with one name per firing alert in firing order, so two full pools read
  `disk-pool-full, disk-pool-full`
- ` · <m> unknown` appended when `m` unknown entries exist, for example `alerts none · 1 unknown`
- `alerts stale <s>s` when the newest host sample is older than 15 seconds, or `alerts stale` before the first sample

The four health components feed `service-down`. Each one is checked on every tick:

- `incus`: `GET /1.0` on the Incus socket returns 200 within 2 seconds. The 2 seconds cover the request and its reply;
  the connect to the Unix socket has no timeout of its own.
- `arca`: a `POST /mcp` with no token to the node's own unix socket returns 401 within 2 seconds. This is the one request
  `arca-alerts` makes that is not a GET. It carries no token and changes nothing.
- `watch`: the newest successful watch sweep is no older than 90 seconds. Sweeps are timed by the clock, so if the clock is set
  back, sweeps pause until it has passed the last sweep's start by 30 seconds. The age counts from the sweep's start.
- `shim`: with `shimSocket` set, `GET /1.0` through the recording shim answers 200 within 2 seconds.

### Restarts

- A firing alert loads as firing, with fresh timers and unknown until its first reading.
- If two open firings of one rule and label set are stored (a lost resolve), the newer stays firing and the older resolves
  once at start with reason `cleared`.

## Export

`GET /metrics` and `GET /alerts` are served on every listener of `serve`, and each needs a bearer token of any stored scope,
the same token as the MCP calls. Without a valid bearer the answer is 401. A request with an `Origin` header is 403, and any
other path is 404. Each route is the reading of one verb, and a token is admitted to the route as it is to the verb:
`/metrics` is `node_metrics` and `/alerts` is `alerts_list`. A token minted with `--tools` therefore needs that verb in
its list, or the route answers 403 with no body; a token with no list reads both, as before. Pass the token in the `Authorization` header:

```sh
curl --unix-socket /run/arca/arca.sock -H "Authorization: Bearer $(cat <token file>)" http://arca/metrics
```

### Metrics

`/metrics` is the Prometheus text format, version 0.0.4, with the content type `text/plain; version=0.0.4`. Every series
carries `node`, the node's id. A `null` reading is left out, never written as 0. No series uses the label `instance`, because
a scraper sets that label itself. The names, types and labels below are fixed.

| Metric | Type | Labels besides `node` | Meaning |
|--------|------|-----------------------|---------|
| `arca_node_cpu_used_ratio` | gauge | | the share of all CPUs busy since the previous sample, 0 to 1 |
| `arca_node_cpu_cores` | gauge | | the number of CPUs |
| `arca_node_load` | gauge | `window` (`15m`, `1m`, `5m`) | the load average |
| `arca_node_memory_total_bytes` | gauge | | the memory of the node |
| `arca_node_memory_available_bytes` | gauge | | the memory available to start applications |
| `arca_node_swap_total_bytes` | gauge | | the swap space |
| `arca_node_swap_free_bytes` | gauge | | the free swap space |
| `arca_node_psi_avg10_ratio` | gauge | `resource` (`memory`, `cpu`), `kind` (`some`, `full`) | the share of time tasks stalled, averaged over 10 seconds, 0 to 1 |
| `arca_node_cpu_temperature_celsius` | gauge | | the CPU temperature |
| `arca_node_gpu_busy_ratio` | gauge | | the share of time the GPU is busy, 0 to 1 |
| `arca_node_network_receive_bytes_total` | counter | | the bytes the physical NICs received |
| `arca_node_network_transmit_bytes_total` | counter | | the bytes the physical NICs sent |
| `arca_pool_size_bytes` | gauge | `pool` | the size of a storage pool |
| `arca_pool_used_bytes` | gauge | `pool` | the bytes a storage pool uses |
| `arca_project_instances` | gauge | `project` | the instances in a project |
| `arca_project_instances_running` | gauge | `project` | the running instances in a project |
| `arca_component_up` | gauge | `component` (`arca`, `incus`, `watch`, `shim`) | 1 when a health component is ok |
| `arca_drift_entries` | gauge | `kind` (`marked_without_entry`, `entry_without_object`) | the drift entries of the newest watch sweep |
| `arca_instance_running` | gauge | `project`, `incus_instance`, `type` (`container`, `vm`) | 1 when an instance runs |
| `arca_instance_starts_total` | counter | `project`, `incus_instance` | the starts of an instance since the server started |
| `arca_instance_cpu_seconds_total` | counter | `project`, `incus_instance` | the CPU time an instance has used |
| `arca_instance_memory_used_bytes` | gauge | `project`, `incus_instance` | the memory an instance uses |
| `arca_latency_seconds` | gauge | `target` (`gateway`, `internet`, `relay`) | the TCP connect time to a probe target |
| `arca_secureboot_enabled` | gauge | | 1 when Secure Boot is enabled |
| `arca_tpm_present` | gauge | | 1 only when incus-osd reports the TPM status `ok`; see "incus-osd readings" |
| `arca_update_last_success_timestamp_seconds` | gauge | | the start time of the last completed incus-osd update check, in epoch seconds |
| `arca_update_failed` | gauge | | 1 for a failed update, no completed check in 14 days, or a node up 24 hours with no completed check and no usable check time; 0 for a known recent success; absent when unknown |
| `arca_smart_passed` | gauge | `device` | 1 when the drive's SMART check passed, 0 when it failed; see "SMART drive labels" |
| `arca_alert_firing` | gauge | `rule`, `severity`, and the alert's own labels | 1 for each firing alert |
| `arca_store_write_failures_total` | counter | | the failed writes since the server started: a commit or a rollup the store refused |
| `arca_store_slow_commits_total` | counter | | the commits since the server started that worked and took over one second |
| `arca_sample_timestamp_seconds` | gauge | | when the newest host sample was taken, in epoch seconds |

Notes on the names above:

- **PSI is a ratio.** `arca_node_psi_avg10_ratio` is the kernel's percentage divided by 100. The `mem-psi-high` threshold in
  `alerts.json` is still in percent, so a threshold of 25 is `0.25` on `/metrics`. There is no `cpu` `full` series.
- **Drift.** See "Drift" above. `arca_drift_entries` has two kinds only.
- **TPM and update.** See "incus-osd readings" above.
- **Alerts.** A firing alert has one series, with its rule, its severity and its own labels. A pending, resolved or disabled
  alert has no series, so an absent series means the alert is not firing. The `store-write-failed` and `store-commit-slow`
  alerts appear here too.
- **Instances.** A stopped instance has `arca_instance_running` 0 and no CPU or memory series.
- **Ordering.** Series are written in order of their label values as text, so the `load` windows read `15m`, `1m`, `5m`.

### The alert events: `GET /alerts?after=<seq>`

`/alerts` returns one `arca.alert.v1` object per line (`application/x-ndjson`), oldest first. It returns the events whose `seq`
is above `after`, which defaults to 0, and at most 500 of them per page. To read all of them, start with `after=0` and
then ask again with `after` set to the largest `seq` of the last page, until a page is empty. A value that is not a whole
number is refused with 400.

Each event has these keys: `schema` (`arca.alert.v1`), `seq`, `alertId`, `node`, `rule`, `severity`, `state` (`firing` or
`resolved`), `labels`, `value` and `threshold` (in the rule's own units), `startedAt`, `resolvedAt` (null while firing), `at`
(when this event was written), and `reason` on resolved events only: `cleared`, `rule-disabled`, `subject-gone` or `no data`. A firing
event's `alertId` is its own `seq`, and its resolve is a new event with the same `alertId`.

```json
{"alertId":1793361600000,"at":1793361785000,"labels":{"pool":"default"},"node":"<node id>","reason":"cleared","resolvedAt":1793361785000,"rule":"disk-pool-full","schema":"arca.alert.v1","seq":1793361600003,"severity":"critical","startedAt":1793361660000,"state":"resolved","threshold":90.0,"value":50.0}
```

Events are written only when their commit works, so an event that waits after a failed commit is not in `/alerts` until the
next commit that works (see "Store failure").

## The store

`<stateDir>/arca.db` holds the points and the alert events. It is SQLite, built into `arca`, so it needs no system library.

### Durability and writes

- The file is in WAL mode with `synchronous=FULL`. A commit is on disk when it returns, so a tick that committed survives a
  crash or a power loss. `NORMAL` is not used, because it could lose the last commits.
- Each 5-second tick is one transaction: the tick's points and its alert events. The measured one-host example under
  "Sizing" has 125 series.
- The WAL is checkpointed automatically every 1000 pages, and after a checkpoint its file is cut back to at most 64 MiB. When
  an hour closes, the rollup checkpoints the WAL fully and returns free pages, which can stall one tick by up to 5 seconds.
- Each connection reads back its journal mode and `synchronous` value. If they are not `wal` and 2, the store refuses to open,
  and `serve` stops with exit 1.
- `alerts_list.commitMs` is the time of the last commit, and `storeBytes` is the size of `arca.db` with its `-wal` file.

### Tiers and retention

| Tier | Resolution | Kept | Rows per series at full retention |
|------|------------|------|-----------------------------------|
| raw | the 5-second points | 1 hour | 720 |
| minute | 1 minute: average, minimum and maximum | 7 days | 10,080 |
| hour | 1 hour: average, minimum and maximum | 90 days | 2,160 |

A counter rolls up by its last value. The rollup runs at start and at the first tick of each minute. It rolls every closed
bucket since its last mark, so a stop of several hours is caught up, and then it deletes the rows past their retention. A
bucket with no data stays a gap. Resolved alerts are kept 90 days, and a firing alert is never deleted.

### Sizing

Measured at full retention with 125 series, the store holds 1,620,000 rows in 70.8 MiB, which is about 0.6 MiB per series. The
test `size_estimate` in `arca-store` checks that 125 series stay under 128 MiB. To estimate a node, multiply its series count
by 0.6 MiB.

### Checking the store

`arca store check --config <file>` opens a scratch store in a new directory inside the state directory, through the same open
path as `arca.db`, and removes the directory. It never opens `arca.db`. It prints one JSON object with `sqlite_version`,
`journal_mode`, `synchronous` and `auto_vacuum`. On a working node `journal_mode` is `wal`, `synchronous` is 2 and
`auto_vacuum` is 2 (incremental). It exits 1 if the scratch store cannot be opened or removed.

### A corrupt file

Each open runs `quick_check`. When the file is corrupt, `arca.db`, `arca.db-wal` and `arca.db-shm` (the ones that exist) are
renamed with the suffix `.corrupt-<ms>`, and a new empty store is created. `arca` never deletes the renamed files, so an
operator can read them first. `alerts_list` then reports `storeReset: true`, and the history in the renamed files is not in the
new store. The rename is not followed by a sync of the state directory.

The first `seq` of the new store is the larger of the old highest `seq` plus one, and the reset time in milliseconds. A
client's `after` cursor therefore never sees `seq` go backwards.

### Store failure

When a commit fails, for example on a full disk:

- The tick still publishes, with `storeError: {at, message}` in `alerts_list`, and that tick's points are dropped.
- Its alert events wait in memory, at most 1,000. Past that the oldest are dropped. The `storeError` message then counts
  every alert event dropped since the server started, and that count does not reset until a restart.
- The next commit that works writes the waiting events with their original times. Until then they appear in `alerts_list`
  without a `seq`, and they are not in `/alerts`.
- `store-write-failed` fires at once, and `arca_store_write_failures_total` counts the failure.
- A commit that works and takes over one second is no failure: its events are kept and nothing waits. `store-commit-slow`, a
  warning, fires at once, and `arca_store_slow_commits_total` counts it.
- A rollup that fails, which runs at the first tick of a minute, takes the same path: that tick's `storeError` is set and
  `store-write-failed` fires.

## Bridged nodes and virtual machines

A bridged node is a host where Arca must not create a network: it runs beside another hypervisor, on that hypervisor's
bridges and storage, and shares its memory. The node config says so:

```json
"kernel": {
  "projects": ["arcak-svc"],
  "droiddProjects": ["app1"],
  "bridged": {
    "bridges": ["br0"],
    "pool": "vmpool",
    "limits": { "virtualMachines": 8, "cpu": 16, "memory": "64GiB", "disk": "1TiB" }
  }
}
```

| Key | Meaning |
|-----|---------|
| `bridges` | The host bridges a virtual machine's NIC may join. Each is an interface name of 1 to 15 letters, digits, `-`, `_` and `.`, listed once; at least one is required. |
| `pool` | The name of an Incus storage pool (as `incus storage list` shows it, a Ceph RBD pool for example), not a Ceph pool name or a path. Every virtual machine's root disk is created on it. |
| `limits` | What all of Arca's virtual machines on this node may use together. All four caps are required: `virtualMachines` and `cpu` are whole numbers of at least 1, `memory` and `disk` are sizes such as `64GiB`. |

Arca writes neither the bridges nor the pool: both must exist before the first create. `arca serve` refuses to start
when a listed bridge is not a bridge on this host. It reads `/sys/class/net/<bridge>/bridge` for that and asks Incus
nothing. A scratch run cannot set `kernel.bridged`. A node that sets `kernel.serviceCidr` beside it is a mixed node,
which "Mixed nodes" below describes; the rest of this section is the bridged node, which has no `kernel.serviceCidr`.

On a bridged node Arca creates no Incus network, no bridge, no network ACL, no profile and no address reservation, so
Incus adds no firewall rule for it. The first `service_create` adds the entry `bridged-arcak-svc`, which creates the
project `arcak-svc` and nothing else. That project is restricted like every kernel project, with these keys more:

| Project key | Value | Why |
|-------------|-------|-----|
| `restricted.devices.nic` | `allow` | Incus needs it before a NIC may name a bridge it does not manage. The provider grants the key in this project alone: to this entry when it creates the project, and to the managed network's entry when a managed node gains host bridges (see "Mixed nodes"). |
| `limits.virtual-machines`, `limits.cpu`, `limits.memory`, `limits.disk` | the caps of `kernel.bridged.limits`, as of the last virtual-machine create | Incus itself refuses an instance that would exceed them. |
| `limits.containers` | `0` | The project holds virtual machines only. |

`restricted.devices.nic=allow` is wider than the one NIC Arca creates: Incus has no per-bridge allowlist for a bridge
it does not manage, so the key admits every NIC type on every host interface for the whole project. The only guard on
which bridge a virtual machine joins is Arca's own: `service_create` checks the request against `kernel.bridged.bridges`
and the service plugin writes one fixed NIC shape. Anything that can write instances in `arcak-svc` outside Arca (an
Incus admin on the host) is not bound by the list.

**The caps.** `service_create` adds up the CPUs, memory and root disks of the virtual-machine records it holds, running
or not, and refuses a create that would pass a cap, before it writes anything, naming the cap and the amounts. Arca's
own check follows the config as soon as `arca serve` is restarted with new caps. The project's `limits.*` keys follow
with the next virtual-machine create: once that create has passed Arca's check, the configured caps are written into
the entry that owns the project and, by a journaled project config write, into the project, before the instance is
sent. This works both ways. A cap lowered below what the node already holds changes nothing: every create is refused,
naming the cap, what is in use and what is allowed, until the cap is raised or virtual machines are removed. Nothing
is to be set by hand. A value set by hand on the project is replaced by the entry's the next time Arca writes the
project: on the next pass where the managed network's entry owns the project, and at the next write of the entry
(a virtual-machine create that finds the caps changed, or the widening) where the bridged entry owns it. Whenever
Arca writes the owner's entry, for a virtual machine or to widen the project for the managed network, the entry takes
the configured caps, so a node whose project was raised by hand under an earlier Arca needs only its config to state
the same caps. A configured count below the number of virtual machines the node holds refuses that create, naming
both numbers and the config key, before anything is written.

**A network is never dropped from under what lives on it.** A node may gain the other network at any time (see "Mixed
nodes"). What `arca serve` refuses, at its start and naming the entry, is a config that would leave a half-state:
`kernel.bridged` without `kernel.serviceCidr` on a node that holds the managed service network, a container service, a
database, a consumer or a virtual machine on that network; no `kernel.bridged` on a node that holds a virtual machine
on a host bridge, or the bridged project entry before it was widened for the managed network; and no caps on a node
that holds any virtual machine. The check runs before the kernel reconciles anything, so a refused config never creates
or repairs an object. A virtual-machine create makes the same check again before its first write. It also refuses,
naming the reason and leaving no entry behind, when an Incus project `arcak-svc` exists that the entry owning this
node's service project did not create (one made by hand).

A template is a virtual machine when its `kind` is `virtual-machine`:

```json
{
  "name": "workspace-vm",
  "kind": "virtual-machine",
  "image": "images:debian/13",
  "limits": { "cpu": "2", "memory": "4GiB" },
  "rootSize": "20GiB",
  "autostart": true
}
```

`image` is `images:<alias>` (the public images server, with an optional `digest` fingerprint) or `local:<alias>`
(an image in the service project's own store, with an optional `digest` that pins it; see "A local image may be
pinned" above).
`limits` must set both `cpu` and `memory`, and `rootSize` is required; all three follow the size rules above. A
virtual-machine template may also set `description` and `autostart`, and `ports` with `provides` when a container is
to link to it: a link opens the port in the managed service network's ACL, so a template with `ports` is refused on a
host bridge. It may also set `firstBoot`, `settings` and `secrets`, which "First boot, settings and secrets of a
virtual machine" below describes. It may not set `env`, `generate`, `entrypoint`, `publishable`, `dashPort`,
`droiddPort`, `data` or `lan`: each of those depends on a container's environment, image or devices.

`service_create` takes two more arguments for a virtual-machine template:

| Argument | Meaning |
|----------|---------|
| `bridge` | The host bridge the NIC joins. It must be in `kernel.bridged.bridges`; any other name is refused. On a bridged node it may be left out when the node allows exactly one bridge. On a mixed node, leaving it out puts the virtual machine on the managed service network. |
| `mac` | For a host bridge only. A fixed MAC, six hex octets joined by `:`. A group address and the all-zero address are refused. So is a MAC another Arca record or another Incus instance in any project holds: on its own device, on a device a profile gives it, as the address Incus assigned it (`volatile.<device>.hwaddr`), or on a live interface. Left out, Incus picks one. |

Arca can compare a MAC only with what Incus knows. A device on the bridge that Incus does not run (another
hypervisor's virtual machine, the router, a physical host) cannot be checked, so a fixed MAC must be chosen to be unique
on that network.

The virtual machine is created in `arcak-svc` with `type: virtual-machine`, no profile, `limits.cpu` and `limits.memory`
from the template, a root disk of `rootSize` on the configured pool, one NIC
`{ "type": "nic", "name": "enp5s0", "nictype": "bridged", "parent": "<bridge>" }` (plus `hwaddr` when `mac` is given) and
the agent's config drive (see "What every virtual machine carries"). It carries the
kernel's mark and `user.arca.template`. It gets its address from the bridge's own network; Arca
neither assigns nor filters it. A bridge whose network hands out no addresses by DHCP (one where another hypervisor
assigns them itself) gives the guest none: put such a virtual machine on the managed service network of a mixed node,
or configure its address inside the guest. `service_status` of the running machine reads its live state and shows the
address its own network card holds, or an empty `ipv4` when it holds none; an address of a bridge the guest made for
its own containers is never shown. `service_list` reads no live state: the machine's row has an empty `ipv4` and
`ipv4In: "state"` (see "When Incus does not answer").

Ownership, the journal and approvals are the same as for a container. `service_create` needs a doctor token, can be
proposed with `action_propose`, and writes the same journal records. A proposal is checked when it is made, so the
owner is never asked to approve a create the node is sure to refuse: the template's kind must fit the node's mode, the
mode must fit the desired state, and the bridge must be an allowed one. A bridge the request leaves out is filled in
then. The owner's card says "Create the virtual machine <name> from template <template> on the host bridge <bridge>"
and lists the bridge and the MAC. The caps are checked when the proposal is made too (see "A proposal the node has
no room for is refused when it is made"), and again when the approved create runs, because the node can fill up while
a proposal waits. A MAC another of the node's records holds is refused when the proposal is made; whether an instance
Arca holds no record of has it is read from Incus when the approved create runs. `service_remove` needs the owner's approval and retires the virtual machine through the same delete plan.
`service_logs` returns the virtual machine's console log.

What a bridged node refuses, and why:

| Call | Refusal |
|------|---------|
| `service_create` with a container template | The node is bridged and runs virtual-machine templates only. |
| `service_create` with a virtual-machine template on a node with neither `kernel.bridged` nor `kernel.vmLimits` | The node sets no virtual-machine caps. |
| `service_create` that would pass a cap of `kernel.bridged.limits` | The cap, what is in use, what was asked and what is allowed. |
| `service_connect`, `service_disconnect` | Links need the service network and its per-service firewall rules. |
| `service_publish`, `service_unpublish` | The virtual machine cannot be published from Arca; reach it on its bridge's own network. |
| `service_secret_set` for a secret the template does not declare | The template does not declare that secret. A declared one is stored (see "First boot, settings and secrets of a virtual machine"). |
| `postgres_create` and the probe consumer | They need `kernel.serviceCidr`. |

### First boot, settings and secrets of a virtual machine

A virtual-machine template may say what the machine does at its first boot, give it settings, and declare secrets:

```json
{
  "name": "example-vm",
  "kind": "virtual-machine",
  "image": "images:debian/13/cloud",
  "limits": { "cpu": "2", "memory": "4GiB" },
  "rootSize": "20GiB",
  "firstBoot": { "file": "example-vm.sh" },
  "settings": { "greeting": "hello" },
  "secrets": ["API_TOKEN"]
}
```

| Field | What it is | How it reaches the machine |
|-------|------------|----------------------------|
| `firstBoot` | `{ "file": "<name>" }`: a plain file beside the template, at most 256KiB, that begins `#!` (a script) or `#cloud-config`. | As the instance's `cloud-init.user-data`. cloud-init runs a script once, as root, at the machine's first boot. The image must carry cloud-init: on the public images server that is the `cloud` variant, for example `images:debian/13/cloud`. |
| `settings` | One JSON object, at most 16KiB. It never holds a secret. | As the instance's `cloud-init.vendor-data`: cloud-init writes the object to `/etc/arca/settings.json` (readable by every user of the machine) before the script runs. |
| `secrets` | Up to 16 names of letters, digits and `_`. | Never through Incus's config. See below. |

The script and the settings are read when the machine is created and are fixed from then on, like everything else in a
virtual machine's Incus config. A template that is edited later applies to machines created after the edit.
`service_create` refuses a template whose script file is missing, naming the file it looked for.

**Secrets of a virtual machine.** A container's secret is an environment value in its Incus config. A virtual
machine's is never there. The steps:

1. `service_create` creates a machine whose template declares secrets **stopped**. This fails closed; it is not a
   promise that the machine never boots without them. A template with `autostart` is started by Incus itself when the
   host restarts, also before the first `instance_start`: the machine then boots with no secret file, and what runs in
   it has to wait for one (the runner pool registers nothing until its credential is there).
2. `service_secret_set` with `{"name", "key", "value"}` stores the value in Arca's private `secrets/` directory, as for
   a container, and answers `{"ok": true}`. A virtual machine's value is a file, so it is stored as given, with the
   line break at its end when it has one: send a key file whole, as it was read. It is 8 to 4096 characters, not
   counting one line break at its end, and at least 8 of them are not line breaks; it may have line breaks (a private
   key in PEM form is several lines) and no other control character. One thing is not byte for byte: a file with
   Windows line endings is stored with plain line breaks (each carriage return and line feed pair as one line feed).
   The call itself carries at most 4097 characters, and there every carriage return counts: a file with Windows line
   endings that is near the limit is refused as too long, and goes through once its line endings are plain.
   A container's value, and a function's, is an environment value: one line of 8 to 4096 characters, and a line break
   at its end is refused in words that say it is the one thing to change. Nothing is sent to the machine yet.
3. `instance_start` with `{"project": "arcak-svc", "instance": "<name>"}` starts the machine, waits for its guest agent
   (up to 120 seconds: a new machine boots first) and writes each stored secret into the guest as the file
   `/etc/arca-secrets/<KEY>`. The directory is root's with mode 0700 and each file is root's with mode 0600. Arca
   makes the directory once and leaves it alone afterwards; if something else is at that path (a directory others can
   enter, or a file), nothing is written and the answer says so. The answer names the secrets it wrote.

A new value: run `service_secret_set` again, then `instance_start` again. A start of a machine that is already running
restarts nothing and writes the files again, so a secret is replaced without an approval and without interrupting the
machine; the programs in the guest read the file when they next need it. `instance_restart` writes them too.

When the guest agent does not answer in time (the image has none, or the machine is still booting), the verb is refused
with `InstanceSecretsNotWritten`: the machine is running, its secrets are not in it, and running `instance_start`
again writes them.

Inside a tenant the steps are the same, with `tenant` in each call: `service_secret_set` with
`{"name", "key", "value", "tenant"}`, then `instance_start` with `{"tenant": "<id>", "service": "<name>"}`, which a
token whose tool allowlist names `instance_start` may call without an approval. The secrets are written into that
tenant's own machine and into no other: Arca addresses the machine it created for that service, from its own record,
and a call cannot name another tenant's project or the node's. A command is still never run inside a tenant's machine.

When secrets were not written, the answer says once what to do: run `instance_start` again when the machine was not
ready, or that running it again changes nothing until the cause it names is put right.

Where a virtual machine's secret is, and is not:

- It is in Arca's `secrets/` directory, and on the machine's own disk in `/etc/arca-secrets`. The second copy is why a
  machine that Incus starts by itself after the host restarts, or that restarts from inside, still works with nobody
  asked. Root inside the machine can read it, and a snapshot or a copy of the machine's disk carries it.
- It is not in the instance's config in Incus (so `incus config show` does not show it), in Arca's desired state, in
  the effect journal, in `instance-actions.jsonl` (which names the secrets written), in `exec-audit.jsonl`, or in any
  answer or refusal. `service_logs`, a failed command's words and a function's output replace a stored value with
  `[secret]`: whole, and for a value of several lines (a key file) each of its lines of 16 characters or more, since a
  log shows it one line at a time. A shorter line of such a value is not hidden by its value: a line of a few
  characters is ordinary text that a log holds in many places. A piece of 16 or more characters of a stored key's
  base64 body is hidden too, printed at another width or with each `/` written `\/`. A PEM block in those texts is
  replaced whole before the text is cut into lines, a stored secret or not, and whatever its label (`RSA PRIVATE KEY`,
  `OpenVPN Static key V1`): from its `-----BEGIN ...-----` line to its `-----END ...-----` line; when no END line
  follows, over the lines after the BEGIN line that look like a key (base64 of 16 characters or more, or a PEM header),
  so what follows the key stays shown; and when a text begins inside a key (an END line with no BEGIN before it), from
  its start to that END line. `service_logs` of a container reads the last 200 lines of its journal whatever it is
  asked for, and cuts to the lines asked for only after they are redacted. `function_runs` redacts a kept output and a
  run's line again when it reads them. Still shown: the last line of a key printed without its END line when it is
  shorter than 16 characters. Certificates, public keys and other public PEM blocks are hidden as `[secret]` too.

**Settings for one service.** A template may let a create replace some of its settings, so that two services of one
template differ without a second template file. It lists their names in `createSettings`, each a key of its
`settings`, whose value there is the default:

```json
"settings": { "url": "https://github.com/example-org", "maxSlots": 5, "pin": "registry.example/tool@sha256:..." },
"createSettings": ["url", "maxSlots"]
```

`service_create` then takes `settings`, an object with the values for this one service:
`{"name": "ci", "template": "example-vm", "settings": {"url": "https://github.com/another-org"}}`. The machine reads
the template's settings with those values in place of the defaults, at `/etc/arca/settings.json` as before. What is
refused, before anything is created, and in words that name the setting:

- a name the template does not list (here `pin`), or any `settings` for a template that lists none;
- a value of another shape than the template's default, at every level: text for a number, `2.5` for a whole number,
  a list for an object; in a list, an element unlike the default's first one; in an object, a key the default does
  not have. The refusal names the place, such as `settings.classes[2].memory`;
- a number that would not read back the same when the create is approved: `2.0` for a whole number, or a whole
  number of 2^53 or more; and a negative number where the default is not negative;
- a control character anywhere in a value, a character that reorders or hides text (a right-to-left override, a
  zero-width space), and a value nested more than eight deep;
- for a setting whose default is an `https://` address, anything but a plain `https://<host>/<path>` on the
  default's own host: no user or password (`@`), no query (`?`), no fragment (`#`), nothing percent-encoded, and no
  other host or port. A create changes the path only. Such a setting decides where the machine sends a credential,
  so a template for another host is a template of its own;
- for a setting whose default is a size such as `4GiB`, anything but a size above zero of at most six digits, and
  under a key named `memory` more than the machine's own memory (the template's `limits.memory`);
- settings that pass 16KiB once merged.

Settings are fixed at create, like the rest of a virtual machine's first boot: to change one, create the service
again. They never hold a secret, and Arca shows them: `service_status` answers `settings`, the object the machine
was created with, and when a create is proposed the owner's card has one row for each setting it replaces, with its
value in words, so an address is seen before it is approved. A list has one row for each element (`Setting classes
1`, `Setting classes 2`), each with its fields, so a long list is shown whole. The card cuts nothing: a proposal whose
setting would need a row of more than 160 characters is refused when it is made, naming the row, and nothing is asked
of the owner. The service's record keeps the replaced values as `vm.settings`.

**What must hold between settings.** A template may also say what must hold between its settings, in
`settingRules`, so that a create the machine itself would refuse at its first boot is refused before anything is
made, and its proposal before any card is shown:

```json
"settingRules": [
  { "need": "maxSlots", "atLeast": { "count": "classes" }, "why": "every class is kept one place under the cap" },
  { "need": 200, "atLeast": "percent", "why": "200 is the most" }
]
```

Each rule says one amount is at least another. An amount is a whole number, the name of a top-level setting whose
value is a whole number, or `{ "count": "<name>" }`, the number of entries of a top-level setting that is a list.
There is nothing else: a rule whose `need` is a number is a ceiling on the other amount. Arca compares the two
amounts and knows nothing of what they mean. The rules are asked of the settings one service would have (the
template's, with the create's values in place of the defaults), when the create is proposed and again when it
runs. The refusal has Arca's words about the two amounts and then the template's `why`: `settings:
settings.maxSlots is 2, below the number of entries in settings.classes (3): every class is kept one place under
the cap`. A template lists at most 16 rules, each with a `why` of 1 to 200 plain characters, and a rule names a
setting in 1 to 64 plain characters. A template is refused
when it is read, naming the rule (`settingRules[1]: ...`), when a rule names a setting it does not have or one of
the wrong type, when an amount has another form, or when the template's own defaults do not pass the rule. Only a
virtual-machine template has rules. They are asked of a create only: a service that exists is not checked again.
The machine's own script keeps its checks; the rules repeat only what it refuses. The two GitLab runner pool
templates state eight: `concurrent` and `jobCpu` from 1 to 999, `memoryOvercommitPercent` from 100 to 200, and 1
to 12 entries in `runners`.

**Which image a machine was made from.** A template may name its image by an alias (`images:debian/13/cloud`), which
does not say which build that was. `service_status` adds `baseImage`, the fingerprint (SHA-256) Incus recorded on the
instance when it created it, so the build can be looked up, and two machines compared, afterwards.

**Whether the installed template names another version.** A service keeps the version it was created with, and the
template installed on the node (`<stateDir>/templates`) may name another one later, for example after `arca templates
install --replace` from a newer release. `service_status` answers both under `update`:

```json
"update": {
  "pin": "ghcr.io/example/app@sha256:1111…",
  "templatePin": "ghcr.io/example/app@sha256:2222…",
  "state": "differs"
}
```

- `pin` is the version the service was created with, from its own record: a container's image and digest, or, for a
  virtual machine, the image by digest its settings hold under the key its template names in `update.pin`.
- `templatePin` is the one the installed template names now.
- `state` is `same`, `differs`, or `unknown` with the reason in `problem` (the template is not installed any more,
  cannot be used as it is, or is of another kind now).

`service_list` carries `"update": { "state": "differs" }` on a row whose template names another version, and no such
key on any other row. Both are read from this node's own records and files: no registry is asked and nothing is
written. Two digests carry no order, so `differs` never says which version is the newer one. A service made from a
system image by an alias has no version of this kind and carries no `update` (`baseImage`, above, is what it has),
and neither does an adopted instance. Today the only way to the template's version is `service_remove` and
`service_create`: this only tells you that there is one.

A virtual-machine template says where its version is with one key:

```json
"settings": { "serverImage": "ghcr.io/example/app@sha256:1111…" },
"update": { "pin": "serverImage" }
```

`update.pin` names a key of `settings` whose value is an image by digest (`<registry>/<name>@sha256:<64 hex
digits>`, in lower case) and which `createSettings` does not list: a create never chooses the version. A template
that breaks this is refused, and so is `update` on a container template, whose version is its `digest`. The shipped
`automagik-workspace` names `serverImage`.

### What every virtual machine carries

Whatever its network, a virtual machine Arca creates has two things more than its root disk and its NIC:

| In the create | Why |
|---------------|-----|
| the device `agent`: `{ "type": "disk", "source": "agent:config" }` | Incus 7.6 offers the guest its config share over 9p only. A QEMU built without virtio-9p (some distributions ship one) gives the guest no share, so the guest agent never starts, `service_status` never shows an address and nothing can be run in the guest. The drive carries the same share. A restricted project always admits it. |
| the NIC's `name`, `enp5s0`, with the instance key `agent.nic_config` set to `true` | The public images configure the interface `enp5s0` and no other. The name the guest's kernel picks follows the PCI slot QEMU gives the NIC, which differs between hosts (a QEMU without 9p has one device fewer, and the NIC came up as `enp4s0`), so the guest never asked for an address. With `agent.nic_config` the agent renames the NIC to the device's `name` when it starts. |

Nothing is changed on a virtual machine's Incus config after its create, so one made by an earlier Arca has neither. Add both by hand
if it needs them: `incus config device add <name> agent disk source=agent:config --project arcak-svc`, and for the NIC
`incus config device set <name> eth0 name=enp5s0 --project arcak-svc` with
`incus config set <name> agent.nic_config=true --project arcak-svc`, then restart it. The rename has been read in
Incus's source and tested against the fake Incus only; no live guest has confirmed it yet.

## Mixed nodes

A mixed node sets `kernel.serviceCidr` and `kernel.bridged` together. Containers, databases and consumers live on the
managed service network, exactly as on a managed node. A virtual machine lives on that network too, or on one of the
allowed host bridges:

```json
"kernel": {
  "projects": ["arcak-svc"],
  "droiddProjects": ["app1"],
  "serviceCidr": "192.0.2.0/24",
  "bridged": {
    "bridges": ["br0"],
    "pool": "vmpool",
    "limits": { "virtualMachines": 8, "cpu": 16, "memory": "64GiB", "disk": "1TiB" }
  }
}
```

`kernel.bridged` is optional there. A node with `kernel.serviceCidr` and `kernel.vmLimits` (the four caps, and nothing
else) also creates virtual machines, on the managed network only.

| `service_create` for a virtual-machine template | Where it goes |
|--------------------------------------------------|---------------|
| no `bridge` | The managed service network. It gets the next free address of the same plan as containers (`.10` to `.239`), a NIC on the managed bridge with that address reserved and MAC and IPv4 filtering on, and its own NIC ACL: everything in is rejected until a link opens a port, and out goes to the internet only. It takes no profile. Its root disk is on `kernel.bridged.pool` when the node sets `kernel.bridged`, and on the default profile's pool otherwise. |
| `bridge` | That host bridge, which must be in `kernel.bridged.bridges`. The virtual machine is exactly a bridged node's: no address from Arca, no ACL, no link. |

What the other verbs do with a virtual machine on the managed network:

| Call | Answer |
|------|--------|
| `service_connect` from a container to it | Allowed when its template declares `ports`: the link opens the first port in its ACL for that container, and gives the container the template's `provides` values. |
| `service_connect` from it | Refused. A link hands its consumer the connection values as container environment, which a virtual machine's programs never read. |
| `service_publish`, `service_unpublish` | Refused. A dash is published by a tailnet node inside an Arca-built container image, and no virtual-machine image carries one. |
| `service_secret_set` | Allowed for a secret its template declares: the value is stored, and the next `instance_start` writes it into the guest. Never an environment value. |
| `mac` on `service_create` | Refused. The address is reserved by Arca's plan; a fixed MAC is for a host bridge. |

**The caps.** One set of caps covers every virtual machine Arca holds, on either network: `kernel.bridged.limits`
when the node sets `kernel.bridged`, `kernel.vmLimits` otherwise. `service_create` checks them before any write, as on
a bridged node. Incus holds only the count on a mixed node (`limits.virtual-machines` on the project): its
`limits.cpu`, `limits.memory` and `limits.disk` count every instance of the project, containers included, so the
project does not carry them, and the CPU, memory and disk caps are Arca's check alone. A virtual machine made by hand
in `arcak-svc` is outside them. Containers have no node cap: each keeps the service profile's limits (2 CPUs, 2GiB, a
16GiB root disk) or its template's, and the address plan bounds containers, databases and managed virtual machines
together at 230.

**One project, widened in place.** A mixed node keeps the one project `arcak-svc`. The entry that created it owns it
and widens it, by a journaled project config write, the first time the node uses the other network:

| The node was | First use of | What changes on the project |
|--------------|--------------|-----------------------------|
| bridged (`bridged-arcak-svc` owns the project) | the managed network | `limits.containers`, `limits.cpu`, `limits.memory` and `limits.disk` are unset and `restricted.devices.proxy` becomes `allow`. The entry `net-arcak-svc` is added and creates the bridge `arcak-svcbr` and the profile `arca-service`. |
| managed (`net-arcak-svc` owns the project) | a virtual machine | `limits.virtual-machines` becomes the configured cap, and follows it at every later virtual-machine create. |
| managed | a virtual machine on a host bridge | `restricted.devices.nic` becomes `allow` and `restricted.networks.access` is unset. From then on the only guard on which bridge a NIC joins is Arca's own, as on a bridged node. |

**Turning a bridged node into a mixed one.** Add `kernel.serviceCidr` to the config, keep `kernel.bridged` as it is, and
restart `arca serve`. The start is not refused, and the running virtual machines and their records are not touched
then or later. The first `service_create` of a container (or `postgres_create`) widens
the project as the table says and creates the managed network. Removing `kernel.bridged` afterwards is refused while a
virtual machine is on a host bridge.

**Imports.** A mixed node may list `kernel.importServices` and import a service another manager created, as a
managed node does: it has the service network an import needs. A bridged node may not.

**A host that runs firewalld.** Arca creates the managed bridge through Incus and writes no host firewall rule. Where
firewalld is active, it drops the DHCP and DNS requests the guests send to the bridge until the bridge is in a zone
that accepts them, so a new service gets no address. This is the operator's step, once, after the first service on the
managed network has created the bridge:
`firewall-cmd --zone=trusted --change-interface=arcak-svcbr --permanent` and `firewall-cmd --reload`. Arca cannot do
it: it holds no rights over the host firewall.

## Publishing a service through a Cloudflare Tunnel

A Cloudflare Tunnel puts one of your services on the public internet under your own domain, for example
`https://app.example.com`, without opening a port on your router or installing anything on the machine itself. Arca runs
Cloudflare's connector (`cloudflared`) as one more service, from the shipped `cloudflare-tunnel` template. The connector
calls out to Cloudflare; nothing calls in. It can reach only the services you link it to.

You need a Cloudflare account with your domain already added to it, and a service in Arca that you want to publish (here
it is called `app`). The steps name each screen. The Arca steps are Arca's own verbs: ask the assistant you use with Arca
to run them. An assistant that may only propose a change (the canvas Doctor) has Arca show `service_create`,
`instance_start` and `service_connect` on the approval page, and each runs when you approve it there. Step 3 is the
exception: `service_secret_set` cannot be proposed, so it needs an assistant that holds a `doctor`-scope token.

1. **Cloudflare dashboard, "Networking" > "Tunnels".** Select "Create a tunnel", enter a name (for example `home`) and
   select "Create Tunnel". The next screen shows an installation command. Do not run it anywhere. Copy only the long
   text at its end, which begins with `eyJ`: that is the tunnel's token. Treat it like a password. Leave this screen open.
2. **Arca: `service_create`** with `{"name": "tunnel", "template": "cloudflare-tunnel"}`. The service is created and
   stays `stopped`, because it has no token yet.
3. **Arca: `service_secret_set`** with `{"name": "tunnel", "key": "TUNNEL_TOKEN", "value": "<the token>"}`. The key is
   exactly `TUNNEL_TOKEN`. This step cannot go through the approval page: the assistant that runs it must hold a
   `doctor`-scope token, and you give it the tunnel token, so the token passes through that assistant's conversation.
   Because the verb is never a proposal, the token is never written to Arca's record of proposals
   (`action-proposals.jsonl`). Arca keeps the token in a private file and answers `{"ok": true}`; no answer, list or log
   shows it again.
4. **Arca: `instance_start`** with `{"project": "arcak-svc", "instance": "tunnel"}`. The connector starts and calls
   Cloudflare. Back on the Cloudflare screen from step 1 the tunnel connects within about a minute; select "Continue".
5. **Arca: `service_connect`** with `{"from": "tunnel", "to": "app"}`. This is what lets the tunnel reach `app`, on
   `app`'s port and nothing else.
6. **Arca: `service_status`** with `{"name": "app"}`. Note its `ipv4` and its first `ports` entry, for example `192.0.2.11`
   and `8080`. This is the address Cloudflare will send visitors to.
7. **Cloudflare dashboard, "Networking" > "Tunnels", your tunnel, the "Routes" tab.** Select "Add route", then "Published
   application". Enter the subdomain (for example `app`), choose your domain (`example.com`) in "Domain", and in "Service
   URL" enter the address from step 6, for example `http://192.0.2.11:8080`. Select "Add route".
8. **Check.** The "Tunnels" page lists your tunnel as `Healthy`, and `https://app.example.com` opens your service.

To publish a second service through the same tunnel, repeat steps 5 to 7 for it. To stop publishing a service, delete its
route in the Cloudflare dashboard and run `service_disconnect` with `{"from": "tunnel", "to": "app"}`. To take the tunnel
away altogether, run `service_remove` for `tunnel` and delete the tunnel in the Cloudflare dashboard, which also makes its
token worthless.

What to know:

- **Where to look when it does not connect.** `service_status` shows whether the connector is `running`; it does not show
  whether Cloudflare accepted it. The Cloudflare "Tunnels" page does (`Healthy`, or `Inactive`, `Down`, `Degraded`), and
  `service_logs` with `{"name": "tunnel"}` shows the connector's own lines: `Registered tunnel connection` four times
  when all is well, or the reason when the token was refused.
- **A new token.** If you replace the token (`service_secret_set` again), the connector reads it at its next start:
  `instance_restart` for `tunnel`, which you approve.
- **After a restart of the machine** the tunnel starts by itself (the template sets `autostart`).
- **Only linked services.** Every service refuses connections it has no rule for. A route in Cloudflare that points at a
  service the tunnel is not linked to shows an error page to visitors and reaches nothing. The tunnel has no port, so no
  service can connect to it either.
- **What the tunnel needs from your network.** Outgoing connections to Cloudflare on port 7844 (TCP and UDP), which
  Arca's service network allows like any other internet traffic. No incoming port is opened.
- **Where the token lives.** In a private file in Arca's `secrets/` directory, and in the connector container's
  environment in Incus (`environment.TUNNEL_TOKEN`), which is how `cloudflared` reads it, as for every template secret.
  It is not in the template, in Arca's desired state or journal, or on the connector's command line, and `service_logs`
  replaces it with `[secret]`. Someone who can read the instance's config in Incus on that machine can read it.
- **The image.** The template pins `docker.io/cloudflare/cloudflared` release 2026.10.7 by digest, so it never updates
  by itself. A later Arca release moves the pin.

## An Automagik workspace in its own virtual machine

The shipped `automagik-workspace` template is one virtual machine that holds one Automagik workspace and publishes it
on the internet under your own domain, for example `https://workspace.example.com`. Inside it a small keeper runs
three containers, in the shape of Automagik's own `server/compose.gate.yml`:

| Container | What it is | Where it listens |
|-----------|------------|------------------|
| `automagik-server` | The Automagik server image. Its state is in `/data`. | Its own loopback, port 3081. It has a network namespace of its own and no published port. |
| `automagik-gate` | The same image by the same digest, started with the gate's entrypoint. The only door to the server: it signs the owner in, and it holds the server's own sign-in credential, which never leaves the machine. | The same loopback, port 3082: it joins the server's network namespace. |
| `automagik-tunnel` (or `automagik-tailnet`, see "On a tailnet instead") | Cloudflare's connector (`cloudflared`). It calls out to Cloudflare; nothing calls in. | Nothing but its metrics, put on that loopback too. It joins the same namespace and hands visitors to the gate's port. |

The three share one loopback that belongs to none of the machine's own addresses. Nothing listens on the machine's
address, on the service network or on the host: the template has no port, no link and no host listener. The virtual
machine is the wall around the workspace: its agents run commands, and they run them there.

This is not the `automagik-server` template with a tunnel linked to it. That server refuses every request whose
`Host` is not its own loopback or a name it was told to trust, and it binds loopback only, so a connector in another
service cannot use it. Here the connector shares the loopback of the gate and the server.

You need a Cloudflare account with your domain already added to it, a node that may create a virtual machine of the
template's size (2 CPUs, 4GiB of memory, a 20GiB disk: the node's `kernel.vmLimits`, or `kernel.bridged.limits`, must
allow it), and the shipped templates installed (`arca templates install`). The Arca steps are Arca's own verbs: ask
the assistant you use with Arca to run them. `service_create` and `instance_start` can be shown on the approval page;
`service_secret_set` cannot be proposed, so it needs an assistant that holds a `doctor`-scope token, and the tunnel's
token passes through that assistant's conversation.

1. **Cloudflare dashboard, "Networking" > "Tunnels".** Select "Create a tunnel", enter a name (for example
   `workspace`) and select "Create Tunnel". The next screen shows an installation command. Do not run it anywhere.
   Copy only the long text at its end, which begins with `eyJ`: that is the tunnel's token. Treat it like a password.
   Leave this screen open. Use a tunnel of its own for each workspace: whoever holds a tunnel's token can serve that
   tunnel's routes.
2. **Cloudflare dashboard, "Zero Trust" > "Access" > "Applications": who may open the address.** Do this before the
   address leads anywhere (step 6), so that it is never open to a stranger. Select "Add an application", then
   "Self-hosted". Give it a name, and as its public host name enter the address the workspace will have
   (`workspace.example.com`). Add a policy with the action "Allow" and one rule, "Emails", that lists your own
   address (`owner@example.com`) and nobody else's. Save it. Note two things, neither of them a secret: your team
   domain (`<your team>.cloudflareaccess.com`, in the Zero Trust settings) and the application's "Application
   Audience (AUD) Tag" on its overview. From now on Cloudflare asks every visitor of that address to prove one of the
   listed e-mail addresses before anything reaches the workspace.
3. **Arca: `service_create`**, with the workspace's own settings in the call:
   `{"name": "workspace1", "template": "automagik-workspace", "settings": {"publicUrl": "https://workspace.example.com", "accessTeamDomain": "<your team>.cloudflareaccess.com", "accessAudience": "<the audience tag>", "accessEmails": ["owner@example.com"]}}`.
   `publicUrl` is the address the workspace will have, with no closing slash; the three Access settings are the two
   values noted in step 2 and the same addresses as the policy. No copy of the template is made: these settings are
   given at create, and `service_status` shows them afterwards. The machine is created and stays `stopped`, because
   it has no token yet. Leave `bridge` out, so that it joins Arca's own service network.
4. **Arca: `service_secret_set`** with `{"name": "workspace1", "key": "TUNNEL_TOKEN", "value": "<the token>"}`. The
   key is exactly `TUNNEL_TOKEN`. Arca answers `{"ok": true}`; no answer, list or log shows the token again.
5. **Arca: `instance_start`** with `{"project": "arcak-svc", "instance": "workspace1"}`. The machine starts and the
   answer names the secret written into it. Its first boot installs Docker and downloads the two images, which takes
   a few minutes. Back on the Cloudflare screen from step 1 the tunnel connects; select "Continue".
6. **Cloudflare dashboard, "Networking" > "Tunnels", your tunnel, the "Routes" tab.** Only now, with the Access
   application of step 2 in place. Select "Add route", then "Published application". Enter the subdomain
   (`workspace`), choose your domain (`example.com`) in "Domain", and in "Service URL" enter `http://127.0.0.1:3082`:
   the gate's port, not the server's 3081. That address is the loopback the connector shares with the gate. Select
   "Add route". Leave the route's "HTTP Host Header" setting empty, here and always: a Host override to a
   loopback name would make the server accept requests it otherwise refuses, and a route to its port would then
   pass the gate by. A route that names 3081 by mistake, without such an override, does not give anyone the
   workspace: the server is never told its public name, so it refuses the requests and live connections its
   interface makes (HTTP 403), its page asks for the server's own sign-in (HTTP 401), and only its static files
   load. A page that never comes to life is that mistake: correct the port in the route.
7. **Check.** `service_logs` with `{"name": "workspace1"}` shows the keeper's status line:
   `arca-automagik-workspace: status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082); the tunnel is running and has registered with Cloudflare; public address https://workspace.example.com, sign-in by Cloudflare Access (team example.cloudflareaccess.com, 1 e-mail address(es))`.
   The Cloudflare "Tunnels" page lists the tunnel as `Healthy`. `https://workspace.example.com` shows Cloudflare's
   own sign-in for your e-mail address first, and then the workspace.

The settings. A `service_create` may give `reach`, `publicUrl` and the three Access settings (the template lists
them in `createSettings`); `serverImage` is the template's own, since the image a workspace runs is not for a
create to choose.

| Setting | Meaning |
|---------|---------|
| `publicUrl` | With `reach` `tailnet`, the machine's own `https://<machine>.<tailnet>.ts.net`. Otherwise the workspace's exact public origin, as a browser writes it, and a name, never an IP address: `https://` and a host name in lower case; no path and no closing slash; a port only when it is not 443 (1 to 65535, without a leading 0). The gate gets it as `AUTOMAGIK_GATE_ORIGIN` and stops on any other spelling, so the script refuses those first. The server is not given it. It ships empty, which means "not set": the server then starts alone, no gate is started, and every status line says so. |
| `accessTeamDomain`, `accessAudience`, `accessEmails` | Your Cloudflare Access application for that address: the team's host name alone (`<team>.cloudflareaccess.com`, no `https://`, and no other domain), the application's audience tag, and the list of e-mail addresses that may sign in (1 to 16, one address in each entry). None is a secret. Set all three or none; they need `publicUrl`. They ship empty. The gate gets them as `AUTOMAGIK_GATE_ACCESS_TEAM_DOMAIN`, `AUTOMAGIK_GATE_ACCESS_AUD` and `AUTOMAGIK_GATE_ACCESS_EMAILS` (the addresses joined by commas). |
| `serverImage` | The Automagik server's public image, pinned by digest. The server and the gate both run from this one value: to move to another build, replace it. A workspace needs a build that has the gate and its Cloudflare Access sign-in (see below). |

| `reach` | How people reach the workspace: `cloudflare` (the steps above; also when the setting is left out) or `tailnet` (see "On a tailnet instead" below). |

Three things are not settings. The gate's port is 3082, as Automagik's compose file fixes it. The connector's image
is fixed in the script at the digest the `cloudflare-tunnel` template pins, and the tailnet node's at a digest of
Tailscale's own image (`docker.io/tailscale/tailscale`, release 1.104.4).

### On a tailnet instead

With `settings.reach` set to `tailnet` the workspace is reachable from your own tailnet and from nowhere else: no
Cloudflare Tunnel, no Cloudflare Access. Two containers stand in the connector's place:

| Container | What it is | Where it lives |
|-----------|------------|----------------|
| `automagik-tailnet` | A Tailscale node (userspace networking: no device, no capability). It serves HTTPS on port 443 of the machine's own tailnet name, `https://<machine>.<tailnet>.ts.net`, never Funnel, and hands every request to the relay with the visitor's `Host` header as it came. | A network namespace of its own. |
| `automagik-relay` | The server image's `node`, copying bytes between one address and the gate's `127.0.0.1:3082`. | The server's namespace; it listens on the server's address on a private Docker network (`automagik-relay`, internal) that only the server and the node join. |

The node is kept out of the server's namespace on purpose. A Tailscale node in userspace networking passes every
connection that reaches its tailnet address, on any port it does not serve itself, to `127.0.0.1` on that port.
Inside the server's namespace that would open the workspace's whole loopback to the tailnet: the server's own port
past the gate, the gate in plain http, and any port a program in the workspace opens for itself. In a namespace of
its own the node's loopback holds nothing, and the relay is its one way to the workspace, to the gate's port alone.
The keeper removes a node it finds anywhere else and never says that such a node serves. The gate signs its owner
in itself, with passkeys.

You need a tailnet with MagicDNS and HTTPS certificates turned on (Tailscale's admin console, "DNS"), since the gate
takes nothing but an exact `https://` address. A certificate's name is published in the public certificate logs, so
choose a machine name you do not mind the world reading.

1. **Tailscale admin console, "Settings" > "Keys".** Generate an auth key: for one use, not ephemeral, pre-approved
   if your tailnet asks for device approval, and with a tag, so that the node's own key does not expire. Copy it;
   treat it like a password. Decide the machine's name (for example `workspace1`) and note your tailnet's name:
   together they are the address, `https://workspace1.<tailnet>.ts.net`.
2. **Arca: `service_create`** with
   `{"name": "workspace1", "template": "automagik-workspace", "settings": {"reach": "tailnet", "publicUrl": "https://workspace1.<tailnet>.ts.net"}}`:
   the reach, and that address with no port and no closing slash. Give none of the three Access settings; with
   `tailnet` they are refused. It stays `stopped`.
3. **Arca: `service_secret_set`** with `{"name": "workspace1", "key": "TAILSCALE_AUTHKEY", "value": "<the key>"}`.
   The key is exactly `TAILSCALE_AUTHKEY`; `TUNNEL_TOKEN` is not used by this reach.
4. **Arca: `instance_start`** with `{"project": "arcak-svc", "instance": "workspace1"}`.
5. **Check.** `service_logs` with `{"name": "workspace1"}` shows the keeper's status line:
   `arca-automagik-workspace: status: the server is healthy (HTTP 401 on its loopback port 3081); the gate is healthy (HTTP 401 on its loopback port 3082); the tailnet node is logged in and serves https://workspace1.<tailnet>.ts.net; reach tailnet: https://workspace1.<tailnet>.ts.net, sign-in by passkey, waiting for its first claim`.
6. **The first claim.** A new gate has no owner. It writes a claim code into its own data, good once and for
   fifteen minutes, and the first person who opens `https://workspace1.<tailnet>.ts.net/_gate/claim` from the
   tailnet with that code registers a passkey and becomes the owner. Arca does not hand that code to you yet: the
   step that shows it once, after your passkey on Arca's approval page, is not built. Until it is, the code can
   only be read on the machine itself, with the command Automagik's server documentation gives for the gate's
   container. That is a step for the person who looks after the node, at a terminal, and never one for an
   assistant: the keeper does not read the code, does not print it, and no Arca verb returns it. After the claim
   the status line says `claimed`.

What to know about this reach:

- **What the status line can say of the node.** `logged in and serves https://<name>` when all is well.
  `NOT logged in` when the tailnet did not take the key (used already, expired or revoked), or when the node waits
  for approval in the admin console. `logged in as <another name>, which is NOT the name in settings.publicUrl`
  when the name was already taken on the tailnet and Tailscale gave the node another: the gate refuses every
  request under a name that is not its own, so free the name or create the workspace again.
- **Claimed or not** is read from one thing: the claim code's file is in the gate's data until the first claim and
  gone after it. The keeper looks at whether the file is there and how old it is, from the machine's side of the
  gate's data, and never at what is in it. A code past its fifteen minutes is said too; a new one is made by
  Automagik's `gate-control.mjs renew-claim` inside the gate's container, or by a restart of an unclaimed gate.
- **The first claim is protected by the code alone.** Everyone who can reach the node's port 443 on your tailnet
  can open the claim page; only the code lets them claim. Claim soon after the start.
- **Your tailnet's access rules are a second lock, not the first.** Limit who reaches the node's tag, and to port
  443, in your tailnet policy. The workspace does not depend on it: what a tailnet device can reach on the node is
  decided by the node having a namespace of its own.
- **A key the tailnet does not take** (used already, expired or revoked) makes the node stop soon after its start.
  The status then says that the node stopped, with its exit code and this likeliest cause; a node that stays up
  without a login says `NOT logged in`. Either way: `service_secret_set` with a new key, then `instance_start`.
- **The auth key** is handled like the tunnel's token: Arca's private `secrets/` directory, the machine's
  `/etc/arca-secrets/TAILSCALE_AUTHKEY` (root's, mode 0600), and a copy in the machine's memory
  (`/run/automagik-workspace/tailnet/authkey`, mode 0400) that the node is pointed at by path. It is in no
  environment value, on no command line and in no output; the keeper keeps only its digest. The node uses the key
  once: afterwards its identity is in its state.
- **The node's state** is the machine's `/var/lib/automagik-workspace/tailnet`. It is the node's identity on your
  tailnet and survives restarts of the container and of the machine. `service_remove` deletes it with the machine;
  remove the node in Tailscale's admin console as well. A snapshot carries the identity: do not run a restored
  copy beside the original.
- **A new key** (`service_secret_set`, then `instance_start`) starts the node again; a node that is already
  logged in keeps its identity and does not need the new key.

What to know:

- **The pinned server image.** `settings.serverImage` is the build of the Automagik server from the Automagik
  repository's commit `5fe0be6`. It has the gate, its Cloudflare Access sign-in, and the gate's own sign-in with
  passkeys, which the tailnet reach uses; both reaches run it. Nothing of the Access sign-in changed between the
  build pinned before (`8fb2e6f`) and this one. An older build has no gate: its gate container stops at once, the keeper says `the gate stopped (exit code ...)` and
  tries again after a pause, and the public address leads nowhere. Everything this script takes from the image
  (three paths, the two ports, the claim code's place and the seven variable names) is in one block at the top of
  `automagik-workspace.sh`, as that commit has them; check the block when you move the pin to a later build.
- **The first sign-in.** The server signs a browser in with a link that it writes once into its own data. Arca
  never reads that link, the keeper never reads the server's or the gate's data, and the keeper never copies their
  output to the console, so the link is in no `service_logs` answer and reaches no assistant. The way in is
  Cloudflare Access: the gate checks the proof Cloudflare attaches to each request of a signed-in visitor (that it
  is signed by your team, is for this application, has not expired and names one of the listed addresses), keeps
  the server's link to itself and signs that visitor in. No link leaves the machine. The sign-in happens when a
  page is opened; the workspace's live connections then ride on the gate's own session. Without the three Access
  settings the gate is still the only door, and nobody can sign in through the tunnel: the gate then waits for a
  first claim on a port of its own (3083). A route in Cloudflare must never name port 3083. So that none can, the
  keeper does not start the connector at all beside a gate that has no Access settings, and the status line says
  why.
- **Access is the lock on the address.** Keep the Access policy to the addresses in `accessEmails`, and add the
  route only after it. The gate refuses a visitor Cloudflare did not sign in; the policy keeps strangers from
  reaching the gate at all. To check a proof the gate fetches your team's public keys from
  `https://<team>.cloudflareaccess.com`, an outgoing call the service network allows like any other.
- **All three Access settings, or none.** The gate does not start with one or two. The first boot checks the
  settings and says on the console what is wrong, and the keeper repeats it; nothing is started with settings that
  cannot be used. Settings are fixed when the machine is created, so the cure is a corrected template and a new
  machine.
- **Where the token lives.** In Arca's private `secrets/` directory, in the machine's `/etc/arca-secrets/TUNNEL_TOKEN`
  (root's, mode 0600), and in a copy in the machine's memory (`/run/automagik-workspace/tunnel/token`) that only the
  connector's user reads. The connector reads that file; the token is not in its environment, on any command line or
  in Docker's record of the container, and the keeper prints nothing of it.
- **A new token.** `service_secret_set` again, then `instance_start` again. The keeper sees the new file and starts
  the connector again with it; the server and the gate keep running.
- **Where to look when it does not come up.** `service_logs` first. The first boot says there what it installs and,
  when it fails, which command is missing or which service did not start. The keeper says each change: a part
  started, healthy, not answering yet, or answering something other than 401; the tunnel waiting for its token,
  running, registered with Cloudflare, or stopped with its exit code. Health is the image's own check, run inside
  the server's and the gate's containers: HTTP 401 on the loopback port, the answer to a visitor who has not signed
  in. A part that stops is started again after a pause that doubles up to five minutes. A tunnel that keeps
  stopping usually has a token Cloudflare does not accept. When the server's container is replaced, the gate and
  the connector are started again beside it, since they live in its network namespace. When Docker itself does not
  answer, the keeper says so and touches nothing: a container is only removed or started once Docker has said that
  it stopped or is not there. A keeper that cannot start at all (a missing command, settings that cannot be used)
  stays up and repeats the reason every five minutes.
- **The data.** The server's `/data` is the machine's `/var/lib/automagik-workspace/data` and the gate's is
  `/var/lib/automagik-workspace/gate`, both on the machine's own disk and owned by uid 1000, the image's user. The
  gate reads the server's data read-only; the server never sees the gate's. Both live and die with the machine:
  `service_remove` deletes them. Take a snapshot (`snapshot_create`) before a change you may want to undo. A
  snapshot carries the token's file too.
- **No Docker Compose.** The keeper starts the three containers with `docker run`, with the same image, entrypoint,
  environment, volumes, shared network namespace and restrictions (read-only file system, no capability, no new
  privileges) that Automagik's compose file gives the gate, and gives the server no environment, as that file
  does. The connector gets the same three restrictions. The first boot installs `docker.io` and `docker-cli`
  (on Debian 13 the `docker` command is a package of its own) and no compose package.
- **No model key.** The template declares no model secret. Sign-ins to model providers happen inside the workspace.
- **After a restart of the node** the machine starts by itself (the template sets `autostart`), and the keeper starts
  the containers again from the token on its disk.
- **The settings are fixed.** They are read when the machine is created. To change the reach, the public address
  or the Access settings, create the workspace again with other settings. To move the server's image pin, edit
  `serverImage` in the template: workspaces created afterwards run the new build.

## A pool of GitHub Actions runners

The shipped `github-runner-pool` template is one virtual machine that runs your GitHub Actions jobs on your own
hardware. Inside it a small controller keeps a set of slots. Each slot registers one runner with GitHub, runs exactly
one job on it in a fresh container, removes it, and starts over with a new runner. The virtual machine is the wall
between your jobs and the machine Arca runs on: nothing outside the pool changes when a job runs.

Read "Who can reach what" below before you create one. In short: every job that runs in the pool can get at the pool's
GitHub credential, so the pool is only as closed as the list of repositories allowed to send it jobs.

You need a node that may create a virtual machine of the template's size (16 CPUs, 32GiB of memory, a 200GiB disk: the
node's `kernel.vmLimits`, or `kernel.bridged.limits`, must allow it), the shipped templates installed
(`arca templates install`), and a GitHub organisation that you administer. The Arca steps are Arca's own verbs: ask
the assistant you use with Arca to run them. `service_create` and `instance_start` can be shown on the approval page;
`service_secret_set` cannot be proposed, so it needs an assistant that holds a `doctor`-scope token, and the
credential passes through that assistant's conversation.

**1. A runner group, on GitHub.** A runner group is GitHub's own list of the repositories that may use a set of
runners. It is the control that decides who can send jobs to the pool; a label is not one, because any workflow in an
allowed repository can ask for any label.

- Open the organisation's "Settings", then "Actions", then "Runner groups", and select "New runner group". Give it a
  name (for example `arca-pool`). For repository access choose selected repositories, and select only the
  repositories whose workflows you trust to run here. Leave public repositories off, which is how GitHub starts a
  group. Select "Create group". (GitHub offers extra runner groups from its Team plan up.)
- Note the group's number: the pool needs it as `runnerGroupId`. It is the number at the end of the group page's
  address in your browser. If you cannot find it, the pool tells you: a pool created with the wrong number does not
  start, and its log (`service_logs`) lists the organisation's runner groups with their numbers.
- Keep GitHub's approval for outside contributions on: the organisation's "Settings", "Actions", "General",
  "Approval for running fork pull request workflows from contributors", set to "Require approval for all external
  contributors". For private repositories leave "Run workflows from fork pull requests" off. An approval is a decision
  to run that person's code inside your pool. GitHub runs `pull_request_target` workflows whatever this setting says.

The pool refuses to start on the organisation's default runner group, which the shipped template names
(`runnerGroupId: 1`): every repository that group admits could send its jobs to the pool. The refusal, in
`service_logs`, lists the groups to choose from. A settings key, `allowDefaultRunnerGroup: true`, starts it there
anyway, and every status then carries a warning. A pool for one repository has no runner group: that repository's
workflows reach it, and so do the pull requests to it that are approved to run. The pool reads whether the repository
is public and warns in every status when it is.

**2. A credential, on GitHub.** The pool registers its runners with it. Know what each kind can do in the wrong
hands before you choose:

| Credential | The permission it needs | What whoever holds it can do |
|------------|-------------------------|------------------------------|
| A GitHub App on the organisation (recommended) | Organization permission "Self-hosted runners": "Read and write", and nothing else | Everything GitHub puts under that permission: register runners with any labels in any runner group, create, change and delete runner groups (one open to every repository included), and remove runners. So the holder can make a runner that receives the jobs of other repositories of the organisation, together with the secrets those jobs are given. It cannot read code or change settings by itself. |
| A fine-grained personal access token on the organisation | The same organisation permission | The same, as you: the token is tied to your account, expires on the date GitHub shows (the pool stops registering runners that day), and carries whatever else was ticked when it was made. |
| A pool for one repository, with either kind | Repository permission "Administration": "Read and write" | Far more than runners. GitHub has no narrower permission for a repository's runners, and this one also covers changing the repository's settings, adding and removing collaborators, changing and removing branch protection, adding deploy keys, and deleting the repository. |

The narrowest is the first: an App with the one organisation permission, with the pool in a runner group of selected
repositories. Use it. A repository-level pool looks smaller and is not: its credential can take the repository over.

To make the App: select your profile picture, then "Your organizations", then "Settings" beside the organisation, then
"Developer settings", "GitHub Apps" and "New GitHub App". Give it a name (for example `example-runner-pool`) and any
homepage URL. Under "Webhook", deselect "Active". Under "Permissions", open "Organization permissions" and set
"Self-hosted runners" to "Read and write"; set nothing else. Create the app. On the app's page, copy its "Client ID"
(it begins `Iv`; the numeric App ID is not what the pool takes). In the private keys part of the page, create a key
(GitHub's guide calls the button "New key"). A file in PEM form is downloaded: that is the private key. Treat it like
a password. Then select "Install App" and install it on the organisation.

For a token instead: your own "Settings", "Developer settings", "Personal access tokens", "Fine-grained tokens",
"Generate new token"; resource owner the organisation; organisation permission "Self-hosted runners", "Read and
write".

**3. Arca: `service_create`**, with the settings of this pool:

```json
{ "name": "ci-pool", "template": "github-runner-pool",
  "settings": { "url": "https://github.com/example-org", "runnerGroupId": 7 } }
```

`url` is your organisation and `runnerGroupId` the number from step 1. The pool is created and stays `stopped`,
because it has no credential yet. Leave `bridge` out, so that it joins Arca's own service network (see "The network"
below). When the create is proposed for your approval, the card shows each setting with its value: check the address,
since it is where the pool's credential will be used.

| Setting | At create | Meaning |
|---------|-----------|---------|
| `url` | yes | The organisation the runners belong to, or one repository (`https://github.com/example-org/example-repo`; read step 2 first). A plain `https` address on the template's own host, `github.com`: a create changes the path only, and Arca refuses another host, a port, a user name, a query or a fragment, as the pool does. |
| `runnerGroupId` | yes | The number of the runner group the runners join (step 1). Not used for a repository. |
| `classes` | yes | The label sets, each with its slot size: `name` (a-z, 0-9), `labels` (what a job's `runs-on` asks for), `cpu`, `memory` and `slots` (how many runners of this class the pool keeps). Given at create, it replaces the whole list. Each class is checked like the template's first: whole numbers for `cpu` and `slots`, and a `memory` above zero and at most the machine's. The card shows one row a class. |
| `maxSlots` | yes | The cap: never more than this many slots at once, whatever the classes add up to. Never below the number of classes: a pool created with less does not start ("A place for every class" below). |
| `allowDefaultRunnerGroup` | no | `true` lets the pool start on the organisation's default runner group. Leave it out. |
| `reserveMemory` | no | Memory kept back for the virtual machine itself. The slots share what is left. |
| `dockerCache`, `dependencyCache` | no | The disk the Docker layer caches and the dependency caches may use, over all slots. |
| `runnerImage`, `dockerImage` | no | The runner's and the Docker daemon's public images, pinned by digest. |
| `enterprise`, `apiUrl` | no | For GitHub Enterprise only. The credential is sent to the API's host, so that host is never guessed: for `github.com` it is `https://api.github.com` and no setting changes it. A `url` on another host is refused unless `enterprise` is `true`; the API is then `https://<host>/api/v3`, or `apiUrl`, which must begin `https://` and be on that host or on `api.<host>`. |

A setting marked "no" is the template's own: to change one, whoever looks after the node copies
`<stateDir>/templates/github-runner-pool.json` to a new name, sets `"name"` to it and edits the value, and the pool is
created from that template. The virtual machine's own size is the template's `limits` and `rootSize` and changes the
same way.

The shipped numbers fit together: two `standard` slots of 2 CPUs and 4GiB and two `large` slots of 6 CPUs and 9GiB are
16 CPUs and 26GiB, and 4GiB is kept back. A virtual machine sees less memory than its size: a 32GiB machine has
27921MiB for slots after the 4GiB kept back, the four slots take 26624MiB of it, and a slot that does not fit whole
is never started (two slots of 10GiB beside the two small ones would not fit). Give a class the memory its heaviest
job needs: a full `cargo test` of a large workspace has been seen to fill a 10GiB slot. Settings are read when the pool is created and are fixed from then on (see "What to
know").

**Sizing so that every slot fits.** A slot is started only while its whole memory fits, so the rule is:

    the memory of all slots together  <=  the memory the machine reports  -  reserveMemory

The machine reports less memory than the size in `limits`: its kernel keeps some for itself. The first pool's machine
of 32GiB (32768 MiB) reported 32017 MiB, which is 751 MiB, or 2.3%, less. So keep the slots and `reserveMemory`
together under about 97% of the machine's size: leave 1GiB free for every 32GiB. Sizes that add up to exactly the
machine's size do not fit. That first pool had two slots of 4GiB, two of 10GiB and 4GiB kept back, 32GiB in all: it
had 27921 MiB for slots, they asked for 28672 MiB, and the fourth slot stayed `queued` beside three idle ones. The
controller now says this at its start and in every status block, as a `WARNING` with the numbers and how much is too
much. (It does not say this when `maxSlots` is below the number of slots: then they are not meant to run all at
once.) A slot that is larger by itself than the memory for slots never starts, whatever `maxSlots` is, and the jobs
that ask for its labels wait for ever: the controller says that too, as a `WARNING` that names the class.

**A place for every class.** `maxSlots` caps registered runners, and a runner that is registered and waits for a job
keeps its place until a job ends. So that no class is left without a runner behind the idle runners of the others,
the pool keeps every class one place under the cap:

- `maxSlots` is never below the number of classes. A pool created with less does not start, and its console says
  both numbers and what to set: `settings: maxSlots is 2 and classes names 3 classes. ... Set maxSlots to 3 or more,
  or name fewer classes`.
- A free place goes to the class that has the fewest runners, so every class has one runner before any has two.
  Among classes that have equally many, and among the slots of one class, the places go round-robin.
- When a class has no runner and every place is held, the pool retires one runner that waits for a job and the class
  gets its place. The runner is one of the class that holds the most places, among the classes that hold two or more.
  The pool first asks GitHub to take the runner off, and removes it only on GitHub's yes. What GitHub was seen to
  answer, for an organisation's runners:

  - A refusal for a runner that is running a job (status 422, "is currently running a job and cannot be deleted").
    So a job that reached the runner in that moment is kept and runs to its end.
  - Yes (204) for every other runner, also for one it no longer has or never had. A just-in-time runner that took a
    job and ended it in that moment is one it no longer has. So after a yes the pool reads the runner's own output:
    a runner that has taken a job is left to end, its job is recorded as every job is, and it is not called retired.

  Every other answer retires nothing, a 404 as well: GitHub does not answer 404 for a runner that is gone, so a 404
  says the request did not reach the runner, and GitHub may still hold it and hand it a job.

What this is not:

- It is not a cap on running jobs, and it promises no job a start time. A just-in-time runner cannot be told to wait:
  every registered runner may take a job at any moment. A runner that has a job is never retired, and neither is a
  class's only runner, nor one that is updating itself. So when every runner that could give its place up is at
  work, the class waits until a job ends.
- A place is not memory. A slot still starts only when its whole memory fits (above), and the pool retires nothing
  for a class whose slot would still lack its memory.
- It is one place a class, not a share. Beyond that one place, the busiest classes can hold all the others.

A pool with one class has nothing to hand out between classes and works as it did.

**4. Arca: `service_secret_set`**, for the App twice:
`{"name": "ci-pool", "key": "GITHUB_APP_CLIENT_ID", "value": "<the client ID>"}` and
`{"name": "ci-pool", "key": "GITHUB_APP_PRIVATE_KEY", "value": "<the whole content of the key file>"}`. The key keeps
its line breaks, the one at its end too; a key pasted on one line works too. For a token, once:
`{"name": "ci-pool", "key": "GITHUB_TOKEN", "value": "<the token>"}`. Arca answers `{"ok": true}` and no answer, list
or log shows a value again. When the App's two secrets and a token are all set, the pool uses the App and says so.

**5. Arca: `instance_start`** with `{"project": "arcak-svc", "instance": "ci-pool"}`. The virtual machine starts and
the answer names the secrets written into it. Its first boot installs Docker and downloads the two images, which
takes a few minutes.

**6. Check.** `service_logs` with `{"name": "ci-pool"}` shows the pool's own lines (below), among them the runner
group it registers in. On GitHub, the organisation's "Settings", "Actions", "Runners" lists runners named
`ci-pool-<the pool's id>-standard-0-...` as idle.

The first boot, before there is a controller, writes its own lines to the machine's console, which is where
`service_logs` reads the controller's lines from. `arca-runner-pool: first boot: installing ...` is its first line and
`first boot done: the controller is installed and started` its last. A first boot that could not finish ends with
`first boot failed:` and the reason: the commands that are still missing after the install, by name, or that Docker
or the controller's service did not start. These first-boot lines have not yet been seen in `service_logs` on a real
pool, so do not read their absence as "still starting". If a pool that is `running` shows no line of the pool after a
few minutes, look inside the machine: `/var/log/cloud-init-output.log` holds everything the first boot printed, its
failure line included, and what the package manager itself said, which is never on the console. A machine whose first
boot failed has no pool; remove it with `service_remove` and create it again once the cause is gone.

**7. Use it.** A job runs on the pool when its repository is in the runner group and its `runs-on` asks for labels
that one class has all of, for example `runs-on: [self-hosted, linux, x64, arca]`, or
`[self-hosted, linux, x64, arca-large]` for a large slot.

### Who can reach what

- **A job can reach the pool's credential.** Jobs get Docker, and the Docker daemon of a slot runs privileged inside
  the virtual machine. A job that sets out to can leave its container, read the virtual machine's disk, and with it
  the GitHub credential in `/etc/arca-secrets`, the other slots and their caches. The pool keeps jobs tidy and apart
  by default; it does not defend them against each other or defend its credential against them.
- **So the runner group is the wall.** Everyone who can get a workflow run in a repository the runner group admits
  (by pushing a branch there, or through an approved pull request) can, in effect, hold the credential, and what the
  credential can do is in the table of step 2. Admit only repositories where you would trust every person with write
  access that far. That is why the pool refuses the default runner group and why public repositories stay off.
- **Nothing beyond the virtual machine.** A job that takes over the virtual machine has the virtual machine: its own
  disk, and the network it is on. It does not have the machine Arca runs on, Arca's other services or Arca's secrets.

### Reading the pool

Every minute the controller writes a status block to the virtual machine's console, which is what `service_logs`
returns, and one line when a job finishes:

```
arca-runner-pool: job slot=0 class=standard labels=self-hosted,linux,x64,arca name="build" result=Succeeded seconds=132 peak_memory_mib=1832 cpu_seconds=201.4 runner=ci-pool-1a2b3c4d-standard-0-5e6f7a8b
arca-runner-pool: slot 0 standard 4096MiB/2cpu busy 41s ci-pool-1a2b3c4d-standard-0-9c0d1e2f job "test"
arca-runner-pool: slot 1 standard 4096MiB/2cpu idle 12s ci-pool-1a2b3c4d-standard-1-0a1b2c3d
arca-runner-pool: slot 3 large 9216MiB/6cpu queued (pausing 40s after a failure)
arca-runner-pool: class standard last 412 jobs: seconds avg 130, peak memory avg 1210 MiB max 3105 MiB, cpu-seconds avg 95, cpus used avg 0.7
arca-runner-pool: recent job finished=2026-01-01T10:00:00Z slot=0 class=standard name="build" result=Succeeded seconds=132 peak_memory_mib=1832 cpu_seconds=201.4
arca-runner-pool: pool ci-pool id 1a2b3c4d for https://github.com/example-org, runner group 7 "arca-pool", selected repositories only, public repositories off
arca-runner-pool: totals busy=2 idle=1 starting=0 queued=1 jobs_done=412 failures=2 runner_updates=0 memory_reserved=17408MiB of 27921MiB slots_cap=4
```

| In the totals | Meaning |
|---------------|---------|
| `busy`, `idle` | Runners with a job, and runners registered and waiting for one. |
| `starting` | Slots registering, starting or cleaning up. |
| `queued` | Slots the pool wants and has not started. The slot's own line says why: the cap, memory no slot has reserved, memory that is free now, a pause after a failure, or no credential. |
| `jobs_done` | Jobs finished since the controller started, whatever their result. |
| `failures` | Failures of the pool itself since the controller started: GitHub refused the credential or a registration, the runner group was refused, a container did not start, a runner went away before it took a job. A job that fails its own steps is a job done, with `result=Failed`. |
| `runner_updates` | Runners that GitHub told to update themselves before their job (see "Pins" below). Above zero means the pool's pinned runner is behind. |

The `pool` line names the runner group the pool registers in and how GitHub has it set; a group open to every
repository, one that admits public repositories, and the default group each show as `WARNING` there, and so does a
public repository for a pool that serves one. The pool reads this again every ten minutes, so a change made on GitHub
after the pool started shows up. If the group has become one the pool refuses (it was made the default group, or
deleted), the line begins `REFUSED`, the runners that already wait or work are left, and no new slot starts
(`queued (held: ...)`) until it is put right on GitHub; the pool then goes on by itself.

A pool with more than one class and more slots than `maxSlots` says in every block what the cap is and is not (`every
class is kept one place under the cap of 4: ...`). While a class has no runner and every place is held, one more line
names the class (`class large: no runner, and every place under the cap of 4 is held. ...`) and says what the pool
does about it: which slot gives up its idle runner's place, or, as a `WARNING`, why none does: no class that holds
two or more places has a runner that waits for a job, or the class's slot would still lack its memory. Each retired
runner is one line of its own (`slot 0 (standard): its idle runner is retired and its place goes to class large,
...`), and so is a runner that GitHub kept because it had a job. If GitHub answers the request to take a runner off
with anything but its yes or that refusal (a 5xx, 401, 403 or 404), or not at all, nothing is retired: the pool says
GitHub's answer once, keeps it in the class's line, and asks again after a pause that starts at five seconds and
doubles up to five minutes. If the runner then stops because GitHub had taken it off after all, the pool says so and
counts no failure. The last ten
finished jobs are repeated in every block (`recent job`), so `service_logs` still reaches them after many blocks. All
job lines are kept in the virtual machine, in `/var/log/arca-runner-pool/jobs.tsv`, one job per line with its finish
time, slot, class, labels, name, result, seconds, peak memory in bytes, CPU seconds and runner name. Past 4MiB (some
twenty thousand jobs) the file is set aside as `jobs.tsv.1`, replacing the one set aside before.

**Sizing runners from real numbers.** The `class` line is what the last 2000 jobs of a class used, beside the size the
class gives them. A class whose peak memory stays far below its `memory` can be given less, and more slots then fit.
`cpus used avg` far below `cpu` says the CPU ceiling is generous. New sizes go into the template, and a pool created
from it has them.

A `max` that is at the class's `memory` does not by itself say the jobs need more. The peak is the kernel's count for
the slot (`memory.peak`), and that count includes the files a job read and wrote that the kernel still keeps in memory
(page cache). A build writes many gigabytes, so it fills whatever its slot has, and the kernel gives that memory back
when the job asks for its own. Such a job may have been held at the limit, slowed or cut short, or may only have been
caching. To tell, look at the job's result and seconds, or run it once in a smaller class and compare. On the first
pool this repository's `cargo test` peaked at exactly the 10240 MiB of its slot. Measured on the machine the
repository is built on, not on the pool, the same job needs about 3.2GiB of its own and 19GB of disk, and under a
4GiB limit it still passes, 5% slower. So the pool's peak is read as page cache, and 10GiB as enough for that class;
neither has been measured on the pool itself.

### What to know

- **One job per runner.** A runner is registered for one job (GitHub calls it just-in-time and ephemeral), runs in a
  container made from the runner's public image, and is removed with everything in it. The next job gets a new
  registration and a new container, so no job finds another's files, processes or installed packages.
- **Admission by memory.** A slot starts only while there is memory for its whole size: memory that no other slot has
  reserved, and memory the machine has free at that moment. A slot's memory is a hard limit with no swap, so a job that
  passes it is stopped by the kernel and no other slot pays for it. CPU is different: each slot has a ceiling of its
  `cpu`, and slots share the machine's CPUs.
- **Docker, and how far jobs are apart.** Each slot has a Docker daemon of its own, started fresh for each job. A
  job's containers, volumes and networks are removed after it; the daemon's images and build cache stay for the next
  job of that slot. Two jobs therefore never see each other's containers or share a daemon. They are not walled off
  from each other the way two virtual machines are ("Who can reach what" above). One shared daemon would have reused
  more cache and separated jobs less; a daemon without root would separate them more and refuse the jobs that need
  privileged containers.
- **Caches.** Each slot keeps its Docker layer cache, a tool cache (`RUNNER_TOOL_CACHE`, which the `setup-*` actions
  fill) and a directory for anything else a workflow wants to keep (`POOL_CACHE_DIR`, `/var/cache/pool` in the job).
  They are on the virtual machine's disk and survive jobs and restarts. Each slot has an equal share of `dockerCache`
  and of `dependencyCache`; a cache that passes its share after a job is emptied, and the log says so. A cache is
  shared by the jobs that run in the same slot one after another, so a job can leave something there for a later one.
- **Where the credential lives.** In Arca's private `secrets/` directory, and in the virtual machine in
  `/etc/arca-secrets`, readable by root there. It is not in Incus's config, in Arca's journals or in any log. The
  controller makes every short-lived token from it by itself and prints none. A runner's one-job registration is
  handed to its container as a file in the virtual machine's memory, not through Docker's own record on disk.
- **A new credential.** `service_secret_set` again, then `instance_start` again: the start restarts nothing, writes
  the new value into the virtual machine, and the controller uses it for the next token. To retire a credential a job
  may have taken, delete the App's key (or the token) on GitHub: that, not Arca, makes it worthless.
- **Stopping and restarting.** `instance_stop` and `instance_restart` need your approval. Runners that are idle are
  taken off GitHub first; a job that is running is lost and GitHub reports it as failed. After the machine Arca runs
  on restarts, the pool starts by itself.
- **Pins, and what you are trusting.** The runner is `ghcr.io/actions/actions-runner` 2.338.2 and the slot's daemon is
  `docker` 29.9.3-dind, both by digest: the bytes are fixed. The virtual machine is `images:debian/13/cloud`, the
  public Debian 13 image with cloud-init (which runs the first boot and grows the disk), named by its alias and not by
  a fingerprint: the public server rebuilds it daily and keeps the last few builds only, so a fingerprint would stop
  resolving within days. For that image you trust the public image server (images.linuxcontainers.org) and its
  build of that day, over TLS, as for every `images:` template; `service_status` shows `baseImage`, the fingerprint
  of the build the machine was actually made from. Docker in the virtual machine is Debian's own package, from
  Debian's signed archive.
- **When the pinned runner gets old.** A pool keeps the runner it was created with; a later Arca release moves the
  pin in the shipped template and does not reach a pool that exists. Once GitHub has a newer runner it tells each
  runner to update itself before it gives it a job. The pool's runners can: each updates inside its own fresh
  container and then takes its job. The cost is a download before every job, since every runner starts from the
  pinned image again; the log says so once per runner and `runner_updates` counts them. A runner and its slot's
  Docker daemon share one fresh copy of the runner's files for each job (the pinned image's files with a layer on top
  that lives for that job), so a job that itself runs in a container (`container:`) gets the updated runner's bundled
  tools too. If the machine cannot layer files, or a runner once fails to start that way, the pool says so once and
  starts runners with their image's own files from then on; a `container:` job then gets the pinned release's tools
  after an update. To end the updates, create a second pool from a template with a newer `runnerImage` (after an Arca
  release: `arca templates install --replace`), check it, and remove the first.
- **Changing a pool.** Settings, sizes and pins are read when the virtual machine is created. To change them, create a
  second pool from the changed template, check it, and remove the first: both can serve the same labels meanwhile.
- **The network.** The pool only calls out: to GitHub, to the image registries and to Debian's mirrors. It has no port,
  and nothing can connect to it. Created without `bridge` it is on Arca's own service network, where a virtual machine
  may reach the internet (over IPv4) and nothing else: not the machine Arca runs on, not your local network, not
  Arca's other services. Created with `bridge` it sits on that host bridge's network like any machine plugged into
  it, with no filter from Arca, and so does every job in it: a job can then reach the other hosts of that network.
  Use the service network for a pool unless you have a reason not to. A GitHub Enterprise host or a package mirror on
  a private address cannot be reached from the service network.

## A pool of GitLab runners

The shipped `gitlab-runner-pool` template is one virtual machine that runs the CI jobs of one GitLab, self-hosted or
gitlab.com, on your own hardware. Inside it runs GitLab Runner (19.5.3, its image by digest) with the docker
executor: every job gets fresh containers from the job's own image, and they are removed when it ends. One pool
serves up to twelve runners you created in GitLab, each with its own token, tags and size of job, under one cap of
jobs at once.

`gitlab-runner-pool-large` is the same pool in a larger machine: 32 CPUs, 64GiB and 300GiB instead of 16 CPUs, 32GiB
and 200GiB, and 20 jobs at once by default instead of 10. Both boot the one script; nothing else differs. A
machine's size is the template's and not a setting of a create, so a pool of yet another size is a copy of the
template under another name with other `limits`.

You need a node that may create a virtual machine of the template's size, the shipped templates installed
(`arca templates install`), and a GitLab where you may create runners. The Arca steps are Arca's own verbs: ask the
assistant you use with Arca to run them. `service_secret_set` cannot be proposed, so it needs an assistant that holds
a `doctor`-scope token, and each token passes through that assistant's conversation.

**1. The runners, in GitLab.** For each set of jobs the pool is to take, create a runner: "CI/CD", "Runners", "New
runner", for the instance, a group or a project. Its tags, and whether it runs untagged jobs, are set there and
nowhere in Arca. GitLab then shows the runner authentication token once: it begins `glrt-`. That token is all the
pool needs; do not run the `gitlab-runner register` command GitLab shows beside it.

**2. Create the pool.** `service_create` with the template and its settings:

```json
{ "name": "ci-pool", "template": "gitlab-runner-pool",
  "settings": { "url": "https://gitlab.example.com", "concurrent": 6,
    "runners": [ { "name": "build", "limit": 2, "jobCpu": 4, "jobMemory": "4GiB" }, { "name": "test" } ] } }
```

| Setting | Default | Meaning |
|---------|---------|---------|
| `url` | none: a create gives it | The address of the GitLab itself, `https://` and a plain host name, not of a group or a project. The tokens are sent there. |
| `concurrent` | 10 (20 in the large pool) | Jobs at once in the whole pool, whichever runner they come through. |
| `jobCpu`, `jobMemory` | 2, `2GiB` | The CPU ceiling and the memory cap of a job's own container. `jobMemory` is a hard cap: a job that passes it is stopped by the kernel, and there is no swap. `jobCpu` is a ceiling and reserves nothing: jobs share the machine's CPUs. |
| `serviceMemory` | `512MiB` | The memory cap of each service container a job starts beside it (`services:`), a database for instance. |
| `jobServices` | 1 | How many services one job may start. A job that asks for more is failed by GitLab Runner. |
| `runners` | one, named `pool` | One entry for each runner created in step 1: its `name` (yours, for the status lines), and optionally `limit` (jobs at once through this runner; 0 for none of its own), `jobCpu` and `jobMemory` (0 and `""` for the pool's). |
| `memoryOvercommitPercent` | 100 | 100 to 200. Above 100 the jobs' memory caps may add up to that many percent of what the machine has for jobs ("When jobs differ in size" below). |
| `reach` | `direct` | `direct`: the machine reaches GitLab over its own network. `tailnet`: it joins your tailnet first (below). |
| `gitlabNode` | empty | Only with `reach` `tailnet`: the name of GitLab's node on the tailnet, for a `url` whose name only the tailnet's DNS knows (below). |

Not for a create: `reserveMemory` (`4GiB`, kept back for the machine, Docker and GitLab Runner), `serviceCpu` (1),
`freeDisk` (`40GiB`), `jobNetworks`, `privileged` and the three image pins. A pool with another one of those is a copy
of the template.

**3. The tokens.** The pool is created stopped. The token of the first entry of `runners` is the secret
`GITLAB_RUNNER_TOKEN_1`, the second entry's `GITLAB_RUNNER_TOKEN_2`, up to `GITLAB_RUNNER_TOKEN_12`: the same number
the owner's card gives the entry (`Setting runners 2`). Set each with `service_secret_set`, then `instance_start`.
The pool starts nothing while an entry has no token, and names it:
`waiting for secrets: runners 2 (test) needs GITLAB_RUNNER_TOKEN_2`.

**What fits.** GitLab Runner admits jobs by their number, not by memory, so the pool checks the sum itself, once, at
its start, against what the machine really reports (less than the size it was given: a 32GiB machine reports about
31.3GiB). A job counts as its `jobMemory` plus `jobServices` times `serviceMemory`. The worst case fills the places of
`concurrent` with the largest jobs first, each runner up to its `limit`. When that plus `reserveMemory` is more than
the machine has, the pool does not start and says every number on its console (`service_logs`), again every five
minutes:

```
arca-gitlab-pool: the pool does not start: its jobs could take 69632 MiB at once (8 of pool at 8704 MiB) and the machine has 27921 MiB for jobs (the 32017 MiB it reports, less reserveMemory 4096 MiB): 41711 MiB too much. ...
```

The create itself does not refuse such a sum yet: it is the machine that says it, so read `service_logs` after the
first `instance_start`. With the defaults of services, a 32GiB pool holds three jobs of 8GiB and the large pool about
six.

CPUs are not reserved and not summed. A job's `jobCpu` is a ceiling on what its container may use, jobs share the
machine's CPUs, and only a `jobCpu` above the machine's own count is refused at the start. A job that needs many CPUs
for a short while (a compile that uses fifteen cores for a minute) is best given a runner of its own with a larger
`jobCpu`: it then bursts without the test jobs being allowed the same.

**When jobs differ in size.** One cap for every job must be the largest job's, and that cap times the jobs at once
is far more memory than a mixed load uses. The honest fix is to tell the sizes apart, in GitLab: one runner for each
size, each with its tag, and the large jobs tagged in `.gitlab-ci.yml`. Each runner is then a lane with its own
`limit` and `jobMemory`, and the sum counts each lane's own limit times its own size, with `concurrent` as the
ceiling over all:

```json
"settings": { "url": "https://gitlab.example.com", "concurrent": 10,
  "runners": [ { "name": "large", "limit": 3, "jobCpu": 8, "jobMemory": "9728MiB" },
               { "name": "standard", "limit": 7, "jobCpu": 4, "jobMemory": "4608MiB" } ] }
```

That is three jobs of 10240MiB (9728 and a service's 512) and seven of 5120MiB: 66560MiB when every lane is full.
The large pool has about 59938MiB for jobs, so as written it is refused, with those numbers. It starts with five
standard jobs instead of seven (56320MiB), with `jobServices` 0 and six standard jobs (56832MiB), or as written with
`memoryOvercommitPercent` 115, which is a small overcommit beside one cap for all.

`memoryOvercommitPercent` is for a load that cannot be told apart, because nobody will tag the jobs: every job then
gets the one large cap, and the setting lets the caps add up to more than the machine has, to at most twice. With
150, the large pool takes eight jobs of 10240MiB (81920MiB against 59938MiB). What stays true: every container keeps
its own hard cap, so a job that passes its cap is the one that is stopped, not its neighbours. What is given up: the
promise that the machine itself cannot run out. If many large jobs peak together it can, and the kernel then picks
what it stops, which may be a job that was within its cap, or Docker. The pool says the factor and this risk at its
start and in every status block whenever the setting is not 100. Measure what your jobs really use before you set it,
and prefer lanes.

**More capacity** is a second pool, on the same node or another, created with the same runners: GitLab lets one
runner's token serve several machines and hands each job to whichever asks first. Create it, set the same tokens,
start it. Nothing in Arca grows a pool by the length of GitLab's queue.

### What keeps jobs apart, and what does not

- **Containers, in one machine.** A job is a set of containers (its own, its services', GitLab Runner's helper) on a
  network of its own, so one job cannot connect to another job's database. No job container is privileged, none gets
  the Docker daemon's socket or a path of the machine, and nothing a job wrote is kept for the next one: no cache
  volume, a fresh clone every time. Jobs start as root inside their container, as GitLab's docker executor has it, so
  `apt-get install` works.
- **That is container isolation, no more.** All jobs of all the pool's runners share one Docker daemon and one
  kernel. A kernel bug or a container escape gives a job the virtual machine: every runner's token in it, the other
  jobs, and with `reach` `tailnet` the machine's place on your tailnet. The virtual machine is the wall that holds:
  a job that takes it over has that machine and the network it is on, not the machine Arca runs on, Arca's other
  services or Arca's secrets.
- **So pool only what trusts each other that far.** Put runners in one pool when their projects could share a
  runner anyway. A runner that signs or attests releases should not share a pool with jobs of merge requests: keep
  it in a pool of its own, or outside Arca.
- **`privileged` is off.** A job that needs Docker itself (docker-in-docker) fails in this pool. A copy of the
  template with `"privileged": true` turns it on for every job; every status block then begins with a warning,
  because a privileged job container can take over the machine without any bug.
- **No disk limit per job.** A job can fill the machine's disk. When less than `freeDisk` is free the pool removes
  the images no running job uses and says so; a disk that stays short is in the status as a warning.

### Reaching a GitLab that is not on the internet

Created without `bridge`, the pool is on Arca's service network, which leads to the internet and nowhere else: a
GitLab on a private address or on a tailnet is not reached from there. Two ways:

- **Through the node's own network.** Create the pool with `bridge` on a host bridge whose network leads to GitLab.
  The machine, and every job in it, is then on that network like any machine plugged into it.
- **`"reach": "tailnet"`.** The machine joins your tailnet itself, which works from the service network. Set the
  secret `TAILSCALE_AUTHKEY` (an auth key from your tailnet's admin pages) beside the tokens. The pool runs a
  Tailscale node (1.104.4, by digest) in the machine's own network; the key is handed over as a file the node is told
  the path of, and the node keeps its identity on the pool's disk, so a one-use key is enough. Every job can then
  reach whatever the tailnet lets this machine reach: give the key a tag whose access rule admits GitLab and nothing
  else.

The machine does not use the tailnet's own DNS, so on a tailnet the pool has to learn where GitLab is. It reads the
tailnet's own list of nodes, and the console says which of three cases applied:

| The `url` names GitLab by | What the pool does | On the console |
|---------------------------|--------------------|----------------|
| its full name on the tailnet (`https://gitlab.<tailnet>.ts.net`, the name a tailnet certificate is for) | Finds the node of that name and tells its address to GitLab Runner and to every job. | `the url's name ... is the name of a node of this tailnet, at ...` |
| a name of your own domain that ordinary DNS answers with GitLab's tailnet address | Nothing more: the name resolves like any name. | `... is not the name of a node of this tailnet, so it is looked up in DNS like any name` |
| a name of your own domain that only the tailnet's DNS knows | Needs `gitlabNode` at create: the name of GitLab's node on the tailnet (its machine name there, or its full tailnet name). Its address is told to GitLab Runner and to every job for the url's name, so GitLab's certificate is still checked against the url's name. | `settings.gitlabNode names the tailnet's node ..., at ...` |

A `gitlabNode` the tailnet does not show is not passed over: the pool starts nothing, lists the nodes the tailnet
does show, and looks again. A node is shown only when the tailnet's access rules let this machine reach it.

Not reached with `reach` `tailnet`: a GitLab that is no node of the tailnet itself but sits behind one that routes a
subnet (the pool accepts no routes), and any other name that only the tailnet's DNS knows, a registry for instance,
which the jobs then cannot resolve.

A pool that cannot reach its GitLab does not start GitLab Runner, and says which of the things in between it is, in
one sentence each, before it tries again:

| On the console | What to look at |
|----------------|-----------------|
| `the name ... does not resolve from this machine` | DNS: the name is known only inside a private network or a tailnet the machine is not on. A pool that is on the tailnet is told about `gitlabNode` instead. |
| `resolves to ... and the connection to port 443 timed out` | A firewall or an access rule between the machine (or the node it goes out through) and GitLab. |
| `resolves to ... and refuses the connection` | Nothing listens on that address and port. |
| `its certificate is refused` | GitLab's certificate is from a private authority, or for another name. The pool trusts the public authorities only. |
| `accepts the connection ... and did not answer`, `GitLab answered 502` | GitLab is reached and is not well. |
| `GitLab answers and does not take GITLAB_RUNNER_TOKEN_2 (403)` | That token: reset, mistyped, or its runner deleted. The other runners run; this one is left out until a new token is set. |

### Reading the pool

Every minute the controller writes a status block to the machine's console, which is what `service_logs` returns,
and one line when a job finishes:

```
arca-gitlab-pool: job runner=build project=45 job=8812 name="compile" result=success reason=- seconds=132
arca-gitlab-pool: runner 1 build limit=2 job=4cpu/4096MiB jobs_done=41 jobs_failed=3
arca-gitlab-pool: runner 2 test limit=none job=2cpu/2048MiB jobs_done=120 jobs_failed=9
arca-gitlab-pool: recent job finished=2026-01-01T10:00:00Z runner=test project=45 job=8813 name="unit" result=failed reason=script_failure seconds=61
arca-gitlab-pool: pool ci-pool for https://gitlab.example.com, reach direct, at most 6 jobs at once, 19456 MiB of the 27921 MiB the machine has for jobs (2 of build at 4608 MiB, 4 of test at 2560 MiB)
arca-gitlab-pool: totals jobs_now=3 of 6 jobs_done=161 jobs_failed=12 runners=2 of 2 failures=0 disk_free=141202MiB
```

- `jobs_now` is the number of jobs that have a container on the machine at that moment, counted by the label GitLab
  Runner gives its containers; `unknown` when Docker did not answer.
- A job's line is GitLab Runner's own: the job's and the project's numbers as GitLab has them, the job's name, the
  runner it came through, `success` or `failed` with GitLab's reason (`script_failure`, `job_canceled`,
  `runner_system_failure` and so on) and its seconds. The pool measures nothing itself: GitLab Runner does not say
  what a job used, so there is no memory or CPU per job here, as there is in the GitHub pool.
- `jobs_done` and `jobs_failed` count since the controller started. `failures` are the pool's own: GitLab not
  reached, a token refused, GitLab Runner or the tailnet node stopped.
- A runner whose token GitLab refuses shows as `runner 2 test LEFT OUT:` with the reason, and `runners=1 of 2`.
- All job lines are kept in the machine, in `/var/log/arca-gitlab-pool/jobs.tsv`; past 4MiB the file is set aside as
  `jobs.tsv.1`.

### What to know

- **Where the tokens live.** In Arca's private `secrets/` directory, in the machine in `/etc/arca-secrets`, and in
  GitLab Runner's `/etc/gitlab-runner/config.toml`, mode 0600 in a directory only root enters. They are never on a
  command line and never printed: the pool does not run `gitlab-runner register`, which takes the token as an
  argument, but writes the configuration itself and asks GitLab whether it takes each token with the token on the
  request's body. GitLab Runner's own output inside the machine names a runner by the first characters of its token,
  as GitLab's pages do; the pool copies none of that to the console.
- **A new token.** `service_secret_set`, then `instance_start`: the start restarts nothing, the controller sees the
  new value, asks GitLab about it and writes the configuration again, which GitLab Runner reads by itself. The pool
  also asks GitLab about every token every ten minutes, so a token reset in GitLab shows in the status.
- **Images.** A job's image is pulled when the job starts; when the registry does not answer, a copy already on the
  machine is used. Name images by digest in `.gitlab-ci.yml`: the pool does not pin what your jobs run. A job without
  an `image:` fails, since the pool has no default one.
- **Stopping.** `instance_stop` and `instance_restart` need your approval. GitLab Runner is told to take no new job
  and the jobs that run get a minute to finish; a job still running then is lost, and GitLab reports it as failed.
  The pool's service holds its stop open through one `sleep infinity` process inside the machine: should that process
  end by itself (killed by hand, or by the machine when it is out of memory), the pool stops in the same order
  without being asked, and its service starts it again ten seconds later.
- **Pins.** GitLab Runner is `registry.gitlab.com/gitlab-org/gitlab-runner` 19.5.3 and its helper is
  `gitlab-runner-helper` `x86_64-v19.5.3`, both by digest, read from GitLab's registry when the template was written;
  the tailnet node is `tailscale/tailscale` 1.104.4, the digest the workspace template pins. A pool keeps what it was
  created with: for a newer GitLab Runner, create a second pool from a newer template and remove the first.
- **Changing a pool.** Settings are read when the machine is created. To change one, create a second pool with the
  same tokens, check it, and remove the first: both take jobs meanwhile.

## Tenants

A node may hold tenants: each is one restricted Incus project of its own, on the node's managed service network, with its
own caps and its own block of addresses. The node's own project, `arcak-svc`, stays the node owner's. The rules are 61 to
67 of `docs/OWNERSHIP.md`.

`kernel.tenants` turns it on. Without the key the node behaves exactly as before, and every tenant verb, and every verb
given a `tenant`, answers "this node has no tenants". The key needs `kernel.serviceCidr` and is refused beside
`kernel.adopt`.

```json
"tenants": {
  "capacity": { "instances": 40, "cpu": 64, "memory": "128GiB", "disk": "2TiB" },
  "maxTenants": 28,
  "defaultCaps": { "instances": 4, "containers": 4, "virtualMachines": 1, "cpu": 8, "memory": "16GiB", "disk": "100GiB" },
  "createRate": { "count": 5, "perSeconds": 3600 },
  "front": "edge"
}
```

| Key | Required | Meaning |
| --- | --- | --- |
| `capacity` | yes | What all tenants may hold together. The sum of every tenant's caps never passes it. |
| `maxTenants` | yes | How many tenants the node holds, 1 to 30. |
| `defaultCaps` | yes | The caps of a tenant whose create names none: `instances` (1 to 16), `containers`, `virtualMachines`, `cpu`, `memory`, `disk`. They must fit inside `capacity`. |
| `createRate` | yes | At most `count` tenants are created in any `perSeconds` seconds. The creates are counted from the journal, so a restart does not reset the count, and a tenant that was removed still counts. |
| `front` | no | The name of one service in `arcak-svc`. Every ordinary tenant service admits that service's address on the ports its template declares, and nothing else of the node's project. The rule follows the front: when it is created, removed or recreated at another address, every tenant service's firewall rules follow within 30 seconds. Without the key no service of the node's project reaches a tenant. |

Each tenant gets a block of the service /24: a /29 (8 addresses), or a /28 when its `instances` cap is above 8, taken from
the top of the subnet down. The node's own services keep `.10` to `.239` without the tenants' blocks. About 28 small
tenants fit a node. In a scratch run (`kernel.scratch`) a tenant's project is `arcak-scratch-t-<id>`, and the kernel
joins the managed bridge the node already has instead of making one.

The verbs:

| Verb | What it does |
| --- | --- |
| `tenant_create {id?, caps?}` | Creates a tenant. Inside the node's policy (caps at or under `defaultCaps`, room in `capacity`, `maxTenants` and `createRate`) it needs no approval, from a token whose tool allowlist names `tenant_create`. Caps above a default run only from a proposal the owner approved (`action_propose`, then the approval). A refusal names the limit; past the rate it gives the time of the next allowed create. Out of room is the refusal `capacity` (always with `scope: "node"` here) and past the rate is `rate`, each with fields beside the code (see "Refusals for want of room and for asking too often"). Without `id` one is made: 8 lowercase base32 characters. |
| `tenant_list` | Every tenant: its id, block, caps, and what its records use. |
| `tenant_status {id}` | One tenant, with the number of instances its project holds. |
| `tenant_usage {id}` | Per instance: `name` (the Incus instance), `service` (the service's name), `kind` (`container` or `vm`), `state`, `limitsCpu`, `limitsMemory`, `rootDiskSize`, and for an instance of a provisional deploy `provisional: true` and `deploy`. No time and no measured use. |
| `deploy_confirm {tenant, deploy}` | Makes a deploy's services ordinary. Only the token that opened the deploy, while it is provisional. |
| `deploy_rollback {tenant, deploy}` | Removes exactly what a provisional deploy created, with no approval: each instance deleted and confirmed gone before its firewall rules, then its secrets and volumes. Only the token that opened the deploy, before a confirm, within 15 minutes of its first create. |
| `deploy_remove {tenant, deploy}` | Removes everything a deploy created, once it is confirmed or its 15 minutes have passed, only from a proposal the owner approved: one approval for all its parts, in the same order. Nothing else of the tenant is touched. A provisional deploy is refused (its token rolls it back), and so is an unknown deploy id. |
| `tenant_remove {id}` | Removes the tenant and everything in it, only from a proposal the owner approved: each instance is deleted and confirmed gone before its firewall rules, then its secrets, and last the tenant's profile and project. A removal that cannot finish says what remains. |

`service_create`, `service_list`, `service_status`, `service_logs`, `service_remove`, `service_connect`,
`service_disconnect`, `service_secret_set`, `snapshot_create`, `snapshot_list`, `snapshot_delete`, `instance_start`,
`instance_stop` and `instance_restart` take an optional `tenant`. With it the verb acts in that tenant's project and sees nothing else: a
name that only another tenant, or the node's own project, holds is "not found". Without it the verb acts in the node's own
project and sees nothing of any tenant. The instance verbs take `tenant` in place of `project`. A tenant's Incus instance
is named `<service>-<tenant id>` (one bridge holds one instance of a name). `tenant_usage` reports that name as `name`
beside the `service`, and the instance verbs take either `instance` or, in a tenant, `service`. In a tenant:

- `instance_start`, `instance_stop` and `instance_restart` need no approval, from a token whose tool allowlist names the
  verb. Each is written to `instance-actions.jsonl` with the token and the tenant. Outside a tenant a stop and a restart
  still need an approved proposal;
- `service_create` takes `deploy`, an id of 1 to 64 of `a-z`, `0-9` and `-`. What
  is created under it is provisional until the same token calls `deploy_confirm`, or 15 minutes after the deploy's first
  create (counted from `tenant-deploys.jsonl`, so a restart does not reset it). Until then that token may call
  `deploy_rollback`. A provisional service is not reached by the front and is flagged in `tenant_usage`. Afterwards it
  is an ordinary service, and its removal needs the owner's approval;
- `service_create` takes `start: false` as in the node's own project (see "Creating a service stopped"): the service is
  created stopped, `service_secret_set` and `service_connect` apply to it, and `instance_start` with `tenant` starts it
  for the first time, with no approval from a token whose tool allowlist names the verb. A deploy's rules do not look
  at whether a service runs: a provisional service that is still stopped is confirmed by `deploy_confirm` and removed
  by `deploy_rollback` like a running one. A virtual machine's secrets are files in its guest, written after it boots:
  `instance_start` starts the machine and writes them into that tenant's own machine, as it does for a machine of the
  node's own;

- a service's address comes from the tenant's block, and its caps are checked before Incus is asked, so a refusal names
  the cap (`limits.instances`, `limits.containers`, `limits.virtual-machines`, `limits.cpu`, `limits.memory`,
  `limits.disk`). Its code is `capacity` with `scope: "tenant"`, and the cap and the amounts beside it. A create the
  node has no room for is `capacity` with `scope: "node"`: the tenant's own caps were not the reason;
- a link joins two services of the same tenant only;
- a virtual machine joins the managed network only; a `bridge` is refused;
- a template with a `lan` port or a dash is refused, and `service_publish`, `service_unpublish`, `service_import` and
  `service_release` take no `tenant`: a tenant's service is reached through the front;
- the snapshot verbs with no `tenant` see nothing of any tenant: `snapshot_list` lists no tenant's snapshot, and a
  tenant's project or snapshot is reached only by naming the tenant;
- a container template's data volume must state its size, so it counts against the disk cap;
- `service_remove` needs the owner's approval as everywhere; proposed with `action_propose`, the proposal names the tenant.
  A direct call's statement names the target `tenant.<id>.<name>` and the arguments `{"name", "tenant"}`;
- a database is a service: the postgres service template, created with `service_create` and linked with
  `service_connect`. The `postgres_*` verbs act in the node's own project only, and refuse a `tenant` in those words;
- `service_logs` answers the console log.

The real-Incus probe is an ignored test. It runs a scratch kernel (`kernel.scratch`) beside the node's own, on the node's
managed bridge, makes two tenants, checks DHCP, DNS, the internet, the isolation, a link, a spoofed address, a virtual
machine and the caps, and removes everything it made:

```sh
ARCA_TENANT_PROBE_SOCKET=/var/lib/incus/unix.socket ARCA_TENANT_PROBE_CIDR=<the node's serviceCidr> \
ARCA_TENANT_PROBE_DROIDD=<the node's droiddProjects, comma-separated> ARCA_TENANT_PROBE_FRONT=<a service, optional> \
  cargo test -p arca --test tenants -- --ignored --nocapture scratch_probe
```

A second, smaller ignored probe covers "Creating a service stopped" on a real Incus, with the same variables: one
scratch tenant, two containers created with `start: false` in a deploy, their secrets and their link set while they are
stopped, and `instance_start` for each. It then reads, inside each container, the environment its first process was
started with, and removes everything it made:

```sh
ARCA_TENANT_PROBE_SOCKET=/var/lib/incus/unix.socket ARCA_TENANT_PROBE_CIDR=<the node's serviceCidr> \
ARCA_TENANT_PROBE_DROIDD=<the node's droiddProjects, comma-separated> \
  cargo test -p arca --test tenants -- --ignored --nocapture created_stopped_probe
```

## Functions

A function is an OCI image by digest that holds one program, and a rule for when to run it. One run is one start of the
function's own container: the program does one batch, prints its result as its last line, and exits. Nothing of a
function runs between its runs. It is for the work that today needs a small service kept running to do something every
half minute: a relay that drains a table, a consumer that takes what waits on a queue.

A kernel-mode node with the managed service network (`kernel.serviceCidr`) runs functions. Six verbs:

| Verb | Who runs it | What it does |
|------|-------------|--------------|
| `function_create {name, image, digest, triggers, command?, env?, secrets?, links?, limits?}` | only an approved action proposal | Creates the function, stopped. Its timer runs it from then on. |
| `function_remove {name}` | only an approved action proposal | Stops the run that is going, deletes the function with its secrets. Its lines in the run log are kept. |
| `function_secret_set {name, key, value}` | a `doctor` token | Sets one of the secrets the definition names. Write-only. |
| `function_list` | a `viewer` token | Every function, with its definition, which secrets are set, whether a run is going, and its newest run. |
| `function_run {name}` | a `doctor` token | Runs it now, once. Answers at once with the run's id. |
| `function_runs {name, limit?, run?}` | a `viewer` token | Its newest runs, one line each, or one run with the end of its output. |

### The definition, and the owner's card

A function is created, and removed, only from a proposal the owner approved: `action_propose` with
`function_create` and the definition. A direct call is refused with `NotApproved`. A new digest is a new function, so
**every release of a function's program is a card the owner approves**: `function_remove`, then `function_create` with
the new digest. A run is not a change and needs no approval.

```
action_propose { "action": "function_create", "args": {
  "name": "relay",
  "image": "registry.example/acme/relay",
  "digest": "sha256:<64 hex digits>",
  "command": ["/relay", "--once"],
  "env": { "BATCH": "500" },
  "secrets": ["DATABASE_URL", "NATS_URL"],
  "links": [{ "to": "nats", "port": 4222 }],
  "triggers": [{ "timer": { "everySeconds": 30 } }],
  "limits": { "seconds": 60, "memory": "128MiB", "cpu": 1 }
} }
```

| Field | Rule |
|-------|------|
| `name` | A service's grammar. Functions and services share one name space: neither takes a name the other holds, and each verb keeps to its kind (`service_status` of a function, and `function_run` of a service, are refused in words). |
| `image`, `digest` | `<registry>/<repository>` in lower case and `sha256:` with 64 hex digits. Never a tag. Arca pulls the image without a credential. |
| `command` | Optional. The program's absolute path inside the image, then its arguments. No shell runs it. Left out, the image's own entry point runs. |
| `env` | Plain values by name, at most 32, each at most 160 characters. The card shows each whole. |
| `secrets` | Names only, at most 16. See below. |
| `links` | Services of this node the function may reach, each on one tcp port that service declares. At most 8. |
| `triggers` | Exactly one, a timer: `everySeconds` from 10 to 86400. |
| `limits` | `seconds` 1 to 900 (default 60), `memory` a size of at least 8MiB (default 128MiB), `cpu` 1 to 64 (default 1). |

What the caller leaves out is written into the proposal, so the card shows every limit and the approval binds it. The
card has one row for each: `Function`, `Program` (image and digest), `Kind`, `Command`, `Runs`, `Limits`, `Secrets`
(the names, never a value), one `Setting <NAME>` for each plain value, `Services on this node`, `Private network` and
`Internet`. A proposal the node would refuse (a link to a service that is not there, or to a port it does not declare;
a name that is taken) is refused when it is made, so the owner is never shown it.

### What a function may reach

Only what its definition names. Its container has its own network card on the service network, with rules of its own
and a default that rejects both ways:

- each **link** is one rule, tcp to that service's address on the linked port, and one rule on that service's side that
  lets the function in. A link gives the way and nothing else: no connection value is put in the function's
  environment. What it connects with is a secret;
- a **private address** outside the node's services is opened exactly as a service's is: the node's config names it for
  the function's name (`kernel.serviceDestinations`), the proposal fills it in, and the card's `Private network` row
  shows it ([A service that may reach one private address](#a-service-that-may-reach-one-private-address));
- **nothing else.** A service's rules end with one that lets everything else out to the internet. A function's do not:
  it can open no connection to a public address. There is no setting for it in this build.

**One way out stays open, and the card says so.** The service network's own DNS answers a function, as it answers
every instance there, and it asks the internet's name servers for a name it does not know. So a program can still
send a little data out in the names it looks up, to whoever runs the name server of a domain. The `Internet` row of
the card says this in the owner's words. Closing it is not in this build.

### Secrets by name

Arca reads no vault. Whoever holds a value sets it into Arca by name, `function_secret_set {name, key, value}`, and a
run is given the secrets its definition names and no other, as environment values: nothing else writes a name into a
function's environment, a linked service's own connection value least of all. A value is write-only: no verb shows
it again. A program that prints one does not put it in the run log as it stands: each stored value is taken out of
the output and of every string of the decoded result, in its plain form and with any of its characters written as a
JSON escape. That is a search for known text, not a proof: a program that prints a secret cut in pieces or across
two lines, or encoded another way (base64, a URL's escapes), prints it. A value is **one line of 8 to 4096 characters**: a credentials file or a PEM
certificate of several lines is not a secret a function can be given. A function with a secret not yet set does not
run: its run line says `notStarted` and names the secret.

### A run

The timer starts a run every `everySeconds`, counted from the last start. One run of a function goes at a time: when
one is still going as the next is due, the next is not started, and a `function_run` meanwhile is refused with
`FunctionBusy`. Nothing is queued and nothing is retried: the next tick is the next try. After a restart of the
kernel every function is due at once, and no missed tick is made up.

**Every run starts from the image's own root.** Before it starts a run the kernel has Incus rebuild the container's
root disk from the function's image. The config, the secrets among it, and the network card stay as they are; the
files are the image's again. So no run finds what an earlier run wrote, and none can leave the next something to
execute.

The first run of a function is the one run with nothing rebuilt before it: its container has never been started, so
its root is still the one its create made from the image, and no program has run in it. It must not be rebuilt
either: Incus cannot start a container that was rebuilt before it ever ran (it answers `No ID map found for
unprivileged container`; seen on Incus 7.5.1, and the source of Incus 7.6.0 gives such a container a map again,
which was read and not run). Whether a container has started is read on the container itself, from
the key Incus writes at its first start. Before that first run the kernel checks what it can of the root: the
container must still say it was made from the image the function's record took, or the run does not start. That
run's line says `root` is `new`.

For every run after the first, the rebuild uses the copy of the image the node already holds, and asks no registry: the copy whose
fingerprint the function's record took when it was created, read from the image Incus fetched for the approved
registry and digest. It is never the image the container itself says it was made from, which whoever may write the
container could change. When that copy is gone (Incus drops a cached image nothing has used for a while; a function's
runs keep using it) the rebuild names the registry and the digest the owner approved, and Incus fetches the image
again. A copy that is there and cannot be rebuilt from is an error, and the registry is not asked. When the root
cannot be rebuilt the run does not start: `notStarted`, and `why` says what was answered.

A function's record may hold no fingerprint: when its create could not tell which one image of the store is the
approved one (two records for the same registry and digest, or an instance that says it was made from another). And
the store's copy may be gone and fetched again under another fingerprint than the record holds. In both cases every
run asks the registry, by the approved digest: safe, slower, and in need of the registry each time. A person sees
it in `function_list`, where `fingerprint` is `null`, or in the run lines, where `root` is `registry` and `rootImage`
is `null` run after run. `function_remove` and `function_create` again, two cards, bring it back to the store's
copy.

**What the program must do.** Run one batch and exit. Print what it likes, up to 1 MiB in one run (past that the
output cannot be read, and the run ends `died` whatever it printed); **its last output line is its result: one JSON
object, at most 4096 bytes, with `"ok": true` or `"ok": false`**. Incus reports no exit status of a container's
program, so the line is the only word the program has. It must be safe to kill at any instant and to run again. It runs
as the container's first process, which ignores a stop it has no handler for: a program that wants to end cleanly at
its time limit handles `SIGTERM`.

**Limits.** At `limits.seconds` the kernel stops the run: a stop the program has 5 seconds to obey, then a kill.
`limits.memory` and `limits.cpu` are the container's own, enforced by the node; a program that passes its memory is
killed.

**Outcomes.** Each run ends as one of:

| Outcome | Meaning |
|---------|---------|
| `ok`, `failed` | The program's own word: `"ok"` in its result line. |
| `timeout` | It passed its time limit and the kernel stopped it. |
| `memory` | It ended without a result and the node counted a kill at its memory limit. Best effort: the count is gone with the container, so a run killed for memory is often `died`. |
| `died` | It ended without a result line. |
| `interrupted` | The kernel stopped it for another reason: the function's removal, or a kernel that started and found a run nobody watched. |
| `notStarted` | Nothing was started, and `why` says so: a secret is not set, the root could not be rebuilt from the image, Incus refused the start, the instance is not the one Arca created. |

Between the end of one run of a function and its next start the kernel keeps 2 seconds. Incus is still putting a
container away for a moment after its program exits, and a rebuild or a start in that moment can be refused. The shortest timer is
10 seconds, so only a `function_run` that follows another run at once waits for it.

### The run log is the meter

One line for each run, appended and never rewritten, in `<stateDir>/function-runs/runs-<year>-<month>.jsonl` (mode
0600). A month has its own file, and no file is cut: runs will be billed from them.

```
{"run":"fr-1793361600000-7","function":"relay","digest":"sha256:...","kind":"container","trigger":"timer",
 "startedAt":1793361600000,"root":"store","rootImage":"<64 hex digits>","rootMs":55,"startMs":265,"durationMs":41,
 "cpuMs":12,
 "memoryPeakBytes":3194880,
 "memoryLimitBytes":134217728,"outcome":"ok","result":{"ok":true,"published":3}}
```

`root` says what the root was before the run: `new` (the root the create made, for a function that had never
started: nothing was rebuilt), `store` (rebuilt from the node's own copy of the image) or `registry` (rebuilt from
the image fetched again). `rootImage` is the image's fingerprint for `new` and `store`, and `rootMs` is how long it
took. `startMs` is how long Incus took to start the container; `durationMs`
runs from there to the exit the kernel saw.
`cpuMs` and `memoryPeakBytes` are read from the container's cgroup while it runs, and only when the node's Incus is on
the machine `arca serve` runs on: the files are gone when the container stops, so the peak is the last reading before
the exit, a lower bound, and both are `null` when nothing could be read. `memoryLimitBytes` is what the run was allowed.
`function_runs` answers the newest lines (100 of each function are held in memory), newest first; with `run`, that
line and the end of the run's output (64 KiB, kept for the 20 newest runs of each function under
`function-runs/output/`). A function's output is untrusted text.

**What a run writes.** To Incus: the container's root disk, rebuilt from its image; its power state; and its console
log, which is cleared before each start so that what is read afterwards is that run's output alone. A rebuild keeps
the container's config, its devices and its uuid. To the state directory: the run's line and the end of its
output. Nothing else: no desired-state entry, no line of the effect journal. The run log's directory is made by the
first run of the node's first function: a node with no function has none. A function's container is never started
by Incus itself (`boot.autostart` is `false`), so a host that comes back up starts none. `instance_start`, `instance_stop` and
`instance_restart` refuse a function's container (`InstanceIsFunction`): only a run starts it.

### What to know

- **A run can write its container's root, and the next run does not find it.** Incus refuses to start an OCI
  container whose root disk is read-only (seen on Incus 7.5.1), so the root is rebuilt from the image before each run
  instead. A function keeps no state in its container: what must outlive a run goes to the services it reaches.
- A function whose image is gone from the node's store needs its registry for its next run. With the registry out of
  reach that run is `notStarted`, and so is each one after it until the image can be fetched.
- An image on a registry that asks for a credential is fetched with the one set for that registry
  ([A registry that asks for a credential](#a-registry-that-asks-for-a-credential)); any other is fetched without
  one.
- On Incus 7.5.1, on one machine and without a network card on the container, a run of a program that prints two lines
  and exits took a third of a second from the clearing of its log to its output being read (median of 20; start 0.27 s),
  and a forced stop 0.63 s. The rebuild before it took 55 ms from the node's own copy of the image and 1.3 s when the
  registry was named (medians of 10). A network card and its rules add to the start.
- A function is created in the node's own project only: not in a tenant, and not on a bridged node without the managed
  service network.
- A service a function links to is not removed while the function is there: `service_remove` is refused, when it is
  proposed and when it runs, and names the function. Remove the function first. The kernel itself refuses, so two
  approved actions in any order never leave a function linked to nothing; a removal refused after its card was
  approved has used that approval up, and says so.
- When `arca serve` cannot make the function runner at its start it says so on its log ("this node runs no
  functions", and why) and serves everything else. Until the cause is put right and `serve` is started again, the
  function verbs answer that the node runs no functions, `function_remove` among them, so a service a function links
  to cannot be removed either; and a run that a kernel which stopped left going is not stopped, since the sweep that
  stops it is the runner's. Starting `serve` again with the cause put right does all of it.
- A function's container has no size limit on its root disk beyond the pool's own.
- A run that cannot start writes a line each time it is tried. A function that is stuck on a 10-second timer writes
  8,640 lines a day, and the run log is never cut.
- A kernel that stops between a program's exit and the writing of its line loses that run from the run log.
- A run of a program that never exits is ended only by its time limit.

## A registry that asks for a credential

A service's or a function's image may be on a registry that gives nothing without a login. Arca reads no vault:
whoever holds the registry's credential sets it into the node, by the registry's address.

| Verb | Who runs it | What it does |
|------|-------------|--------------|
| `registry_secret_set {server, username, password}` | a `doctor` token | Sets the credential of the registry `https://<host>`. Write-only. |
| `registry_list` | a `viewer` token | The registries a credential is set for, by address. Never a value. |
| `registry_secret_remove {server}` | a `doctor` token | Removes it. The next fetch from that registry is made without one. |

From then on every image the node's own projects fetch from that registry is fetched with it. Nothing in a tenant
is.

A credential belongs to exactly the registry it was set for: the address is compared whole, the port with it, in
lower case. A host that differs by one character, a dash where the other has a dot, is another registry and is
given nothing. No function or service can ask for a registry's password as a secret of its own: it is the node's.
`registry_list` is open to a `viewer` token, so the addresses, which may be hosts of an internal network, are read
by every token of the node; never a value.

**Where the credential goes, and where it rests.** Arca keeps it in its secret store and in nothing else of its own:
no record, proposal, card, journal line, log or answer holds it. It leaves the store in one place. When an image
is fetched, the address Arca sends to Incus names the registry with the credential in it, which is how Incus takes
one. **Incus then keeps that address in the record of the image it fetched, for as long as it keeps the image, and
whoever may use Incus on this machine can read it there**: root, the members of the group that may use Incus's
socket, and the user `arca serve` runs as. That is the same reader who already reads a container's environment
secrets. So:

- **set only a credential that can read, and only what this node pulls.** A registry's own pull token for one
  project is the right kind. Never one that can push or delete;
- a create whose image is on such a registry says so on its card, in a `Registry` row, and runs only from an
  approved proposal: a direct `service_create` is refused with `NotApproved`, whether it names the registry or
  not. The proposal fills the row in. A card approved after the credential was removed does not run;
- Incus is told not to go back to the registry for the image by itself (`auto_update` is turned off on it): an image
  by digest never changes. Arca turns it off right after every create and every rebuild that was sent the
  credential, whatever that create or rebuild answered;
- the image, and the credential with it, leaves Incus when its last function or service is removed;
- **every ten minutes**, and when `arca serve` starts, the node looks at the images its own project fetched with a
  credential. It turns `auto_update` off on one that has it on, and deletes one that no service or function is made
  from once it has seen it so for **fifteen minutes** (a create may still be between its fetch and its instance
  before that). It knows such an image by the image's own record in Incus, the address kept there, and not by what
  the secret store holds: removing a credential forgets no image. It never touches an image with no source address
  (a local one, one imported by hand) or one from an address without a credential, and never deletes an image a
  service or a function is made from, a stopped function's included. Each thing it does, and each image deleted
  at a removal, is one line in `registry-images.jsonl` in the state directory: when, the project, the fingerprint,
  what and why, and never the address. A round goes on past an image Incus will not let it change, so one such
  image does not leave the others as they were, and the quarter of an hour is counted on a clock that only goes
  forward, whatever the machine's own clock does. A round that fails says so once on the log of `arca serve` and
  is tried again at the next; it stops nothing and no create or run waits for it;
- **what this does not give:** between a fetch and the turn-off that follows it, `auto_update` is on: for seconds,
  or, when the turn-off failed, until a round can turn it off. A round is tried every ten minutes; while Incus
  keeps refusing, it stays on. Incus's own round for cached images is hours apart, and what
  it would do in it is ask the same registry for the same digest with the address it keeps. Closing that at the
  source takes a key on the project itself (`images.auto_update_cached`), a change to the config of a project that
  already exists: planned as its own change, after it is seen to work on the Incus the nodes run;
- a new credential replaces the old one for every later fetch. An image already fetched keeps the address it was
  fetched with until it is removed.

A password is 8 to 4096 of letters, digits and `- . _ ~`: it stands in an address, and only such a word is known to
reach the registry as it is. A registry's token is one.

A function whose store copy is gone fetches its image again with the credential set at that moment; with none set,
or with the registry out of reach, that run is `notStarted`.

Two fetches use the credential with no card that names it, both of an image the owner approved by digest on the
registry the credential was set for: a function approved before the credential was set, the next time its root is
rebuilt from the registry; and a service the kernel creates again after its instance was lost.

Seen on Incus 7.5.1 against a registry that asks for a login: a create without a credential is refused, with it the
image is fetched; the image's record keeps the address with the credential, the instance's record and the list of
operations do not; `auto_update` can be turned off. Read in the source of Incus 7.6.0, and not seen live: Incus
hands the program that fetches for it the password in a temporary file only its own user reads, with only the file's
path on the command line; and Incus's own log holds the address with the credential only when its daemon runs
verbose or debug, in lines that then outlive the image. Keeping the credential out of Incus altogether would take Arca fetching the image itself and handing
Incus the files: planned, not in this build.

## When an approved action fails

An owner's approval is spent when Arca accepts it, before the action runs. When the action then fails, the answer is
the action's own refusal, beginning "The approval was accepted, but <verb> did not complete:". `action_approve` returns
it, the approval page shows it in place of its progress line and drops the card, and `action-proposals.jsonl` records
it as the result event's `message` beside its `code`. The page also says after fifteen seconds that Arca is still
working: a new virtual machine can take minutes.

A `service_create` whose instance was created and then failed to start is a defined state, not a half-done one. The
message carries Incus's reason and says so: the instance exists and is stopped, `instance_start` starts it once the
cause is fixed, and `service_remove` removes it. It says "stopped" only when that state was read; otherwise it says
what Incus reported. A create that Incus refused outright, leaving nothing, carries Incus's reason too. Secret file
contents are replaced in both. The service's record stays, `service_status` shows it `stopped`, and
the effect journal holds the instance's create as applied with its delete as the inverse, so no intent is left open and
later writes are not held up.

## Tests

The tests use the fake Incus and the recording shim from the Incus crate. They never open a real Incus socket, a tailnet
address or a system service. The approval tests sign with software keys from fixed seeds, listed in a signers file the test
writes itself, so no owner key or host file is read. They run under the default temporary directory, because a longer socket path breaks the unix
socket's length limit. Run them with `cargo test -p arca`.

Service database and consumer creates read `devices.root.pool` from the default project's default profile. A missing pool
refuses creation. The internal desired config carries that pool; each instance creates its root disk there. Database images
must already be present in the service project's image store at the requested fingerprint.


### The runner pool

`cargo test -p arca --test runner_pool` runs the shipped controller, `templates/github-runner-pool.sh run`, unchanged,
against a fake GitHub API (a small HTTP server in the test that checks every token and the App's signature) and a fake
`docker` (`tests/runner_pool/fake-docker.sh`, which plays a runner that takes one job with the configuration it was
started with). It needs `bash`, `curl`, `jq`, `openssl` and `flock` on the machine that runs it. It covers
registration with an App and with a token, one job per runner, replacement, admission by reserved and by free memory,
the cap, the job records and their bound, the pause after failures, the refusal of the default runner group at the
start and after a later change on GitHub, a public repository's warning, a runner that GitHub tells to update itself,
the shared layer of the runner's files and both ways back from it, a token renewed before an old runner is removed,
cleanup of exactly this pool's leftovers, the URL and API host rules, and that no credential is in any output, file
or command line. `crates/arca/tests/services/vm_secrets.rs` and `create_settings.rs` cover the kernel's half against
the fake Incus.

The controller runs in these tests with nothing on its `PATH` but the stand-ins and the programs the script declares
(`POOL_PROGRAMS`), so a program it starts without declaring fails them; the first boot checks that same list on the
machine after its install and names what is missing. No wait in these tests is without a bound, and none
ends a controller with the `kill` program. Given a negative number, to signal a process group, that program reads
only the number's first digit in procps-ng up to 4.0.4: it signals another group or, when the digit is 1, every
process of the user. A failed test once waited for ever for a controller that program had not killed. Ubuntu 24.04,
which the pool's job image is, carries that version, and so does Debian 12; Debian 13, which the pool's own machine
is, has the fix (4.0.4-9), as has procps-ng from 4.0.5. Nothing else in this repository signals a process group
through that program: the scripts use their shell's own `kill`, and the other uses name one process.
One test runs the first boot itself as far as a test can, with a
stand-in `apt-get` and two programs left missing: it must stop, name the two, and say so on the console. No test here
runs apt, so none can prove that the packages of `POOL_PACKAGES` bring those programs on a real Debian.

Seen on a real node, on the first pool (a Debian 13 machine on a cloud node, serving this repository's own CI):

- the virtual machine boots from `images:debian/13/cloud`, cloud-init runs the first-boot script and writes the
  settings, and the guest agent takes both secret files into `/etc/arca-secrets`;
- the virtual machine reaches Debian's mirrors, the registries and GitHub;
- Debian's `docker.io` installs and its daemon runs. That first boot then stopped: on Debian 13 the `docker` command
  is a package of its own, `docker-cli`, which `docker.io` only recommends, and the script did not name it. With
  `docker-cli` installed by hand and the script run again, the rest went through at the first try;
- the two pinned images pull and start as the script starts them, GitHub accepts the App's token, answers the
  runner-group requests and the registrations, and gives the registered runners jobs: four runners registered and
  ran this repository's gate;
- the controller's lines reach `service_logs` through the virtual machine's console;
- a slot's memory limit holds for what runs in the runner itself: a job that filled its slot stayed within it;
- a slot whose runner was running a job was shown as idle, and the totals said `busy=0`. The pool asked Docker for
  the runner's processes in a form Docker refuses, so no job was ever seen as running and a job's seconds counted the
  runner's wait before it. The pool now reads the runner's own "Running job" line, which it had read correctly all
  along for the job's name.

Not proven yet, by these tests or on that node:

- that a first boot with `docker-cli` in its package list finishes by itself on a fresh machine;
- that the first boot's own lines reach `service_logs`: they are now written to the machine's console as the
  controller's are, and the test reads them from a file in the console's place;
- a real runner told to update itself, inside its container, with its just-in-time registration: that it replaces
  its directories, comes back and takes its job, and prints the lines the pool looks for. The fake plays what the
  runner's own source does (`update.sh.template`, `run-helper.sh.template`);
- the shared layer of the runner's files on a real machine: the overlay mount, the runner starting with its home
  mounted, and the daemon seeing the updated tools. A machine where it does not work falls back by itself, which is
  tested; that the layered start works is not;
- jobs that use `container:` and `services:`;
- that GitHub lists a runner as `offline` when it is, and answers a repository's `private` field as its
  documentation says;
- that the memory limit and the CPU ceiling of a slot's slice hold for a job's own containers, those it starts
  through the slot's Docker daemon, and that the peak memory and CPU seconds read from the slice are the job's;
- the cache bounds on a real disk, a real job's cleanup, and behaviour over days (token renewal after an hour, a host
  restart);
- a pool inside a tenant;
- the GitHub screens named above, which follow GitHub's documentation and were not walked through in this work, and
  that a runner group's number is at the end of its page's address.

### The GitLab pool

`cargo test -p arca --test gitlab_runner_pool` runs the shipped controller, `templates/gitlab-runner-pool.sh run`,
unchanged, against a fake GitLab (a small HTTP server in the test that answers the one call the controller makes, the
token check) and a fake `docker` (`tests/gitlab_runner_pool/fake-docker.sh`, which keeps containers as directories
and plays no GitLab Runner: a test writes the runner's output lines and makes the jobs' containers itself). It needs
`bash`, `curl` and `jq`. As in the GitHub pool's tests, the controller's `PATH` holds the stand-ins and the programs
the script declares and nothing else, no wait is without a bound, and no controller is ended with the `kill` program.

It covers: every refusal of a setting, in its words; the sum of the jobs' memory against what the machine reports,
with a runner's limit and a service's share, and the refusal with every number; which secret each entry of `runners`
needs and that nothing starts without it; `config.toml` to the letter, with its modes, for one runner and for
several; a token GitLab refuses, an old-kind token that is never sent, and a new secret that comes in without a
restart of GitLab Runner; the sentences for a GitLab that cannot be reached; the status block, each finished job
recorded once from GitLab Runner's output, and `jobs_now=unknown` when Docker does not answer; sizes too large to count with; a quote, a backslash and a line
break in every setting that reaches the configuration; GitLab Runner started
again when it stops; two lanes of different sizes and `memoryOvercommitPercent`, with its warning in every status; the tailnet node's
start, a key the tailnet refuses, GitLab found by its full tailnet name in the node's own status (written as the
real one writes it: the name with its last dot, in another case, an IPv6 address first), by the node `gitlabNode`
names, or left to DNS, a named node the tailnet does not show, and GitLab's tailnet address told to the
token check, to GitLab Runner and to the jobs; the disk kept free; `privileged` said in every status; that the two
shipped definitions differ only in name, size and jobs at once and that the kernel takes a create's settings for
them; the first boot's refusal when programs are missing; and that no token and no tailnet key is in any output, file
or command line outside the three places they belong.

Three of the sentences for an unreachable GitLab (a name that does not resolve, a connection that times out, a
certificate that is refused) are driven by the exit status curl documents for each, which the test's curl wrapper
ends with; a refused connection, a GitLab that takes the connection and does not answer, an answer that is not a
GitLab's and a refused token are the real curl against the test's own server.

Not proven by these tests, and not yet on any node, since the pool has not run against a real GitLab:

- that GitLab Runner 19.5.3 runs from a `config.toml` it did not write itself (no `register`), takes the system id
  the controller wrote beside it, and reads the file again when it changes;
- that GitLab's token check answers as its documentation says (200 and 403) for a token with that system id;
- that GitLab Runner's output has the fields the controller reads (`msg`, `job`, `job_name`, `project`,
  `runner_name`, `job-status`, `failure_reason`, `duration_s`; read from its source at that release), and its
  containers the labels the controller counts;
- the limits themselves: that a job's container is capped at `jobMemory` and `jobCpu` and a service at
  `serviceMemory`, that `services_limit` fails a job with more services, and that a job's network keeps another job
  out;
- a real job: pulling an image by digest, running as root, `apt-get`, artifacts passed between stages;
- `GitLab Runner` told to finish (`docker stop --signal QUIT`) letting its jobs end;
- everything of `reach` `tailnet` beyond the commands: that the node comes up in the machine's own network with the
  capability and the device it is given, that containers' connections leave through it, and that the node's status
  lists GitLab as the test's copy of it does (the fields are read from Tailscale's source at that release);
- that the packages of `POOL_PACKAGES` bring every program of `POOL_PROGRAMS` on a real Debian 13: the first boot
  checks it on the machine and names what is missing.

### The Automagik workspace

`cargo test -p arca --test automagik_workspace` runs the shipped script, `templates/automagik-workspace.sh`,
unchanged, against a fake `docker` (`tests/automagik_workspace/fake-docker.sh`, which keeps each container as a
directory, refuses a container that joins the network namespace of one that is not running, and plays the image's
health check). The script's `PATH` holds the programs it lists for itself and nothing else. It needs `bash` and `jq`
on the machine that runs it. It covers what each of the three containers is started with (no published port and no
host network, a server that is never given its public name, the gate and the connector in the server's namespace,
the gate's entrypoint, restrictions, environment and volumes, the token as a file, the metrics listener on
loopback), a server alone when no public address is set, a Docker that does not answer (nothing is removed or
started), a keeper that cannot start (it stays up and says why), the wait for the token, an empty token, a new token replacing the connector only, each part
stopping, the gate and the connector following a replaced server, the health words for 401, 200, another code and
no answer, a start that Docker refuses, the settings that are refused (by the keeper and, on the console, by the
first boot), the first boot's packages, unit and console lines, its three failures, and that the token is in no
output, no file outside the machine's memory and no command line.

For the tailnet reach it covers the node's exact command line (a namespace of its own, never the server's,
userspace networking, the key as a file's path, the state directory), the private network and the relay's command
line, the serve file (port 443 to the relay's address, nothing on a loopback, no Funnel), a node found in the
server's or the machine's namespace being removed before anything is said of what it serves, a gate
started with the remote claim and none of the Access settings, that port 3083 is named nowhere, the key's file
modes, a new key replacing the node only, a node that is not logged in, one that waits for approval, one under
another name, the claim state said from the code file's presence and age, and that neither the key nor the claim
code is in any output or command line. One test holds the script to its one reader of the claim code, which
nothing calls.

Not proven for the tailnet reach, because it needs a real tailnet. The first of these is the one that matters most,
and no test here can stand in for it: the fake `docker` cannot show what a tailnet device reaches.

- **From a second device on the tailnet, against the node's tailnet address: port 443 answers, and ports 3081,
  3082 and any other (try 9229, and a port a program in the workspace has opened on its loopback) are refused.**
  This is what the node's own namespace is for; a node in the server's namespace would answer on all of them;
- that the relay carries the gate through the private network (a request with the public `Host` to the relay's
  address reaches the gate), and that nothing but the server and the node is on that network;

- that Tailscale's image starts as the script starts it (userspace networking with no capability, the key read
  from `TS_AUTHKEY=file:<path>`, `TS_AUTH_ONCE`, the state directory) and logs in with a tagged key;
- that its serve file is applied: HTTPS on 443 with a certificate for the tailnet name, every request handed to
  the relay with the visitor's `Host` header unchanged (Tailscale's proxy keeps it for such a target and replaces
  it for a unix-socket one, which is why the relay has an address), and WebSockets carried;
- that `tailscale status --json` on the socket the script names gives `BackendState` and `Self.DNSName` as the
  keeper reads them;
- that the node keeps its identity across a restart of the container and of the machine;
- the gate's claim and passkey sign-in under a `ts.net` name, end to end, and that the claim code's file is there
  and gone as the keeper expects;
- the Tailscale screens named in the steps, which follow Tailscale's documentation and were not walked through.

Not proven by these tests, because they need a real virtual machine, the images and a Cloudflare account:

- that the machine boots from `images:debian/13/cloud`, that cloud-init runs the script and writes the settings, and
  that the keeper's lines reach `service_logs` through the console;
- that Debian's `docker.io` and `docker-cli` install and that the pinned images pull and start as the script starts
  them: the server writing to the mounted `/data` as uid 1000, the gate on a read-only file system with no
  capability, the connector as uid 65532 on a read-only file system with no capability, reading its token with
  `--token-file` and keeping its metrics on `127.0.0.1:20241` (those three restrictions are the ones Cloudflare's
  own Helm chart gives the connector; they were not run here), and two containers joining the first one's network
  namespace;
- anything of the gate on the real image: the pinned build was not pulled or run. The entrypoint path, the health
  check's behaviour in the gate's container, the answer 401, the variable names and the rules the gate applies to
  the Access settings were read from the Automagik repository at commit `8fb2e6f`, the pinned build's;
- that nothing in the machine listens beyond loopback (the image's own services were not listed on a live machine);
- that the connector says `Registered tunnel connection`, the line the keeper looks for, and that a route to
  `http://127.0.0.1:3082` carries the workspace's pages, WebSockets and long-lived streams through Cloudflare;
- what a route to the server's port 3081 gets (403 for the interface's requests and connections, 401 for the page,
  the static files loading): that is the server's own behaviour for a `Host` it was not told to trust, as its
  makers report it for the pinned build; it was not exercised here;
- the sign-in through Cloudflare Access, end to end;
- the Cloudflare screens named in the steps, which follow the tunnel section above and Cloudflare's documentation
  and were not walked through in this work.

### Functions

`crates/arca/tests/functions.rs` runs the six verbs against the fake Incus: the card and the create it binds, the one
name space, secrets by name, each outcome of a run, the timer, the removal, and that ten runs leave the effect journal
and every other file of the state directory byte for byte as they were. `crates/arca-incus/tests/function_run.rs`
holds the writer of a run to a function's instance: a service's instance is refused there before any write, and the
service path still forces no stop. `crates/arca-netproject` checks a function's rules against a service's.

What the fake cannot say was read from a real Incus by an ignored test,
`function_run::proof::a_real_incus_runs_a_function_shaped_container` in `crates/arca-incus`, which does nothing unless
`ARCA_REAL_INCUS_PROOF=1` and runs only in a project whose name begins with `arcak-scratch-`: how long a start and a
stop take, that Incus refuses to start an OCI container with a read-only root, that the console log is empty after a
clear and holds a run's output after it, that Incus reports no exit status, and that two runs with no pause between
them can find the older run's output in the log. A second one beside it,
`function_run::proof::a_real_incus_gives_a_container_its_images_root_again`, read that a snapshot restore puts the
instance's config back too (so it would undo a secret set since), and that a rebuild from the image removes what a run
wrote, keeps the config and the uuid, takes 55 ms from the store's copy, and fetches the image again when the store no
longer holds it. Not proven anywhere yet: a function on a live node with its network
card and rules, a link that carries a real connection, and a run killed at its memory limit.

### Service wiring

`service_connect`, `service_disconnect` and `service_secret_set` update config and ACLs through the effect journal. Arca
currently applies the environment on the instance's next start; it does not start or restart instances for these calls.
Droidd restarts a running consumer and starts a stopped service once its environment is complete. Adding that state-write
capability needs a separate review of the closed journal vocabulary.

Publication needs `serveHost` (this process's serve DNS name) and `tailnet` (the owner's tailnet label, without `.ts.net`)
in node config. Both must agree, for example `host.example.ts.net` and `example`. They are never discovered by a lookup.
Adopted services support list, status and console logs. Their write verbs return an explicit `adopted` refusal.

### Omni verbs

`omni_instance_create`, `omni_pair` and `omni_status` reach the API of a service made from the `omni-api` template, and
nothing else of it. Each takes the service's `name`; an `instance` is an Omni instance id (a UUID) or a plain name, never a
path. Arca makes four fixed requests: create an instance, list the first 100, read one instance's status, and ask for a
pairing code. It calls the service's address at its template's `droiddPort`, the one port the service's ACL opens to the
bridge gateway. It follows no redirect, reads at most 1 MiB of an answer and waits at most 15 seconds for one.

Every request sends `x-api-key` from the service's `OMNI_API_KEY` secret, which `service_create` makes. A `telegram`
instance adds the owner's `telegram_bot_token` secret, and `omni_pair` sends the owner's `whatsapp_phone_number`. The owner
sets both over the owner's socket (`service_secret_set`); a doctor token cannot. A verb that needs an unset secret refuses
and names the slot, before anything is sent. The number must be `+` and 10-15 digits, because Omni echoes a number it
refuses. A result shows the number masked (`+55*********21`).

Results keep a fixed set of fields, and they are untrusted, because Omni and the people on its channels wrote them. Every
string in a result or an error is redacted of the three secrets: the number in any formatting, without its `+`, inside a
WhatsApp JID, and with or without a Brazilian mobile's ninth digit. Any Omni key or bot token is redacted too. A refusal
that quotes Omni carries `untrusted: true` beside its `refusal` code. Another pairing code for the same instance is refused
while one lasts. Any attempt that reached Omni holds the next one off for 60 seconds, even a failed one; an attempt that
reached nothing does not. Either refusal is `rate`, with the wait in `retryAfterSeconds`. The verbs read the service record and its secrets only. They change no desired state, send
nothing to Incus and run no process. An adopted service, or a service of another template, is refused.

The exec audit is local, private and separate from `journal_list`. A crash can leave a key in the private tmpfs key directory
until unpublish or instance stop, and can leave recordings until the next startup sweep. A crash before the operation id is
synced leaves a recording the sweep cannot name; deleting the instance removes it. A real tailnet join requires the owner's
key and is a deployment acceptance check, not a hermetic test.

### Owner approval requests

An assistant that needs the owner's approval for a call (a `service_remove`, say) asks for it with
`owner_approval_request`: `kind` `arca`, the `action` (`delete`, `publish`, `spend` or `enroll`), the call's `target`, and its `args`
without `approval`. Arca checks the fields as the statement will carry them and refuses, in plain words, a request no
statement can carry: a missing action ("an arca request needs action"), a bad target, args that are not an object, args
over 4096 bytes or nested more than 8 levels, an args key longer than 64 characters, a control or bidi character, or an
argument that reads as an approval string. A target that names a delete plan (`plan-` and its digest) is refused: a plan
is approved through its own card on the passkey page. Everything a request keeps must come back through
`owner_approval_list` as that verb's result allows, so a request the list could not show is refused when it is made.
Kind `upgrade` is an identity phone's step: it is refused with `upgrade_not_here` and approved through droidd's MCP.
A token keeps at most 5 pending requests and Arca at most 20 (past either, the refusal is `rate`); an identical pending request from the same token is the same
request. The request names the passkey page in `dashboard`. Requests and statuses are a doctor's or an operator's, and
`owner_approval_list` is a viewer's too. The list's arguments and fields are untrusted, because the assistant wrote them.

The request waits on the passkey page under "Asked by your assistant", drawn as text. One tap on Approve asks Arca for the
statement (this host, the request's action, target and argument digest, a fresh nonce, valid 15 minutes), the passkey signs
it, and Arca checks the proof as the call will, spending nothing. The owner has 120 seconds from the tap. Only passkeys made
for the page's address are offered; a passkey made for an earlier address is refused with `passkey_rp_changed`, and the
owner adds it again on the page. Approval is the owner's built-in passkey only. Decline is final and needs no passkey:
whoever can open the passkey page at its address can decline a request there, as they can decline an action proposal,
because declining only takes away. The page's request posts need the page's exact `Origin` and an `application/json` body.

`owner_approval_status` (with `waitMs` up to 30000 to wait for the decision) answers the state. The approval string goes
to the token that asked, never to another token or to the page, and the assistant passes it to its call, which spends it
once. With a device-bound passkey that counts its signatures (synced passkeys report 0), approvals must be spent in
the order they were tapped: once a later approval is spent, an earlier one is refused as `SignCountRegressed`. A spend that
is refused after the signature has verified (for another host, action, target, arguments or time) also uses up that
approval's count, so the owner taps again.

**Enroll.** A service that runs on the node can ask the node's owner to let it make someone an owner of that service,
which is how the service's first owner is made without a terminal. The service shows the person a code and asks with
`owner_approval_request`, `action` `enroll`: the `target` is the code (6 to 12 uppercase letters and digits, refused
otherwise) and the `args` are `{service: <name>}` and nothing else. The passkey page shows the request as "Approve
<service> to make the person showing code <CODE> an owner", with the code large, so the owner compares it with the code in
front of them before they tap. An `enroll` approval changes nothing in Arca. The token that asked reads it from
`owner_approval_status` and spends it with `enroll_confirm {target, args, approval}` (a doctor's or an operator's verb, as
the request is). Arca checks the approval with its own verifier against `enroll`, that code and those args, spends its
nonce once and answers `{confirmed: true}`; the service then makes the owner. Only the token whose request the owner
approved can spend the approval, and only an approval a request holds: another token's call, or a statement built and
signed outside a request, is refused and spends nothing. A second spend, another code, other args, an approval for
another action, or one past its 15 minutes is refused with `owner_approval`, and an `enroll` approval is refused by every
other call. Each confirmation is a line in `approval-requests-audit.jsonl` (`owner_enroll_confirmed`, naming the request,
the code, the args' digest and the token that asked, never the approval). The passkeys that can approve a request are
this node's own and those of its `approval.peerKeys` sources; see "More than one page host".

A pending request expires after 60 minutes. The requests live in `approval-requests.json` in the state directory (mode
0600), which every request verb, the page's list and each page step reads and, when anything changed, rewrites whole (a
temporary file renamed over it). On that read an approved request's approval string is removed from the file once its
nonce is spent or its statement expires, and the request is kept without it. A request that is neither pending nor holding
an unspent approval is kept while it is among the newest 50 of the last 24 hours (what the list shows) and dropped after.
Until the next read, a spent or expired string stays in the file, where it can no longer be spent. The rewrite does not
scrub the disk blocks the older file used. Each request, approval and decline is saved first, then written as a line in
`approval-requests-audit.jsonl` (append-only, never pruned) naming the request, its digest and the requester, never an
approval string, a credential or a challenge.

**What is recorded, and no delivery.** There is no verb that delivers an approval into a request. The page's
`POST /approve/requests/approve` is the one way a request becomes approved, and it takes the passkey's answer over the
statement `begin` minted: an SSH signature or a whole approval string sent as `proof` is refused as `invalid`, and the
request stays pending. An SSH approval goes to the call itself (`service_remove`, say), which spends it with no request.
The approval's audit line names the passkey and how it signed
(`approved:<id>:arca:<action>:<target>:params=<digest>:requester=<token>:key=passkey:<fingerprint>:via=passkey:count=<n>:synced=<0|1>`).
A spend, by either kind of key, with or without a request, is recorded as one line in `approval-nonces.jsonl` (its
time and nonce). The spend's line names no key and no signers file, so which key approved is read from the approval's
audit line, and an SSH spend has none.

## Continuous integration

The repository's gate runs on GitHub Actions: on Arca's own runner pool (the shipped `github-runner-pool` template;
see "A pool of GitHub Actions runners") for the events we trust, and on GitHub's hosted runners for a pull request
from a fork.

**What runs.** `.github/workflows/ci.yml` has four jobs, each on a runner of its own:

| Job | Command | Pool class |
|-----|---------|------------|
| `fmt` | `cargo fmt --all --check` | `standard` |
| `clippy` | `cargo clippy --workspace -- -D warnings` | `large` |
| `test` | `cargo test --workspace` | `large` |
| `canvas` | in `packages/arca-canvas-dsh`: `npm ci`, `npx playwright install --with-deps chromium`, `node --test` (on the pool with `--test-concurrency`, see below) | `standard` |

A fifth, `runners`, comes first and only decides where those four run (below).

The Rust toolchain is the one `rust-toolchain.toml` names (1.97.1, with clippy and rustfmt), installed by rustup
1.29.3, which `scripts/ci-setup.sh` fetches and checks by SHA-256. Node is 24.11.0, the version the canvas package's
README develops with. The two actions, `actions/checkout` (v7.0.1) and `actions/setup-node` (v7.1.0), are named by
commit, with the version in a comment beside it; to move one, change both together. The workflow's token can read the
repository's contents and nothing else, and a job that passes its time limit is stopped. A newer push to a pull
request replaces that pull request's run that is still going. Nothing else is replaced: every commit that reaches
`dev` is checked to the end, and so is every entry of the merge queue.

**Which events may use the pool.** The repository is public, a job on the pool runs whatever the commit says, and a
job can reach the pool's GitHub credential ("Who can reach what"). So the pool's labels are used only by events that
people with write access to this repository cause:

- a push to `dev`;
- the merge queue;
- a person with write access starting the workflow by hand ("Actions", the workflow, "Run workflow");
- a pull request whose branch is in this repository, opened by a person and last pushed to by a person.

People are not the only ones who can push a branch here: a bot or an app with write access to the contents can too
(Dependabot and Renovate do), and what they bring is other people's code, such as a new release of a dependency with
its build script. So a pull request that a bot opened (its author's type is `Bot`), or that a bot pushed to (the
actor's name ends in `[bot]`), does not use the pool, whatever repository its branch is in. Once a person with write
access has merged it, it reaches the pool like every other commit on `dev`.

A pull request from a fork or from a bot runs the same four jobs on GitHub's hosted runners (`ubuntu-latest`), and so
does any event that is later added to the workflow without being added to that list. The rule is one condition, in the
`runners` job of `ci.yml`; the other jobs take their runner from its answer and name no label themselves. There is
no `pull_request_target` trigger, and none may be added.

That condition routes; it does not guard. For a pull request GitHub reads the workflow files of the pull request
itself, so a fork can change the condition, or add a workflow of its own that asks for the pool's labels. Three things
outside the workflow files stop that, and all three have to be in place:

1. **Before any pool is registered for this repository**, in the repository's "Settings", "Actions", "General", under
   "Approval for running fork pull request workflows from contributors", choose "Require approval for all external
   contributors". With the other two choices, anyone who has had one change merged runs workflows with no approval,
   and from the moment a runner with the pool's labels exists such a run can land on it. Check the setting again
   whenever a pool is created or replaced.
2. Nobody approves a waiting run without reading the pull request. One that adds or changes a file under `.github/`,
   or that names `self-hosted` or the pool's labels, is not approved.
3. The pool that serves this repository serves nothing else, and no client's jobs run on it. Its `settings.url` is
   this repository, or, registered for the organisation, its runner group admits this repository alone. Such a group
   has to admit public repositories, so the pool's status line carries its `WARNING` about them for as long as it
   runs; for this pool that warning is expected, and this list is the answer to it.

What is left if all of that fails is what the pool gives every job: one job per runner, in a virtual machine that is
the wall between the job and the machine Arca runs on.

**On the pool.** `fmt` and `canvas` ask for `[self-hosted, linux, x64, arca]`, the `standard` class; `clippy` and
`test` ask for `[self-hosted, linux, x64, arca-large]`, the `large` class. The pool that serves this repository has
two slots of each, `standard` with 2 CPUs and 4GiB and `large` with 6 CPUs and 10GiB, so all four jobs run at the
same time once the machine has memory for all four slots ("Sizing so that every slot fits"). Those are not the
shipped template's sizes (its two `large` slots have 6 CPUs and 9GiB). The workflow carries the two CPU numbers
(`CI_SLOT_CPUS`); change them there when the pool's classes change.

Every job has a time limit, 30 minutes for `cargo test` and 15 for the others: a job that hangs gives its slot back
then, and without a limit GitHub lets it hold the slot for six hours.

A pool job starts in a small image with no compiler, so `scripts/ci-setup.sh` installs what is missing first: the
toolchain, `gcc`, and the programs the tests start. A job sees every CPU of the pool's virtual machine and not its
slot's ceiling, so the script tells cargo, the tests and Node's test runner (`--test-concurrency`, or six Chromiums
would start in a 4GiB slot) how many CPUs the job has. It reads the number from the job's
own control group when the runner puts a limit there. Today's pool does not (the limit is on the slot's group, which
the job cannot read) and gives the job no variable with it, so on the pool the script uses the class's number from
the workflow (`CI_SLOT_CPUS`). On a hosted runner it tells them nothing.

rustup, the cargo registry, the target directory, npm's cache and the test browser are kept in the pool's
`POOL_CACHE_DIR`, and Node in the runner's tool cache, so the next job in the same slot does not start from nothing.
Each slot has its own, and the pool empties one that passes the slot's share of `dependencyCache`. The setup step
prints what the cache holds; if the target directory alone comes close to that share, give the pool that serves
this repository a larger `dependencyCache`.

**Comparing with GitHub's runners.** `.github/workflows/bench.yml` is started by hand only. It runs the `test` job
twice on one commit, on `ubuntu-latest` and on the pool, and writes for each, in the run's summary: the CPU model,
the CPUs and memory the job sees, and the seconds the toolchain, the build and the tests took. `pool_label` chooses
the class (`arca-large`, which `test` uses, or `arca`). With `pool_cache` off, both build from nothing, which
compares the machines; with it on, the pool uses its kept cache as `ci.yml` does, which shows what an everyday run
costs.
