# Changelog

What changed in each release of Automagik Arca, in plain words. The newest release is first.

## Unreleased

<!-- The release writes the next section here by itself: one line per change that landed, from its commit subject.
Write under this comment only what a reader needs beyond those lines; the release puts its heading above it. -->

Two new templates keep a cold copy of one node's NATS streams on a second node, over a private link between the two: `nats-primary` is the `nats` template with its leaf node port 7422 also offered on the node's link address (`privatePort`, so its create needs `kernel.serviceListenAddresses` and the owner's card), and `nats-replica` is a NATS of its own whose one leaf node remote dials that port, through `kernel.serviceDestinations`, with the address and the primary's leaf password as two secrets the owner sets. Each stream on the replica is a mirror of the primary's, made with the nats CLI, and OPERATING.md gives the set-up, how to read the mirrors' lag, and the promotion when the primary is lost. The `nats` template is unchanged. No pair has run on two nodes yet.

## 0.27.0 (2026-11-13)

- A service template whose entrypoint runs a shell script (postgres-autopg) is created and starts on Incus 7.6, which
  refuses a line break in `oci.entrypoint`, whichever liblxc that Incus links: Arca sends the script as one word with no
  whitespace in it (its base64, with `${IFS}` for each space), which the container's own shell decodes and runs. This
  needs Incus 7.3 or later. An entrypoint argument with a line break that is not such a script is refused before
  anything is created. A service made earlier from a script entrypoint cannot start or be updated on Incus 7.6: remove
  it and create it again.

A service template may set `privatePort`, which offers one port of the service on one host address that the node's config names (`kernel.serviceListenAddresses`), for a second node over a private link, and on no other address; the owner's create card names the address and the port. See OPERATING.md.

The `nats` template now writes its own configuration (every message fsynced, a JetStream domain, monitoring on 8222, and a leaf node listener on 7422 that only the user `leaf` may join, though no link reaches it yet) and pins NATS 2.14.10, which keeps the fsync promise that 2.10 does not for a block compaction; clients on 4222 still log in with nothing and `NATS_URL` is unchanged. A `nats` made before keeps its command line; `service_update` can move its image and keeps `/data` (a nats made up to 0.22.0 has none and is refused), but NATS documents no jump from 2.10 to 2.14 and its rollback puts 2.10 back over a store 2.14 may have rewritten, so a new service made from the template, which has the new settings, is the safe path (see "A message bus: nats" in docs/OPERATING.md).

- Merge PR 156: nats template v2 on NATS 2.14.10 by digest, every message fsynced, a JetStream domain, a leaf node listener and monitoring, and CI runs its script under the image's shell
- Merge PR 159: a service can listen on one private address of its node, named in the node config, for a peer node over a private link
- Merge PR 158: a script entrypoint reaches the container as one word with no whitespace, so it starts on every liblxc, and the fake models how Incus and liblxc split it

## 0.26.0 (2026-11-13)

A service template may set `noNewPrivileges: true`, so a setuid or setcap binary inside the container cannot raise
privileges. It is off unless set, so every existing template and service is unchanged. It needs
`restricted.containers.lowlevel=allow` on the service project, a step on the node that Arca does not take; see
OPERATING.md. `service_update` gives an existing service the key, or takes it away, to match its template, with a
restart (which interrupts it) and a health check, and no rebuild, so its data stays where it is.

- The guide names `admin` tokens beside `doctor` and `operator`, and says what puts the declared host network back
  and what does not.

- Merge PR 155: the guide names admin tokens with doctor and operator, and says exactly what is and is not repaired by itself
- Merge PR 154: a service template can run its container with no-new-privileges, gained or dropped on a running service by an update that restarts it and puts the old setting back if it fails

## 0.25.0 (2026-11-13)

Arca now owns each host's forwarding and firewall as declared state, so a reboot or a Docker restart cannot take the
containers' way out again (Docker 29 turns IP forwarding on by itself and sets the `FORWARD` policy to `DROP`). The
owner's file `/etc/arca/host.json` says whether the host forwards and which bridges may reach out. A new root part of
the `arca` binary, `arca host-network run`, started by the system unit `arca-host-network.service`, sets the two
forwarding switches and puts one accept in and one for the answers out for each bridge into Docker's `DOCKER-USER`
chain, adding nothing twice. It does this when it starts: at boot after `docker.service`, and again on every Docker
start, since the unit is tied to Docker. Every minute it reads the host again and writes a status file (in sync, or
which item drifted), and it answers on a socket to two verbs only, `read` and `apply-declared-state`, neither with an
argument. The node raises the new critical alert `host-network-drift` for each item that drifts, and a new action,
`host_network_restore`, is a normal approval card: when the owner approves it, the node asks the socket to apply, and
the result is the read-back. `arca host-network install --takeover <unit>` writes the declared file from what the host
has now, copies the binary to a root-owned place, starts the unit and disables the hand-made unit named, so the hand fix
is replaced and not doubled; `--dry-run` prints every step. See "Host forwarding and the firewall" in docs/OPERATING.md.
Not done here: IPv6 firewall rules, and the canvas does not yet draw the new card (the owner's approval page does).

`service_update` no longer erases a service's data silently. It refuses, before it writes anything, a service whose
template declares a data volume but whose instance has none, which is how a database, a nats server or an Omni API made
before the volumes keep their data: the update would rebuild the root disk and the data with it. The refusal says so in
plain words and names the service and its template; it holds for a direct call, for `action_propose` and for an approved
proposal when it runs. To update such a service and start over with no data, `eraseData: true` is stated with the
digest, and it needs the owner's approval: `service_update` can now be proposed with `action_propose`, and the card says
that all the data is erased and cannot be brought back. A service that has its volume, and a service whose template
declares none, update exactly as before.

Two nodes joined by a private link, such as a fiber cable, can now use it for the link between them. The `link` config
section takes `peers`, a map from a peer's name to the `ip:port` addresses it is also reached on, and `port`, a fixed
UDP port so that address stays the same. While a named address works it carries the traffic ahead of the VPN, a LAN
address and the relay; when it does not, the link falls back to the normal path. Only the side that opens a connection
chooses its paths, so set `port` on both nodes and name each other in `peers`, or one direction may still use the VPN.
`arca link peers` now shows, while `link serve` runs, the path each peer is reached on from its open connections:
`direct` or `relay` with the address and whether the config named it, or `idle` when no connection is open. A taken
`port` stops `link serve` with a message that names the port. Without the two keys the link behaves as before; a
serving node also writes a small `paths.json` every 2 seconds. This turns on iroh's `unstable-custom-transports`
feature, which iroh 1.4.1 requires to prefer a path; iroh stays pinned to that exact version.

- Merge PR 152: a link prefers a named private path such as a fiber, falls back on its own, and arca link peers shows the path of each open connection
- Merge PR 150: service_update keeps a service's data, refuses an old-style service that would lose it, and erasing needs the owner
- chore(lock): arca-hostnet follows the workspace version, 0.24.0
- Merge PR 148: a node declares its host firewall and forwarding in /etc/arca/host.json, a root unit applies it, and drift raises an alert with a restore action

## 0.24.0 (2026-11-13)

A new token scope, `admin`, sits above `operator`, and a node holds one admin token at a time: `arca token mint --scope
admin` with another admin token on the node is refused naming it, and `--replace` retires the old one and has a running
node read its tokens at once. The admin token can approve a waiting action proposal from the terminal of an MCP client
that declared elicitation: the call stays open, the client is asked in the card's words, and an explicit accept with yes
approves and runs the proposal once. The journals record it as approved in the terminal by that admin token (the run is
made as `terminal-approval:<token id>`), never as the owner's. A decline, a cancel or a timeout leaves the proposal on
its card, and every other token and client is answered as before. See "Approving a plan from the terminal" in
OPERATING.md.

- Merge PR 102: Automagik's brand is copied into docs/brand from Automagik 00765fdb, and a brand copy cuts no release
- Merge PR 149: an admin token held by one terminal approves gated actions in that terminal through MCP elicitation; every other client keeps the web card

## 0.23.0 (2026-11-13)

A node can now record a program that runs outside Incus, such as a macOS LaunchAgent next to the Incus VM, so it shows
in `service_list`, and on the machine's canvas of a node that runs services (a node that only looks shows it in
`service_list` alone). `external_add` takes a name, the kind `host-program`, an `http` or
`https` address and an optional `health` label; `external_remove` takes the record out. Both need a doctor token and run
at once, with no approval card, because they only write a line in the node's state (`external.jsonl`, which is also
its journal) and change nothing on the machine. The row says it is recorded and not managed by Arca; `service_status`
does one TCP connect to its address (3 seconds) and says up or down. Every verb that changes a service refuses it in
words that name `external_remove`.

A database (`postgres-autopg`), a nats server and an Omni API (`omni-api`) made from the shipped templates now keep
their data on a volume of their own: 20GiB for the database, 10GiB for nats and for Omni's media. `service_update`
rebuilds a service's root disk from the new image and keeps only its data volume, so data on the root disk would be
gone after an update. A service made before this release keeps its data on its root disk and gets no volume: the
changed templates are brought up to date by `arca templates install`, and apply to the services made from them
afterwards. Updating a service that has no data volume erases what it wrote to its root disk, and the docs now say so.

An `action_propose` answer now says where and by when: the approval page's address (`approvalUrl`), the time the card
expires, fifteen minutes after it was proposed, and how many owner passkeys the node itself trusts (`passkeysTrusted`).
While none is trusted, its `nextStep` says how to enrol one first, with the `ssh -o ExitOnForwardFailure=yes -L` line
when the page is on localhost only. A node set to trust another page host's passkeys (`approval.trustOwnerKeysFrom` or
`approval.peerKeys`) says instead that the card is approved on that host's page and needs no passkey on the node.
Before, the answer named none of this, so the person who asked for the action did not know where to approve it, nor
that it would lapse.

- Merge PR 137: an action_propose answer says where to approve the card and by when
- Merge PR 144: postgres-autopg, nats and omni-api keep their data on a volume of their own
- Merge PR 145: a node records a program that runs outside Incus, and status checks it by one TCP connect

## 0.22.0 (2026-11-13)

Each release now also publishes `arca-<version>-linux-aarch64`, beside the x86_64 binary and in `SHA256SUMS`, built
on GitHub's arm64 runner with the same glibc 2.34 ceiling, so a Linux virtual machine on an ARM Mac can install and
upgrade from a release. `install.sh` picks the binary by `uname -m`: `aarch64` and `arm64` get the aarch64 one,
`x86_64` the x86_64 one, and any other machine is refused in plain words, as before.

- Merge PR 143: Arca publishes a linux-aarch64 binary beside x86_64

## 0.21.0 (2026-11-13)

A service made from `automagik-server` now follows Automagik's releases by itself: every 10 minutes (`follow.intervalSeconds`)
the node reads Automagik's published record of its newest server image, and when it names another digest and a newer
version than the service runs, moves the service with `service_update`, which keeps its data and puts the old version
back if the new one's port does not answer. The version a service runs is kept in its record (it starts from the
template's pin), so the follower orders it however many releases are cut and across restarts. It does not start a
service its owner stopped, waits while Incus is still working on the service's instance, and deletes the image a move
leaves unused. Each change is a line in `service-updates.jsonl`, with the image's attestation result (a failed check
is logged, not refused). `service_update {name, follow: false}` keeps a service where it is, and `{name, follow: true}`
follows again. `automagik-workspace` is a virtual machine and does not follow.

- Merge PR 138: Arca follows new Automagik server releases and moves automagik-server to the new digest by itself

## 0.20.0 (2026-11-13)

`action_propose` now says in its input schema what each of its 22 actions takes: `args` has one branch per action, titled with the action, with that verb's fields, the ones it requires, and an example, so an assistant no longer learns them by trial and error. The door holds `args` to the branch its `action` names, so a refusal names the field of that action (`/args: missing required field name`) before the action's own check runs. A field the kernel fills in when the proposal is made (a GPU's instance kind, a disk's model, a snapshot's pool) takes any value, as before, and its description says the kernel fills it in.

- Merge PR 136: action_propose's args name each action's fields, and a wrong one is refused in that action's words

## 0.19.0 (2026-11-13)

Four sentences in the README and the guide said more, or less, than Arca does. Arca cannot run an app you name as a
service, but a small program in an image you name can run on a timer as a function once you approve its card. Each
machine runs its own Arca; machines you pair form a group, and an assistant can call a paired machine with a token that
machine made, but Arca does not yet spread services across machines by itself. `arca init` does find the default
config when `--config` is left out, like every command that writes. A function's image is pulled with the credential
set for its registry, if there is one.

The approval page speaks to the owner. It says "Arca asks before it changes anything", not only before a delete. An
action's card leads with one sentence of what the assistant wants ("Your assistant wants to create a Postgres database
called blog."), and says how to undo it in a `To undo` row. A create from a template also says what it takes of the
node in a `Uses` row (the CPUs and memory its template sets, or Arca's default share for a service, 2 CPUs and 2GiB;
in a tenant, its 16GiB disk; and its data disk), read from the template the approval binds. The verb's name, the target, the image by digest, the template's content and the fingerprint are in a
Details disclosure that starts closed and is named for its card, and a delete plan's and a request's fingerprints are
folded the same way; an image with no digest stays up front, since that row says what runs is not held to the approval. No
card row names a verb any more: a database's `Made as` row and a stopped create's `Start` row say what can be done
in words. After an approval the page says "Approved. Done: " and the card's sentence, not the verb that ran. The
signed statement and what the approval binds are unchanged.

When the node refuses the viewer token `arca status` found beside a missing `assistant-1.secret`, the refusal now names
the file it used, by its name alone; before, it spoke of a token file the owner never named. `arca --help` and
`docs/OPERATING.md` now say this step of `arca status` beside the other defaults.

- Merge PR 135: README and guide say what Arca runs and how paired machines relate, truthfully
- Merge PR 133: arca status names the token file it used when the node refuses it; tests for the PR 123 guard
- Merge PR 131: approval cards say in plain words what a change does and uses

## 0.18.0 (2026-11-12)

On a node that only looks, a read that worked no longer begins with "this node only looks: nothing here can be
changed ...": the answer comes first and that note after it, so an assistant that reads only the first text block no
longer reports a refusal. The text of `watch_report`, `service_list` and `arca_status` now carries the answer itself
(each instance, each service, and whether Incus answers and how long the node has run), not only a count.

A running service made from an image by digest now moves to a new digest in one call: `service_update {name, digest}`
stops it, rebuilds its root from the new digest (its data volume, config, secrets and firewall rules stay), starts it,
checks that it answers (through its host port, within 120 seconds or `timeoutSeconds`) and records the digest so a
restart keeps it. A failure at any step puts the old version back and the answer says it was rolled back. It needs a
`doctor` token and no owner approval, and works inside a tenant. It does this for `automagik-server`; there is no
update path for `automagik-workspace`, a virtual machine, yet. This replaces moving such a service by a remove and a
create, which deleted its data.

- Merge PR 132: the README's link row puts the Website first
- Merge PR 127: service_update rebuilds a running service on a new image digest and rolls back on a failed health check
- Merge PR 130: watch-only reports put the answer first
- Merge PR 125 delta: the real-Chromium RS256 test reaches the page and proves an RSA passkey enrols and approves

## 0.17.0 (2026-11-12)

Arguments a tool does not take are now answered as a tool result with `isError: true` and `droidd/error` `invalid_arguments`, in the same words, instead of a JSON-RPC error `-32602`, so an assistant reads why and corrects the call (MCP 2025-11-25). A value outside a closed list, such as an `action_propose` action that is not one, now lists the allowed values and names the nearest one. `arca call` prints such a failure as `{"code": "invalid_arguments", "message": ...}`. A call that names no tool, or a tool the server does not have, is still a JSON-RPC error.

A new Automagik starts from Automagik's published image: `automagik-server` and `automagik-workspace` now pin
Automagik's release 3.136.4 by its digest (they pinned 3.95.0 and a build of commit `5fe0be6`), with nothing built on
the node: Incus pulls the server's image at the create, and a workspace's Docker pulls it after `instance_start`. Omni channels are not in 3.136.4; they come through Automagik's welcome once
Automagik's fixed release is pinned. The hand-built `automagik` template and its build script stay as they were, for
the Omni channels plugin until then and for publishing on the tailnet. An installed
copy nobody edited is brought up to date by the upgrade; a service made before keeps running its own image, and
`service_status` shows the newer release under `update`. Keep such a service on its release: moving it today is a
remove and a create, and the remove deletes its data.

On the approval page, while no passkey is trusted, a waiting card's button says "Set up your passkey first" instead of
"Approve", and a tap on it takes the owner to the passkey setup, which is at the top of the page. Before, it asked for a
passkey there was none of and failed. Once a passkey is trusted the cards are drawn again with their Approve buttons.

The MCP server names the release it is. `serverInfo.version` said `0.1.0`, the version of the verbs' contract, on
`initialize` and beside every `2026-07-28` result; it is now the release, the version `arca --version` prints. A
`server/discover` answer now carries `serverInfo` too, where before the server was named only inside its `_meta`.

Passkeys can now be RSA keys (RS256) as well as P-256 keys (ES256). Some Windows Hello and TPM authenticators make only
RSA keys, and setting up such a passkey failed at its last step. The page now offers both, enrols either, and checks
either's signature on every approval and sign-in. A passkeys line names its key's `alg`, `ES256` or `RS256`.

`arca status` works on a node that was upgraded rather than installed fresh. Such a node keeps its viewer token beside
its config under another name (`viewer.secret`), and `arca status` with no `--token-file` asked only for the
installer's `assistant-1.secret`, so the first command after an upgrade told the owner to mint a token. When that file
is not there, `arca status` now uses a viewer token of this node from the same directory; a token of a wider scope is
never taken in its place. A `--token-file` or `ARCA_TOKEN_FILE` that names a file is still read alone.

- Merge PR 124: arguments a tool does not take are a tool result the model can read and correct, with the nearest name for a mistyped one
- Merge PR 129: the earlier-version comments and docs say the Automagik files shipped unchanged up to 0.16.0
- Merge PR 123: arca status on an upgraded node uses a viewer token beside the config when the installer's token file is missing
- Merge PR 125: passkeys that sign with RS256 enrol and approve, as ES256 ones do; EdDSA is not offered until something verifies it
- Merge PR 126: the MCP server names Arca's release as its version, as arca --version does, and server/discover says it too
- Merge PR 121: a new Automagik runs Automagik's published image v3.136.4; the hand-built template stays for Omni and tailnet publish
- Merge PR 122: while no passkey is trusted, every card says to set one up first and takes the owner to the setup

## 0.16.0 (2026-11-12)

The canvas Doctor can now propose a tenant's create and removal, a deploy's removal, and a function's create and
removal: every action the kernel lets a viewer propose. Each still waits for the owner's passkey on its card. To know
what to propose it now reads the tenants (with what each instance uses and the deploy it came from) and the functions
with their newest runs. A removal of a tenant, a deploy or a function deletes data or secrets for good, so its card and
its Approve button are drawn in the alert colour.

The `automagik` image is now named after everything it is built from: `arca-automagik-<commit>-dsh-<harness>`, 12 hex
digits of the Automagik commit and of a SHA-256 over the build's other inputs (the build script with its Node, Tailscale
and plugin pins, the DSH package files, and the dashboard's port). Before, it was named after the commit alone, so a
build with a newer DSH or any other new pin was refused as an image that exists, and the old one had to be deleted by
hand. The template changed, so an installed copy nobody edited is brought up to date by the upgrade. A node's image of
before stays, and a service made from it keeps running; a new `automagik` service is refused with the new name until
that image is built with `templates/automagik.build.sh`.

- Merge PR 120: v0.15.0's automagik.json is on the list of earlier versions, so an upgrade replaces it
- Merge PR 109: the Automagik image is named after every input of its build, so changing any pin builds a new image beside the old one
- Merge dev at v0.15.0 into set G
- Merge PR 107: the tenant removal card says the tenant's address block, when it was made, and the services it holds as of the proposal, and that services made later go too
- Merge PR 103: the canvas Doctor proposes all 22 kernel actions and reads tenants and functions, with the kernel's untrusted note first in what it reads

## 0.15.0 (2026-11-12)

The `automagik` image is now named after the harness too: `arca-automagik-<commit>-dsh-<harness>`, 12 hex digits of the
Automagik commit and of the SHA-256 of the DSH lockfile. Before, it was named after the commit alone, so a build with a
newer DSH was refused as an image that exists, and the old one had to be deleted by hand. The template changed, so an
installed copy nobody edited is brought up to date by the upgrade. A node's image of before stays, and a service made
from it keeps running; a new `automagik` service is refused with the new name until that image is built with
`templates/automagik.build.sh`.

More of a key is hidden in `service_logs`, a failed command's words and a function's output, and less of what follows
it. A PEM block whose END line was not printed is hidden over the lines after its BEGIN line that look like a key
(base64 of 16 characters or more, or a PEM header), and no longer to the end of the text, so a log or a function's
result after it stays shown. A text that begins inside a key (an END line with no BEGIN line before it) is hidden
from its start to that END line, so a window of a container's journal that starts inside a key shows none of the key
when the key's END line is inside the window. A run of 16 or more base64 characters that lies inside a stored key's
body is hidden, printed at another width or with each `/` written `\/`. A label may hold lowercase letters and
hyphens (`OpenVPN Static key V1`). A function's kept outputs and its runs' lines are redacted again when
`function_runs` reads them, with the secrets stored then.

Known gaps: a stored key whose letters were changed in case is still shown; for a key that is not a stored secret, its
lines after a BEGIN line with no END are still shown when each carries a prefix (a journal's timestamp, a colour code),
and so is a key longer than a journal window when neither its BEGIN nor its END line is inside the window; a stray END
line hides the text from its start, so a function's result that names such a line is hidden too; a stray BEGIN line
hides everything up to the next END line. `docs/OPERATING.md` lists them all.

- Merge PR 108: the 0.14.0 notes say what 0.14.0 shipped, and the key-hiding notes say when a window inside a key is hidden and what is still shown
- Merge dev at v0.14.0 into set F
- Merge PR 99: a key is hidden by its shape without hiding what follows it, in one pass over any log, and the journal is read at the lines asked
- Merge PR 104: the Automagik image is named after its commit and its DSH harness, so a new DSH builds a new image beside the old one and none is deleted by hand
- Merge PR 105: the guide's table says one step lets a node change things, and what that step leaves to the config
- Merge set E fix into set F
- Merge PR 65 commit 2: the Automagik image is built with DSH 0.2.2-rc.2

## 0.14.0 (2026-11-12)

The metrics sampler no longer reads the listing of every instance with its live state each tick. On a node where one
running virtual machine's guest agent is slow, that listing took up to its 10 second deadline, so a tick often had no
instance readings at all, and it slowed every service create made at the same time. Each tick now lists the instances
without state and reads each running instance's own state one after another, 2 seconds each and 4 seconds for all, the
ones asked longest ago first. `node_metrics` names, in each sample's new `instancesNotRead`, a running instance whose
CPU and memory a tick did not read, and why, in fixed words; its CPU and memory are left out of that tick's export.

On such a node the `crash_loop` alert was unknown whenever that listing failed, since it reads the instances' starts.
It is now evaluated there again: it can fire for a crash loop that was hidden before, and an alert held firing as
unknown can now clear.

A virtual-machine template can say what must hold between its settings (`settingRules`: one amount at least
another), and a create whose settings would not pass is refused when it is proposed, before any card. The two GitLab
runner pool templates now state what their script already refused on the machine's console: `concurrent` and
`jobCpu` from 1 to 999, `memoryOvercommitPercent` from 100 to 200, and 1 to 12 `runners`. Both files changed, so an
installed copy that was not edited is brought up to date by the upgrade, and a card for one of them that waits across
the upgrade is refused when approved, and must be proposed again.

A create is refused when the instance it would get has the name of another service's instance on the node: the
node's own service `x-<tenant id>` and that tenant's service `x`. Before, the second one was recorded and Incus
refused it.
A secret of several lines, such as a key file a virtual machine reads, is now hidden in `service_logs`, in a failed
command's words and in a function's output. Before, a log was cleaned line by line before the stored values were looked
for, so a value of several lines was never found and a key the guest printed on its console was shown to any token
that may read logs. Each line of such a secret of 16 characters or more is now hidden as a value of its own, and every
PEM block (BEGIN to END) is hidden whole before the log is cut into lines, a stored secret or not. A PEM block whose
END line was not printed is hidden to the end of the text, so a log after such a line shows `[secret]` and nothing more.

More of what a `service_create` would refuse is now refused when it is proposed, in the create's words, so no card is
shown for it: a name that is taken, a deploy the approved create could not add to, a tenant's machine on a host
bridge, a tenant's service from a template that opens a host port, a MAC another record holds, and a config with no
service project. A proposal with a bad name is told about the name first. What is read from Incus (a free address,
an instance Arca does not know, a local image) is still asked by the approved create alone; `docs/OPERATING.md`
lists it.

No token can be minted with the id `owner-approval` any more: it is the name an approved action runs under. If your
token directory holds `owner-approval.hash` from an earlier release, the node still starts, ignores that file and says
so in its log; remove it with `arca token revoke owner-approval`.

The refusal of a create that names a deploy somebody else opened is reworded, so it is true for every caller: "deploy
<id> was opened by another caller: only the caller that opened a deploy adds to it, and an approved action runs as the
owner's approval, never as the token that proposed it". A client that matched the old sentence must match this one.
A stop of an Automagik workspace or of a GitLab runner pool could be lost inside the machine. The script that
keeps each of them took its stop signal in the same process that did its work, and bash 5.2 can drop such a signal
without running the script's stop (seen under bash 5.2.21; Debian 13's own bash, which these machines run, was not
tested). A workspace's keeper then ran on until systemd killed it, a minute and a half later. A pool's controller
was killed too, after two and a half minutes, and that kill reached the controller alone: GitLab Runner and the
jobs' containers are Docker's, not the service's. So GitLab Runner was never told to stop taking jobs and went on
taking them, with no controller, until Docker stopped it as the machine went down or the pool's next start removed
it. Both scripts now take the signal in a process that does nothing else and stops the work from there, so the stop
cannot be dropped. A machine made from an earlier release keeps the script it was made with.

The GitHub runner pool's script had the same fault, in its controller and in every slot, and has the same change. A
lost stop there left runner containers running and registered on GitHub with no controller. The controller and its
slots now read their stop from a line that the one process holding the signal lets go of, and none of them has a
trap. The controller no longer signals its slots at a stop: each has read the same line and is cleaning up, and is
given the same minute to finish.

A card that waits while the node is upgraded to the version that binds a template's content (a create, or a database
made from the template) is now refused when its approval is submitted, and the approval is not used; propose it
again. Before, the approval was accepted and the create then refused.

A `service_create` the node has no room for is now refused when it is proposed, in the words the create uses (what is
in use, what is asked, what is allowed, and the config key), so the owner is never shown a card for it. Before, the
card was shown, the approval was accepted and used up, and only then did the create refuse. This covers the node's
caps on virtual machines (`kernel.vmLimits`, `kernel.bridged.limits`) and a tenant's caps. The approved create still
checks them, because the node can fill up while a card waits. One refusal for room can still follow an approval: no
free address in the service subnet.
Every service call read the live state of every instance on the node before it did anything, and Incus takes a
running virtual machine's state from its guest agent: on a node with a machine full of containers (a runner pool) that
one read took about ten seconds, the whole time it is given, so calls failed. Service calls no longer read live state:
`service_list`, `service_status`, `service_logs`, a secret, a link and the tenant reads answer at once whatever a
guest is doing. One thing works now that the guide promised and the node never did: `service_status` of a running
virtual machine on a host bridge shows the address its own network card holds (until now `ipv4` was always empty for
such a machine, in the list and in the status). It reads that one machine's state for it and, when Incus does not
answer, still answers, with `ipv4Problem` saying why. `service_list` reads no state, so such a machine's row has an
empty `ipv4` and the new `ipv4In: "state"`, which says the address is there to be read and is not "none". A create still reads live state, to pick an
address nothing holds. A read of Incus that fails fast is made once more.

A service call that failed because Incus was slow or did not answer said only `internal: the tool failed`, and the
node's log had no line for it. Such a call is now refused in words that say Incus did not answer, for which service,
and that the same request may be made again, under the code `incus_unavailable` (`incus_answer` when Incus answered
something this node cannot use, `state_not_saved` when the kernel could not save the change). When an answer is still
`internal`, the node's log gets one line with the verb and the reason. A virtual machine's secret may be 4096
characters and one line break at its end (a key file read whole). Such a value needs 8 characters that are not line
breaks: one of only line breaks is refused, and so is one that was accepted before, of fewer than 8 characters and
line breaks to make up the count (seven characters and a line break). A stored secret that long, or one of many
bytes a character, is taken out of logs and outputs like any other when it is one line; a secret of several lines is
not taken out by its value yet. A
container's or a function's secret that ends with a line break is refused in words that name the line break. A text
that is too short or too long for its field is refused in words that name the limit. See "When Incus does not answer,
and when the answer is internal" in docs/OPERATING.md.

A template whose image is `local:<alias>` and that already carried a `digest` was accepted before, and the digest was
ignored. It is enforced now: a create from such a template is refused unless the image under the alias has that
fingerprint, and a digest that is not `sha256:` and 64 lowercase hex digits is refused when the template is read. Check
a local template's `digest` against `incus image list` before you upgrade, or take the digest out.

Every run of a function now starts from its image's own root. Before a run starts, its container's root is rebuilt
from the function's image: from the copy the node already holds, the one the function's record took when it was
created, or from the registry by the approved digest when that copy is gone. So no run finds what an earlier one
wrote, which replaces the limit 0.13.0 named. A root that cannot be rebuilt is a run that does not start, and its
line says why. A run's line gains `root`, `rootImage` and `rootMs`. A function's first run rebuilds nothing, and its
line says `root` is `new`: a container that has never started still has the root its create made from the image, and
Incus cannot start one that was rebuilt before its first start.

What the owner approves on a card is now what is created. A proposed `service_create` or `postgres_create` is given,
by the node, the image its template names and a fingerprint of the template's content (its file and the scripts it
names). The card shows an `Image` row and ends with a `Template content` row, and the approved create runs only while
the template on the node is still exactly that one: an edit, or a template installed again by an upgrade, refuses the
create with "propose it again". An image named without a digest is shown with words that say so. Two new optional
arguments, `image` and `templateDigest`, are filled in by the node; a caller that states one must state the node's
own. A proposal for a template that is not installed, or whose script is missing, is now refused when it is made.

**If your node has templates from 0.13.0 or older:** upgrading with the installer now brings them up to the release.
A shipped template that nobody edited is replaced with this release's and named in the report; one you edited is
kept and named, with the command that replaces it. For a node the installer does not run as a service, run `arca
templates install --config <file>` once after the upgrade (or `arca init --enable-changes` again). 0.13.0 said to
use `--replace` for this, which also overwrites your edits: it is no longer needed. A service that already exists is
not changed.
A service's or a function's image can now be on a registry that asks for a credential. The credential is set by the
registry's address (`registry_secret_set`, with `registry_list` and `registry_secret_remove`) and is kept in the
node's secret store; no record, proposal, card or journal line holds it. Incus is sent it in the registry's address
when the image is fetched, and Incus keeps that address with the image for as long as it keeps the image, where
whoever may use Incus on the machine can read it: so set only a credential that can read, and only what the node
pulls. A create whose image is fetched that way says so on its card and runs only from an approved proposal; the
image, and the credential with it, leaves Incus with its last function or service. Every ten minutes the node looks
at the images it fetched that way, turns off Incus's own re-fetching of them where it is on, and deletes one that
nothing has been made from for fifteen minutes; each such step is a line in `registry-images.jsonl`.
`docs/OPERATING.md`, "A registry that asks for a credential", has the whole of it.

A commit to the node's store that works but takes over one second is no longer called a failed write. It has its own
counter, `arca_store_slow_commits_total`, and its own alert, the warning `store-commit-slow`.
`arca_store_write_failures_total` and the critical alert `store-write-failed` now mean only a write the store refused. A
dashboard or an outside alert built on `arca_store_write_failures_total`, or on `store-write-failed`, stops firing for a slow
disk: watch the new counter or the new alert for that. Both counters start at 0 when the server starts, as before.

A GitHub runner pool now keeps every class one place under `maxSlots`. A free place goes to the class with the fewest
runners, and when a class has no runner while every place is held, one runner that waits for a job is retired for it;
a runner that has a job never is. A pool created with `maxSlots` below the number of classes does not start, and says
both numbers and what to set. It is still a cap on registered runners, not on running jobs, and a place is not
memory: "A place for every class" in `docs/OPERATING.md` says what is kept and what is not. Pools that exist keep the
script they were created with; a pool of one class works as before.

The GitHub runner pool template now states that rule (`settingRules`), so a create with `maxSlots` below the number
of classes is refused when it is proposed, before any card, and no machine is made for it. The rule is the plain
one: a pool of one class created with `maxSlots` 0, which ran with one place, is refused at create too; set 1. The
template's file changed, so an installed copy that was not edited is brought up to date by the upgrade, and a card
for it that waits across the upgrade is refused and proposed again.

- Merge: the fake Incus answers slowly again for the sampler tests, and reads that must miss their deadline are never answered (fixes the set E build: PR 92 removed slow_reads, PR 95 used it)
- Merge PR 96: clippy runs on every target in CI and in the release, and the test code's lints are fixed
- Merge PR 97: the link race test stops and joins its status thread before it fails, and the canvas snapshot test's waits are bounded and still catch a cache that waits on the device
- Merge PR 98: a GitHub runner pool keeps GitHub's own refusal even when its answer is cut off, says when a runner is updating, and a slot started at a stop registers nothing
- Merge PR 95: the metrics sampler lists instances without live state and reads each running one's state within a budget, naming the ones it did not read
- Merge PR 94: asking whether link serve runs no longer makes a starting serve stop as already running
- Merge PR 92: four load-flaky tests wait on the thing itself, and the canvas snapshot test counts held Arca requests
- Merge PR 91: a GitHub runner pool asks a runner again after its update, and a lost answer to a sent request is not a pool failure
- Merge PR 90: a multi-line secret is hidden line by line and a PEM block is hidden whole before a log is cleaned
- Merge PR 89: a settings rule names its setting in 1 to 64 plain characters, functions and template databases are name-checked, GitLab pools state their ceilings
- Merge PR 88: the GitHub runner pool takes a stop in one process, so bash 5.2 cannot lose it
- Merge PR 87: a service call no longer waits on a guest's agent; instances are listed without live state
- Merge PR 86: settings rules are refused when proposed
- Merge PR 80: a GitHub runner pool keeps one runner per class
- Merge PR 82: a refused secret value is refused in words
- Merge PR 85: an upgraded node replaces the keeper and GitLab pool scripts earlier releases shipped; CI fetches tags
- fix(service): more of what a create refuses is refused when it is proposed, and no token takes the name approved actions run under
- fix(templates): the workspace keeper and the GitLab runner pool take a stop signal in a process that runs no command substitution
- feat(registry): an image on a registry that asks for a credential is fetched with the one set for that registry, and the node sweeps what it fetched
- fix(service): a create the node has no room for is refused when it is proposed
- fix(alerts): a slow commit to the store is called slow, and only a write that failed is a failure
- fix(service): a card's rows come from one read of its template
- fix(function): a function's first run rebuilds nothing, because Incus cannot start a container rebuilt before its first start
- an upgrade through the installer brings the shipped templates up to the release and keeps what the owner edited, and no printed command holds a placeholder a shell would misread
- an approval card for a service or a database shows its image and binds the template's exact content, and the create refuses a template changed since
- every run of a function starts from its approved image's own root, rebuilt from the copy the node recorded at create
- fix: a GitHub runner pool looks at every slot once per pass, so a place that frees goes on in turn and no free slot starts a tick late
- a template's local image may be pinned by its fingerprint, and a create refuses when the name holds another image
- fix: a database card that waits across a node restart keeps what it said, and says 'generates' only when a password will be generated
- fix: the canvas plugin loads on a DSH 0.2.2-rc.2 host as well as rc.1, with its range pinned by a test (diff read by the Orchestrator)

## 0.13.0 (2026-11-12)

On a node that only looks, the refusals of an instance start, stop or restart, of a storage change and of a GPU
assignment no longer hold the word `watch-only`, which they did in 0.12.0: they carry the same sentence as every other
refusal and every answer of such a node, `this node only looks: nothing here can be changed until the node is running
with changes turned on (arca init --enable-changes, then restart the node if init did not)`. Their codes are unchanged
(`InstanceLifecycleDisabled`, `StorageDisabled`, `GpuDisabled`): match on the code, not on the words.

**If you installed the templates with 0.12.0 or older:** the shipped `postgres-autopg` template now makes the
database's password itself, so a database you approve on the page starts by itself. Your node keeps the template it
already has. To get the new one, run `arca templates install --replace --config <file>`; it overwrites shipped
templates you edited. A `postgres-autopg` service that already exists is not changed, and keeps its password.
Arca now runs functions: a program in an OCI image by digest, run on a timer, each run one start of a container of its
own that is stopped again when the program exits. A function is created and removed only from a proposal the owner
approves, and its card shows the image and digest, the command, the timer, the limits of a run (time, memory, CPU), the
names of its secrets and everything it may reach: the services of this node its links name, each on one tcp port, the
private addresses the node's config names for it, and no connection to any public address. One way out stays, and
the card says it: names are looked up through the node's DNS, which asks the internet's name servers, so a program
can send a little data out in the names it asks for. Secrets are set by name
(`function_secret_set`) and a run is given only the ones its function names. Each run writes one line to a run log
that is also the meter (start, duration, CPU time, memory peak, outcome, the program's result line), read with
`function_runs`; a run writes nothing to the desired state or the effect journal. A program's last output line is its
result, one JSON object with `"ok"`. Six new verbs: `function_create`, `function_remove`, `function_secret_set`,
`function_list`, `function_run`, `function_runs`. Known limit: a run can write its container's root file system, and
the next run finds what it wrote (Incus refuses to start an OCI container with a read-only root). `docs/OPERATING.md`,
"Functions", has the whole of it.

`postgres_create` now works on a node set up from a clean install. On a node whose config sets no
`kernel.postgresImage`, it makes a service from the node's `postgres-autopg` template, with the image fetched by
digest and the password the template generates: give the name only. A database made this way is a service: connect
to it with `service_connect`, remove it with `service_remove`; `postgres_list` shows it with `service: true`. A proposal's card
shows the image and binds it, and a template changed before the approval refuses the create.

No call chooses a database's image any more. A `doctor` or `operator` token's own `postgres_create` used to run any
digest-pinned image it named; it now uses the node's template, or `kernel.postgresImage` where the config sets it, and
a call that states another image is refused. A node whose config sets `kernel.postgresImage` keeps the old kind of
database whole, and its records and lists are unchanged; `image` is now optional in the call.

- a database from a clean install is the shipped Postgres service, made through the owner's card, and no token names its own image
- functions, a program that runs on a timer in a container of its own, created only from the owner's card, with limits and a run log that is the meter
- after arca init a person sees one next action, the shipped Postgres service makes its own password so it starts through the card alone, and the README says what 0.12 does
- a node that only looks still shows its services, their status and their logs, and refuses every write first, in one sentence
- a runner pool obeys a stop at once, in every loop and during a request

## 0.12.0 (2026-11-12)

- the installer shows the one line that connects an assistant, arca status says who approves and which tokens do not wait, and a machine with no screen is told what to run
- fix: token reload, link pair and link join never let a stale ARCA_CONFIG pick the node
- one rule for finding the config on every command, a stale ARCA_CONFIG never picks the node for a command that hands out or removes, and an unknown field is named
- fix: arca init calls the node's address the way token reload left it, so the merged set compiles (diff read by the Orchestrator)
- arca init --enable-changes, one guided step from a node that only looks to one that can make changes, and it names every token that could change things without the passkey
- arca token reload, a checked reload that never signals a stranger or a node still starting, and every hint names it

## 0.11.0 (2026-11-12)

The MCP server now speaks protocol version `2026-07-28` beside `2025-11-25`. For a client of `2025-11-25`, or one that
sends no version header, `initialize`, `ping`, `tools/list` and `tools/call` answer as before. The server used to read
no version from a request and now does, so four answers changed for such a client:

- `server/discover` answers; it used to be an unknown method.
- A `MCP-Protocol-Version` header naming a version other than `2026-07-28`, `2025-11-25`, `2025-06-18` or `2025-03-26`
  is refused with `400` and the list of supported versions.
- A `MCP-Protocol-Version` header sent twice, or one that is not visible ASCII, is refused with `400`.
- A version in `params._meta` is read even with no header: an unknown one, one that is not a string, and one that
  differs from the header are refused with `400`.
A service of the node's own project can now be let through to one private address on one tcp port, for example a
GitLab on another bridge of the node or on its tailnet. It takes two keys, and neither alone: the node's config names
it (`kernel.serviceDestinations: [{ "service", "address", "port" }]`, one IPv4 address, never a range or a name), and
the owner approves the service's create, whose card carries a `Private network` row with the address. A direct create
of a named service is refused with `NotApproved`. The service's own Incus ACL is the only thing that changes, and only
tcp to that address on that port becomes reachable: every other tcp port, udp, icmp and every other IP protocol to it
stay refused. Nothing of the host's firewall or routing is touched, so the node must already forward and masquerade
that path, and the destination then sees the node's own address: whatever trusts that address there trusts everything
in the service. A tenant's service, an address of the node's own and a virtual machine on a host bridge never get one;
`serve` refuses to start when the config loses an entry a service's record carries; `service_list` and
`service_status` show `destinations`, and status says for each whether it stands. An existing service is not changed
by an entry: remove it and create it again. Known limit: taking an entry out of the config does not close the address;
remove the service first. Not yet run on a live node. See "A service that may reach one private address" in
`docs/OPERATING.md`.

- a service of the node's own project may reach one private address on one tcp port, named in the node's config and shown on the owner's card
- the approval page on one machine with no domain, at http://localhost, and the first passkey's setup code on the terminal
- a loopback HTTP MCP door by default, the installer prints the line that connects an assistant, and arca status says whether the door is open
- every route of the approval page answers with or without a trailing slash, and the canvas page loads its assets from both
- a node never set up for services answers an empty service and tenant list, and one that still holds service records keeps refusing
- the common commands need no flag, arca status says the node in words, and a refusal says the next step
- the release build's path check reads a path whole, stops when it cannot search, and judges by what is committed
- the MCP door speaks protocol version 2026-07-28 beside 2025-11-25, and answers server/discover

## 0.10.0 (2026-11-12)

`service_status` now answers `update` for a service whose version is known: `pin`, the version it was created with,
`templatePin`, the one the template installed on the node names now, and `state`, `same` or `differs` (`unknown`
with a `problem` when the installed template cannot say). `service_list` carries `update` only on a service that
differs, and the canvas marks it "Other version". Both versions come from the node's own records and files: no
registry is asked, nothing is written, and it never says which of the two is newer. A virtual-machine template says
where its version is with a new key, `"update": { "pin": "<a key of settings>" }`; the shipped `automagik-workspace`
names `serverImage`. A container's version is its `digest`. It only tells you: the way to the template's version is
still `service_remove` and `service_create`. See "Whether the installed template names another version" in
`docs/OPERATING.md`.

- a service's status says whether the installed template names the same version as the one running, another one, or cannot tell

## 0.9.0 (2026-11-12)

- The README and `docs/QUICKSTART.md` now fetch the installer from the newest release
  (`https://github.com/automagik-si/arca/releases/latest/download/install.sh`) and no longer from the `dev` branch:
  what runs is the released `install.sh`, which that release's `SHA256SUMS` lists.
- A new template, `gitlab-runner-pool`: one virtual machine that runs the CI jobs of one GitLab, self-hosted or
  gitlab.com, with GitLab Runner 19.5.3 (by digest) and the docker executor. One pool serves up to twelve runners
  created in GitLab, each with its own token (`GITLAB_RUNNER_TOKEN_1` to `GITLAB_RUNNER_TOKEN_12`), limit and size of
  job, under one cap of jobs at once; `url`, `concurrent`, `jobCpu`, `jobMemory`, `serviceMemory`, `jobServices`,
  `runners`, `memoryOvercommitPercent` and `reach` are given at create. Every job runs in fresh containers on a network of its own, never
  privileged and never with the Docker daemon's socket; the virtual machine, not the container, is the boundary. The
  pool checks the jobs' memory against what the machine reports and does not start when they do not fit, with the
  numbers (`memoryOvercommitPercent`, 100 to 200, lets the caps add up to more than the machine has, for a mixed
  load whose jobs cannot be told apart); it says in one sentence each why a GitLab is not reached (the name, a timeout, the certificate, a token);
  and its status block has the jobs that run now and each finished job's result. With `reach` `tailnet` the machine
  joins a tailnet (`TAILSCALE_AUTHKEY`) to reach a GitLab that is only there, found by its full tailnet name, by
  ordinary DNS, or by the node `gitlabNode` names. `gitlab-runner-pool-large` is the same
  pool in a machine of 32 CPUs and 64GiB. It has not run against a real GitLab yet: see "A pool of GitLab runners"
  and "The GitLab pool" in `docs/OPERATING.md`.

- a pool of GitLab runners as a template, in two sizes: one machine, up to twelve runner registrations under one limit of jobs, each job capped and on its own network

## 0.8.0 (2026-11-11)

Releases are now cut by themselves: every change that lands on `dev` and ships is released, numbered and published
without anyone writing a version or a tag. The numbers changed with it. A release is `0.N.P`, with no `alpha` word:
`0.8.0` follows `0.1.0-alpha.7`, a change to what the binary is built from takes the next `N`, and a change that ships
without changing the binary takes the next `P`. The canvas package carries the same version as the binary. Arca is
still an alpha for single hosts; the number no longer says so, `docs/OPERATING.md` does.

Each release now carries `install.sh`, the installer, beside the binary. It installs the release it comes with, and
`SHA256SUMS` lists it with the binary: check a download with `sha256sum --check --ignore-missing SHA256SUMS`, and
stop unless it prints `arca-<version>-linux-x86_64: OK`.

- A workspace's own settings are given when it is created: the `automagik-workspace` template lists `reach`,
  `publicUrl` and the three Cloudflare Access settings in `createSettings`, so `service_create` takes them in
  `settings` and no copy of the template is made. `serverImage` is not listed: the image a workspace runs stays the
  template's to pin. `publicUrl` now ships empty, which means "not set".
- The `automagik-workspace` template can be reached on a tailnet instead of through a Cloudflare Tunnel:
  `settings.reach` is `cloudflare` (as before, and when it is left out) or `tailnet`. With `tailnet` the machine
  runs a Tailscale node (Tailscale's own image, by digest) in a network namespace of its own, which serves HTTPS on
  port 443 of the machine's tailnet name, never Funnel, and reaches the gate through a relay on a private network
  that leads to the gate's port alone. The node never shares the server's namespace, where it would hand every
  loopback port of the workspace to the tailnet. Its one secret is `TAILSCALE_AUTHKEY`, handled like the tunnel's token, and the node's identity is kept
  on the machine's disk. The gate then signs its owner in itself, with passkeys, after a first claim; the keeper
  says whether the node is logged in, under which name, and whether the gate is claimed, and never reads the claim
  code. Arca does not yet hand that code to the owner. Both reaches now pin the Automagik server build that has
  the passkey sign-in (`5fe0be6`). With the Cloudflare reach the connector is no longer started beside a gate that
  has no Access settings, where nobody could sign in through it. See "On a tailnet instead" in `docs/OPERATING.md`.
- An installer, `scripts/install.sh`: one command that leaves what `docs/QUICKSTART.md` leaves, a read-only node
  for the user who runs it. It checks the machine first (Linux, x86_64, glibc 2.34 or newer, `curl`, `sha256sum`, a
  runtime directory that is the user's alone, an Incus socket this user may open) and, when something is missing, says
  what and how to fix it and changes nothing. It refuses to run as root. It downloads one named release (the one its
  own `ARCA_VERSION` line names, which is the release it was published with, or the one `--version` names; it never
  asks GitHub which release is the latest), checks the program against the release's `SHA256SUMS` and puts it at
  `~/.local/bin/arca`; an older release is never put over a newer program unless `--version` names it. Where there is
  no config it writes the quickstart's and mints one `viewer` token; a config that exists is not changed. It runs the
  node as `arca.service`, a systemd user unit, and ends with the node's answer to `arca_status` and the two ways to
  connect an assistant. It starts a node only for a config it wrote, or through its own service from an earlier run,
  and only where it can ask the node afterwards: for any other config, and where `systemctl --user` does not answer,
  it starts nothing, prints the one command that starts the node and ends with exit code 3. A node that answers and
  cannot reach Incus is a failure. A second run replaces the program and restarts the service; `--uninstall` removes
  the unit and the program, while the program is still the file it installed, and keeps the config, the tokens and
  the state. It never runs `sudo`, writes only in the user's home, never prints the token, and works from a pipe and in
  a shell with no `XDG_RUNTIME_DIR`. It refuses a user who already has the operating guide's units
  (`arca-watch.service`, `arca-shim.service`). The addresses in the quickstart still read the script from the
  `dev` branch. It is tested against stand-ins, and was run for real only on a machine that already had Incus and
  Arca.
- Two families of refusal have their own codes, so a caller no longer reads the sentence to tell them from any other
  refusal. `capacity`: the call would pass a cap of a tenant or of the node (instances, containers, virtual machines,
  CPUs, memory, disk, tenants, addresses). `rate`: the caller asked too often or already has too much waiting
  (`kernel.tenants.createRate`, the 5 and 20 pending approval requests, the Omni pairing hold-off). Until now all of
  these came back as `refused`. **A caller that matched `refused` for them must match the new codes.** Every sentence
  is unchanged. Fields stand beside the code in the error's `data`. For `capacity`: `scope` (`tenant` or `node`:
  whose cap it is, since a tenant under its own caps can be refused because the node is full), `limit`, and when
  known `have`, `want` and `max` (bytes for memory and disk, a count otherwise). For `rate`: `retryAfterSeconds` when
  the wait is known. Either gains `proposeAgain: true` when it came from running an approved action, whose approval
  is spent. `RateLimited` and `TooManyProposals`, the codes of the two proposal refusals, keep their names;
  `RateLimited` gains `retryAfterSeconds`. The OpenRPC document lists the four codes under `x-arca-refusals`, each
  with its `family` (`capacity` or `rate`) and the schema of its `data`, and dropping a listed code, family or value
  is a break against a released document. The approval page's 403 answers carry the same fields, and its 429 answers
  carry `refusal: "rate"` beside `message`. The table is in `docs/OPERATING.md`, "Refusals for want of room and for
  asking too often".
- The README is now one short page: what Arca is, how to get it, how it works and what it runs, with the Arca logo and
  one diagram (the files are in `assets/readme/`, each with one file for a light page and one for a dark page). Its
  long sections moved into `docs/`: the quickstart is `docs/QUICKSTART.md`, with "Install with one
  command" above its steps by hand; the status table and "What it does today" open `docs/OPERATING.md`; the crate
  table is `docs/ARCHITECTURE.md`; "Build and test" and "Releasing" are `docs/BUILDING.md`. A link to
  `README.md#quickstart-a-read-only-node-in-five-minutes` now stops at the top of the README: the quickstart's address
  is `docs/QUICKSTART.md`.
- A new shipped template, `automagik-workspace`: one Automagik workspace in its own virtual machine (2 CPUs, 4GiB,
  20GiB), published through a Cloudflare Tunnel whose connector runs inside the same machine. A keeper runs the
  Automagik server, its gate (the same image, the only door to the server) and the connector in one network
  namespace, in the shape of Automagik's own compose file; nothing listens on the machine's addresses, and the
  template has no port, no link and no host listener. Its one secret is `TUNNEL_TOKEN`. Its settings carry the
  public address and the owner's Cloudflare Access application (team domain, audience tag, allowed e-mail
  addresses), all three or none. The first boot says on the console what it installs and what failed, and the keeper
  says whether each part is healthy (HTTP 401) and whether the tunnel has registered. The pinned server image is the
  first build with the gate and its Cloudflare Access sign-in. No workspace was brought up on
  a real machine in this work: see "An Automagik workspace in its own virtual machine" in `docs/OPERATING.md` for
  the steps and for what stays unproven.
- Automagik Arca carries the Open Intelligence tag of the [we unfold](https://weunfold.ai/) movement, in its
  one-colour cut with the we unfold mark: in the two READMEs, and at the foot of every page `arca serve` returns (the
  approval page, the canvas, its sign-in page and its error page). On a page it is an inline picture 170 px wide, in
  the page's ink on the page's ground in light and dark, loaded from nowhere and never a link. The files and where they
  come from are in `assets/open-intelligence/`.
- A service may talk to the kernel. A container template declares `kernelAccess: { "port": <n> }`, and its service, in
  the node's own project, finds the kernel's MCP at `http://127.0.0.1:<n>/mcp` inside its instance: one Incus proxy
  device that listens inside the instance and connects to the node's new services socket, `listen.services`. That
  socket serves the MCP routes alone and never the approval page, to the server's own user and to root, so no other
  local user reaches it and it has no port. Nothing crosses the bridge and no ACL rule changes. The token the service
  is given still decides everything: without one every call is 401. Such a service is created only from a proposal the
  owner approved, whose card says the access in words; it is refused in a tenant and on a node that sets no
  `listen.services`. `serve` refuses to start while a service has the access through a socket the config no longer
  serves. `service_list` and `service_status` show it. See "A service that may talk to the kernel" in
  `docs/OPERATING.md` and Rule 69 in `docs/OWNERSHIP.md`.
- `GET /metrics` and `GET /alerts` are held to a token's tool list, as the verbs they read are: a token minted with
  `--tools` needs `node_metrics` for `/metrics` and `alerts_list` for `/alerts`, or the route answers 403. A token
  with no tool list reads both, as before. **If a scraper's token has a tool list, add the verb to it.**
- `serve` binds the services socket only where every directory above it, too, belongs to the server's user or to root
  and cannot be written to by anyone else (a sticky directory such as `/tmp` may be above it).

- the release build no longer takes the runner-pool template's /home/runner paths for a leak, and names any path it does refuse
- versions are cut automatically: a push to dev that changes the product publishes a release with its program, its installer and their checksums
- a short README with the logo and the diagram; the quickstart, the layout and the build notes move into docs (owner's wording, safety lines exact)
- the workspace template takes its address, reach and sign-in settings at create, on the owner's card
- a tenant's block filled by something else is the node's capacity, not the tenant's
- the workspace template can be reached on a tailnet only, through a node in its own network and a relay to the gate
- out-of-room and rate refusals have their own codes, say whose limit it was and when to try again
- the approval card shows every element of a list setting; sizes, numbers and characters in settings are checked before approval
- settings at create for templates that list them, the runner pool's follow-ups, and default sizes that fit
- scripts/install.sh, one command that sets up a read-only node
- a finished folder copy leaves the list before it is journalled; the tests that depended on a machine's speed or on downloaded crates no longer do
- the runner pool's first boot installs the docker command and says what is missing; pool tests can no longer hang; every CI job has a limit
- the read-only web routes follow a token's tool list; the services socket checks the directories above it; a rebuilt record follows the device (security re-check SHIP)
- a template may give its service access to the kernel, through a socket that serves the token-protected API alone
- automagik-workspace, one Automagik workspace in its own virtual machine behind its gate and a tunnel
- the README opens with what Arca is, what works today and what does not yet (wording from the page's fact table)
- a link to a project that does not exist always lands on the default project
- Merge pull request #1 from automagik-si/fix/ci-timing-tests
- service_create takes start: false and creates the service stopped
- the Open Intelligence tag of we unfold, one colour, in the READMEs and in the pages' footers
- a virtual machine in a tenant has its stored secrets written into its guest
- the repository's own CI on Arca's runner pool for trusted events, hosted runners for the rest; a bench against GitHub's runners
- a virtual machine takes a first-boot script, settings and secrets; github-runner-pool template

## 0.1.0-alpha.7

The first release under the name Automagik Arca, and the first with a binary to download since `0.1.0-alpha.2`. The
tags `v0.1.0-alpha.3` to `v0.1.0-alpha.6` were never published as releases, so this section covers everything since
`alpha.2`.

**The name.** The product is now called Automagik Arca. The binary is still `arca`, and nothing a running node reads
has changed: the config keys, the paths, the verb names, the marks on Incus objects and the project names are the same.
The repository is `github.com/automagik-si/arca`.

**New in the binary.**

- `arca --version` (also `arca version` and `arca -V`) prints `Automagik Arca <version> (<short commit>, <target triple>)`.
  `arca_status` now reports this release's version, `0.1.0-alpha.7`, where it used to say `0.1.0`.
- A portable Linux x86_64 binary, linked against glibc 2.34, so it starts on Enterprise Linux 9, Ubuntu 22.04,
  Debian 12 and newer. `scripts/release.sh` builds it with its `SHA256SUMS`.

**Owner approvals.**

- `arca serve` serves an approval page. The owner approves with a passkey, one tap per change. The first passkey is
  enrolled with a one-time code shown on the node's own screen, with no terminal step, and a trusted passkey adds or
  removes another.
- An assistant can propose an action (`action_propose`) or ask for an approval (`owner_approval_request`); the owner
  approves or declines it on the page. The cards say what will happen and, afterwards, exactly what happened, also when
  an approved action then fails.
- One page can approve changes for another kernel on the same machine or on another machine.
- Every destructive verb takes an optional `confirm`.

**Services.**

- Services from templates: create, list, status, logs, removal, connections between services, secrets, and publishing.
  The shipped templates are installed in one step with `arca templates install`, and include a template that publishes
  a service through a Cloudflare Tunnel.
- Services have limits, autostart, data volumes of their own and a LAN port.
- Postgres databases over MCP, with consumer links.
- A service another manager created can be imported in place, and released again.
- The Omni verbs, built on the service model.

**Machines.**

- Virtual machines, on bridged nodes (the host's own bridges, with required caps) and on mixed nodes (containers on the
  managed network beside virtual machines).
- Tenants: isolated service projects on one node, each with its own caps and addresses, with provisional deploys.
- `instance_start`, `instance_stop` and `instance_restart` for Arca's own instances.
- Per-card GPU inventory and readings, and assignment approved by passkey.
- Adoption takes stopped containers too, lists what a container already uses as allowances, and `adopt verify` can
  check a service through an http address.

**Snapshots.**

- The owner plugs in a disk, picks it as snapshot storage, and takes snapshots of apps and data volumes onto it, each
  step approved. Folders are snapshotted through an unprivileged copy helper.

**Health.**

- Alerts: rules in `alerts.json`, `alerts_list`, a HEALTH line, and a Prometheus export (`GET /metrics`, `GET /alerts`).
  Readings cover disk pools, instances and crash loops, connection latency, SMART drive health, and incus-osd updates,
  Secure Boot and TPM.
- The node keeps its metric points and alert events in a SQLite store of its own, with rollups and retention.
- Host metrics read the Incus host's own totals and choose local or remote at runtime. The watcher sweeps on a schedule
  instead of on each call.

**Nodes talking to each other.**

- `arca link`: two nodes pair with a one-time code, keep their membership in sync, and carry MCP calls to each other
  (`arca call --peer <name>`).

**The canvas.**

- `packages/arca-canvas-dsh`, a DSH plugin: machines, projects, services, GPUs, phones and desktops on one canvas, every
  Arca host in one view, a Doctor chat that answers in plain language, and approvals inside the canvas. A remote canvas
  sits behind the owner's passkey.

**Tokens and the MCP door.**

- A token can be limited to a list of tools, and revoked; `SIGHUP` reloads the tokens.
- `journal_list` and `ontology_query` are new reads. The OpenRPC document carries each verb's `readOnlyHint`.

**Not in this release.** There is no `arca init`; Linux x86_64 is the only published binary.

## 0.1.0-alpha.2

The last release published before this one, on 2026-10-30.
