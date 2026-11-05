# Arca canvas for DSH

A read-only host plugin for stock DSH and an embedding DSH host. It polls Arca
through a Unix socket and serves a cached snapshot at `GET /arca-canvas/snapshot`.
`GET /arca-canvas` provides a minimal page linking to that snapshot. Canvas drawing
and optional sidebar integration will be added separately. The browser module has
no host imports, credentials or required services.

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
`client.js` is committed, so the git-path install needs no build hook.

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
  droiddUrl: ''
  droiddTokenPath: ''
```

| Key | Meaning |
| --- | --- |
| `socketPath` | Required absolute path to Arca's `listen.unix` socket. DSH must run as the same user as Arca. |
| `viewerTokenPath` | Required absolute path to a regular token file with mode exactly `0400`. Symlinks are refused. |
| `pollMs` | Integer delay between completed polls, at least 100 ms; default 5000. Polls never overlap. |
| `droiddUrl` | Optional MCP URL, empty by default. Reserved for the device adapter; unused in this version. |
| `droiddTokenPath` | Optional reader-token file path, empty by default. Reserved for the device adapter; unused in this version. |

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
`machine`, `instances`, `databases`, `metrics`, `alerts`, and `plans`.

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

## Develop

```sh
npm ci
npm test
npm run build
git diff --exit-code client.js
```

Tests use fixture data and temporary Unix sockets only. The host entry is plain
JavaScript; `npm run build` regenerates only the browser module using exact-pinned
esbuild. Keep `client.js` in the commit with any browser-source change.
