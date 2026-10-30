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
| `arca shim --upstream <socket> --listen <socket> --log <file>` | forwards GET, refuses the rest | the shim summary, on SIGTERM |

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
  "listen": { "unix": "/run/arca/arca.sock", "tailnet": null },
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
| `tokens.hashDir` | yes | The directory of hash files. See below. |
| `kernel.projects` | no | The project family the kernel may write in. With projects named, the writer serves only those projects. With none, the provider reads only, and a removal cannot reach Incus. |
| `kernel.scratch` | no | When `true`, every kernel project must start with `arcak-scratch-`. |
| `kernel.droiddProjects` | yes | droidd's projects. Required, and it must not be empty while `kernel.projects` is not. |
| `kernel.reservedCidrs` | no | Subnets reserved for droidd. Read by later steps only. |
| `approvalHost` | no | The host name every owner approval statement names. When it is not set, the system's host name is used. |

The config is refused, with exit 2, when:

- a key is unknown, a required key is missing, or a value has the wrong form;
- `approvalHost` is not a host name: one letter or digit, then up to 252 letters, digits, dots or hyphens;
- `listen.tailnet` is not a tailnet address: it must be a fixed port, not a wildcard, inside `100.64.0.0/10` or
  `fd7a:115c:a1e0::/48`, and not on port 80, 443, 7443 or 9443;
- a kernel project is `bench`, `default`, or a name in `kernel.droiddProjects`;
- `kernel.projects` is not empty and `kernel.droiddProjects` is empty;
- `kernel.scratch` is `true` and a kernel project does not start with `arcak-scratch-`.

No command reads Incus before the config passes these checks.

### Tailnet listener

Serving on a tailnet address is a separate step. The owner approves it before `serve` is started with a tailnet address, and
until then the node runs on its unix socket alone. A peer outside the tailnet ranges is closed with no bytes sent.

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
  report has no entries to compare, and every kernel-marked object is reported as drift.
- `token_list` lists each token's id and scope as they were when the server started. It never lists a secret or a digest.
- `plan_list` lists each waiting delete plan with the statement its owner signs. See the next section.
- `plan_decline` retains a waiting plan's objects. A decline is final and survives a restart.
- `approval_submit` checks an owner's approval for a waiting plan and, when it holds, deletes the plan's objects once.

`serve` removes its socket on SIGTERM or SIGINT, then exits 0. A socket that another server answers on is left alone.

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
| `secrets/` | the secret files the writer reads, if any |

`serve` refuses a state directory that is not mode 0700 before it creates the node key, and a state file that is a link, is not
mode 0600, does not parse, or is not signed by this node's key. Each refusal is a config or runtime failure, and nothing is
served.

A removal of a desired entry whose objects are class B (deleted by an Incus delete) does not delete them. It waits as a delete
plan until an owner approves it. A plan lapses fifteen minutes after it was made.

`plan_list` returns each waiting plan with `plan` (its digest), `entry`, `createdAt`, `expiresAt`, `objects`, `paramsDigest`, and
`statement`. The statement is the exact text the owner signs, built fresh for each listing. Its keys are sorted, it has no
spaces, and it ends without a newline:

```
{"action":"delete","expiresAt":"<ten minutes after issue>","host":"<host>","issuedAt":"<now>","nonce":"<32 hex digits>","paramsDigest":"<digest of {\"plan\":<plan>}>","target":"plan-<plan>"}
```

The owner signs that text with a hardware key in the namespace `arca-kernel-approve`, and the approval string is the
base64 of the statement, a dot, and the base64 of the armoured signature. The owner key is read from the signers file
`/etc/arca/owner-signers`, root-owned, which lists the owner's keys for that namespace. A file that is not there holds no key,
so every approval is refused with `NoVerifier`.

`approval_submit` takes the plan digest and the approval. It refuses, and spends nothing, when:

- the signature does not verify against a listed key (`BadSignature`), or no key is listed (`NoVerifier`);
- the statement names another host, action, target or arguments (`WrongHost`, `WrongAction`, `WrongTarget`, `WrongParams`);
- the statement is issued more than five minutes ahead of the node's clock (`IssuedAhead`), has expired (`Expired`), or
  runs for more than fifteen minutes (`TtlTooLong`);
- its nonce is spent (`Replayed`).

A plan that waits under no digest is `NoSuchPlan`. A valid approval for such a plan is still checked, so a spent approval reads as
`Replayed`. The approval string is never stored or echoed, and an approval spends its nonce once, before its delete runs.

**A scratch kernel can take its owner keys from a file named in `ARCA_TEST_OWNER_SIGNERS`.** That is honoured only when
`kernel.scratch` is `true`, every kernel project starts with `arcak-scratch-`, and the build has debug assertions. Any other kernel
refuses to start while the variable is set. It exists so that the tests run the real verifier against keys they write
themselves.

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

## Tests

The tests use the fake Incus and the recording shim from the Incus crate. They never open a real Incus socket, a tailnet
address or a system service. The approval tests sign with software keys from fixed seeds, listed in a signers file the test
writes itself, so no owner key or host file is read. They run under the default temporary directory, because a longer socket path breaks the unix
socket's length limit. Run them with `cargo test -p arca`.
