# App update: one verb for an app installed from packages

**Status: not built.** This is a design for issue #240. Nothing in it exists in the code. It was written against `dev`
at `1c68d8c2`.

The live case, in general words: GitLab CE inside a plain Ubuntu container was upgraded by hand. A person took a
snapshot, ran `apt-get install gitlab-ce=<version>`, and checked the app. Arca has no verb for that. This design adds
one: `app_update`.

Decided since the first draft: that case is served by a service Arca makes from a shipped template named `gitlab`,
not by adoption. That template and the move of the app's data into the service are a separate pull request; this
design writes no template file. Section 3, an adopted instance, is the design for later.

Every sentence about what Arca does **today** names a file and line at that base. A path with no folder is under
`crates/arca/src/`. What was not read is marked *inferred*.

## What stands today

- The only update path is `service_update`. It moves a container to another image digest by rebuilding its root
  (`service_update.rs:1-16`). It refuses an adopted service, a virtual machine and a function
  (`service_update.rs:820-827`).
- Arca runs commands inside an instance only from a closed list. No command takes a raw argument list, a path or a
  shell program (`crates/arca-incus/src/exec.rs:76-104`; Rule 50, `docs/OWNERSHIP.md:851-857`). The path refuses an
  adopted instance, an OCI container that is not the node's edge, and a stopped instance (`service_exec.rs:152-168`,
  `service_exec.rs:186-188`), and reaches only the node's own service project
  (`crates/arca-incus/src/exec.rs:318-328`).
- Every project Arca makes is restricted (Rule 30, `docs/OWNERSHIP.md:334-335`), and a restricted project refuses an
  Incus snapshot. `service_update` asks for one, gets the refusal, and goes on without (`service_update.rs:1534-1545`,
  `docs/OPERATING.md:1959-1961`). Its rollback then rebuilds the old root from the old image
  (`service_update.rs:1720-1732`).
- `service_update` never snapshots the data volume, so a rollback keeps what the new version wrote there
  (`service_update.rs:33-34`, `docs/OPERATING.md:2106-2107`).
- `snapshot_create` is another thing: an owner-approved copy onto a disk the owner plugged in, kept in the project
  `arcak-snapshots` (`storage.rs:14-23`, `crates/arca-incus/src/snapshot.rs:3-5`). Arca has no step that restores one
  (`docs/OPERATING.md:24`, `docs/OPERATING.md:1129-1130`).

So this design needs three new things under the verb: a way to run a command a template declares, Incus snapshots of
an instance and its volumes in a project that allows them, and a restore of both.

## 1. Template fields

A template gains one optional field, `appUpdate`, with three parts. All three are required when the field is present.
The first worked example is the formula of the `gitlab` template:

```json
"appUpdate": {
  "update":  { "command": ["/usr/bin/env", "DEBIAN_FRONTEND=noninteractive", "/bin/sh", "-ec",
                           "apt-get update && apt-get install --yes gitlab-ce={version}-ce.0"],
               "seconds": 3600 },
  "health":  { "command": ["/usr/bin/curl", "--fail", "--silent", "--max-time", "5",
                           "http://localhost/-/readiness"],
               "seconds": 900 },
  "version": { "command": ["/usr/bin/awk", "NR == 1 { print $2 }", "/opt/gitlab/version-manifest.txt"] }
}
```

`app_update {name: "gitlab", version: "19.4.8"}` runs the update command with `gitlab-ce=19.4.8-ce.0`, waits up to
900 seconds for the readiness address to answer, and reads `19.4.8` back. Each command was checked against GitLab's
public documentation; none was run.

- **update.** The upgrade page gives `sudo apt update && sudo apt install gitlab-ce=<version>-ce.0` for a named
  version of the Linux (Omnibus) package (`docs.gitlab.com/update/package/`). The same page says the package backs up
  its database before it installs a newer version. That is the app's own safety net, beside Arca's snapshot.
- **health.** `GET /-/readiness` can be reached from localhost with default settings and answers 503 on failure
  (`docs.gitlab.com/administration/monitoring/health_check/`). `curl --fail` turns that into a non-zero exit. A
  redirect is not a failure to curl, so this check fits a template whose app answers plain HTTP on that address.
- **version.** "The version of GitLab and all bundled libraries" is in `/opt/gitlab/version-manifest.txt`
  (`docs.gitlab.com/administration/package_information/`). That its first line is the package's name and then its
  version is *inferred*: the page does not give the layout, and the template's pull request checks it on a real
  install.

| Field | Required | Meaning | Limits |
|---|---|---|---|
| `update.command` | yes | The command that moves the app to a version. A list of arguments, never one line. | 1 to 32 elements, each 1 to 512 bytes of plain text. The first is an absolute path. `{version}` appears at least once, never in the first element. |
| `update.seconds` | yes | How long the command may run. | 10 to 7200. No default: the template's author knows the app. |
| `health.command` | yes | Exit code 0 means the app is healthy. Tried every 5 seconds. | Same list limits. No `{version}`. One try may take 30 seconds. |
| `health.seconds` | no | How long health has to come, in all. | 1 to 900, 120 when absent: the range and default of `service_update`'s `timeoutSeconds` today (`service_update.rs:50-53`). |
| `version.command` | yes | Prints the app's version as the first line of its output. | Same list limits. No `{version}`. |
| `version.seconds` | no | How long the reader may run. | 1 to 60, 15 when absent. |

The whole field is at most 8 KiB as compact JSON. "Plain text" is the rule templates already use for words an owner
reads (`plain_text`, `service.rs:2773-2777`).

The field is its own name on purpose. A template already has `update`, which is a virtual machine's pinned version
(`{ "pin": ... }`, `service.rs:175-182`) and is refused on a container template (`service.rs:2157-2161`). `appUpdate`
does not touch it.

**Which templates.** In the first build only a `system` container template may carry `appUpdate`. Virtual machines
are in section 2. The same app can also ship as an image named by digest. A service made that way is an OCI
container, and it takes today's path: `service_update` to another digest. Its root is replaced
(`service_update.rs:9-11`), so nothing a package manager installs there lasts. A formula's health check and version
reader do not apply to it in this design: the closed list refuses an OCI container that is not the node's edge today
(`service_exec.rs:165-168`, `service.rs:518-521`), and a formula has all three parts or none. Giving `service_update`
a formula's health check and version reader is left for later; it changes no field of this design.

**The health check is a command, not a port.** The node can reach a service's port only through a proxy, so today's
check falls back to "the instance stayed running for 10 seconds" for most services (`service_update.rs:1239-1252`,
`docs/OPERATING.md:1971-1974`). A command inside the instance can ask the app itself.

### A version can never become shell text

Two rules together give that.

1. **Arca builds no command line.** `update.command` is a list. Arca replaces the exact text `{version}` inside each
   element and sends the list to Incus as a list, the way it sends its closed commands today
   (`crates/arca-incus/src/exec.rs:239-242`). No shell reads it unless the template names a shell as its own program,
   as the example does. One element stays one argument. The first element is fixed text, so a version never picks the
   program.
2. **A version is a plain word.** The verb takes a version only when all of this holds:
   - it is 1 to 64 bytes;
   - its first byte is an ASCII letter or digit;
   - every other byte is an ASCII letter, a digit, or one of `.` `_` `+` `-` `~` `:`;
   - `~` never comes straight after `:`.

   None of those characters ends a word, quotes, or starts a substitution in a shell. A first byte that is a letter
   or digit means a version is never read as an option (`--force`). The last rule is there because some shells expand
   `~` after `:` in a word that looks like an assignment. The set covers package versions with an epoch and a
   revision, such as `5:27.0.3-1~ubuntu.24.04`.

Anything else is refused as an invalid argument before any step. `{version}` is the only placeholder. There is no
escaping and no quoting to get wrong, because nothing outside the set is ever let in.

### An older Arca

Template fields are strict today: `Template` refuses a key it does not know (`service.rs:1645-1647`), and a template
that does not parse "cannot be read" (`service.rs:1965-1966`). So an Arca older than this feature refuses a template
that has `appUpdate`: it creates nothing from it, and `service_status` says the template "cannot be used as it is"
(`service.rs:3038-3043`). A service already made from that template keeps running; that part is *inferred*, from the
record holding what the service needs (`service.rs:3001-3003`). Two consequences. A shipped template gains the field
only in the release whose code reads it. Before going back to an older build, the newer template files must be
replaced by that build's own (`arca templates install`, `cmd_templates.rs:1`).

## 2. The verb: `app_update`

`app_update` takes `{name, version}` and the optional `confirm` every interrupting verb takes
(`docs/OPERATING.md:2237`). `name` is the service. `version` follows the rules above. It needs a `doctor` token, like
`service_update` today (`crates/arca-api/src/verbs.rs:537-542`).

The answer of a good update:

```json
{ "updated": true, "service": "gitlab",
  "version": { "asked": "19.4.8", "before": "19.3.5", "after": "19.4.8" },
  "snapshot": "pre-app-update-<time>", "volumes": ["gitlab-data"], "healthBefore": true,
  "output": ["<the last lines of the update command, masked>"], "outputCut": true,
  "message": "service gitlab was updated from 19.3.5 to 19.4.8 and its health check passed" }
```

A call for the version the reader already prints changes nothing and answers `updated: false`, as `service_update`
does for the digest a service already runs (`service_update.rs:875-882`). The call answers when the update ends,
which can be an hour. A caller that stops waiting loses nothing: the run goes on to its end, and `service_status`
shows it (below).

### Steps, in order

Each step is one line in a new journal file, `app-updates.jsonl`, in the state directory. A line has the shape
`service_update`'s journal lines have today: `{update, at, service, step, ok, detail}` (`service_update.rs:322-330`),
in a private file that is set aside at 8 MiB (`service_update.rs:331-345`). It is a new file so that
`service-updates.jsonl` keeps its own promise that no line holds a secret (`service_update.rs:4-5`).

0. **Checks, nothing written.** The service exists and has a formula (section 3 says where it is read from). The
   version is valid. No other update of this service runs: one claim per service, as today
   (`service_update.rs:106-128`). The instance is the one Arca's record names, by its uuid
   (`service_update.rs:915-918`). The version reader runs if the instance runs.
1. **`begin`.** One line with the version asked, the version read, whether the instance ran, the snapshot's name and
   the SHA-256 of the formula. If it cannot be written, the call is refused, as today (`service_update.rs:984-986`).
2. **`snapshot`.** Taken of the instance **as it is, running or stopped**. Arca stops nothing for it, so an update
   adds no downtime of its own. First an Incus snapshot of the instance, named `pre-app-update-<time>`, then one of
   the same name of each custom volume the instance mounts (for a service, its data volume, `service.rs:666-674`).
   The instance snapshot holds its root, config and devices (`crates/arca-incus/src/lifecycle.rs:343-344`). The line
   names every part taken, in order, with the time of each.
3. **`start`**, only for a service that was stopped: no command runs in a stopped instance today
   (`service_exec.rs:186-188`). It was snapshotted in step 2 while still stopped, so nothing wrote during its
   snapshot.
4. **`health before`.** The health check, journalled and answered (`healthBefore`), never judged. Decided: an app
   that is unhealthy before its update is still updated, since an update is often the fix. An instance that already
   ran gets one try. One just started gets tries until `health.seconds` have passed, so the app is up first.
5. **`command`.** Two lines. The first says the command starts, and it must be written: the read at a restart depends
   on it. Then the update command runs with the version in place. The second line has its exit code, how long it
   ran, and the kept output.
6. **`health`.** The health command is tried every 5 seconds until it exits 0 or `health.seconds` have passed.
7. **`version`.** The reader runs. Its answer is reported, not judged. Decided: a version that differs from the one
   asked, with health passing, is a good update, and the answer shows both. Package versions carry suffixes, so the
   two often differ in text.
8. **`drop snapshot`**, when an older pre-update snapshot of this service exists (see "The pre-update snapshot").
9. **`end`.**

A failure from the `command` step on rolls back **by itself**: stop, restore the instance snapshot, restore each
volume snapshot, start again if it ran before, run the health check for at most 30 seconds (the time `service_update`
gives an old version today, `service_update.rs:60-61`), read the version. Each is a journal line. The call is then
refused with the code `rolled_back` and says what happened. A stopped service is started for the update and left
running after a good one; after a rollback it is stopped again as it was.

**The snapshot is crash-consistent.** The instance and each volume are snapshotted one after the other while the app
runs, not at one instant, and nothing asks the app to pause or to flush. After a rollback the app starts as it would
after a power cut, with its disks a few moments apart. Arca promises that each disk comes back as Incus held it at
its own snapshot, and it journals the order and the times. Arca does not promise that the app recovers: that is the
app's own crash recovery, and the rollback's health check reports whether it did.

### When a step fails or hangs

| What fails | What Arca does | The service is left | What the caller gets |
|---|---|---|---|
| A check in step 0, or `begin` cannot be written | Nothing. | As it was. | A refusal that ends "Nothing was changed". |
| The snapshot cannot be taken: Incus refuses, the project does not allow one, there is no room | Removes the parts already taken. | As it was. A running service never stopped. | A refusal with Incus's reason, "Nothing was changed". There is no update without a snapshot: a package install cannot be undone by rebuilding the root from an image. |
| Incus does not finish the snapshot within its deadline (one hour) | Incus may still be at work, so no part is removed now. Writes `end`. | As it was, running or stopped. | `update_unsettled`: the code `service_update` uses when Incus may still be at work (`service_update.rs:1356-1368`). The refusal names the snapshot. Parts left under that name go in the next update's `drop snapshot` step, or at 7 days. |
| A stopped service cannot be started for the update | Stops what Incus left half started. Removes the snapshot. | Stopped, as it was. | A refusal, "Nothing was changed". |
| The line that says the command starts cannot be written | Removes the snapshot. Stops the instance again if it was stopped. | As it was. | A refusal, "Nothing was changed". |
| The update command exits non-zero | Rolls back. | The old version, as it was at the snapshot. | `rolled_back`, with the exit code and the kept output. |
| The update command runs past `update.seconds` | The command is killed inside the instance at its limit, as every command Arca runs is today (`crates/arca-incus/src/exec.rs:230-237`). Rolls back. | The old version, as it was. | `rolled_back`, "ran past N seconds", with the kept output. |
| Incus does not answer about the command by Arca's own deadline | Rolls back. The stop ends what still runs inside. | The old version, as it was. | `rolled_back`. |
| Health never comes within `health.seconds` | Rolls back. | The old version, as it was. | `rolled_back`, with the last lines of the health command. |
| The version reader fails after a good health check | Nothing: the update is good. | The new version, running. | `updated: true`, `version.after: null`, and the message says the version could not be read. |
| After a rollback the old version does not pass health in 30 seconds | Nothing more. | The old version, running. | `rolled_back`, with a note, as `service_update` notes it today (`service_update.rs:1416-1418`). An app that did not recover from a crash-consistent snapshot shows here. |
| The rollback itself fails: the stop, a restore, or the start | Stops trying. Keeps every snapshot. Journals which parts were restored. | Stopped. | `rollback_failed`, with both failures and the snapshot's name: `service_update`'s code for this (`service_update.rs:1424-1444`). `app_rollback` tries the restore again (section 4). |
| Any other journal line cannot be written after `begin` | Goes on, and says so on Arca's own log, as today (`service_update.rs:357-363`). | - | - |

**Arca restarts in the middle.** The claim is in memory today (`service_update.rs:106-110`) and nothing at this base
reads the update journal back at a start (*inferred*: a search found only writers of that file). `app_update` adds a
read at start. For each `begin` in `app-updates.jsonl` with no `end`:

- no line that says the command starts: nothing inside the instance was changed, and it was never stopped. Arca
  removes any snapshot that carries the name in `begin`, stops the instance again if `begin` says it was stopped,
  and writes `end` with `interrupted: true`;
- that line and no good `health` line: the update is of unknown state. Arca rolls it back by itself, exactly as the
  verb does, and writes `end` with `interrupted: true` and whether the rollback worked. A command still running
  inside the instance is ended by the rollback's stop;
- a good `health` line: the update was good. Arca writes the `end`.

The read runs beside the start, never in front of it: a slow restore does not keep Arca from serving. The service
refuses a second `app_update` until its `end` is written.

### What Doctor and the owner see

Doctor gets the answer or the refusal above. `service_status` gains an `appUpdate` block for a service with a
formula: where the formula comes from (`template` or `record`), the last version Arca read and when, the update that
runs now and its step, and the last update's outcome (`updated`, `rolled_back`, `rollback_failed`, `nothing_changed`,
`unsettled`, `interrupted`) with its snapshot's name. Arca runs no command for a status read: the version shown is
the one the last update read. The owner is asked nothing for an update. The owner sees what Doctor says, and the
service's state where services are listed. After `rollback_failed` that state is `stopped`.

### What is kept of the command's output

Today a command's output is capped at 64 KiB and a longer one is an error (`crates/arca-incus/src/exec.rs:11`,
`service_exec.rs:325`); only one command's error output is read at all (`service_exec.rs:244-250`); and the exec
audit holds no arguments and no output (`docs/OWNERSHIP.md:854-855`). A package manager prints far more, and its last
lines are what explain a failure. So for a formula's commands:

- Arca reads both outputs as a stream and keeps the **last 64 KiB**. A longer output is never an error; the journal
  and the answer say `outputCut: true`.
- That text is masked the way `service_logs` masks a log today (`handlers.rs:8699-8735`): every PEM block is hidden
  whole first, then each line is cleaned of control bytes (`clean_log`, `service.rs:4185-4212`) and has the node's
  stored secret values, the instance's own environment values and the known credential shapes replaced by `[secret]`
  (`service.rs:4214-4235`, `service.rs:4363-4369`). A text that begins inside a key is hidden up to the key's end
  (`service.rs:4278-4293`), which is what a cut tail needs.
- The **last 60 lines**, each cut at 300 characters, go into the `command` journal line and into the answer. The
  answer marks them untrusted, as `service_logs` is (`crates/arca-api/src/verbs.rs:507-513`): they are the app's
  words, not Arca's.
- The health command keeps its last 5 lines the same way, only when it fails. The version reader's output is never
  shown raw: its first line is the version when it passes the rules of section 1, and `null` otherwise.
- The recordings are deleted from the instance on every outcome, and the exec audit stays as it is: no arguments, no
  output (`service_exec.rs:287-304`, `service_exec.rs:311`).

### The pre-update snapshot

It is kept after a good update, so that the owner can still go back (section 4). Decided: it is kept until the next
good update of that service, and at most 7 days. Arca removes it, with no card, in three cases:

- after a rollback that worked, since the instance is then what the snapshot holds;
- when the **next** good update of the same service passes its health check, in that update's `drop snapshot` step;
- when it is 7 days old. Arca looks at each start and once an hour after, and writes the removal as a line in
  `app-updates.jsonl`.

So a service has at most one, plus the one of an update that is running. After a removal, `app_rollback` changes
nothing and answers that the service has no pre-update snapshot, with the date the last one was removed and which of
the three cases removed it. The 7 days count for the snapshot of an update that ended good. After `rollback_failed`
the snapshot is the only way back to a working service, so in this design it stays until that rollback is finished.
That is question 2. Removing the service removes its snapshots with it (*inferred* from Incus deleting an instance's
and a volume's snapshots with them).

These are Incus's own snapshots, inside the instance's project. They are not `snapshot_create` copies and
`snapshot_list` does not show them.

### Container and virtual machine

**Containers only in the first build**: a `system` container of the node's own service project. An adopted
container comes later (section 3).

A virtual machine is left out, for three reasons. Arca has never run a command in a guest; its log read avoids it
because "an exec needs a guest agent the image may not run" (`handlers.rs:8654`). A virtual-machine template's
version today is its `update.pin` (`service.rs:2985-2997`), and #241 is about to change how that works. And the live
case is a container. The formula's shape is the same for a machine, so adding it later changes no template. A service
in a tenant is left out too: no command is run inside a tenant's instance today
(`crates/arca-incus/src/exec.rs:330-334`).

### What changes under the verb

- **A second kind of command.** Rule 50's closed list stays closed. Beside it, one new type holds a formula's
  command: built only from a formula that passed section 1's limits and a version that passed its rules, wrapped in
  the same in-instance time limit, with no environment added and run as root, as today
  (`crates/arca-incus/src/exec.rs:230-242`). It keeps Rule 50's identity checks and its audit. It is a new rule in
  `OWNERSHIP.md`.
- **Snapshots in the service project.** Decided: the service project allows Incus snapshots. Its owner entry gains
  one more widening step in Rule 55's table (`docs/OWNERSHIP.md:956-965`): `restricted.snapshots` set to `allow`,
  written by Arca the first time an app update needs it, with no card. An adoption project already allows snapshots
  (`crates/arca-incus/src/adopt.rs:433-447`).
- **Volume snapshots and their restore**, and the removal of a pre-update snapshot: new, narrow writes beside the
  state changer that snapshots and restores an instance today (`crates/arca-incus/src/lifecycle.rs:315-361`).

## 3. An adopted instance gets a formula (#244)

**For later.** Decided: the first real case does not come through adoption. It is a service made from the `gitlab`
template, so nothing in the first build needs this section. It is the design for an adopted instance and the last
pull request of section 6. Its question, which adoption path (question 1), stays open and holds nothing else back.

**Today.** A service record of an adopted instance is built from the other manager's three keys on the instance (its
template name, address and ports), with `adopted: true` (`handlers.rs:7094-7106`, `handlers.rs:7154-7184`). Arca
reads no template for it (`handlers.rs:6613-6615`), and its record writes nothing to the instance
(`service.rs:1286-1290`). A record that exists is kept as it is on later reads (`handlers.rs:7269-7294`).

**Where the formula lives.** The service record gains one optional field, `appUpdate`, with exactly the shape of
section 1. It is written only when set, so every other record reads and writes as before. A service has one formula,
from one place:

- a service made from a template: the template's `appUpdate`, as installed on the node when the verb is called. A
  corrected command reaches every service made from that template with no further step;
- an adopted service: its record's `appUpdate`.

**Who may attach or change it.** A new verb, `app_formula_set`, takes `{name, appUpdate}` (or `appUpdate: null` to
take it off) for an adopted service. It checks the formula by section 1's limits and writes the record. It needs a
`doctor` token and no card. Decided: a formula set over MCP is not shown to the owner first. `service_import` also
takes an optional `appUpdate`, so that an import and its formula are one card: an import already needs the owner's
approval (`docs/OPERATING.md:5046-5054`), and the card then shows the three commands.

**How the verb updates it.** `app_update {name, version}` runs the same steps. Two things differ. The lines are
masked with the credential shapes and PEM blocks only, since Arca stores no secret for an adopted service. And Arca
has no template to compare it with: `service_status` shows the `appUpdate` block and no `update` block, as today
(`handlers.rs:6613-6615`).

**An older Arca.** A service record is strict too (`service.rs:410-412`), so a build older than this feature cannot
read a record that carries a formula. Before going back to an older build, take the formula off each adopted service.
The same is asked today of imports before a downgrade (`docs/OPERATING.md:5087-5089`).

**What stands in the way today** (this is question 1).

1. A hand-made container cannot be imported. `service_import` takes only an instance that already carries the other
   manager's service keys, in a project of that manager (`docs/OPERATING.md:5016`, `docs/OPERATING.md:5037-5039`).
   A plain container with an app installed by hand carries none (*inferred*: nothing hand-made sets them).
2. An imported instance stays in the other manager's project, where the one write Arca makes is one key (Rule 52,
   `docs/OWNERSHIP.md:891-893`), and every other writer refuses that project (`docs/OWNERSHIP.md:905-907`). An update
   has to snapshot that instance, run a command in it, and maybe stop and restore it.

The other adoption path, `arca adopt`, fits better: it takes a container out of the default project into its own
adoption project (`docs/OPERATING.md:4820-4821`), which Arca owns and which already allows snapshots
(`docs/OPERATING.md:4864`). But it makes no service record for a container without those keys, since a record is
built only from them (`handlers.rs:7094-7106`), and it is run from a terminal today (`docs/OPERATING.md:4836-4848`).

## 4. The approval model

The owner's rule, word for word: "work first, make safe later, so an update needs no passkey and a rollback that
loses data does."

**Today.** `service_update` needs a `doctor` token and asks the owner for nothing (`docs/OPERATING.md:1953-1954`).
A `viewer` token only reads and proposes (`docs/OPERATING.md:36`); the owner then approves on the card, or in the
owner's signed-in session, where a passkey tap counts for 12 hours (`docs/OPERATING.md:2551`). An action that
deletes or erases is never approved in the session: it keeps the passkey card (`action_proposal.rs:286-301`,
`action_proposal.rs:1885-1889`).

| The call | Who | What it needs |
|---|---|---|
| `app_update`, called by a `doctor` token | Doctor | **No card.** This covers its snapshot, its command, its health check, its own rollback, the first widening of the project for snapshots, and the removal of an older pre-update snapshot. |
| The rollback inside `app_update`, after a failed step, and the same rollback at a restart | Arca | **No card, no person** (issue #240's acceptance). |
| `app_formula_set`, called by a `doctor` token (section 3, later) | Doctor | **No card.** Decided. |
| `service_status` with its `appUpdate` block | any token | Nothing: a read. |
| `app_update` or `app_formula_set`, proposed by a `viewer` token through `action_propose` | the signed-in owner | **One yes in the owner's signed-in session**, or the card. No passkey beyond the session's own. A viewer writes nothing by itself today either. |
| `service_import` with a formula | the owner | What an import needs today: the owner's approval of its card (`docs/OPERATING.md:5054`). |
| `app_rollback` of an update that ended good | the owner | **The passkey card.** Never asked in the session. |
| `app_rollback` of an update whose own rollback failed | Doctor | **No card**: it finishes the rollback that was already Arca's to make. |

**When a rollback "loses data".** A rollback puts the instance and its volumes back to the moment of the snapshot.
Everything the app wrote after that moment is erased.

- The rollback **inside the verb** runs minutes after the snapshot, on an update that failed its own check. What it
  erases is what the app wrote in that window, most of it by a version that never became healthy. No person accepted
  that version. It runs by itself.
- A rollback **asked for later** is different. The update ended good, the verb answered, and the app went on
  serving. People used it. Putting the snapshot back erases every write since, and nothing brings them back. That is
  a rollback that loses data in the rule's sense. It is its own verb, `app_rollback {name}`, it runs only from an
  approved proposal, and its action counts as one that erases, so the signed-in session lists it and never approves
  it. Its card says in words that everything the app stored since the update, with the update's date, is erased.

`app_rollback` stops the instance, restores the one pre-update snapshot the service has, starts it, then runs the
health check and reads the version. With no such snapshot it changes nothing and says why (section 2).

Nothing else in this design asks the owner for anything, and no guard is added beyond what the steps need to end in a
known state.

## 5. How this sits with #241 and #245

#245 is the goal: Doctor deploys and maintains any app, updates included. #240 is its first working piece, for the
app that already runs and is updated from inside. It needs no image build and no repository.

#241 is about apps that update **themselves**. It needs the same four parts this design builds, and should call them
rather than build them again: the version reader, the health check, the snapshot of an instance with its volumes, and
the rollback to it. Here they sit behind `app_update`, with the caller choosing the moment and the version. In #241
the app chooses the moment and Arca must learn of it first. This design leaves all of that to #241: the field that
says a template updates itself, the signal an app sends before it updates, the end of comparing a service against a
pinned version, virtual machines and their `update.pin`, and a fleet view that reads live versions. The `appUpdate`
block in `service_status` is the place such a view would read from.

One rule is shared: how many pre-update snapshots Arca keeps, and for how long. Decided for this verb: one for each
service, until its next good update and at most 7 days. #241 should take the same rule. This design does not cover
#242 (from a repository to a running service) or #243 (suggesting code changes); a formula written by Doctor for a
new app would use section 1's shape as it is.

## 6. Build plan

Each pull request is inside the size band (at most 25 files and 2,000 handwritten insertions) and stands on its own.

| # | What it builds | What it proves alone | Tests |
|---|---|---|---|
| 1 | The `appUpdate` field on a template: parsing, limits, the version rules, the `{version}` replacement. `service_status` shows where a formula comes from. No command is run. | The shape is fixed, and a version cannot leave its argument. | Unit tests of every limit and of hostile versions (spaces, quotes, `$(`, `;`, a leading `-`, `:~`). Every shipped template still reads the same. |
| 2 | The formula command in `arca-incus`: the new command type, the stream read that keeps a tail, the masking. The new rule in `OWNERSHIP.md`. | A declared command runs in an owned container, is bounded in time, and its output is kept short and masked. | The fake Incus: exit codes, a command past its time, an output over the cap, a secret and a cut key in the output, an instance that changed meanwhile. The closed list's own tests unchanged. |
| 3 | Snapshots: an instance with its volumes, taken with no stop; the restore of both; the removal; the service project allows snapshots (Rule 55's new row). | A running instance is snapshotted without a stop, and it and its data volume come back as each was at its snapshot. | The fake Incus for each request and refusal, and that a snapshot sends no stop and no start. One test on a real Incus: a running instance that writes, snapshot, change both disks, restore, compare; the same for a stopped one. |
| 4 | `app_update`: the steps, `app-updates.jsonl`, the rollback, the answers and refusals, the run that outlives its caller, the `appUpdate` block's `running` and `last`. Its section in `OPERATING.md`. | The table in section 2, row by row. A good update never stops the instance. | One scenario test per row on the fake Incus, each asserting the journal lines, the answer, the state left, and that a stop is sent only by a rollback. A stopped service: snapshot before its start. |
| 5 | The read at start of interrupted updates. | An update cut at any step ends in a known state with no person. | A journal cut after each line, then a start: the expected lines and state for each. Cut before the line that says the command starts: no stop, no restore. Cut after it: a rollback. |
| 6 | `app_rollback`, its card and its action; the retention of pre-update snapshots. | A later rollback runs only from the passkey card, and says what it erases. A snapshot goes at 7 days. | The card's words; the session never approves it; the restore run; the retry after `rollback_failed` with no card; a snapshot 7 days old is removed and `app_rollback` then answers that there is none; the one of a failed rollback stays. |
| 7 | The proof of #240's acceptance on a real Incus: a container with a small package-installed app is updated to a named version and reports it; the same with a health check that fails is rolled back with no person. | The two acceptance lines of the issue. | A real-Incus test beside the ones the repository already has for snapshots. It also asserts that the good update never stopped the container. |
| 8 | Adopted services: the `appUpdate` field on a record, `app_formula_set`, the optional `appUpdate` of `service_import`, the formula command on an adopted container. Waits for the answer to question 1. | #244: an adopted instance is updated by the same verb. | The fake Incus for the record writes and the import card; every existing record still reads the same; the path question 1 picks, on a real Incus. |

1, 2 and 3 do not depend on each other. 4 needs all three. 5, 6 and 7 need 4. 8 is last: it needs 4 and the answer to
question 1, and nothing before it waits for that answer. The shipped `gitlab` template and the move of the app's data
are not in this plan: they are a separate pull request. Its template can carry the formula of section 1 once 1 is
merged, and `app_update` works on it once 4 is.

## 7. Open questions

Five questions of the first draft are decided and now stand in the text (sections 1 to 4). Two are open.

1. **Which adoption path carries a hand-made instance (#244)?** Not needed for the first build.
   (a) `arca adopt`: the container moves into its own adoption project, and Arca then makes a service record for it.
   (b) `service_import`: it stays where it is, the import learns to take an instance with no service keys, and
   Rule 52 gains the writes an update needs in another manager's project.
   **Recommend (a).** Arca already owns an adoption project and it already allows snapshots; (b) turns "one key" into
   snapshot, command, stop and restore in a project that is not Arca's.
2. **Does the 7-day limit also remove the snapshot of an update whose own rollback failed?**
   (a) No: it stays until `app_rollback` finishes that rollback, or the service is removed (the design as written).
   (b) Yes: every pre-update snapshot goes at 7 days.
   **Recommend (a).** After `rollback_failed` the service is stopped and that snapshot is the only way back to a
   working one. It is not a copy kept for a change of mind.
