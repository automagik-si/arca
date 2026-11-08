# Arca canvas for DSH

A read-only host plugin for stock DSH and an embedding DSH host. It polls Arca
through a Unix socket and serves a cached snapshot at `GET /arca-canvas/snapshot`.
`GET /arca-canvas` draws a workspace from the snapshot: a top bar with the project
picker, local clock and fleet summary; a machines strip with CPU and memory bars;
the selected project's cards on a dotted canvas with curved SVG edges; and a
Doctor panel with Chat and Steps tabs, recent Arca activity and waiting approvals.
The Doctor composer is a working read-only chat backed by a persisted DSH session named
`Doctor` (model `deepseek-flash`). It works without a sidebar.
When the sidebar is installed, an Arca canvas tab embeds this same page, sharing
its renderer, polling and styles. The canvas is read only; waiting plans link to
the configured approval page.

## Install

Use Node **24.9.0 or newer** and **`@deepseek-ai/dsh@0.2.2-rc.1`**. npm's `latest`
tag was checked against that exact release on 2026-11-05. Install the package at
an exact full commit containing this directory:

```sh
dsh plugin --profile <profile> add -w "github:weunfold/arca#<commit>&path:/packages/arca-canvas-dsh"
```

Replace `<profile>` and `<commit>` with the desired profile and full commit SHA.
The quotes keep `&path:` in the same argument. DSH forwards the arguments to
pnpm, whose git spec splits the commit and `path:` selectors at `&`. The built
`client.js`, `page.js` and `page.css` are committed, so the git-path install needs no build hook.

Use the same command with an embedding DSH host's profile. Fully quit that host
before editing its profile, and restart it afterwards. The package contributes
one `arca-canvas` row via `dsh.bundle.patch`; the host must provide `webServer`.
No sidebar provider is required.

## Configure

Set these keys in the inserted `arca-canvas` row's `config` map:

```yaml
config:
  socketPath: /run/arca/arca.sock
  viewerTokenPath: /path/to/arca-viewer.token
  pollMs: 5000
  nodeName: 'Example machine'
  address: ''
  approvalUrl: ''
  devicesUrl: ''
  devicesTokenPath: ''
  chatEnabled: true
  chatStatePath: ''
  dailyTokenBudget: 1000000
```

| Key | Meaning |
| --- | --- |
| `hosts` | Optional non-empty array of host readers, described below. |
| `socketPath` | Required when `hosts` is absent; absolute path to Arca's `listen.unix` socket. DSH must run as the same user as Arca. |
| `viewerTokenPath` | Required when `hosts` is absent; absolute path to a regular token file with mode exactly `0400`. Symlinks are refused. |
| `pollMs` | Integer delay between completed polls, at least 100 ms; default 5000. Arca polls never overlap; the device phase runs independently. |
| `nodeName` | Optional human machine title. Defaults to the host OS hostname; an empty or whitespace-only name becomes `Machine`. The raw Arca node ID is never the card title. |
| `address` | Optional LAN host name or IP for a service's Open link. Bare host or IP only, no scheme or port. When set, a service card gets `http://<address>:<lanPort>` only when the real `service_list` row has a `lanPort` host listener; a container port alone never becomes a link. Empty by default. |
| `chatEnabled` | Enable the Doctor chat routes. Default true; when false the chat routes answer 503 and no model is called. |
| `chatStatePath` | Optional absolute path of the persisted chat state (DSH session id, daily token counter and the panel transcript). Defaults to `$DSH_HOME/storages/arca-canvas-chat.json`. |
| `dailyTokenBudget` | Positive integer total-token budget per UTC day, default 1000000. When reached the Doctor answers with the daily-budget card and spends no model call. |
| `approvalUrl` | Optional absolute HTTP(S) URL of this deployment’s Arca `/approve` page, empty by default. URLs with credentials are refused. Waiting plans link there only when configured; otherwise the badge is plain text. |
| `devicesUrl` | Optional absolute device service HTTP(S) MCP URL without credentials or fragments, empty by default. HTTPS permits any host. HTTP requires a literal loopback, RFC1918, CGNAT (100.64/10), or fd00::/8 address; hostnames and public addresses are refused. |
| `devicesTokenPath` | Optional absolute device reader token file path; requires a regular file with mode exactly `0400`, with symlinks refused. Empty by default. |
| `projects` | Optional owner project groups. Each entry has a unique non-empty `name`, an optional `domain`, and a non-empty `members` array of `{ host, incusProject, instance }`. Every member must name a configured host, and one instance can belong to only one project. |
| `defaultProject` | Optional project name selected on load. Otherwise the page selects the first configured project, then the first service project, then the first other Incus project. An internal Arca project is never selected. Must name a configured project when `projects` is present. |

To show multiple hosts, set `hosts` in the same config map:

```yaml
config:
  hosts:
    - name: alpha
      socketPath: /example/alpha/arca.sock
      viewerTokenPath: /example/alpha/viewer.token
      address: 192.0.2.10
      devicesUrl: https://devices.example.com/mcp
      devicesTokenPath: /example/alpha/device-reader.token
    - name: beta
      socketPath: /example/beta/arca.sock
      viewerTokenPath: /example/beta/viewer.token
  pollMs: 5000
  approvalUrl: https://example.com/approve
  defaultProject: Studio
  projects:
    - name: Studio
      domain: studio.example
      members:
        - { host: alpha, incusProject: service, instance: app }
        - { host: beta, incusProject: service, instance: api }
```

Each entry requires a non-empty, unique `name`, used as its machine card and
project host title, and absolute `socketPath` and `viewerTokenPath` paths. Token
files follow the same regular-file, `0400`, no-symlink rules. Optional
`devicesUrl` and `devicesTokenPath` use the same validation rules as the
top-level settings. Hosts appear in array order; the first is the **primary**
host. If its device settings are omitted, it uses the existing top-level device
settings. Other hosts never inherit those settings. Devices are collected for
compatibility but are not drawn in this layout.

When `hosts` is absent, the existing single-host keys work unchanged: the name
comes from `nodeName` or the OS hostname. With `hosts` present, each entry supplies
its own socket, token and name; top-level `socketPath`, `viewerTokenPath` and
`nodeName` are unused. Each host polls concurrently on its own `pollMs` schedule.
An unreachable socket, refused token or failed tool affects only that host;
successful fields and the other hosts keep rendering. A failed watch report
leaves the machine card online but reports that its instance count is unknown;
configured owner-project members on that host are reported as unknown, never as
missing or as an empty host. No per-host mode is inferred from `arca_status`.

`projects` is the owner's grouping layer, not an Incus project list. A listed
member is resolved on its host and appears only inside its owner project. An
unlisted instance falls back to a project named after its Incus project on that
host, with one exception: the internal `arcak-svc` grouping is never a project.
Each unlisted instance in `arcak-svc` becomes its own project named after the
service (the instance name, humanized; `web1` becomes `Web1`), so a
service is a first-class project by default and an owner project can group
several. The internal `arcak-svc` id never appears as a project, in the picker or
in a project subtitle. To group services, list them like any other member:
`{ host: alpha, incusProject: arcak-svc, instance: web1 }`. An Incus
project whose instances are all listed in owner projects is not rendered at all.
Owner projects render in config order; service projects follow, then the other
fallback projects by host and project name, with the host appended only when a
display name collides.

All example paths must be replaced with paths on the machine running DSH. Paths
are literal; `~` and environment variables are not expanded. Config contains
file paths, never bearer values. An unreachable Arca or missing token does not
prevent the plugin from serving its page and an error snapshot.

Mint a viewer token using the existing Arca configuration:

```sh
arca token mint --scope viewer --id canvas-viewer \
  --out /path/to/arca-viewer.token --config /path/to/arca.json
```

Arca writes the secret directly into a new `0400` file and stores its hash in the
configured token directory. Existing files and token IDs are refused. The plugin
reads the token again before **each HTTP request**, including `initialize`; token
rotation therefore takes effect without restarting DSH. It never sends `Origin`.

The route is served on the DSH listener and inherits that listener's access
boundary. It contains machine data; expose the DSH listener only to its intended
readers.

## Snapshot

The JSON object has `at` (ISO timestamp), `approvalUrl` (empty or the configured
approval URL), `projects` (the configured owner groups, each with `name`,
`domain` and `members`), `defaultProject`, and `hosts` in configured order. Each
host entry has `name`, `address` (the configured LAN address or empty), `at`,
`error` (null or safe error text), `nodeName`
(resolved by the host), `machine`, `instances`, `instancesKnown` (false when
`watch_report` failed), `projects` (the Incus project ids from `watch_report`),
`databases`, `services`, `metrics`, `alerts`, `plans`, `journal`, `journalKnown` (false when
`journal_list` failed), `journalPending` (true while a deep journal read finishes
in the background), `devices`, `devicesAt` (ISO time of the last good device
list, or null), `phones`, and `desktops`. `devices` is `off`, `ok`, `unavailable`,
`unreachable`, or `stale`.

- `machine` and `metrics` hold `arca_status` and `node_metrics` results.
- `instances` flattens `watch_report.projects[].instances[]`, adding `project`.
- `databases` and `plans` contain the arrays from `postgres_list` and `plan_list`.
- `services` holds the `service_list.services` rows (name, template, state, ipv4, ports,
  lanPort, links, published). `lanPort` is the template's host proxy port, or null when the
  service has no lan listener. A service whose instance is on the canvas contributes an Open
  link only from the configured host `address` and its real `lanPort`.
- `journal` holds the newest `journal_list` records, at most 200 per host. Every
  poll re-reads a trailing window of the newest ids and merges by id with the
  latest record winning, so an effect that moved from `Intent` to `Failed` (or
  `Applied` to `Confirmed`/`Reverted`) updates instead of staying at the first
  state it was seen in. A snapshot waits for at most two journal pages and a deep
  first read finishes in the background with `journalPending` true meanwhile. A
  journal that restarts from zero (its ids go backwards or disappear) is detected
  and re-read from the start, dropping the stale cache for that host. The newest
  records across hosts feed the Doctor's Chat and Steps views. A failed journal
  read leaves the array empty and `journalKnown` false, so the page shows an
  unavailable notice instead of invented rows.
- `alerts` holds the full `alerts_list` result, preserving firing, unknown and
  recent resolved alerts plus health metadata. This layout does not draw alert
  cards yet; the data stays in the snapshot for later phases.

Failed reads leave `machine`/`metrics` null or the other fields empty arrays.
Successful reads survive partial failures. Requests use MCP protocol
`2025-11-25`, `initialize` then `tools/call`, over HTTP POST to `/mcp`. Error
messages omit upstream bodies and filesystem details, and successful payloads
are scrubbed of any bearer echo. Snapshot strings are never inserted as HTML.

The optional device reader uses HTTP POST to the exact configured `devicesUrl`,
with the same protocol and initialization sequence and its own per-request bearer.
It calls only `droid_list`, `desktop_list`, `droidd_status` and `phone_status`.
Structured list results use `droids` and `desktops` keys; older JSON text results
are also accepted. Phone entries retain name, state, network type, battery level
and screen size (`[width, height]`); desktop entries retain name, status and memory limit.
Every phone is eligible for a status read regardless of its `droid_list` state;
one phase reads at most two phone statuses, rotating through the list. A failed or
timed-out phone is kept with `state: 'unreachable'` and is not retried for at
least sixty seconds; the skip is based on that last call failure, never on
`droid_list` state. Both device settings empty means `devices: 'off'`, with no
device requests. Configured readers report `ok` after a successful phase, or
`unavailable` on invalid settings, missing/refused tokens, an unreachable service
or a first failed read with no prior good list. Device validation happens in the
host reader and never prevents canvas startup. Refused settings make no device
HTTP requests. A failed list or service-status read keeps the last good phone and
desktop list and reports `devices: 'unreachable'`; a failed `phone_status` keeps
that phone visible with `state: 'unreachable'`, alongside all other phones and
desktops. An HTTP 429 keeps the last good list and reports `devices: 'stale'`;
`devicesAt` preserves the time of that list, and the topbar chip shows its age.
With no prior good list, even a 429 reports `unavailable` with empty lists.

Device reads start alongside Arca reads and update cached device cards in the
background under one five-second deadline. The phase is single-flight: a poll
never starts another phase while one is pending. Arca snapshots and paints never
wait for that phase. In-flight device requests are aborted on deadline or host
stop; a later poll may retry except for a phone still inside its sixty-second
failure window. A muted `Devices unavailable`, `Devices unreachable` or aged
`Devices stale` topbar chip appears for those states, with no device error banner.
The host logs one safe line per device state change, naming `devicesUrl` or
`devicesTokenPath` for a rejected setting. Setting values, device credentials and
upstream error details never enter logs, the snapshot or the browser bundle. Arca
reads keep their existing error behavior.

The page has four regions. The top bar holds the project picker, the
`Arca self-hosted | Arca Cloud` segmented control (Cloud is disabled and marked
coming soon), the local clock and a fleet summary computed from the latest
`node_metrics` (`machines · cores · GB`; a part disappears when no host reports
it). The machines strip draws one compact card per configured host with online
state from `arca_status.incusReachable`, a CPU bar from `latest.cpu.usedPct`
(shown as used cores / total cores), a memory bar from
`memory.totalBytes` and `usedBytes` (falling back to `totalBytes -
availableBytes`), and the instance count. A null CPU load renders `CPU n/a` with
no bar, never zero. When the host reports `cpu.reason` or `cpu.cores` is zero,
the card says `cores n/a` and keeps the reason as the meter's tooltip; a memory
`reason` renders `Mem n/a` with the same tooltip and no bar. A real reading is
never replaced by an invented one.
Two dashed placeholder cards leave room for storage and phones. The project
canvas draws the selected project's instances and matched databases as cards
connected to a project hub with curved dashed SVG edges on the dotted canvas.
Cards carry only real snapshot data: status, kind, owner class, the instance's
own absolute URL when present, and database address, port and link count when
present. A domain card appears only when the project config sets `domain`.

The right-hand Doctor panel has Chat and Steps tabs. Chat is a real read-only
conversation; Steps renders the newest `journal_list` records across every
machine. A waiting-plan card links to the configured `approvalUrl`. When
`journal_list` is unavailable Steps says so without fabricating activity. The
chat's waiting-plan card appears under the conversation too.

The Doctor answers only from the eight read-only Arca tools (`arca_status`,
`watch_report`, `postgres_list`, `node_metrics`, `alerts_list`, `plan_list`,
`journal_list`, `service_list`). They are registered globally with a per-session
`when` condition, the Doctor's scope is restricted with
`restrict({ allow: READ_TOOLS })`, and a scoped `tools/pre-execute` guard denies
every other name (including the reserved `run_code`) at dispatch. A registry that
cannot restrict or enumerate fails setup, so the chat sends a fixed
"Doctor unavailable" error and makes no model call. Write verbs are never
registered. A request to change something is answered with what the change would
do and the configured `/approve` link; the panel adds the approval card. Inline
passkey approval is phase 3. The page never receives a model credential.

Selection lives in the URL fragment as `#project/<name>`, encoded with
`encodeURIComponent`. Loading with a known project restores it; an unknown or
missing fragment selects `defaultProject`, else the first configured project,
then the first service project, then the first other Incus project, and replaces
the fragment. The page never pans, zooms or drags. Polls rebuild a region only
when its data changed, so selection, focus and scroll survive unchanged polls.
There are no write actions and no routes beyond the existing `/arca-canvas`
assets and snapshot.

## Doctor chat contract

The Doctor chat is served by this package on the DSH listener. The kernel proxies
the two routes behind the `__Host-arca-canvas` passkey cookie and its `Origin`
check, then relays the stream. The DSH routes themselves are loopback-only and
trust the kernel's gate; they read no cookie and hold no page secret. A direct
loopback caller is inside the same trust boundary as the snapshot route.

### `POST /arca-canvas/chat`

One Doctor turn. The kernel exposes it as `POST /approve/canvas/chat`.

- Request `Content-Type: application/json`; UTF-8 body `{ "message": "<text>" }`.
  The body is capped at 64 KiB and the message at 16384 characters.
- Success is `200 text/event-stream; charset=utf-8` (`Cache-Control: no-store`,
  `X-Accel-Buffering: no`, `Connection: keep-alive`). Every frame is `event:
  <name>` plus exactly one `data: <json>` line; `JSON.stringify` escapes every
  newline inside a string, so model text can never close a frame or forge an
  event. A comment line `: hb` is written at least every 15 seconds, including
  while a tool call is slow, so the kernel's 60-second idle limit is never
  reached. The turn is bounded at nine minutes, under the kernel's ten; agent setup is bounded
separately, and the single-turn slot is released only after the DSH run reports
`turn/end` (or its cancel bound expires). A client that disconnects during setup
gets no model call.
- Refusals before a stream are JSON: `400 { "error": "invalid-json" |
  "message-required" }`, `405 { "error": "method-not-allowed" }` (with
  `Allow: POST`), `413 { "error": "body-too-large" | "message-too-long" }`,
  `415 { "error": "json-required" }`, `429 { "error": "chat-busy" }` (one turn
  at a time), and `503 { "error": "chat-unavailable" }` (no DSH agent/tools or
  the plugin is shutting down). None of these starts a model call.

SSE events, in order. Every `data` is one JSON object.

| Event | Data | Meaning |
| --- | --- | --- |
| `start` | `{ "session": "Doctor", "sessionId"?: "<dsh-session-id>" }` | Always first, sent before agent or model work so the kernel's idle timer never starts on a silent socket. `sessionId` is present when a persisted session is already known. |
| `text` | `{ "text": "<delta>" }` | One assistant text delta; concatenate in arrival order. |
| `tool` | `{ "callId", "name", "state", "input"?, "summary"?, "host"? }` | One card per real tool dispatch. `state` is `running`, then `done` or `error`; `name` is one of the eight reads. |
| `approval` | `{ "url", "title", "detail" }` | Advisory card when the message looks like a write request and an approval URL is configured. `url` is the configured `/approve` URL. Presentation only. |
| `usage` | `{ "turnToolCalls", "turnOutputTokens", "dailyTokens", "dailyBudget", "day" }` | Token accounting from the model's own usage; `dailyTokens` counts input plus output for the UTC day. |
| `budget` | `{ "scope": "daily", "used", "budget", "message" }` | Daily budget reached; the message is `Daily budget reached, try tomorrow.`. The turn then ends. |
| `cap` | `{ "reason", "limit", "message" }` | Per-turn cap reached: `reason` is `tool-calls` (40) or `output-tokens` (64000). |
| `error` | `{ "message": "<safe text>" }` | Safe, fixed text; upstream details and the model key never enter the stream. |
| `done` | `{ "reason", "turnToolCalls", "turnOutputTokens" }` | Terminal. `reason` is `complete`, `tool-calls`, `output-tokens`, `budget`, `timeout`, `error`, or `closed`. |

A stream always ends cleanly. A client must treat an `event: arca-cut` added by
the kernel as advisory (`The answer was cut off.`) and must also show that
notice when the stream ends without `done`; the kernel's cut is never a security
decision. Caps are enforced from the model's own usage and the tool dispatcher:
a 41st tool call fails before dispatch and the turn ends with `tool-calls`; more
than 64000 output tokens ends it with `output-tokens`; the daily counter persists
in `chatStatePath` and blocks the next turn before any model call.

### `GET /arca-canvas/chat/history`

The kernel exposes it as `GET /approve/canvas/chat/history`. Plain JSON, no
model call:

```json
{
  "session": { "id": "<dsh-session-id>", "name": "Doctor" },
  "messages": [
    { "role": "user", "text": "...", "at": "<ISO time>" },
    { "role": "assistant", "text": "...", "at": "<ISO time>",
      "tools": [{ "callId": "...", "name": "arca_status", "state": "done",
                 "host": "alpha", "summary": "..." }] }
  ],
  "usage": { "day": "YYYY-MM-DD", "dailyTokens": 0, "dailyBudget": 1000000,
             "budgetExhausted": false },
  "limits": { "toolCallsPerTurn": 40, "outputTokensPerTurn": 64000 }
}
```

`tools[].host` and `tools[].summary` are optional. `session` is `null` before the first turn.
The transcript is capped at the newest
200 messages and the body is kept under 1.5 MiB (oldest replay rows are dropped)
so it fits the kernel's 2 MiB forward cap. The model transcript itself is the
persisted DSH session; this JSON is the panel's replay.

### Session, model, and credential

The first turn resumes the persisted session id in `chatStatePath`, or creates one
and names it `Doctor`; later turns and restarts reuse it. The route selects the
DSH model `deepseek-flash` on the `deepseek-official` provider from the runtime's
own credentials. The plugin never reads, receives, logs or returns that
credential; it appears in no page asset, SSE frame, snapshot or log line. Message
bodies are never logged either. A model credential failure becomes the safe
`error` event.

## Develop

```sh
npm ci
npx playwright install chromium
npx -y node@24.11.0 --test
npx -y node@24.11.0 build.mjs
git diff --exit-code -- client.js page.js page.css
```

Tests use fixture data, temporary Unix sockets and an ephemeral loopback browser
server. Playwright 1.64.0 is pinned to the Chromium used by the rendering checks.
The tests assert light/dark colors, layout regions and literal rendering of
untrusted strings in a real browser. They also rebuild every browser artifact
into a temporary directory and compare SHA-256 hashes. Chat tests use a fake
DSH agent/stream and the synthetic Arca socket fixtures: they cover streamed
text, one card per real call, the approval card and read-only refusal, the tool
and token caps, the daily budget, the persisted `Doctor` session, and the
absence of the model key from the page.

`npm run build` regenerates the browser module and standalone page assets using
exact-pinned esbuild. Commit all three artifacts with browser-source changes.
Set `ARCA_CANVAS_EVIDENCE_DIR` to an existing directory to save the browser test’s
1440×900 light/dark screenshots (`canvas-v28-light.png`, `canvas-v28-dark.png`)
and `rectangles.json` outside the repository. The standard browser fixture has
two hosts, two owner projects, one service project, one fallback project,
real-shaped journal rows and one waiting plan.

The client contract is pinned to the sidebar’s 0.24.1 API: package metadata uses
`dsh.client.inject: ["dsh-better-sidebar"]` (package IDs), while the client exports
`inject = ['betterSidebar']` (Cordis service names). DSH calls `apply(ctx)`;
`ctx.betterSidebar.registerTab({ id, title, component })` returns the disposer
owned by `ctx.effect`. The React component uses DSH’s module-loader `react`
export, without bundling a second React. It returns a same-origin frame of
`/arca-canvas`. Without the sidebar, Cordis leaves this client consumer waiting;
the host still starts with only `inject = ['webServer']` and serves the page.
The client also guards a missing service when invoked directly.
