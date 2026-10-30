# Operating the `arca` binary

The `arca` binary runs one node's one-shot host reads, mints scoped bearer tokens, runs the recording shim, and serves the
Arca MCP. Every command that needs the node reads one JSON config file, given with `--config`.

## Commands

| Command | Reads | Prints |
|---------|-------|--------|
| `arca watch --once --json --config <file>` | every instance in every project, through Incus GET | the `watch_report` result |
| `arca metrics --json [--sample-for <s>] --config <file>` | the host and the configured pools | the `node_metrics` result |
| `arca serve --config <file>` | runs until SIGTERM or SIGINT | nothing |
| `arca call <verb> --config <file> --token-file <file>`, with `--args <json>` or `--args-file <file>` | one tool call against the node that `serve` runs | the verb's result, as JSON |
| `arca token mint --scope <s> --id <id> --out <file>` with `--config <file>` or `--hash-dir <dir>` | nothing | the id and the scope |
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
| `incus.pools` | no | Storage pools whose space the sampler reads. |
| `identityPhones` | no | Instance names the watch flags with `identity: true`. |
| `listen.unix` | yes | The MCP socket. Created with mode 0600. |
| `listen.tailnet` | no | One tailnet address, or `null`. See below. |
| `listen.proxy` | no | `{ "addr": "<ip>:<port>", "peer": "<ip>" }`, or `null`: one address for a reverse proxy on the same host, served to the single peer address `peer`. See below. |
| `tokens.hashDir` | yes | The directory of hash files. See below. |
| `kernel.projects` | no | The project family the kernel may write in. With projects named, the writer serves only those projects. With none, the provider reads only, and a removal cannot reach Incus. |
| `kernel.scratch` | no | When `true`, every kernel project must start with `arcak-scratch-`. |
| `kernel.droiddProjects` | yes | droidd's projects. Required, and it must not be empty while `kernel.projects` is not. |
| `kernel.reservedCidrs` | no | Subnets reserved for droidd. Read by later steps only. |
| `kernel.adopt` | no | `[{ "instance": "<name>", "to": "arcak-adopt-<name>" }]`: the containers `arca adopt` may take from the default project, each with its adoption project. A config that lists one is for `arca adopt` alone, and `serve` refuses it. |
| `kernel.attachNetworks` | no | The bridges in the default project an adoption project's grant may name, though they carry no kernel mark. Nothing writes them. An adoption whose `eth0` is on a bridge this list leaves out plans that bridge's grant as an allowance. |
| `approvalHost` | no | The host name every owner approval statement names. When it is not set, the system's host name is used. |

The config is refused, with exit 2, when:

- a key is unknown, a required key is missing, or a value has the wrong form;
- `approvalHost` is not a host name: one letter or digit, then up to 252 letters, digits, dots or hyphens;
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

## Tokens

A token is a 256-bit secret of the form `arca_` followed by 43 base64url characters. The server never stores the secret. It
reads one hash file per token from `tokens.hashDir`:

```
scope=viewer
sha256=<64 lowercase hex digits>
```

The file name is `<id>.hash`, and the id is 1 to 64 lowercase letters, digits or hyphens. The scopes are `viewer`, `doctor`
and `operator`.

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

`arca token mint` writes both files at mode 0400, refuses to overwrite an existing id or output file, and removes its hash
file if it cannot write the secret. It needs the token directory to exist and to have no group or other write bit. Create
the directory and mint into it before the mode is tightened to 0500, because a 0500 directory cannot take new files.

```sh
arca token mint --scope viewer --id viewer-1 --hash-dir /etc/arca/tokens --out "$HOME/.config/arca/viewer-1.secret"
```

`--config <file>` may name the token directory instead, as `tokens.hashDir` of the node config, and then `--hash-dir` is not
needed. The config is loaded and checked first. When both are given, `--hash-dir` must name that same directory; a different
one is refused with exit 2 and nothing is written. Giving neither is refused with exit 2.

**Run `arca token mint` as the user that runs `serve`.** A mint run as another user writes hash files owned by that user, and
the server refuses every file it does not own, so the load fails until those files are minted again as the server's user.

The secret file holds the secret and one newline. A client reads that file, strips the newline, and sends the secret as
`Authorization: Bearer <secret>`. Keep the secret out of command lines, logs and shell history.

### Who may call what

| Caller | Result |
|--------|--------|
| no `Authorization`, a malformed bearer or an unknown token | 401 |
| a `doctor` calling `token_list` | 403 |
| a `viewer` calling `token_list`, or any verb that writes (`plan_decline`, `approval_submit`) | 403 |
| any request carrying an `Origin` header | 403, before the body is read |
| a call the caller may make | 200, with the verb's result or a JSON-RPC error |

`tools/list` shows a caller only the verbs it may call.

## The MCP server

`serve` answers `POST /mcp` with JSON responses (streamable HTTP, protocol version `2025-11-25`) on the unix socket. The verbs
in this build are `arca_status`, `node_metrics`, `watch_report`, `token_list`, `plan_list`, `plan_decline` and
`approval_submit`.

The unix socket serves only a peer whose credentials carry the server's own uid. Any other peer is closed before a byte of
its request is read.

- `arca_status` reports the node id, `watch-only` mode, uptime, whether Incus answers, and `appliedSeq` as `null`.
- `node_metrics` takes `minutes` from 1 to 60. The server samples the host every 5 seconds into an hour-long ring, on its own
  thread. A call reads the last samples published and never waits on Incus. Until the first sample is taken, `latest` and
  `mean` are `null`.
- `watch_report` reads every project and instance through Incus GET. Live desired-state entries do not exist yet, so the
  report has no entries to compare, and every kernel-marked object is reported as drift. A sweep sends a GET for every
  project and instance, so the server keeps its answer for 30 seconds and runs one sweep at a time: a call within 30
  seconds of the last sweep gets that sweep's report, and calls that arrive during a sweep wait for it. Every answer
  carries `ageMs`, the milliseconds since its sweep ended. A failed sweep is not kept.
- `token_list` lists each token's id and scope as they were when the server started. It never lists a secret or a digest.
- `plan_list` lists each waiting delete plan with the statement its owner signs. See the next section.
- `plan_decline` retains a waiting plan's objects. A decline is final and survives a restart.
- `approval_submit` checks an owner's approval for a waiting plan and, when it holds, deletes the plan's objects once.

`serve` listens on the unix socket, and on the tailnet and proxy addresses when the config names them. If any listener
fails, all of them stop and `serve` exits 1. On SIGTERM or SIGINT it stops every listener, removes its socket and exits 0.
A socket that another server answers on is left alone.

## Calling the server

`arca call <verb>` makes one call and prints its result. The token is read from `--token-file`, which must be a regular file
with no group or other permission bit, and holds the secret and one newline. Its text is never printed. The arguments are a JSON
object: `--args <json>` for the ones that carry no approval, or `--args-file <file>`, which must be private in the same way.

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
`MemoryMax=67108864`. A second `watch_report` within 30 seconds carries an `ageMs` above 0. The service is stable after 24
hours active with `NRestarts=0`, a `MemoryPeak` under `MemoryHigh`, and a live shim summary of `refused: 0`.

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
   container the move would change or that needs what no allowance admits (see `cmd_adopt::preflight`).
2. `arca adopt apply --instance svc1 --plan-digest <digest> --dry-run --config adopt.json` takes no lock and sends no write.
   It prints the same allowances.
3. Stop the service that holds the state directory (`systemctl --user stop arca-watch.service`), then run `apply` without
   `--dry-run`. It takes `desired.lock` and runs CreateProject, Stop, Snapshot, PinNic, LocalizeRoot (only when the plan
   localizes the root), Move, Mark and Start. Each step
   writes an intent line before its request and an outcome line after it to `<stateDir>/adoptions.jsonl` (mode 0600,
   synced). A fresh plan's digest must be the one given.
4. `arca adopt verify --instance svc1 --port 8080 [--http-path /] [--wait <s>] --config adopt.json` checks that the instance
   runs under the uuid the record holds, with the pre-image's MAC and IPv4 on `eth0`, then connects to `<ipv4>:<port>`
   and needs an HTTP answer below 500 within 10 s. It tries again every 2 s for up to `--wait` seconds (60 by default, at
   most 3600), so a lease still being given or a service still starting is waited for, and then reports the last failure.
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
`security.acls*` keys, a `hwaddr` or `ipv4.address` other than the running MAC and IPv4, a disk on a pool other than the
root, a host path a comma-separated list cannot hold exactly, any other device type, and any device or config key but
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

**Resume.** When `apply` stops at any point, whether a kill, a crash or a failed step, run the same `apply` with the same
digest again. It goes on from the plan in the record, never from a fresh preflight, which would refuse an instance that has
moved. A step whose intent is the last line runs again without a second intent, and a step that had landed sends nothing.
The digest must be the recorded plan's. Only one adoption may be open at a time, and a complete adoption applied again sends
nothing.

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
DeleteProject all run. An adopt step left at its intent is closed as failed first, and each release step reads before it
sends. When the instance is in its adoption project without the pin, or with a localized root that RestoreRoot would not
take off, the release, and its dry run, refuse before any write, since those steps would refuse only after the instance was
stopped and unmarked. The pin and the snapshot are kept. DeleteProject is the adoption's one DELETE: it deletes the adoption
project only when it carries this adoption's mark and holds no instance, and a project already gone is the step done. A
release recorded before releases deleted the project ends at ReleaseStart, and `release` finishes it with DeleteProject.
Once a release has begun, `apply` refuses to resume and `release` finishes it. A release done in full, run again, sends
nothing. `verify` then checks the instance in `default`.

Exit codes: `2` for a refusal before any write: an instance the config does not list, a preflight check, a stale digest, a
digest that is not the recorded plan's, a record that holds no such adoption, another open adoption, a resume after a
release has begun, a release of an instance that lost its pin or whose localized root RestoreRoot would keep, a verify before
the adoption or release completed, or a bad
`--port`, `--http-path` or `--wait`. `1` for a held
lock, a failed step (the record names it) or a failed verify.

## Tests

The tests use the fake Incus and the recording shim from the Incus crate. They never open a real Incus socket, a tailnet
address or a system service. The approval tests sign with software keys from fixed seeds, listed in a signers file the test
writes itself, so no owner key or host file is read. They run under the default temporary directory, because a longer socket path breaks the unix
socket's length limit. Run them with `cargo test -p arca`.
