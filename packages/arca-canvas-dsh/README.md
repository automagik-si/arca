# Arca canvas for DSH

A read-only host plugin for stock DSH and an embedding DSH host. It polls Arca
through a Unix socket and serves a cached snapshot at `GET /arca-canvas/snapshot`.
`GET /arca-canvas` draws machine, instance, database, alert, phone and desktop cards from the
snapshot. It works without a sidebar. When the sidebar is installed, an Arca
canvas tab embeds this same page, sharing its renderer, polling and styles.
Steps shows a hidden-state placeholder until the journal adapter is available.
The canvas is read only; waiting plans open the configured approval page.

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
  approvalUrl: ''
  droiddUrl: ''
  droiddTokenPath: ''
```

| Key | Meaning |
| --- | --- |
| `socketPath` | Required absolute path to Arca's `listen.unix` socket. DSH must run as the same user as Arca. |
| `viewerTokenPath` | Required absolute path to a regular token file with mode exactly `0400`. Symlinks are refused. |
| `pollMs` | Integer delay between completed polls, at least 100 ms; default 5000. Polls never overlap. |
| `approvalUrl` | Optional absolute HTTP(S) URL of this deployment’s Arca `/approve` page, empty by default. URLs with credentials are refused. Waiting plans link there only when configured; otherwise the badge is plain text. |
| `droiddUrl` | Optional absolute HTTP(S) MCP URL, empty by default. No address is assumed. |
| `droiddTokenPath` | Optional reader-token file path; requires a regular file with mode exactly `0400`, with symlinks refused. Empty by default. |

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

The JSON object always has `at` (ISO timestamp), `error` (null or safe error text),
`approvalUrl` (empty or the configured approval URL), `machine`, `instances`,
`databases`, `metrics`, `alerts`, `plans`, `phones`, and `desktops`.

- `machine` and `metrics` hold `arca_status` and `node_metrics` results.
- `instances` flattens `watch_report.projects[].instances[]`, adding `project`.
- `databases` and `plans` contain the arrays from `postgres_list` and `plan_list`.
- `alerts` holds the full `alerts_list` result, preserving firing, unknown and
  recent resolved alerts plus health metadata.

Failed reads leave `machine`/`metrics` null or the other fields empty arrays.
Successful reads survive partial failures. Requests use MCP protocol
`2025-11-25`, `initialize` then `tools/call`, over HTTP POST to `/mcp`. Error
messages omit upstream bodies and filesystem details, and successful payloads
are scrubbed of any bearer echo. Snapshot strings are never inserted as HTML.

The optional device reader uses HTTP POST to the exact configured `droiddUrl`,
with the same protocol and initialization sequence and its own per-request bearer.
It calls only `droid_list`, `desktop_list`, `droidd_status` and `phone_status`.
Structured list results use `droids` and `desktops` keys; older JSON text results
are also accepted. Phone entries retain name, state, network type, battery level
and screen size; desktop entries retain name, status and memory limit. Status is
read only for ready phones; other states remain visible with unavailable details.
If either device setting is empty, no device request is made. A device transport,
token or tool failure omits all device entries for that poll, without an error
banner or log. Arca reads keep their existing error behavior. Device credentials
and upstream error details never enter the snapshot or browser bundle.

A ResizeObserver on the stage batches reflow into an animation frame when the
window or embedding sidebar changes width. Reflow measures the stage content box;
empty columns consume no space, and populated columns wrap to keep cards inside it.
The observer disconnects when the canvas stops.

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
into a temporary directory and compare SHA-256 hashes.

`npm run build` regenerates the browser module and standalone page assets using
exact-pinned esbuild. Commit all three artifacts with browser-source changes.
Set `ARCA_CANVAS_EVIDENCE_DIR` to an existing directory to save the browser test’s
1440×900 light/dark screenshots and measured rectangles outside the repository.

The client contract is pinned to the sidebar’s 0.24.1 API: package metadata uses
`dsh.client.inject: ["dsh-better-sidebar"]` (package IDs), while the client exports
`inject = ['betterSidebar']` (Cordis service names). DSH calls `apply(ctx)`;
`ctx.betterSidebar.registerTab({ id, title, component })` returns the disposer
owned by `ctx.effect`. The React component uses DSH’s module-loader `react`
export, without bundling a second React. It returns a same-origin frame of
`/arca-canvas`. Without the sidebar, Cordis leaves this client consumer waiting;
the host still starts with only `inject = ['webServer']` and serves the page.
The client also guards a missing service when invoked directly.
