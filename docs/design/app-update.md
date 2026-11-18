# App update: one verb to update an app, by a new image or from packages

**Status: not built.** This is a design for issue #240. Nothing in it exists in the code. It was written against
`dev` at `1c68d8c2`.

The live case, in general words: GitLab CE inside a plain Ubuntu container was upgraded by hand. A person took a
snapshot, ran `apt-get install gitlab-ce=<version>`, and checked the app. Arca has no verb for that. This design adds
one: `app_update`.

Decided since the first draft: that app becomes a service Arca makes from a shipped template named `gitlab`, an OCI
image named by digest with one data volume. So the first build covers two kinds of service with the one verb. For a
**by-digest** service the update step is the move to another image digest that `service_update` makes today. For a
**system container** it is a command the template declares, such as a package install. The snapshot, the health
check, the version reader and the rollback are the same for both. The template and the move of the app's data into
the service are a separate pull request; this design writes no template file. An adopted instance (section 3) is the
design for later.

Every sentence about what Arca does **today** names a file and line at that base. A path with no folder is under
`crates/arca/src/`. What was not read is marked *inferred*.

## What stands today

- The only update path is `service_update`. It moves a container to another image digest by rebuilding its root
  (`service_update.rs:1-16`). It refuses an adopted service, a virtual machine and a function
  (`service_update.rs:820-827`).
- A service verb runs a command inside a service's instance only from a closed list: serve's service exec, under Rule
  50 (`docs/OWNERSHIP.md:851-857`). No command of that list takes a raw argument list, a path or a shell program
  (`crates/arca-incus/src/exec.rs:76-104`). That path refuses an adopted instance, a stopped instance, and an OCI
  container that is not the node's edge, save for one connector status read (`service_exec.rs:152-168`,
  `service_exec.rs:186-188`). It reaches only the node's own service project
  (`crates/arca-incus/src/exec.rs:318-328`).
- Other writers start commands of their own, fixed in code, in machines and helpers that are not a service's
  instance. Two of them: a runner pool starts one in its own job machine through the Incus agent, detached, and waits
  on the operation (Rule 77, `docs/OWNERSHIP.md:1752-1758`; `crates/arca-incus/src/runner_vm.rs:238-293`), and the
  folder snapshot helper runs one fixed script (`crates/arca-incus/src/folder.rs:717-745`).
- `service_update` takes the new digest as an argument and keeps the repository the service's record names
  (`service_update.rs:794-800`, `service_update.rs:242-253`). `service_status` shows the digest a service was created
  with beside the one its installed template names now (`service.rs:3018-3023`).
- The service project does not allow Incus snapshots. The provider pins `restricted` on every kernel project it
  creates and lets a plugin set no other guarded key (`crates/arca-providers/src/incus.rs:599-605`; Rule 30,
  `docs/OWNERSHIP.md:334-344`), and Incus refuses a snapshot in a restricted project that does not set
  `restricted.snapshots` to `allow`. `service_update` asks for one, gets the refusal, and goes on without
  (`service_update.rs:1534-1545`, `docs/OPERATING.md:1959-1961`). Its rollback then rebuilds the old root from the
  old image (`service_update.rs:1720-1732`). Other projects differ: an adoption project is restricted and allows
  snapshots (`crates/arca-incus/src/adopt.rs:433-447`), a tenant's project forbids them unless it allows them
  (`crates/arca-incus/src/lifecycle.rs:315-317`), and `arcak-snapshots` sets no `restricted` key
  (`crates/arca-incus/src/snapshot.rs:147-158`).
- `service_update` never snapshots the data volume, so a rollback keeps what the new version wrote there
  (`service_update.rs:33-34`, `docs/OPERATING.md:2106-2107`).
- `snapshot_create` is another thing: an owner-approved copy onto a disk the owner plugged in, kept in the project
  `arcak-snapshots` (`storage.rs:14-23`, `crates/arca-incus/src/snapshot.rs:3-5`). Arca has no step that restores one
  (`docs/OPERATING.md:24`, `docs/OPERATING.md:1129-1130`).

So this design needs three new things under the verb: a way to run a command a template declares, Incus snapshots of
an instance and its volumes in a project that allows them, and a restore of both.

## 1. Template fields

A template gains one optional field, `appUpdate`. It is one name, read strictly, with two shapes that the template's
kind picks. The first worked example is the formula of the `gitlab` template, a by-digest (OCI) template. It has no
`update` part, since the update is the image move:

```json
"appUpdate": {
  "health":  { "command": ["/usr/bin/curl", "--fail", "--silent", "--max-time", "5",
                           "http://localhost/-/readiness"],
               "seconds": 900 },
  "version": { "command": ["/usr/bin/awk", "NR == 1 { print $2 }", "/opt/gitlab/version-manifest.txt"] }
}
```

`app_update {name: "gitlab"}` moves the service to the digest its installed template names, waits up to 900 seconds
for the readiness address to answer, and reads the version back. Each part was checked against GitLab's public
documentation; none was run.

- **the image.** A GitLab image "contains all of the necessary services in a single container"
  (`docs.gitlab.com/install/docker/`). An upgrade makes the container again from a newer image with the same volumes,
  and "on the first run, GitLab reconfigures and upgrades itself" (`docs.gitlab.com/update/docker/`).
- **health.** `GET /-/readiness` can be reached from localhost with default settings and answers 503 on failure
  (`docs.gitlab.com/administration/monitoring/health_check/`). `curl --fail` turns that into a non-zero exit. A
  redirect is not a failure to curl, so this check fits an app that answers plain HTTP on that address.
- **version.** "The version of GitLab and all bundled libraries" is in `/opt/gitlab/version-manifest.txt`
  (`docs.gitlab.com/administration/package_information/`). Three things are *inferred* and are for the template's
  pull request to check on the image: that the first line is the package's name and then its version, that the image
  holds this file, and that it has `curl`, `awk` and `timeout` at these paths.

The second example is a system container with the same app installed from its Linux package, which is the live case
as it was done by hand. Its `appUpdate` has the same `health` and `version` and one more part, the command:

```json
"update": { "command": ["/usr/local/sbin/gitlab-package-update", "{version}"], "seconds": 3600 }
```

`app_update {name, version: "19.4.8"}` starts that file with `19.4.8` as its one argument. The file is the template's
own script inside the container. It holds the two commands GitLab's upgrade page gives for a named version of the
Linux package, `apt update` and `apt install gitlab-ce=<version>-ce.0`, with its argument in the place of `<version>`
(`docs.gitlab.com/update/package/`). The same page says the package backs up its database before it installs a newer
one. A formula of one command needs no script, as long as the package index in the container is current:
`["/usr/bin/apt-get", "install", "--yes", "gitlab-ce={version}-ce.0"]`.

**A gap this design does not fill.** A container service has no create-time setting today: a template's `settings`
and `createSettings` are a virtual machine's only (`service.rs:1701-1708`, `service.rs:2144-2150`). So a value the
owner must give, such as the app's external address, is set as a secret after a `service_create` with `start: false`
and before the first start (`docs/OPERATING.md:1047-1062`). Section 7 lists it.

| Field | Required | Meaning | Limits |
|---|---|---|---|
| `update.command` | system container: yes. By-digest: never, the field is refused. | The command that moves the app to a version. A list of arguments, never one line. | 1 to 32 elements, each 1 to 512 bytes of plain text. The first is an absolute path. `{version}` stands in exactly one element, once, never the first. |
| `update.seconds` | with `update.command` | How long the command may run. | 10 to 7200. No default: the template's author knows the app. |
| `health.command` | yes | Exit code 0 means the app is healthy. Tried every 5 seconds. | Same list limits. No `{version}`. A try is cut at 30 seconds, or at what is left of `health.seconds` when that is less. |
| `health.seconds` | no | How long health has to come, in all. | 1 to 900, 120 when absent: the range and default of `service_update`'s `timeoutSeconds` today (`service_update.rs:50-53`). |
| `version.command` | yes | Prints the app's version as the first line of its output. | Same list limits. No `{version}`. |
| `version.seconds` | no | How long the reader may run. | 1 to 60, 15 when absent. |

The whole field is at most 8 KiB as compact JSON. "Plain text" is the rule templates already use for words an owner
reads (`plain_text`, `service.rs:2773-2777`).

The field is its own name on purpose. A template already has `update`, which is a virtual machine's pinned version
(`{ "pin": ... }`, `service.rs:175-182`) and is refused on a container template (`service.rs:2157-2161`). `appUpdate`
does not touch it.

**Which templates.** Two kinds may carry `appUpdate` in the first build. An OCI template has `health` and `version`
and no `update`: the image move replaces the root (`service_update.rs:9-11`), so nothing a package manager installs
there lasts. A `system` container template has all three. Any other mix is refused when the template is read, and so
is the field on a virtual-machine template (section 2).

**The health check is a command, not a port.** The node can reach a service's port only through a proxy, so today's
check falls back to "the instance stayed running for 10 seconds" for most services (`service_update.rs:1239-1252`,
`docs/OPERATING.md:1971-1974`). A command inside the instance can ask the app itself.

### What a version can and cannot do

The construction, decided:

1. **No shell.** `update.command` is a list of arguments. Arca starts its first element, a fixed absolute path, and
   passes the rest as arguments, the way it sends its closed commands today
   (`crates/arca-incus/src/exec.rs:239-242`). Arca builds no command line, and no shell reads the list.
2. **One argument.** `{version}` stands in exactly one element, never the first. Arca replaces that exact text, and
   the element stays one argument whatever the version is. `{version}` is the only placeholder.
3. **A plain version string.** 1 to 64 bytes, each an ASCII letter, a digit, or one of `.` `_` `+` `~` `:` `-`. The
   first byte is a letter or a digit, so a version that fills a whole argument is never read as an option
   (`--force`). The set covers package versions with an epoch and a revision, such as `5:27.0.3-1~ubuntu.24.04`.
   Anything else is refused as an invalid argument before any step.
4. **A formula that needs a shell names its own script file.** More than one command, a pipe, a variable: those are
   written in a script inside the instance. The formula names that file, and the version reaches it as one argument.
   A formula must not put `{version}` inside the text of a program, and no example here does; nothing refuses one
   that does (below).

What this promises: a version never picks the program Arca starts, never becomes a second argument, and is read by no
shell on its way in. What it does not promise is what the named program does with its arguments. A template that
names a program which runs one of its arguments as a program (`env`, a shell with `-c`, `xargs`) and puts the version
there has given the version that power. The rules above do not catch that. It is a wrong template, and pull request
1's tests carry it as a known case the rules do not refuse. The same holds for a script that uses its argument
carelessly; by rule 3 that argument has no space, no quote and no `$` in it.

### An older Arca

Template fields are strict today: `Template` refuses a key it does not know (`service.rs:1645-1647`), and a template
that does not parse "cannot be read" (`service.rs:1965-1966`). So an Arca older than this feature refuses a template
that has `appUpdate`: it creates nothing from it, and `service_status` says the template "cannot be used as it is"
(`service.rs:3038-3043`). A service already made from that template keeps running; that part is *inferred*, from the
record holding what the service needs (`service.rs:3001-3003`). Two consequences. A shipped template gains the field
only in the release whose code reads it. Before going back to an older build, the newer template files must be
replaced by that build's own (`arca templates install`, `cmd_templates.rs:1`).

## 2. The verb: `app_update`

`app_update` takes `{name}`, a `version` or a `digest`, and the optional `confirm` every destructive verb takes
(`docs/OPERATING.md:2237`). `name` is the service. It needs a `doctor` token, like `service_update` today
(`crates/arca-api/src/verbs.rs:537-542`). What names the target depends on the kind of service:

- **System container:** `version`, required, by the rules above. It goes into the update command. A `digest` is an
  invalid argument.
- **By-digest service:** a digest. It is the `digest` argument when one is given, taken as `service_update` takes it
  today: `sha256:` and 64 hex digits, in the repository the service's record names (`service_update.rs:794-800`,
  `service_update.rs:242-253`). With no `digest` it is the one the installed template names (`service.rs:2985-2999`),
  so `app_update {name}` is enough once a newer template is installed. `version` is optional here and never picks the
  image. It is what the caller expects to find, and it is answered as `asked` beside the version read. When the two
  do not agree the digest stands, the update is good by the rule of step 7, and the message says in words that the
  version read is not the one asked.

**Where a digest comes from.** Arca cannot turn a version into a digest for this app: its one lookup of a registry's
tags is for another image, for a card's label (`image_version.rs:1-3`). So "update it to 19.4.8" becomes a call in
one of two ways. A newer template is installed, by an Arca release or `arca templates install`, and the call is
`app_update {name}`. Or Doctor has the digest of that version from the publisher, and the call is `app_update {name,
digest, version: "19.4.8"}`. A digest is always pulled from the repository in the service's record. If the installed
template names another repository, its digest is not found there: the rebuild fails at the pull and the update rolls
back with nothing lost.

The answer of a good update of a by-digest service:

```json
{ "updated": true, "service": "gitlab", "update": "<update id>",
  "digest": { "from": "sha256:<old>", "to": "sha256:<new>" },
  "version": { "asked": null, "before": "19.3.5", "after": "19.4.8" }, "notSnapshotted": [],
  "snapshot": "pre-app-update-<time>", "volumes": ["gitlab-data"], "healthBefore": true,
  "message": "service gitlab was updated from 19.3.5 to 19.4.8 and its health check passed" }
```

A system container's answer has no `digest` and adds `output` and `outputCut`, the last lines of the update command
(below). `update` is the id every journal line of this run carries; `app_rollback` takes it (section 4).

**A call that has nothing to do.** A by-digest call for the digest the service already runs changes nothing and
answers `updated: false`, as `service_update` does today (`service_update.rs:875-882`). For a system container the
version asked is compared, as text, with what the reader prints, when the instance runs. Equal: nothing changes, and
the answer is `updated: false`. Not equal, or a stopped instance: the update runs. A reader that prints a suffix
(`19.4.8-ce.0` for `19.4.8`) never compares equal; the package manager then finds nothing to do, and the cost is one
snapshot.

The call answers when the update ends, which can be an hour. A caller that stops waiting loses nothing: the run goes
on to its end, and `service_status` shows it (below).

### Steps, in order

Each step is one line in a new journal file, `app-updates.jsonl`, in the state directory. A line has the shape
`service_update`'s journal lines have today: `{update, at, service, step, ok, detail}` (`service_update.rs:322-330`),
in a private file that is set aside at 8 MiB (`service_update.rs:331-345`). It is a new file so that
`service-updates.jsonl` keeps its own promise that no line holds a secret (`service_update.rs:4-5`).

The two kinds differ in when the snapshot is taken. A good update never stops a system container, so its snapshot is
taken while it runs. A by-digest service is stopped by its own move, so its snapshot is taken just after that stop:
decided. Both begin the same way.

0. **Checks, nothing written.** The service exists and has a formula (section 3 says where it is read from). The
   version or digest is valid. No other update of this service runs: one claim per service, as today
   (`service_update.rs:106-128`). The instance is the one Arca's record names, by its uuid
   (`service_update.rs:915-918`). A by-digest service also passes every check `service_update` makes today before it
   writes (`service_update.rs:820-933`), answered in its words, save the one for a declared port: the formula's
   health check replaces the port check. The version reader runs if the instance runs. For an instance that is marked
   hung, a clearing stop comes in front of this step ("What changes under the verb").
1. **`begin`.** One line with the version or digest asked, the version read, the digest the service runs, whether the
   instance ran, the snapshot's name, the parts the snapshot will have, and the SHA-256 of the formula. If it cannot
   be written, the call is refused, as today (`service_update.rs:984-986`).

**A by-digest service** then goes on like this:

2. **`health before`.** One try of the health check if the instance runs. It is journalled and answered
   (`healthBefore`), never judged. Decided: an app that is unhealthy before its update is still updated, since an
   update is often the fix.
3. **`move starts`**, a line that must be written: the read at a restart depends on it. Then **`stop`**, the stop
   `service_update` makes today (`service_update.rs:1525-1526`).
4. **`snapshot`**, of the stopped instance. First an Incus snapshot of the instance, named `pre-app-update-<time>`,
   then one of the same name of each custom volume the instance mounts (for a service, its data volume,
   `service.rs:666-674`). The instance snapshot holds its root, config and devices
   (`crates/arca-incus/src/lifecycle.rs:343-344`). The app has shut down in its own order and nothing writes, so the
   snapshot is exact. No stop is added for it: the move's own stop lasts as much longer as the snapshots take.
   `service_update`'s own snapshot (`service_update.rs:1527-1533`) is not taken; this one stands in. The line names
   every part taken, with its time, every disk not taken (below), and the instance's `last_used_at` as Incus reports
   it then: the time of its last start.
5. **`rebuild`** of the root from the new digest, then **`start`**: the steps `service_update` makes today
   (`service_update.rs:1554-1563`). Incus keeps the config, the devices and the data volume
   (`service_update.rs:9-11`). Each has the deadline `service_update` gives a step, 600 seconds
   (`service_update.rs:56-57`).
6. **`health`.** The health command is tried every 5 seconds until it exits 0 or `health.seconds` have passed. It
   runs in the new root.
7. **`version`.** The reader runs. Its answer is reported, not judged. Decided: a version that differs from the one
   asked, with health passing, is a good update, and the answer shows both.
8. **`record`.** The new digest is written to the service's record, after the health check and never before, as today
   (`service_update.rs:1567-1568`, `service_update.rs:1683-1684`).
9. **`drop snapshot`**, when a snapshot of an earlier update of this service exists, a good one's or an unfinished
   one's (see "The pre-update snapshot"). Then **`end`**.

**A system container** goes on like this:

2. **`snapshot`**, of the instance as it is, running or stopped, with the same parts and names as above. Arca stops
   nothing for it, so an update adds no downtime of its own.
3. **`start`**, only for one that was stopped: no command runs in a stopped instance today
   (`service_exec.rs:186-188`). It was snapshotted while still stopped, so nothing wrote during its snapshot.
4. **`health before`**, as above. An instance that already ran gets one try. One just started gets tries until
   `health.seconds` have passed, so the app is up first.
5. **`command starts`**, a line that must be written. Then **`command`**: the update command runs with the version in
   place. Its line has the exit code, how long it ran, and the kept output.
6. **`health`**, **`version`**, **`drop snapshot`** and **`end`**, as above. Package versions carry suffixes, so the
   version asked and the one read often differ in text.

**The rollback.** A failure after the root or the app was touched (from `rebuild`, or from `command`) is rolled back
**inside the same run**, straight after the step that failed. The rollback's lines have their own step names, as
`service_update`'s have today (`restore`, `rebuild back`, `start` with `old`, `old answers`;
`service_update.rs:1717`, `service_update.rs:1722`, `service_update.rs:1741-1743`). Here they are: `rollback starts`;
`rollback stop`; `restore`, of the instance snapshot; `restore volume`, once for each volume snapshot; `start old`,
if the instance ran before; `old answers`, the health check for at most 30 seconds (the time `service_update` gives
an old version today, `service_update.rs:60-61`); `old version`, the version read. `old answers` and `old version`
run only after `start old`, as `service_update` checks an old version only when it ran before
(`service_update.rs:1739-1744`). A rollback that leaves the instance stopped makes no check and is recorded
`rolled_back_healthy`, as today's code holds that answer true. No line of a rollback is ever named `health`, `start`
or `version`. The call is then refused with the code `rolled_back` and says what happened. `old answers` is the
rollback's own result, never the update's: the refusal carries it as `oldAnswers`, and the run is recorded as rolled
back healthy or rolled back unhealthy by it. A stopped service is left running after a good update; after a rollback
it is stopped again as it was. With one more at a restart, for an instance that has not run since its snapshot
(below), these are the only restores Arca makes with no person. Every other one is `app_rollback`, on the owner's
card (section 4).

**Every run ends in exactly one recorded state.** Decided. A run that wrote `begin` writes `end` as its last line, as
`service_update` does on each of its outcomes today (`service_update.rs:1306`, `service_update.rs:1357-1358`,
`service_update.rs:1373-1374`, `service_update.rs:1395`, `service_update.rs:1429`). The line names one state, and
`service_status` shows it:

- `updated`: the new version passed its health check. Only this state ever records a new digest.
- `rolled_back_healthy`: the old version is back and `old answers` passed.
- `rolled_back_unhealthy`: the old version is back and `old answers` did not pass in its 30 seconds.
- `kept_for_owner`: the update is left unfinished (below). The line also says why: `rollback_failed`, `unsettled`, or
  `interrupted`.

A run that ended before its update step touched anything is not an update at all: its `end` says `nothing_changed`,
as every refusal that ends "Nothing was changed" does. It is the record of a refused call, not a fifth outcome of an
update; when the instance was left stopped, `service_status` says so and names `instance_start`, as the refusal does.
A `begin` with no `end` means that Arca itself stopped in the middle, or that the `end` could not be written. The
read at a start takes such a run by its lines (below). A run whose `end` says rolled back is never read again.

**Why a by-digest update gets this rollback.** Such an app changes its stored data when a new version first starts:
the example's image "reconfigures and upgrades itself" on its first run. Today's rollback puts the old root back and
keeps what the new version wrote on the data volume (`service_update.rs:33-34`), so the old version would start on
data it may no longer read. Here both go back: the instance snapshot holds the old root, config and devices, and the
volume snapshot holds the data.

**A system container's snapshot is crash-consistent.** The instance and each volume are snapshotted one after the
other while the app runs, not at one instant, and nothing asks the app to pause or to flush. After a rollback the app
starts as it would after a power cut, with its disks a few moments apart. Arca promises that each disk comes back as
Incus held it at its own snapshot, and it journals the order and the times. Arca does not promise that the app
recovers: that is the app's own crash recovery, and the rollback's health check reports whether it did. A by-digest
service's snapshot has no such limit: its app was stopped.

**What a snapshot does not hold.** It holds the instance's root and each custom volume the instance mounts. A disk
that is a path of the host is in no Incus snapshot (*inferred*: Incus's behaviour, not read here); an adoption
project may allow such disks today (`crates/arca-incus/src/adopt.rs:81-82`). The `snapshot` line and the answer list
every disk that was not snapshotted (`notSnapshotted`). After a rollback, what the new version wrote there stays.
That is the gap `service_update` has for its data volume today (`docs/OPERATING.md:2106-2107`). It is recorded, not
refused.

**`service_update` stays as it is**, for every service, with a formula or without: the same move, its own port check,
and its own rollback, which keeps the new version's writes on the volume. No refusal is added to a call that works
today. For a service with a formula its answer gains one line that names `app_update`. `withTemplate`, `eraseData`
and `follow` stay `service_update`'s alone. One thing changes around it. Once the service project allows snapshots,
`service_update` takes its own instance snapshot there, as its code does wherever a project allows one
(`service_update.rs:1527-1533`), and its rollback restores it in place of a rebuild (`service_update.rs:1710-1719`).
Nothing at this base removes that snapshot (*inferred*: a search found no removal), so the look that removes old
snapshots in this design removes those too, at 7 days.

**What else may touch the service meanwhile.** A run can last hours. `app_update`, and its read at a restart, take
the one claim that `service_update` and `service_reconfigure` take for a service (`service_update.rs:106-128`,
`service_update.rs:883-884`, `service_reconfigure.rs:741-742`), so none of them runs beside another on one service.
`instance_start`, `instance_stop`, `instance_restart` and `service_remove` take no such claim (*inferred*: a search
found it taken in those two files only), and this design adds none. A stop or a restart during the update command
ends the command, which is a failed step and so a rollback. A `service_remove` during the run takes the instance
away; each write of the state changer reads the instance again and refuses one that is gone or was made again
(`crates/arca-incus/src/lifecycle.rs:323-327`, `crates/arca-incus/src/lifecycle.rs:346-350`), and the update ends
with that failure in its journal.

### When a step fails or hangs

| What fails | What Arca does | The service is left | What the caller gets |
|---|---|---|---|
| A check in step 0, or `begin` cannot be written | Nothing. | As it was. | A refusal that ends "Nothing was changed". |
| The line that says the update step starts cannot be written | System container: removes the snapshot, and stops the instance again if it was stopped. | As it was. | A refusal, "Nothing was changed". |
| By-digest: the stop fails | Starts it again if it ran. | As it was. | A refusal, "Nothing was changed", as `service_update` answers it today (`service_update.rs:1370-1387`). |
| The snapshot cannot be taken: Incus refuses, or there is no room | Removes the parts already taken. By-digest: starts the instance again if it ran. | As it was. A running system container never stopped. | A refusal with Incus's reason, "Nothing was changed". There is no update without a snapshot: a package install cannot be undone by rebuilding the root from an image, and what a new version wrote on a data volume cannot be undone at all. |
| Incus does not finish the snapshot within its deadline (one hour) | Incus may still be at work, so no part is removed and nothing is started now. Writes `end` with `nothing_changed`. | System container: as it was. By-digest: stopped, with its old root. | `update_unsettled`: the code `service_update` uses when Incus may still be at work (`service_update.rs:1356-1368`). The refusal names the snapshot and, for a by-digest service, `instance_start`. Nothing inside was changed, so the parts left are not a way back: the next look removes them once Incus has ended. |
| A stopped system container cannot be started for the update | Stops what Incus left half started. Removes the snapshot. | Stopped, as it was. | A refusal, "Nothing was changed". |
| By-digest: the rebuild, the start or the record fails | Rolls back. | The old version on the old data, exactly as at the snapshot. | `rolled_back`, with Incus's reason. |
| By-digest: Incus does not finish the rebuild in time | Rolls nothing back, so as not to rebuild and restore at once, as today (`service_update.rs:1356-1368`). Keeps the snapshot. Records `kept_for_owner`. | Stopped, with the root as Incus leaves it. | `update_unsettled`. The update is left unfinished (below). |
| System container: the update command exits non-zero | Rolls back. | The old version, as it was at the snapshot. | `rolled_back`, with the exit code and the kept output. |
| System container: the update command runs past `update.seconds` | The command is killed inside the instance at its limit, as every Rule 50 command is today (`crates/arca-incus/src/exec.rs:230-237`). Rolls back. | The old version, as it was. | `rolled_back`, "ran past N seconds", with the kept output. |
| Incus does not answer about a formula's command by Arca's own deadline: the update command, a `health` try or the `version` read after it | Marks the instance hung, as a Rule 50 command does today. The step has failed. Rolls back: the rollback's stop ends what still runs inside and clears the mark. | The old version, as it was. | `rolled_back`. |
| Health never comes within `health.seconds` | Rolls back. | The old version, as it was. | `rolled_back`, with the last lines of the health command. |
| The version reader fails after a good health check | Nothing: the update is good. | The new version, running. | `updated: true`, `version.after: null`, and the message says the version could not be read. |
| After a rollback the old version does not pass `old answers` in 30 seconds | Nothing more: the snapshot is removed, as after any rollback that worked. Records `rolled_back_unhealthy`. | The old version, running. | `rolled_back` with `oldAnswers: false`, and a note, as `service_update` notes it today (`service_update.rs:1416-1418`). An app that did not recover from a crash-consistent snapshot shows here. |
| The rollback itself fails or does not end in time: the stop, a restore, or the start | Stops trying, in this run and after it. Keeps the snapshot. Journals which parts were restored. Records `kept_for_owner`. | Stopped, if the stop worked. If the stop is what failed: as it is, which can be the new version running, or a command that did not answer still running under the hung mark. | `rollback_failed`, with both failures, the snapshot's name and the update's id: `service_update`'s code for this (`service_update.rs:1424-1444`). The update is left unfinished (below). |
| The `drop snapshot` step fails after a good update | Nothing: the update is good. | The new version, running. | `updated: true`. The older snapshot is still there; the next look removes it. |
| Arca restarts after the update step began (the read at a start, below) | By-digest, with a whole snapshot and an instance that has not run since it: restores the snapshot and starts the old version, by itself. Otherwise: restores nothing. | The old version on the old data, in the first case. Otherwise as the restart found it. | No caller waits any more. `service_status` shows rolled back, healthy or unhealthy, or `kept_for_owner` with `interrupted` and why a card is needed. |
| Any other journal line cannot be written after `begin` | Goes on, and says so on Arca's own log, as today (`service_update.rs:357-363`). | - | - |

**An update left unfinished.** That is an update that ended `rollback_failed`, or `update_unsettled` in its rebuild,
or that a restart cut after its update step began while the instance ran since its snapshot, or while Arca cannot
tell. Arca makes no restore for it by itself, at any later time: the app may have served people since. Its recorded
state is `kept_for_owner`. Three things are then true.

- **The service stays as it was left**, until a person or Doctor acts. Arca starts nothing, stops nothing and
  restores nothing. That can be a stopped instance, the new version running, or a system container half way through
  its packages.
- **Its snapshot is kept.** Decided: no time limit ever removes the snapshot of an unfinished update.
- **`service_status` says so**: the update's id, how it ended, the state of the instance as Incus reports it, the
  snapshot's name and that it is kept because the update is unfinished, and whether a rollback of it waits for the
  owner.

There are two ways on. Going back is `app_rollback {name, update}`: it is proposed, the owner approves it with the
passkey, and it restores exactly the snapshot that update's `begin` line names (section 4). While the card waits,
nothing changes. Going forward is a new `app_update`. It runs as any update does, with its own snapshot of the
service as it stands, and when it ends good its `drop snapshot` step removes the unfinished update's snapshot with
the others. A restore that Incus is still working on makes a later one fail with Incus's own reason, and nothing is
changed by it.

**Arca restarts in the middle.** The claim is in memory today (`service_update.rs:106-110`) and nothing at this base
reads the update journal back at a start (*inferred*: a search found only writers of that file). `app_update` adds a
read at start, for each `begin` in `app-updates.jsonl` with no `end`. It holds the service's claim while it works. An
Incus operation outlives an Arca restart, so the read first waits until no operation that Incus still runs names the
instance or its volumes, by the listing Rule 44 uses today (`docs/OWNERSHIP.md:603-607`). Arca removes no snapshot
and starts no instance under a running operation. Then it takes the first of these cases that fits:

1. **A rollback line is there** (`rollback starts` or any after it): the run was rolling back, and it is never taken
   as good, whatever other lines it has. If the lines show the rollback whole (`restore`, a `restore volume` for each
   volume `begin` names, and `start old` when `begin` says the instance ran), Arca finishes it as rolled back: it
   runs `old answers` if the run had not and the instance runs, removes the snapshot, as a rollback that worked does,
   and writes `end` with `rolled_back_healthy` or `rolled_back_unhealthy`. If a rollback line is a failed one, the
   run's own rollback failed: Arca never restores such a run by itself, and writes `end` with `kept_for_owner`. If
   the rollback was only cut, the case is 5.
2. **No line that says the update step starts:** nothing inside the instance was changed, and Arca stopped nothing.
   Arca removes any snapshot that carries the name in `begin`, stops a system container again if `begin` says it was
   stopped, and writes `end` with `nothing_changed`.
3. **That line, by-digest, no `rebuild` line, and a snapshot that is not whole:** the rebuild never began, since it
   only follows a whole snapshot. Arca looks at the snapshot itself for this, not only at the journal. It removes the
   parts, starts the instance if `begin` says it ran, and writes `end` with `nothing_changed`. Nothing is restored.
4. **A good `health` line, and no failed `record` line and no rollback line after it:** the new version passed its
   health check, so the update was good. Arca finishes what was left of it: the digest in the record for a by-digest
   service, the `drop snapshot` step, and `end` with `updated`. `health` is only ever the new version's check; a
   rollback's is `old answers`. A failed `version` or `drop snapshot` line does not change this: the table calls both
   a good update.
5. **Every other case:** the update step began, and the update neither ended good nor was rolled back whole. Arca
   then asks one thing: has the instance run since its snapshot? If it **has not**, nothing can have been written
   after the snapshot. Arca puts the old version back by itself, with no card: `restore`, `restore volume`, `start
   old` if `begin` says it ran, `old answers`, `old version`. It then removes the snapshot, as after any rollback
   that worked, and writes `end` with `rolled_back_healthy` or `rolled_back_unhealthy`. If a part of that fails, or
   if the instance **ran** since its snapshot, or if Arca **cannot tell**, Arca restores nothing. No check failed
   straight before, and the app may have written since. It writes `end` with `kept_for_owner` and `interrupted`:
   going back is the owner's card. A system container's update command may still run inside; Arca marks the instance
   hung again, since the mark in memory did not outlive the restart (below).

Every `end` the read writes also says `interrupted: true`.

**How Arca knows that an instance has not run since its snapshot.** Three facts, and all three must hold.

1. From its own journal: this update's `snapshot` line is there and good, and it holds the instance's `last_used_at`
   as Incus reported it when the snapshot was taken (step 4).
2. From Incus, now: the instance's status is `Stopped`, the status Arca reads of an instance today
   (`crates/arca-incus/src/lifecycle.rs:75-76`).
3. From Incus, now: the instance's `last_used_at` is still the journalled value. Arca already takes a change of that
   time for a start: a move reads it before and after a copy and treats a change as "was started"
   (`instance_move.rs:2380-2384`, `instance_move.rs:2509-2515`), and the usage sampler counts a start by it
   (`crates/arca-metrics/src/sample.rs:883-884`).

Then the instance was stopped at its snapshot and no start came after, and a stopped instance writes nothing. What
differs from the snapshot is only what the rebuild did to the root, so the restore erases nothing an app wrote. When
Arca cannot tell, it is the card: the `snapshot` line is missing or holds no time, Incus reports no `last_used_at`,
the status is anything but `Stopped`, the read fails, or the two times differ. That a rebuild and a restore leave
`last_used_at` alone is *inferred*, not read in Incus's source; if one moved it, the rule would only fall on the safe
side. Pull request 8's test on a real Incus checks it.

This applies to both kinds, and it only ever holds for a by-digest service. Its snapshot is taken of a stopped
instance, and the rebuild runs before any start. A system container's update command runs only in a running instance
(step 3), so once its update step has begun the instance has run since its snapshot, and the restore is the card. A
system container that was stopped and cut before its start is case 2: nothing was changed.

The read runs beside the start, never in front of it, so it does not keep Arca from serving.

### What Doctor and the owner see

Doctor gets the answer or the refusal above. `service_status` gains an `appUpdate` block for a service with a
formula: where the formula comes from (`template` or `record`), the last version Arca read and when, the update that
runs now and its step, and the last update's id and its one recorded state (`updated`, `rolled_back_healthy`,
`rolled_back_unhealthy`, `kept_for_owner` with its reason, or `nothing_changed`). It also lists each pre-update
snapshot the service holds: its name, the update it belongs to, its date, and why it is kept (the last good update's,
with the day it goes; or an unfinished update's, kept until a rollback or a later good update settles it). For an
update a restart cut it says whether the instance ran since its snapshot (`yes`, or `unknown` with the fact that was
missing). When the instance is marked hung it says so in plain words: a command of the last update did not answer,
and the next update or rollback stops the instance to end it. Arca runs no command for a status read: the version
shown is the one the last update read. The owner is asked nothing for an update. The owner sees what Doctor says, and
the service's state where services are listed, which after an update left unfinished is whatever Incus reports.

### What is kept of the command's output

Today a command's output is capped at 64 KiB and a longer one is an error (`crates/arca-incus/src/exec.rs:11`,
`service_exec.rs:325`); only one command's error output is read at all (`service_exec.rs:244-250`); and the exec
audit holds no arguments and no output (`docs/OWNERSHIP.md:854-855`). A package manager prints far more, and its last
lines are what explain a failure. So for a formula's commands:

- Arca keeps the **last 64 KiB** of the command's two outputs. A longer output is never an error; the journal
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

Each snapshot belongs to one update, by the update's id in the journal. A good update's snapshot is kept, so that the
owner can still go back (section 4). Decided: it is kept until the next good update of that service, and at most 7
days. Arca removes a snapshot, with no card, in these cases:

- after a rollback that worked, in the run, by the read at a restart, or by `app_rollback`, whether the old version
  then answered or not, since the instance is then what the snapshot holds;
- when the **next** good update of the same service passes its health check: its `drop snapshot` step removes the
  snapshots of the updates before it, an unfinished one's among them;
- when a good update's snapshot is 7 days old. Arca looks at each start and once an hour after, and writes the
  removal as a line in `app-updates.jsonl`. The same look removes three other things: at once, a snapshot a failed
  `drop snapshot` step left (an earlier update's, good or unfinished, that the later good update had already
  settled), the parts a snapshot left when Incus did not finish it in time, once Incus has ended, and
  `service_update`'s own `pre-update-<time>` snapshots at 7 days.

Decided: that look never takes the snapshot of an update left unfinished. It stays until a rollback or a later good
update settles it (above).

So "at most one" holds for good updates only. A service can hold the snapshot of its last good update; beside it, the
snapshot of an update that is running or was left unfinished; and, until the next look, parts Incus left.
`app_rollback` names the update it goes back to, so it never has to choose between them. For an update whose snapshot
is gone it changes nothing and answers that there is none, with the date it was removed and which case removed it.
Removing the service removes its snapshots with it (*inferred* from Incus deleting an instance's and a volume's
snapshots with them).

These are Incus's own snapshots, inside the instance's project. They are not `snapshot_create` copies and
`snapshot_list` does not show them; `service_status` does.

**A snapshot under a newer one.** A service can hold a newer snapshot above the one a rollback names:
`service_update`'s own, or a later update's. Incus's storage documentation says a ZFS pool restores only an
instance's newest snapshot unless the pool is set to remove the newer ones, which it is not by default
(`linuxcontainers.org/incus/docs/main/reference/storage_zfs/`). Which drivers a service project's pool may have was
not established here. So this is a named risk, not a decided behaviour: on such a pool an `app_rollback` past a newer
snapshot fails with Incus's reason and changes nothing. The rollback inside a run always restores the newest
snapshot, its own. Pull request 3's test on a real Incus restores past a newer snapshot and records what happens.

### Container and virtual machine

**Containers only in the first build**: an OCI container or a `system` container of the node's own service project.
An adopted container comes later (section 3).

A virtual machine is left out, for three reasons. The new command type is for containers of the service project. No
service verb starts a command in a service's guest today, and the log read avoids one because "an exec needs a guest
agent the image may not run" (`handlers.rs:8654`); the one guest command Arca starts is a runner machine's, by
another writer in another project (Rule 77, `docs/OWNERSHIP.md:1752-1758`). A virtual-machine template's version
today is its `update.pin` (`service.rs:2985-2997`), and #241 is about to change how that works. And the first real
case is a container. The formula's shape is the same for a machine, so adding it later changes no template. A service
in a tenant is left out too: no service command is run inside a tenant's instance today
(`crates/arca-incus/src/exec.rs:330-334`).

### What changes under the verb

- **A second kind of service command.** The closed list stays closed, and it goes on refusing an OCI container
  (`service_exec.rs:165-168`). Beside it, one new type holds a formula's command: built only from a formula that
  passed section 1's limits and a version that passed its rules, with no environment added and run as root, as a Rule
  50 command is (`crates/arca-incus/src/exec.rs:239-242`). It runs in a `system` container and in an OCI container of
  the node's own service project. It makes the checks Rule 50 makes there (the owner's mark, the uuid before and
  after, no tombstone, a running instance) writes the same audit, and keeps its hung mark (below). It is refused for
  an adopted instance (section 3 says what would change that), a tenant's instance and a stopped one. Its time limit
  is the same in-instance `/usr/bin/timeout` (`crates/arca-incus/src/exec.rs:230-237`), so an image must carry that
  program: without it no check of the formula passes, and `health before` shows that first.
- **Rule 50 is edited, and a rule is added.** Rule 50 says today that serve may execute only the closed enum, that it
  refuses OCI instances, and that a recording is capped at 64 KiB (`docs/OWNERSHIP.md:851-854`). Its first sentence
  has to name the second type, and the new rule states that type's own limits: which containers, the kept tail in
  place of the cap, the checks above.
- **The hung mark stays, and it is Arca's to clear.** Decided. Today a command that passes Arca's own deadline marks
  the instance's uuid hung, in memory, and every later command but a cleanup is refused until the uuid changes
  (`service_exec.rs:218-227`, `service_exec.rs:314-321`; Rule 50, `docs/OWNERSHIP.md:855`). The new type keeps that
  mark. A formula command that Incus does not answer for sets it, whichever command it is: the update command, a
  health try, the version reader, `health before`. No formula command runs in a marked instance. A rebuild keeps the
  uuid (`service_update.rs:9-11`), and so does a restore (*inferred*: not read in Incus's source), so here a changed
  uuid is not what clears the mark. Stopping the instance is: a stopped instance runs nothing, so the old command is
  gone. Arca makes that stop itself, and nobody is left stuck. Any mark is cleared this one way, whatever command set
  it.
  - **In the run that set it.** After the update step began, the rollback's stop clears it, straight after the
    command that did not answer, so the rollback's own `old answers` and `old version` run. A mark set earlier in a
    run, by the version reader of step 0 or by `health before`, is cleared by a clearing stop as below, and the run
    goes on from where it was.
  - **In the next call, when that stop had failed.** One order holds for `app_update` and `app_rollback`, and for
    both kinds of service. First the call checks its arguments and takes the service's claim. Then, holding the
    claim, it writes a journal line of its own, `clearing stop`, with whether the instance ran; stops the instance;
    waits until Incus shows it stopped; and clears the mark. Only then come the rest of step 0 and everything after
    it. The clearing stop is in front of step 0's version reader, and that reader is not run on a stopped instance,
    so no formula command of the call ever meets the mark. That is a wait inside the call, not a refusal.
  - **What `begin` records.** `begin` says whether the instance ran as it was **before** the clearing stop. So the
    retry starts the app, and a rollback of the retry starts it again: an app that was serving is not left stopped.
    If a check of step 0 fails after the clearing stop, Arca starts the instance again before it refuses.
  - **Both kinds.** For a by-digest service the clearing stop is the stop its move makes anyway, moved in front of
    step 0: its `health before` is not run, and its step 3 finds the instance already stopped. For a system container
    this one retry stops the instance: it is snapshotted stopped, started, and then updated.
  - **A restart in between.** If Arca itself stops between the clearing stop and `begin`, the read at a start finds a
    `clearing stop` line with no `begin` after it. If Incus shows the instance stopped, Arca starts it again when the
    line says it ran, and writes that run's `end` with `nothing_changed`. If Incus shows it still running, the stop
    never took: the mark is set again, and the `end` is the same.
  - **Status.** Until the mark is cleared, `service_status` says in plain words why the service waits. The mark is in
    memory, so the read at a restart sets it again for a run with no `end` whose command may still be running
    (above).
- **How that command is run.** It takes the way a runner machine's command is run today: started detached, with no
  websocket, then waited on through its Incus operation in bounded waits
  (`crates/arca-incus/src/runner_vm.rs:238-293`). The reason is the length: a command of up to two hours should not
  hang on one request, and the operation's id is journalled as soon as Incus gives it. It does not use that writer.
  `RunnerVms` writes only a pool's own machines in the pool's project, takes its caller's environment and records no
  output (Rule 77, `docs/OWNERSHIP.md:1752-1758`); a formula's command is a service's, has no environment added, and
  has its output recorded.
- **Snapshots in the service project.** Decided: the service project allows Incus snapshots. Its owner entry gains
  one more widening step in Rule 55's table (`docs/OWNERSHIP.md:956-965`): `restricted.snapshots` set to `allow`,
  written by Arca the first time an app update needs it, with no card. The provider has to learn it too. Today it
  refuses a patch of any guarded project key but three named allowances (`crates/arca-providers/src/incus.rs:96-126`,
  `crates/arca-providers/src/incus.rs:1923-1942`; Rule 30, `docs/OWNERSHIP.md:339-344`). This is a fourth: an entry
  that may own the service project, that project, `restricted.snapshots`, the value `allow`.
- **Volume snapshots and their restore**, and the removal of a pre-update snapshot: new, narrow writes beside the
  state changer that snapshots and restores an instance today (`crates/arca-incus/src/lifecycle.rs:315-361`). That
  snapshot write is described for a stopped instance today (`crates/arca-incus/src/lifecycle.rs:315-317`); a system
  container's is taken while it runs, so its words and its tests widen.

## 3. An adopted instance gets a formula (#244)

**For later, and not buildable under today's rules.** Decided: the first real case does not come through adoption. It
is a service made from the `gitlab` template, so nothing in the first build needs this section. Until its question is
answered, `app_update` refuses an adopted service, as `service_update` does today (`service_update.rs:820-822`). What
follows is the design for the day it is answered, and the last pull request of section 6.

**Today.** A service record of an adopted instance is built from the other manager's three keys on the instance (its
template name, address and ports), with `adopted: true` (`handlers.rs:7094-7106`, `handlers.rs:7154-7184`). Arca
reads no template for it (`handlers.rs:6613-6615`), and its record writes nothing to the instance
(`service.rs:1286-1290`). A record that exists is kept on later reads, save its `kernelAccess`, which follows the
instance's device (`handlers.rs:7269-7294`).

**Where the formula lives.** The service record gains one optional field, `appUpdate`, of section 1's
system-container shape, written only when set, so every other record reads and writes as before. A service has one
formula, from one place. One made from a template uses the template's, as installed when the verb is called, so a
corrected command reaches every such service with no further step. An adopted service uses its record's.

**Who may attach or change it.** A new verb, `app_formula_set`, takes `{name, appUpdate}` (or `appUpdate: null` to
take it off) for an adopted service. It checks the formula by section 1's limits and writes the record. It needs a
`doctor` token and no card. Decided: a formula set over MCP is not shown to the owner first. `service_import` also
takes an optional `appUpdate`, so that an import and its formula are one card: an import already needs the owner's
approval (`docs/OPERATING.md:5046-5054`), and the card then shows the three commands.

**How the verb updates it.** `app_update {name, version}` runs a system container's steps. The output is masked
exactly as in section 2, the instance's own environment values included: those are on the instance whoever made it,
and `service_logs` hides them today (`handlers.rs:8699-8709`). `service_status` shows the `appUpdate` block and no
`update` block, as today (`handlers.rs:6613-6615`).

**An older Arca.** A service record is strict too (`service.rs:410-412`), so a build older than this feature cannot
read a record that carries a formula. Before going back to an older build, take the formula off each adopted service.
The same is asked today of imports before a downgrade (`docs/OPERATING.md:5087-5089`).

**What each adoption path needs.** This is question 1, and it is undecided because neither path can carry an update
under today's rules.

- **`service_import`.** It takes only an instance that already carries the other manager's service keys, in a project
  of that manager that the node's config lists under `kernel.importServices` (`docs/OPERATING.md:5016`,
  `docs/OPERATING.md:5037-5039`, `config.rs:485-489`). A plain container with an app installed by hand carries no
  such key (*inferred*: nothing hand-made sets them). The instance stays in that project, where the one write Arca
  makes is one key (Rule 52, `docs/OWNERSHIP.md:891-893`) and every other writer refuses the project
  (`docs/OWNERSHIP.md:905-907`). So this path needs: an import that takes an instance with no service keys; the
  owner's edit of the node's config for each instance; and Rule 52 gaining a snapshot, a restore, a stop, a start and
  a declared command in a project that is another manager's.
- **`arca adopt`.** It takes a container out of the default project into its own adoption project
  (`docs/OPERATING.md:4820-4821`), which already allows snapshots (`docs/OPERATING.md:4864`). But `AdoptWriter` is
  the only path that writes there, through a closed table of steps (Rule 45, `docs/OWNERSHIP.md:656-663`); the state
  changer refuses a project outside the kernel's family before any request
  (`crates/arca-incus/src/lifecycle.rs:571-576`); `serve` refuses a config that lists an adoption, and the adoption
  is run from a terminal (`docs/OPERATING.md:4822`, `docs/OPERATING.md:4836-4848`); and no service record is made for
  a container without the service keys (`handlers.rs:7094-7106`). So this path needs: new steps in Rule 45's table,
  or the state changer's family widened to adoption projects, for the snapshot, the restore, the stop and the start;
  a service record for such a container; and a way for `serve` to act on an adoption.
- **Both** need the rule for declared commands to reach past the node's service project, which the exec target and
  the path of its recordings are fixed to today (`crates/arca-incus/src/exec.rs:318-328`,
  `crates/arca-incus/src/exec.rs:422-425`), and past the refusal of an adopted instance that Rule 50 and the service
  exec make (`docs/OWNERSHIP.md:851-852`, `service_exec.rs:152-154`). Both also need an identity source. Rule 50
  takes identity from the newest live instance effect (`docs/OWNERSHIP.md:852-853`), and step 0 reads the uuid Arca
  journalled when it created the instance (`service_update.rs:915-918`); an adopted instance has no such effect. An
  adoption's own record holds the container's uuid (Rule 45, `docs/OWNERSHIP.md:656-660`); an import has nothing like
  it yet.

## 4. The approval model

The approval principle of this design, given for issue #240: an update needs no passkey, and a rollback that loses
data needs one. It adds no exception to the approval rule Arca has today. The verb's removal of its own pre-update
snapshots is not an erasing plan of that rule: those are Incus snapshots the verb made as its own way back, with a
decided lifetime, and not the owner's `snapshot_create` copies, whose delete keeps the passkey
(`action_proposal.rs:288-297`). Issue #240 itself asks only for the rollback Arca makes by itself. The kept snapshot
and the later `app_rollback` go beyond the issue; they are here for the second half of the principle.

**Today.** `service_update` needs a `doctor` token and asks the owner for nothing (`docs/OPERATING.md:1953-1954`). A
`viewer` token only reads and proposes (`docs/OPERATING.md:36`); the owner then approves on the card, or in the
owner's signed-in session, where a passkey tap counts for 12 hours (`docs/OPERATING.md:2551`). An action that deletes
or erases is never approved in the session: it keeps the passkey card (`action_proposal.rs:286-301`,
`action_proposal.rs:1885-1889`). Whether an action erases is decided from its kind and its arguments alone;
`service_remove` erases by its kind (`action_proposal.rs:288-297`).

| The call | Who | What it needs |
|---|---|---|
| `app_update`, called by a `doctor` token | Doctor | **No card.** This covers its snapshot, its command or its image move, its health check, the rollback inside the same run, the first widening of the project for snapshots, and the removal of older pre-update snapshots. |
| The rollback inside an `app_update` run, straight after its failed step | Arca | **No card, no person** (issue #240's acceptance). |
| The restore at a restart, of an update cut after its snapshot, when the instance has not run since that snapshot | Arca | **No card, no person.** Nothing was written after the snapshot, so nothing an app stored is erased. |
| `app_rollback {name, update}`, in every other case: after a restart cut an update whose instance ran since its snapshot or Arca cannot tell, after an unsettled rebuild, after a rollback that failed or whose stop failed, and past an update that ended good | the owner | **The passkey card.** It runs only from an approved proposal and is never approved in the session: its action erases by its kind, as `service_remove`'s does. |
| `service_update`, on a service with a formula or without | Doctor | As today: no card (`docs/OPERATING.md:1953-1954`). Unchanged. |
| `app_formula_set`, called by a `doctor` token (section 3, later) | Doctor | **No card.** Decided. |
| `service_status` with its `appUpdate` block | any token | Nothing: a read. |
| `app_update` or `app_formula_set`, proposed by a `viewer` token through `action_propose` | the signed-in owner | **One yes in the owner's signed-in session**, or the card. No passkey beyond the session's own. A viewer writes nothing by itself today either. |
| `service_import` with a formula (section 3, later) | the owner | What an import needs today: the owner's approval of its card (`docs/OPERATING.md:5054`). |

**Where the line is.** A rollback puts the instance and its volumes back to the moment of the snapshot. Everything
the app wrote after that moment is erased.

- The rollback **inside the run** comes straight after a step of that same update failed. What it erases is what the
  app wrote between the snapshot and the failure, most of it by a version that never became healthy; for a by-digest
  service only the new version wrote at all, since the old one was stopped before the snapshot. No person accepted
  that version, and the verb has not answered. It needs no card.
- The restore **at a restart, of an instance that has not run since its snapshot**, erases nothing an app wrote: the
  instance was stopped at the snapshot and was not started after (section 2 says which facts show that). It needs no
  card either.
- **Every other restore** is `app_rollback`, and it is an erasing plan like any other today. That is plain past a
  good update: people used the app, and the restore erases every write since. It is also the rule after a restart
  whose instance ran since the snapshot, after an unsettled rebuild and after a failed rollback. No check failed
  straight before, time has passed, and the service may have run and served in between, so Arca cannot say that
  nothing would be lost.

**`app_rollback`.** It always names one update by its id and restores exactly the snapshot that update's `begin` line
names, never another. Doctor proposes it; the card says in words that everything the app stored since that update,
with the update's date, is erased. Approved, it stops the instance, restores the snapshot of the instance and of each
volume, starts it, then runs the health check and reads the version. For a by-digest service it also writes that
update's old digest back to the service's record. For an update whose snapshot is gone it changes nothing and says
why (section 2). While the card waits the service stays as it was left, and `service_status` shows the unfinished
update and that its rollback waits for the owner.

Nothing else in this design asks the owner for anything, and no guard is added beyond what the steps need to end in a
known state.

## 5. How this sits with #241 and #245

#245 is the goal: Doctor deploys and maintains any app, updates included. #240 is its first working piece, for an app
that already runs: updated by a new image digest, or from inside by its packages. It needs no image build and no
repository.

#241 is about apps that update **themselves**. It needs the same four parts this design builds, and should call them
rather than build them again: the version reader, the health check, the snapshot of an instance with its volumes, and
the rollback to it. Here they sit behind `app_update`, with the caller choosing the moment and the version. In #241
the app chooses the moment and Arca must learn of it first. This design leaves all of that to #241: the field that
says a template updates itself, the signal an app sends before it updates, the end of comparing a service against a
pinned version, virtual machines and their `update.pin`, and a fleet view that reads live versions. The `appUpdate`
block in `service_status` is the place such a view would read from.

One rule is shared: how many pre-update snapshots Arca keeps, and for how long. Decided for this verb: a good
update's snapshot is kept one for each service, until its next good update and at most 7 days; an unfinished update's
is kept, with no time limit, until a rollback or a later good update settles it. #241 should take the same rule. This
design does not cover #242 (from a repository to a running service) or #243 (suggesting code changes); a formula
written by Doctor for a new app would use section 1's shape as it is.

## 6. Build plan

Each pull request is meant to fit the size band (at most 25 files and 2,000 handwritten insertions) and to stand on
its own. The sizes are estimates: nothing is written yet, and pull request 4 is the tightest.

| # | What it builds | What it proves alone | Tests |
|---|---|---|---|
| 1 | The `appUpdate` field on a template, in both shapes: parsing, limits, which kind takes which shape, the version rules, the `{version}` replacement. `service_status` shows where a formula comes from. No command is run. | The shape is fixed, and a version stays one plain argument. | Unit tests of every limit, of each shape on the wrong kind, and of hostile versions (spaces, quotes, `$(`, `;`, a leading `-`). One test that documents what the rules do not refuse: a template that hands its version to a program that runs it. Every shipped template still reads the same. |
| 2 | The formula command in `arca-incus`, for a `system` container and for an OCI container of the node's own project: the new command type, the detached start and the bounded waits on its operation, the kept tail, the masking, the hung mark kept and cleared by a stop Arca has seen. The edit of Rule 50 and the new rule in `OWNERSHIP.md`. | A declared command runs in an owned container of either kind, is bounded in time, and its output is kept short and masked. A command that does not answer blocks the next one until the instance was stopped. | The fake Incus: exit codes, a command past its time, an output over the cap, a secret and a cut key in the output, an instance that changed meanwhile, the refusals that stay. A command Incus does not answer for sets the mark; no formula command and no closed-list command runs while it stays; a stop that Arca has seen clears it, and a restore or a rebuild alone does not. The closed list's own tests unchanged, its OCI refusal and its own hung mark among them. |
| 3 | Snapshots: an instance with its volumes, taken as it is; the restore of both; the removal; the list of disks not taken. The service project allows snapshots: Rule 55's new row and the provider's fourth allowance. | A running instance is snapshotted without a stop, a stopped one exactly, and each comes back with its data volume as it was at its snapshot. `service_update`'s snapshot branch, live in an Arca project for the first time, works there. | The fake Incus for each request and refusal, and that a snapshot sends no stop and no start. The provider: the one new allowance passes, every other guarded key is still refused. On a real Incus: a running instance that writes, snapshot, change both disks, restore, compare; the same for a stopped one; a rebuild from another image and then a restore of the snapshot taken before it; `service_update` with a failing health check, restored from its own snapshot; a restore past a newer snapshot, with what the pool's driver answers. |
| 4 | `app_update` for a by-digest service: the target digest, the steps with the snapshot after the stop, `app-updates.jsonl`, the rollback of root and volume, the answers and refusals, the run that outlives its caller, the `appUpdate` block. Its section in `OPERATING.md`. | The by-digest rows of section 2's table. `service_update` answers as before. | One scenario test per row on the fake Incus, each asserting the journal lines, the answer and the state left, and that the snapshot is asked for only after the stop. The digest from the argument and from the template. `service_update`'s own tests unchanged. |
| 5 | `app_update` for a system container: the snapshot while it runs, the start of a stopped one, the command step, the output in the answer and the journal, the call that has nothing to do. | The system-container rows of the table. A good update never stops the instance. | One scenario test per row on the fake Incus, and that a stop is sent only by a rollback. A stopped service: snapshot before its start. A stop and a remove from outside during the command. A command that did not answer and a rollback whose stop failed: the retry stops the instance first, clears the mark, and then updates it as a stopped one. |
| 6 | The read at start of interrupted updates. | An update cut at any step ends in exactly one recorded state, and a run that was rolling back is never read as updated. Arca restores by itself only an instance that has not run since its snapshot. | A journal cut after each line, for each kind, the rollback's own lines among them, then a start: the expected lines and state, the finished `drop snapshot` step among them. A by-digest cut after its snapshot with an instance that has not run since: restored and started with no person. The same with a moved `last_used_at`, a status that is not `Stopped`, or a `snapshot` line without the time: no restore, the update shown as unfinished. A system container cut after its command began: no restore. A running Incus operation on the instance: nothing is removed or started until it ends. A `service_update` asked during the read meets the claim. |
| 7 | `app_rollback {name, update}`, its card and its action; the retention of pre-update snapshots, `service_update`'s own among them; the snapshot list and the unfinished state in `service_status`. | Every restore after the run needs the passkey card and says what it erases. It restores its own update's snapshot and no other. | The card's words; the session never approves it; a `doctor` token alone cannot run it; with two snapshots held, each call restores the one its update names; a good update's snapshot goes at 7 days and `app_rollback` then answers that there is none; an unfinished update's snapshot is still there after 7 days and `service_status` says why; a later good update removes it. |
| 8 | The proof on a real Incus. First the shape of the `gitlab` case end to end, with a small stand-in: an image by digest with one data volume, whose newer version rewrites its data at its first start. It is moved to the newer digest and reports its version; with a health check that fails, the old root and the old data are back with no person. Then #240's two acceptance lines for a container with a small package-installed app. | The first real case's shape, and the issue's acceptance. The real image is proven by the template's own pull request, which also checks the three *inferred* facts of section 1. | Real-Incus tests beside the ones the repository already has for snapshots. They also assert that a good update of the system container never stopped it, and that a start moves `last_used_at` while a rebuild and a restore do not. |
| 9 | Adopted services: what question 1's answer needs in the rules (section 3), the `appUpdate` field on a record, `app_formula_set`, the optional `appUpdate` of `service_import`, the formula command on an adopted container. Waits for the answer to question 1, and may need to be split in two to stay inside the band. | #244: an adopted instance is updated by the same verb. | The fake Incus for the record writes and the import card; every existing record still reads the same; the path question 1 picks, on a real Incus. |

1, 2 and 3 do not depend on each other. 4 needs all three. 5 and 7 need 4. 6 needs 4 and 5. 8 needs 4 and 5, and is
the pull request that proves the `gitlab` case's shape end to end. 9 is last: it needs 5 and the answer to question
1, and nothing before it waits for that answer. The shipped `gitlab` template and the move of the app's data are not
in this plan: they are a separate pull request. That template can carry its formula once 1 is merged, and
`app_update` works on the real service once 4 is.

**Why a new verb, and what was set aside.** The nearest smaller change is a data-volume snapshot inside
`service_update`. It was set aside for three reasons. `service_update` would gain a refusal it does not have today
(no update without a snapshot) and a rollback that erases what the volume gained, for every caller it already has. It
has no place for a health check that asks the app, or for a version. And it does nothing for an app that is updated
from inside its container. `snapshot_create` copies were set aside too: they need storage the owner plugged in and
the owner's approval, and Arca cannot restore one (`docs/OPERATING.md:24`). What is new is kept to one verb for both
kinds, one template field, one journal, and the image move `service_update` already makes.

**Risks.** A system container's snapshot is crash-consistent, so its rollback is as good as the app's own crash
recovery. A pool may refuse to restore a snapshot under a newer one (section 2). An update left unfinished stays as
it is until a person acts. Snapshots take pool space until they are removed. And a run of hours holds one service's
claim for that long.

## 7. Open questions

Decided, and now in the text: snapshots in the service project; how long a pre-update snapshot is kept; that the
snapshot of an unfinished update is never removed by age; when a by-digest service is snapshotted; no card for a
formula set over MCP; a version that differs from the one asked; an app unhealthy before its update; no shell in a
formula; and the passkey card for every restore after the run, save the one at a restart for an instance that has not
run since its snapshot. Also decided: every run ends in exactly one recorded state, and the new command type keeps
the hung mark, which Arca clears by a stop. One question is open.

1. **Which adoption path carries a hand-made instance (#244)?** Not needed for the first build. (a) `arca adopt`: the
   container moves into its own adoption project, and Arca then makes a service record for it. (b) `service_import`:
   it stays where it is, in another manager's project. Section 3 lists what each needs in the rules; both need
   changes, and neither is small. **Recommend (a).** Every new write then stays inside a project Arca made for that
   one instance. (b) puts a snapshot, a restore, a stop and a command into a project that is another manager's, and
   asks the owner to edit the node's config for each instance.

**Not decided, and not in the design.** Each of these would add a refusal or a new behaviour, so none is built unless
it is asked for.

- A limit, when a template is read, on where `{version}` may stand or on which programs a formula may name.
- At a restart, running the health check on an update that was cut, and finishing it forward when the check passes.
- Refusing a new `app_update` while an earlier one is left unfinished, in place of letting it go forward.
- Refusing a by-digest call that names a `version` and no `digest`, since Arca cannot tell which version a digest
  holds before it runs.
- Refusing at step 0 a template digest from another repository than the service's record names, in place of the
  failed pull and the rollback.
- Refusing an `app_rollback` early when a newer snapshot stands above the one it names, in place of Incus's own
  answer.

**Gap, not built here.** A container service has no create-time setting (section 1). Until it has one, a value the
owner must give goes in as a secret before the first start.
