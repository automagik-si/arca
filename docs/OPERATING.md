# Operating the `arca` binary

The `arca` binary runs one node's one-shot host reads, mints scoped bearer tokens, runs the recording shim, and serves the
Arca MCP. Every command that needs the node reads one JSON config file, given with `--config`.

## Commands

| Command | Reads | Prints |
|---------|-------|--------|
| `arca watch --once --json --config <file>` | every instance in every project, through Incus GET | the `watch_report` result |
| `arca metrics --json [--sample-for <s>] --config <file>` | the host and the configured pools | the `node_metrics` result |
| `arca store check --config <file>` | nothing from the node: it opens a scratch store in the state directory and removes it | the SQLite version and the pragmas the store reads back, as JSON; see "The store" |
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
| `kernel.projects` | no | The project family the kernel may write in. With projects named, the writer serves only those projects. With none, the provider reads only, and a removal cannot reach Incus. |
| `kernel.scratch` | no | When `true`, every kernel project must start with `arcak-scratch-`. |
| `kernel.droiddProjects` | yes | droidd's projects. Required, and it must not be empty while `kernel.projects` is not. |
| `kernel.serviceCidr` | no | Service bridge `<ipv4-network>/24`, outside every `kernel.reservedCidrs` range. Required for Postgres creation and probe consumer add; no default. |
| `kernel.reservedCidrs` | no | Subnets reserved for droidd. Read by later steps only. |
| `kernel.adopt` | no | `[{ "instance": "<name>", "to": "arcak-adopt-<name>" }]`: the containers `arca adopt` may take from the default project, each with its adoption project. A config that lists one is for `arca adopt` alone, and `serve` refuses it. |
| `kernel.attachNetworks` | no | The bridges in the default project an adoption project's grant may name, though they carry no kernel mark. Nothing writes them. An adoption whose `eth0` is on a bridge this list leaves out plans that bridge's grant as an allowance. |
| `link` | no | The node link: `name`, `forward` and `pairMinutes`. See "The link". |
| `approvalHost` | no | The host name every owner approval statement names. When it is not set, the system's host name is used. |
| `approvalOrigin` | no | The page origin the owner approval page is served at: `https://` plus a lowercase host name and at most a non-443 port, with no path (for example `https://arca.example.com`). Its host is the WebAuthn rp id; it is not the host an approval statement names. |
| `canvasOrigin` | no | The loopback DSH origin the remote canvas proxies: an `http://` literal loopback IP and port, with no path. Default `http://127.0.0.1:3081`; any other origin refuses the config before `serve` listens. |

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
- an adopted instance is not a plain name (`^[a-z][a-z0-9-]{0,56}$`), its `to` does not start with `arcak-adopt-` or is
  not in `kernel.projects`, an instance or a `to` is listed twice, an attach network is not a plain name, or
  `kernel.scratch` is `true` while `kernel.adopt` is set.

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
not create as `unknown` without touching them. `service_logs` returns console lines as untrusted text with stored
secrets redacted and a capped, truncated result. The five shipped templates live in `templates/`, with their pinned
images and vendored npm lockfile.

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

`tools/list` advertises each verb's MCP `outputSchema`: the natural result schema, wrapped under its `output.key` when the result is not an object, with `truncated` and (for an untrusted verb) `untrusted` added and every object level open. The OpenRPC document keeps the natural schemas.

`tools/call` answers a success with a short summary text block and `structuredContent` that carries the result, the untrusted note when the verb is untrusted, and `truncated` only when the result was cut; `_meta` carries `droidd/untrusted` and `droidd/truncated`. A refusal is a JSON-RPC error with `data.refusal`. A tool-level failure that is not a refusal is `isError: true` with `<code>: <message>` and `_meta` `droidd/error`.

## The MCP server

`serve` answers `POST /mcp` with JSON responses (streamable HTTP, protocol version `2025-11-25`) on the unix socket. The verbs
in this build are `arca_status`, `node_metrics`, `watch_report`, `alerts_list`, `token_list`, `journal_list`, `plan_list`, `plan_decline`, `approval_submit`, the Postgres and ontology verbs, and `service_create`, `service_list` and `service_status`.

The unix socket serves only a peer whose credentials carry the server's own uid. Any other peer is closed before a byte of
its request is read.

- `arca_status` reports the node id, mode (`kernel` when kernel projects are configured, `watch-only` otherwise), uptime,
  whether Incus answers, `appliedSeq`: the applied desired-state document's sequence, or `null` before any document applies,
  and `metricsSource` (`local` or `remote`) with the reason it was chosen. See "Host metrics".
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
- `plan_list` lists each waiting delete plan with the statement its owner signs. See the next section.
- `plan_decline` retains a waiting plan's objects. A decline is final and survives a restart.
- `approval_submit` checks an owner's approval for a waiting plan and, when it holds, deletes the plan's objects once.

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
   host name, without case. A match is local; a mismatch is remote.
3. If `/1.0` cannot be read, or it does not carry a server name, or the local host name cannot be read, the socket path
   decides: the standard local daemon sockets (`/var/lib/incus/unix.socket` and `/var/lib/incus/unix.socket.user`) are local
   and every other path is remote.

The chosen mode and a value-free reason are logged once at startup and returned by `arca_status` as `metricsSource`
(`local` or `remote`) and `metricsSourceReason`. The reason never carries a host name. A proxy whose upstream is the local
daemon, such as the recording shim, therefore needs no `"local": true`: its server name matches the local host name. A
container that mounts the host's Incus socket at the standard path is still remote when its own host name differs from the
Incus server name, so the sampler never reports the container's `/proc` as the managed host's.

- **Local daemon.** The sample is this machine's: `cpu.cores` and `cpu.usedPct`, the `load` averages,
  `memory.totalBytes`, `memory.availableBytes` and `memory.usedBytes`, PSI, CPU temperature, GPU busy, swap and NIC rates come
  from the node's `/proc` and `/sys` trees. This is the reading the sampler has always taken.
- **Remote Incus.** The host totals come from the node's own Incus, `GET /1.0/resources`: `cpu.cores` is `cpu.total`
  (threads), `memory.totalBytes` is `memory.total`, and `memory.usedBytes` is `memory.used`.
  `memory.availableBytes` is `memory.total - memory.used`. `cpu.coresKnown` is `true`. No local `/proc` or `/sys` reading is
  reported: `cpu.usedPct`, every `load` average, PSI, CPU temperature, GPU busy, swap and NIC rates are `null`. `cpu.reason`
  and `load.reason` say `not available for a remote host`. The pools, instance counts, incus-osd reading and SMART state
  still come from the same Incus, so they describe the remote machine too.
- **The resources read fails.** On a remote node the totals are `null` and `cpu.coresKnown` is `false`; `cpu.cores` stays an
  integer in the schema, so its `0` is a placeholder a consumer must not show as a count. `cpu.reason` and `memory.reason`
  say `the Incus resources read failed`. The node never falls back to its own `/proc`: a failed read is unknown, not the wrong
  machine. `load.reason` still says `not available for a remote host`.

A mean over a window that holds both successful and failed reads averages the successful ones and takes its reason from the
newest successful read, so a late failure does not label averaged figures. A window with no successful read is `null` with
the failure reason. The failure and provenance fields are additions: `cpu.coresKnown`, `memory.usedBytes`, `cpu.reason`,
`load.reason` and `memory.reason`. Every field the earlier schema carried keeps its name and meaning, and a local node's
figures do not change.

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

The owner approves that text in the namespace `arca-kernel-approve`, with an SSH key or a passkey. The approval string is the
base64 of the statement, a dot, and the base64 of the proof. A proof whose first byte is `{` is a passkey proof. Any other proof
is an armoured SSH signature.

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
- `GET /approve/plans` returns the waiting plans with the same fields as `plan_list`.
- `POST /approve/setup-code` issues a one-time 8-digit setup code to the machine's own screen while no passkey is trusted.
- `POST /approve/approve` takes `{ "plan": "<digest>", "approval": "<string>" }` and runs the plan through the same kernel
  approval path as `approval_submit`.
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
A cut answer ends with a final `event: arca-cut` whose data names `idle`, `total`, `cap` or `upstream`
(`{"arcaCut":{"reason":"..."}}` line for the NDJSON/JSON media types); a client disconnect closes the upstream connection
without a cut event. `GET /approve/canvas/chat/history` is the same bounded three-second forward to
`/arca-canvas/chat/history`. Every canvas route carries the CSP with `frame-ancestors 'none'`, `nosniff`, `no-referrer`, and
`Cache-Control: no-store` on every canvas body, including refusals.

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

## Alerts

On every 5-second tick, `serve` reads the node, evaluates eight rules and writes the points and any alert events to
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
value stays at or below `threshold - clearMargin` for `clearForS` seconds. Every rule's default is `forS` 60 and `clearForS`
120, so a condition that lasts 59 seconds never fires.

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

A yes-or-no rule reads 1 when it is bad and 0 when it is fine, so its threshold of 0 fires on 1. The thresholds are a
starting point. Tune them after a week of data.

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
  `unknown: true` while its input is missing, and no event is written; if the input stays unknown for `clearForS`, the alert
  resolves once with reason `no data` instead of firing forever. A fresh reading before then clears the unknown and keeps the
  alert firing.
- `alerts_list.unknown` lists each unknown rule and label set, with reason `missing` (no reading yet) or `stale` (too old).
- The one exception is `subject-gone`. A fresh, known reading that no longer has a firing alert's label set (a deleted pool
  or instance, or a drive that left the drive list) resolves that alert once.
- When Incus does not answer, the instance and pool readings are unknown. A firing alert stays firing and marked unknown
  until `clearForS` passes without a fresh reading, then resolves once with `no data`. `node_metrics` still lists `projects`
  and `pools` as empty arrays, because its released schema has no null, so those lists alone do not show an outage.
- A drive listed with an ID but no usable SMART result (its SMART block is missing, null or disabled) is unknown, not passed.
  A firing `smart-warning` for it is held as unknown, and resolves with `no data` after `clearForS` without a result; a drive
  that was OK and then loses its SMART result stays listed as unknown while the holding window lasts. A SMART block with a
  non-empty `error` makes the whole SMART reading unknown.
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

## Tests

The tests use the fake Incus and the recording shim from the Incus crate. They never open a real Incus socket, a tailnet
address or a system service. The approval tests sign with software keys from fixed seeds, listed in a signers file the test
writes itself, so no owner key or host file is read. They run under the default temporary directory, because a longer socket path breaks the unix
socket's length limit. Run them with `cargo test -p arca`.

Service database and consumer creates read `devices.root.pool` from the default project's default profile. A missing pool
refuses creation. The internal desired config carries that pool; each instance creates its root disk there. Database images
must already be present in the service project's image store at the requested fingerprint.


### Service wiring

`service_connect`, `service_disconnect` and `service_secret_set` update config and ACLs through the effect journal. Arca
currently applies the environment on the instance's next start; it does not start or restart instances for these calls.
Droidd restarts a running consumer and starts a stopped service once its environment is complete. Adding that state-write
capability needs a separate review of the closed journal vocabulary.

Publication needs `serveHost` (this process's serve DNS name) and `tailnet` (the owner's tailnet label, without `.ts.net`)
in node config. Both must agree, for example `host.example.ts.net` and `example`. They are never discovered by a lookup.
Adopted services support list, status and console logs. Their write verbs return an explicit `adopted` refusal.

The exec audit is local, private and separate from `journal_list`. A crash can leave a key in the private tmpfs key directory
until unpublish or instance stop, and can leave recordings until the next startup sweep. A crash before the operation id is
synced leaves a recording the sweep cannot name; deleting the instance removes it. A real tailnet join requires the owner's
key and is a deployment acceptance check, not a hermetic test.
