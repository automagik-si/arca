# Operating the `arca` binary

The `arca` binary is Automagik Arca's one program. It runs one node's one-shot host reads, mints scoped bearer tokens, runs
the recording shim, and serves the Arca MCP. Every command that needs the node reads one JSON config file, given with
`--config`.

## Commands

| Command | Reads | Prints |
|---------|-------|--------|
| `arca watch --once --json --config <file>` | every instance in every project, through Incus GET | the `watch_report` result |
| `arca metrics --json [--sample-for <s>] --config <file>` | the host and the configured pools | the `node_metrics` result |
| `arca store check --config <file>` | nothing from the node: it opens a scratch store in the state directory and removes it | the SQLite version and the pragmas the store reads back, as JSON; see "The store" |
| `arca templates install --config <file> [--replace]` | nothing from Incus: it writes the shipped service templates, each with its entrypoint or first-boot script, into `<stateDir>/templates` | `{ "dir", "installed", "unchanged", "kept" }` as JSON; a file that differs is kept unless `--replace` is given |
| `arca probe consumer add --name <n> --image <fingerprint> --config <file>` / `remove --name <n> --config <file>` | locks desired state; add reads the default profile root pool with GET, then proposes a consumer entry | entry id and sequence |
| `arca serve --config <file>` | runs until SIGTERM or SIGINT; SIGHUP reloads the token store only | nothing |
| `arca link serve --config <file>` | runs the node's link until SIGTERM or SIGINT | nothing on a clean run |
| `arca link pair --config <file> [--minutes <n>]` | the running link's status | `{ "link", "expires" }` as JSON on stdout; a terminal QR on stderr |
| `arca link join [--] <link> --config <file>` | pairs with the endpoint in the link | `{ "id", "name" }` as JSON |
| `arca link peers --config <file>` | the paired peers file | the peers array, as JSON |
| `arca link remove [--] <name> --config <file>` | removes every peer with that name through the running link, or from disk when stopped | `{ "removed": <count> }` as JSON |
| `arca call [--peer <name>] <verb> --config <file> --token-file <file>`, with `--args <json>` or `--args-file <file>` | one tool call against the local node, or through a paired peer with `--peer` | the verb's result, as JSON |
| `arca token mint --scope <s> --id <id> --out <file> [--tools <comma list>]` with `--config <file>` or `--hash-dir <dir>` | nothing | the id, scope and tool list |
| `arca token revoke <id>` with `--config <file>` or `--hash-dir <dir>` | the token's hash file | the id and `revoked: true` |
| `arca shim --upstream <socket> --listen <socket> --log <file> [--log-max-bytes <n>]` | forwards GET, refuses the rest | the shim summary, on SIGTERM; the summary so far on stderr, on SIGUSR1 |
| `arca adopt plan\|apply\|release\|verify\|status --instance <name> ... --config <file>` | one listed container, and its adoption record | the plan, the steps run, or the check made; see "Adopting a container" |
| `arca --version`, `arca version` or `arca -V` | nothing | one line: `Automagik Arca <version> (<short commit>, <target triple>)`; the commit reads `unknown` when the binary was built with no git checkout |
| `arca service import plan --name <service> --config <file>` | one instance `kernel.importServices` lists, and the instance listing, through Incus GET | the import's dry run as JSON: every check, the one write and the pre-image digest; see "Importing a service another manager created" |

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
| `listen.tailnet` | no | One tailnet address, or `null`. See below. |
| `listen.proxy` | no | `{ "addr": "<ip>:<port>", "peer": "<ip>" }`, or `null`: one address for a reverse proxy on the same host, served to the single peer address `peer`. See below. |
| `tokens.hashDir` | yes | The directory of hash files. See below. |
| `kernel.projects` | no | The project family the kernel may write in. With projects named, the writer serves only those projects. With none, the kernel is watch-only: the reconciler's provider reads only, service writes are refused, no GPU device writer exists, and a removal cannot reach Incus. |
| `kernel.scratch` | no | When `true`, every kernel project must start with `arcak-scratch-`. |
| `kernel.droiddProjects` | yes | droidd's projects. Required, and it must not be empty while `kernel.projects` is not. |
| `kernel.serviceCidr` | no | Service bridge `<ipv4-network>/24`, outside every `kernel.reservedCidrs` range. Required for Postgres creation and probe consumer add; no default. |
| `kernel.postgresImage` | no | A digest-pinned image (`name@sha256:<64 hex>`). A proposed `postgres_create` uses exactly this image and may not choose another; without it, a proposed database create is refused. |
| `kernel.reservedCidrs` | no | Subnets reserved for droidd. Read by later steps only. |
| `kernel.adopt` | no | `[{ "instance": "<name>", "to": "arcak-adopt-<name>" }]`: the containers `arca adopt` may take from the default project, each with its adoption project. A config that lists one is for `arca adopt` alone, and `serve` refuses it. |
| `kernel.attachNetworks` | no | The bridges in the default project an adoption project's grant may name, though they carry no kernel mark. Nothing writes them. An adoption whose `eth0` is on a bridge this list leaves out plans that bridge's grant as an allowance. |
| `kernel.bridged` | no | Bridged mode: `{ "bridges": ["<host bridge>", ...], "pool": "<Incus storage pool>", "limits": { "virtualMachines": n, "cpu": n, "memory": "<size>", "disk": "<size>" } }`, every key required. The host bridges, which already exist, that a virtual machine's NIC may join. Without `kernel.serviceCidr` the node is bridged: it runs virtual machines only and Arca creates no Incus network on it. With `kernel.serviceCidr` the node is mixed. See "Bridged nodes and virtual machines" and "Mixed nodes". |
| `kernel.tenants` | no | `{ "capacity", "maxTenants", "defaultCaps", "createRate", "front"? }`: the node may hold tenants. It needs `kernel.serviceCidr` and is refused beside `kernel.adopt`. See "Tenants". |
| `kernel.vmLimits` | no | `{ "virtualMachines": n, "cpu": n, "memory": "<size>", "disk": "<size>" }`, every key required: the virtual-machine caps of a node that has `kernel.serviceCidr` and no `kernel.bridged`. Without it such a node creates no virtual machine. It is refused beside `kernel.bridged`, which states the same caps in its `limits`. See "Mixed nodes". |
| `kernel.importServices` | no | `[{ "name": "<service>", "project": "<project>", "cidr": "<ipv4-network>/24" }]`: the services another manager created that `service_import` may take over where they stand. Each `project` must be listed in `kernel.droiddProjects`, and `cidr` is that project's service subnet. Empty by default: nothing is imported and nothing is written in another manager's project. See "Importing a service another manager created". |
| `link` | no | The node link: `name`, `forward` and `pairMinutes`. See "The link". |
| `approvalHost` | no | The host name every owner approval statement names. When it is not set, the system's host name is used. |
| `approvalOrigin` | no | The page origin the owner approval page is served at: `https://` plus a lowercase host name and at most a non-443 port, with no path (for example `https://arca.example.com`). Its host is the WebAuthn rp id; it is not the host an approval statement names. |
| `canvasOrigin` | no | The loopback DSH origin the remote canvas proxies: an `http://` literal loopback IP and port, with no path. Default `http://127.0.0.1:3081`; any other origin refuses the config before `serve` listens. |
| `approval.trustOwnerKeysFrom` | no | Absolute paths of other kernels' owner passkeys files on this machine, which this kernel also trusts, read-only, for action approvals only (never to change its own passkeys). See "Approving on another kernel of this machine". |
| `approval.origins` | with `trustOwnerKeysFrom` | The page origins (`approvalOrigin` form) a passkey from those files may answer from. A line for any other origin is not trusted. |
| `approval.linkedHosts` | no | `[{ "name": "<host>", "socket": "<path>", "tokenFile": "<path>" }]`: the kernels on this machine whose proposals this kernel's approval page lists and relays approvals to. |

The config is refused, with exit 2, when:

- a key is unknown, a required key is missing, or a value has the wrong form;
- `approvalHost` is not a host name: one letter or digit, then up to 252 letters, digits, dots or hyphens;
- `approvalOrigin` is not `https://`, a lowercase host name, and at most a non-443 port, with no path;
- `canvasOrigin` is not `http://`, a literal loopback IP, and a nonzero port, with no path;
- `listen.tailnet` is not a tailnet address: it must be a fixed port, not a wildcard, inside `100.64.0.0/10` or
  `fd7a:115c:a1e0::/48`, and not on port 80, 443, 7443 or 9443;
- `listen.proxy.addr` is a wildcard, a tailnet address (IPv4-mapped included), port 0, or port 80, 443, 7443 or 9443;
  `listen.proxy.peer` is missing or a wildcard; or `listen.proxy` carries any other key;
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

No command reads Incus before the config passes these checks.

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

`arca token revoke <id>` removes the token's hash file. With `--config`, it sends `SIGHUP` to a live `arca serve` for that
config: it checks `<stateDir>/serve.pid` only when its pid is a live `arca serve`, and otherwise finds the same process in
`/proc`. The server writes that pid file only after its own unix socket accepts connections and removes it only when it still
holds its own pid. A second start that fails to bind leaves the running server's pid file unchanged. The answer carries
`reloadSignalled`; `token_list` drops the token and a call with it is 401 without a restart. With `--hash-dir` alone no state
directory or serve is known, so no signal is sent (`reloadSignalled: false`) and a running server keeps the token until it is
restarted or signalled. The hash files stay mode 0400. The signal uses the host's `kill` command.

The secret file holds the secret and one newline. A client reads that file, strips the newline, and sends the secret as
`Authorization: Bearer <secret>`. Keep the secret out of command lines, logs and shell history.

A service is one template-derived container in the service project. Its template lives in
`<stateDir>/templates/<name>.json`; `service_create` allocates `.10`-`.239` on the service bridge, creates the
service profile and NIC ACL, records the desired entry, and starts the container when every environment secret it
needs is available. `service_list` and `service_status` derive state from Incus and report instances that Arca did
not create as `unknown` without touching them. A row's `ports` are the container's own ports;
`lanPort` is the host proxy port when the template sets `lan`, or null. Only `lanPort` is a host
listener. `service_logs` returns console lines as untrusted text with stored
secrets redacted and a capped, truncated result. The eight shipped templates live in `templates/`, with their pinned
images and vendored npm lockfile. Two of them, `workspace-vm` and `github-runner-pool`, are virtual machines (see
"Bridged nodes and virtual machines", "Mixed nodes" and "A pool of GitHub Actions runners").

`arca templates install --config <file>` installs the shipped templates in one step: each template's JSON and the
script its entrypoint or its first boot reads (`postgres-autopg.sh` for `postgres-autopg`, `github-runner-pool.sh` for
`github-runner-pool`), from the build itself, so no checkout is needed. An entrypoint's `{ "file": "<name>" }` names a plain file in the templates directory: letters, digits, dots,
dashes and underscores, not starting with a dot, so no path. A template whose script is missing is refused by
`service_create` with the file it looked for (`templates/<name>`). A template whose `image` is `local:<alias>`
(`automagik`; `automagik-server` is an OCI image) needs that image in the service project's image store first:
`service_create` refuses with the alias when the project's image listing answers and no image there carries it,
before anything is created. A listing that cannot be read decides nothing, and the create goes on.

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

Every destructive verb (`destructiveHint: true`: `approval_submit`, `action_approve`, `postgres_delete`, `service_remove`, `service_import`, `service_release`, `gpu_detach`, `storage_add`, `snapshot_delete`, `instance_stop`, `instance_restart`) takes an optional boolean `confirm`: absent or `true` runs the call as without it, `false` is refused (`NotConfirmed`, "confirm is false: nothing was removed and no approval was spent") before the handler runs, and the door takes it out first, so it never enters a statement's `paramsDigest`, a plan, a proposal, the journal or the desired state.

## The MCP server

`serve` answers `POST /mcp` with JSON responses (streamable HTTP, protocol version `2025-11-25`) on the unix socket. The verbs
in this build are `arca_status`, `node_metrics`, `watch_report`, `alerts_list`, `token_list`, `journal_list`, `plan_list`,
`action_propose`, `plan_decline`, `approval_submit`, `action_approve`, `peer_keys_sync`, the Postgres and ontology verbs, `service_create`, `service_list` and
`service_status`, `gpu_attach` and `gpu_detach`, `storage_list`, `storage_add`, `snapshot_create`, `snapshot_list` and
`snapshot_delete`, and `instance_start`, `instance_stop` and `instance_restart`.

The unix socket serves only a peer whose credentials carry the server's own uid. Any other peer is closed before a byte of
its request is read.

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

`serve` listens on the unix socket, and on the tailnet and proxy addresses when the config names them. If any listener
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
`instance_restart` (see [Starting, stopping and restarting instances](#starting-stopping-and-restarting-instances)). `action_propose` takes `{action, args}`; `args` is checked against that verb's own parameter schema, with
the inline `approval` field removed because the proposal replaces it. An unknown action is an invalid call and leaves no
record. A proposal cannot choose a container image: `postgres_create` uses `kernel.postgresImage`, and an explicit image that
differs from it is refused. `service_create` names a template from the kernel's catalog, whose name the schema constrains.
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
unexpired proposals wait. A ninth is refused with `TooManyProposals`, and the eight that wait are left alone. The same
`argsDigest` returns the record that already waits instead of adding another; that deduplicated request records nothing.
One token may record thirty new proposals in ten minutes, and a deduplicated or refused proposal counts nothing against
that window; the thirty-first new proposal gets `RateLimited`. A proposal lapses fifteen minutes after it was made. The
journal writes `proposed` when the record is made, `approved` when its proof verifies, `declined` on a decline, and `result`
after the one run. On start every `proposed` record is validated again: the verb's parameter schema without `approval`, its
semantic checks, and the kernel's configured `kernel.postgresImage`, never the image the file itself carries. A record that
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

The GETs need no token and admit no `Origin` or the configured one. Apart from the canvas chat POST described below, the
POSTs admit only the exact configured `Origin` and refuse any other before reading the body. `/mcp` and every other MCP route
keep refusing every `Origin`.

With `approvalOrigin` set, the same page origin also serves the remote canvas at exactly these URLs: `GET /approve/canvas`
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
   running `serve`; it is never shown in the page, a log or an HTTP answer.
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
first failed commit, or at the first commit that takes over one second, and it clears after 120 seconds of commits that work.
It shows in `alerts_list`, in the HEALTH line and as `arca_alert_firing`.

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
other path is 404. Pass the token in the `Authorization` header:

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
| `arca_store_write_failures_total` | counter | | the failed writes since the server started |
| `arca_sample_timestamp_seconds` | gauge | | when the newest host sample was taken, in epoch seconds |

Notes on the names above:

- **PSI is a ratio.** `arca_node_psi_avg10_ratio` is the kernel's percentage divided by 100. The `mem-psi-high` threshold in
  `alerts.json` is still in percent, so a threshold of 25 is `0.25` on `/metrics`. There is no `cpu` `full` series.
- **Drift.** See "Drift" above. `arca_drift_entries` has two kinds only.
- **TPM and update.** See "incus-osd readings" above.
- **Alerts.** A firing alert has one series, with its rule, its severity and its own labels. A pending, resolved or disabled
  alert has no series, so an absent series means the alert is not firing. The `store-write-failed` alert appears here too.
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
- `store-write-failed` fires at once, and `arca_store_write_failures_total` counts the failure. A commit that takes over one
  second also counts as a failure, although its events are kept.
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

`image` is `images:<alias>` (the public images server, with an optional `digest` fingerprint) or `local:<alias>`.
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
or configure its address inside the guest. `service_list` and `service_status` show the address the guest reports, or an empty
`ipv4` when the guest reports none.

Ownership, the journal and approvals are the same as for a container. `service_create` needs a doctor token, can be
proposed with `action_propose`, and writes the same journal records. A proposal is checked when it is made, so the
owner is never asked to approve a create the node is sure to refuse: the template's kind must fit the node's mode, the
mode must fit the desired state, and the bridge must be an allowed one. A bridge the request leaves out is filled in
then. The owner's card says "Create the virtual machine <name> from template <template> on the host bridge <bridge>"
and lists the bridge and the MAC. The caps and the MAC are checked when the approved create runs, because both can
change while a proposal waits. `service_remove` needs the owner's approval and retires the virtual machine through the same delete plan.
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

1. `service_create` creates a machine whose template declares secrets **stopped**, so it never boots without them.
2. `service_secret_set` with `{"name", "key", "value"}` stores the value in Arca's private `secrets/` directory, as for
   a container, and answers `{"ok": true}`. The value is 8 to 4096 characters; for a virtual machine it may have line
   breaks (a private key in PEM form is several lines), and no other control character. Nothing is sent to the machine
   yet.
3. `instance_start` with `{"project": "arcak-svc", "instance": "<name>"}` starts the machine, waits for its guest agent
   (up to 120 seconds: a new machine boots first) and writes each stored secret into the guest as the file
   `/etc/arca-secrets/<KEY>`. The directory is root's with mode 0700 and each file is root's with mode 0600. The answer
   names the secrets it wrote.

A new value: run `service_secret_set` again, then `instance_start` again. A start of a machine that is already running
restarts nothing and writes the files again, so a secret is replaced without an approval and without interrupting the
machine; the programs in the guest read the file when they next need it. `instance_restart` writes them too.

When the guest agent does not answer in time (the image has none, or the machine is still booting), the verb is refused
with `InstanceSecretsNotWritten`: the machine is running, its secrets are not in it, and running `instance_start`
again writes them.

Where a virtual machine's secret is, and is not:

- It is in Arca's `secrets/` directory, and on the machine's own disk in `/etc/arca-secrets`. The second copy is why a
  machine that Incus starts by itself after the host restarts, or that restarts from inside, still works with nobody
  asked. Root inside the machine can read it, and a snapshot or a copy of the machine's disk carries it.
- It is not in the instance's config in Incus (so `incus config show` does not show it), in Arca's desired state, in
  the effect journal, in `instance-actions.jsonl` (which names the secrets written), in `exec-audit.jsonl`, or in any
  answer or refusal. `service_logs` replaces a stored value with `[secret]` when it fits on one line.

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

## A pool of GitHub Actions runners

The shipped `github-runner-pool` template is one virtual machine that runs your GitHub Actions jobs on your own
hardware. Inside it a small controller keeps a set of slots. Each slot registers one runner with GitHub, runs exactly
one job on it in a fresh container, removes it, and starts over with a new runner. The virtual machine is the wall
between your jobs and the machine Arca runs on: nothing outside the pool changes when a job runs.

You need a node that may create a virtual machine of the template's size (16 CPUs, 32GiB of memory, a 200GiB disk: the
node's `kernel.vmLimits`, or `kernel.bridged.limits`, must allow it), the shipped templates installed
(`arca templates install`), and a GitHub organisation, or one repository, that you administer. The Arca steps are
Arca's own verbs: ask the assistant you use with Arca to run them. `service_create` and `instance_start` can be shown
on the approval page; `service_secret_set` cannot be proposed, so it needs an assistant that holds a `doctor`-scope
token, and the credential passes through that assistant's conversation.

**1. The template for your organisation.** The settings are part of the template, so each pool has its own copy.
Whoever looks after the node copies `<stateDir>/templates/github-runner-pool.json` to a new name, for example
`example-ci.json`, sets `"name"` to `example-ci`, and sets `settings.url` to your organisation
(`https://github.com/example-org`) or to one repository (`https://github.com/example-org/example-repo`). This is the
one step that is a file and not a verb: Arca has no verb that edits a template. The script `github-runner-pool.sh`
stays as it is, beside it.

| Setting | Meaning |
|---------|---------|
| `url` | The organisation or the repository the runners belong to. |
| `runnerGroupId` | The runner group the runners join. `1` is GitHub's default group. |
| `classes` | The label sets, each with its slot size: `name` (a-z, 0-9), `labels` (what a job's `runs-on` asks for), `cpu`, `memory` and `slots` (how many runners of this class the pool keeps). |
| `maxSlots` | The cap: never more than this many slots at once, whatever the classes add up to. |
| `reserveMemory` | Memory kept back for the virtual machine itself. The slots share what is left. |
| `dockerCache`, `dependencyCache` | The disk the Docker layer caches and the dependency caches may use, over all slots. |
| `runnerImage`, `dockerImage` | The runner's and the Docker daemon's public images, pinned by digest. |

The virtual machine's own size is the template's `limits` and `rootSize`. The shipped numbers fit together: four
`standard` slots of 2 CPUs and 4GiB and one `large` slot of 8 CPUs and 8GiB are 16 CPUs and 24GiB, and 4GiB is kept
back. That is 28GiB of the machine's 32GiB on purpose: a virtual machine sees a little less memory than its size, and
a slot that does not fit whole is never started. Settings are
read when the pool is created and are fixed from then on (see "What to know").

**2. A credential, on GitHub.** A GitHub App is the better one: its key does not expire, and it can do nothing but
manage runners.

- Select your profile picture, then "Your organizations", then "Settings" beside the organisation, then "Developer
  settings", "GitHub Apps" and "New GitHub App". Give it a name (for example `example-runner-pool`) and any homepage
  URL. Under "Webhook", deselect "Active". Under "Permissions", open "Organization permissions" and set "Self-hosted
  runners" to "Read and write" (for a pool that serves one repository: "Repository permissions", "Administration",
  "Read and write"). Create the app.
- On the app's page, copy its "Client ID" (it begins `Iv`). In the private keys part of the page, create a key
  (GitHub's guide calls the button "New key"). A file in PEM form is downloaded: that is the private key. Treat it
  like a password.
- Select "Install App" and install it on the organisation (or on the one repository).

Or a fine-grained personal access token: your own "Settings", "Developer settings", "Personal access tokens",
"Fine-grained tokens", "Generate new token"; resource owner the organisation; organisation permission "Self-hosted
runners", "Read and write" (for one repository: repository permission "Administration", "Read and write"). A token
expires on the date GitHub shows, and the pool stops registering runners that day.

**3. Arca: `service_create`** with `{"name": "ci-pool", "template": "example-ci"}`. The pool is created and stays
`stopped`, because it has no credential yet.

**4. Arca: `service_secret_set`**, for the App twice:
`{"name": "ci-pool", "key": "GITHUB_APP_CLIENT_ID", "value": "<the client ID>"}` and
`{"name": "ci-pool", "key": "GITHUB_APP_PRIVATE_KEY", "value": "<the whole content of the key file>"}`. The key keeps
its line breaks; a key pasted on one line works too. For a token, once:
`{"name": "ci-pool", "key": "GITHUB_TOKEN", "value": "<the token>"}`. Arca answers `{"ok": true}` and no answer, list
or log shows a value again.

**5. Arca: `instance_start`** with `{"project": "arcak-svc", "instance": "ci-pool"}`. The virtual machine starts and
the answer names the secrets written into it. Its first boot installs Docker and downloads the two images, which
takes a few minutes.

**6. Check.** `service_logs` with `{"name": "ci-pool"}` shows the pool's own lines (below). On GitHub, the
organisation's "Settings", "Actions", "Runners" lists runners named `ci-pool-standard-0-...` as idle.

**7. Use it.** A job runs on the pool when its `runs-on` asks for labels that one class has all of, for example
`runs-on: [self-hosted, linux, x64, arca]`, or `[self-hosted, linux, x64, arca-large]` for the large slot.

### Reading the pool

Every minute the controller writes a status block to the virtual machine's console, which is what `service_logs`
returns, and one line when a job finishes:

```
arca-runner-pool: job slot=0 class=standard labels=self-hosted,linux,x64,arca name="build" result=Succeeded seconds=132 peak_memory_mib=1832 cpu_seconds=201.4 runner=ci-pool-standard-0-1a2b3c4d
arca-runner-pool: slot 0 standard 4096MiB/2cpu busy 41s ci-pool-standard-0-5e6f7a8b job "test"
arca-runner-pool: slot 1 standard 4096MiB/2cpu idle 12s ci-pool-standard-1-9c0d1e2f
arca-runner-pool: slot 4 large 8192MiB/8cpu queued (pausing 40s after a failure)
arca-runner-pool: class standard last 412 jobs: seconds avg 130, peak memory avg 1210 MiB max 3105 MiB, cpu-seconds avg 95, cpus used avg 0.7
arca-runner-pool: totals busy=3 idle=1 starting=0 queued=1 jobs_done=412 failures=2 memory_reserved=16384MiB of 27955MiB slots_cap=5
```

| In the totals | Meaning |
|---------------|---------|
| `busy`, `idle` | Runners with a job, and runners registered and waiting for one. |
| `starting` | Slots registering, starting or cleaning up. |
| `queued` | Slots the pool wants and has not started. The slot's own line says why: the cap, memory no slot has reserved, memory that is free now, a pause after a failure, or no credential. |
| `jobs_done` | Jobs finished since the controller started, whatever their result. |
| `failures` | Failures of the pool itself since the controller started: GitHub refused the credential or a registration, a container did not start, a runner went away before it took a job. A job that fails its own steps is a job done, with `result=Failed`. |

The same job lines are kept in the virtual machine, in `/var/log/arca-runner-pool/jobs.tsv`, one job per line with
its finish time, slot, class, labels, name, result, seconds, peak memory in bytes, CPU seconds and runner name.

**Sizing runners from real numbers.** The `class` line is what the last 2000 jobs of a class used, beside the size the
class gives them. A class whose peak memory stays far below its `memory` can be given less, and more slots then fit;
one whose `max` is at its `memory` has jobs that were cut short for memory. `cpus used avg` far below `cpu` says the
CPU ceiling is generous. New sizes go into the template, and a pool created from it has them.

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
  from each other the way two virtual machines are: the slot's daemon runs privileged, so a job that sets out to can
  reach the virtual machine itself, and through it the other slots, their caches and the pool's GitHub credential.
  The pool keeps one customer's jobs tidy and apart by default; it does not defend them against each other. Run only
  workflows you trust on it (do not let pull requests from forks use these labels), and prefer the GitHub App, whose
  only power is managing runners. One shared daemon would have reused more cache and separated jobs less; a daemon
  without root would separate them more and refuse the jobs that need privileged containers.
- **Caches.** Each slot keeps its Docker layer cache, a tool cache (`RUNNER_TOOL_CACHE`, which the `setup-*` actions
  fill) and a directory for anything else a workflow wants to keep (`POOL_CACHE_DIR`, `/var/cache/pool` in the job).
  They are on the virtual machine's disk and survive jobs and restarts. Each slot has an equal share of `dockerCache`
  and of `dependencyCache`; a cache that passes its share after a job is emptied, and the log says so. A cache is
  shared by the jobs that run in the same slot one after another, so a job can leave something there for a later one.
- **Where the credential lives.** In Arca's private `secrets/` directory, and in the virtual machine in
  `/etc/arca-secrets`, readable by root there. It is not in Incus's config, in Arca's journals or in any log. The
  controller makes every short-lived token from it by itself and prints none.
- **A new credential.** `service_secret_set` again, then `instance_start` again: the start restarts nothing, writes
  the new value into the virtual machine, and the controller uses it for the next token.
- **Stopping and restarting.** `instance_stop` and `instance_restart` need your approval. Runners that are idle are
  taken off GitHub first; a job that is running is lost and GitHub reports it as failed. After the machine Arca runs
  on restarts, the pool starts by itself.
- **Pins.** The virtual machine is `images:debian/13/cloud`, the public Debian 13 image with cloud-init, which is what
  runs the first boot and grows the disk. It is named by its alias and not by a fingerprint: the public server
  rebuilds it daily and keeps the last few builds only, so a fingerprint would stop resolving within days. Docker in
  the virtual machine is Debian's own package. The runner is `ghcr.io/actions/actions-runner` 2.338.2 and the slot's
  daemon is `docker` 29.9.3-dind, both by digest. GitHub stops giving jobs to a runner that is more than 63 days
  behind the newest release unless it updates itself, which the runner does when a job arrives; a later Arca release
  moves the pin.
- **Changing a pool.** Settings, sizes and pins are read when the virtual machine is created. To change them, create a
  second pool from the changed template, check it, and remove the first: both can serve the same labels meanwhile.
- **The network.** The pool only calls out: to GitHub, to the image registries and to Debian's mirrors. It has no port,
  and nothing can connect to it.

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
| `tenant_create {id?, caps?}` | Creates a tenant. Inside the node's policy (caps at or under `defaultCaps`, room in `capacity`, `maxTenants` and `createRate`) it needs no approval, from a token whose tool allowlist names `tenant_create`. Caps above a default run only from a proposal the owner approved (`action_propose`, then the approval). A refusal names the limit; past the rate it gives the time of the next allowed create. Without `id` one is made: 8 lowercase base32 characters. |
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

- a service's address comes from the tenant's block, and its caps are checked before Incus is asked, so a refusal names
  the cap (`limits.instances`, `limits.containers`, `limits.virtual-machines`, `limits.cpu`, `limits.memory`,
  `limits.disk`);
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
the cap, the job records, the pause after failures, cleanup of what an earlier controller left, and that no
credential is in any output, file or command line. `crates/arca/tests/services/vm_secrets.rs` covers the kernel's half
against the fake Incus.

Not proven by these tests, because they need a real virtual machine and a real GitHub organisation:

- that the virtual machine boots from `images:debian/13/cloud` on a real node, that cloud-init runs the first-boot
  script and writes the settings, and that the guest agent takes the secret files;
- that Debian's Docker package installs, and that the two pinned images pull and start as the script starts them: the
  slot's privileged daemon, the runner sharing its network and socket, jobs that use `container:` and `services:`;
- that GitHub accepts the App's token and the registration requests as the fake does, lists a runner as `offline`
  when it is, and gives a registered runner a job;
- that the memory limit and the CPU ceiling of a slot's slice hold for a job's own containers, and that the peak
  memory and CPU seconds read from the slice are the job's;
- that the controller's lines reach `service_logs` through the virtual machine's console;
- the cache bounds on a real disk, a real job's cleanup, and behaviour over days (runner self-update, token renewal
  after an hour, a host restart);
- the GitHub screens named above, which follow GitHub's documentation and were not walked through in this work.

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
reached nothing does not. The verbs read the service record and its secrets only. They change no desired state, send
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
A token keeps at most 5 pending requests and Arca at most 20; an identical pending request from the same token is the same
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
