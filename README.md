<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/readme/arca-logo-night.svg">
    <img src="assets/readme/arca-logo-day.svg" alt="Arca" width="260">
  </picture>
</p>

<h1 align="center">Be your own cloud.</h1>

<p align="center">
  Infrastructure for AI agents. Yours, end to end.
</p>

<p align="center">
  Arca turns the machines you already own into your own cloud.
</p>

<div align="center">

[**Quickstart**](docs/QUICKSTART.md) · [**Guide**](docs/OPERATING.md)

</div>

## Works today, and not yet

Arca is an alpha. Read this before you install.

| Works today | Not yet |
|---|---|
| **Install with one line.** `curl -fsSL https://get.automagik.si/arca \| sh` on a Linux machine (x86_64, glibc 2.34 or newer) with a running Incus your user can reach. It installs one release, runs the node as your own user's service, and leaves a node that only looks: an assistant with its token reads your projects, instances, host readings and alerts, and changes nothing. | No program for ARM, and no installer for a Mac: the installer stops on anything but Linux on x86_64. On another architecture it names [building from source](docs/BUILDING.md). It does not install Incus or give your user access to it. It was run for real on one machine that already had Incus and Arca, and not on a machine that never had either. |
| **Connect an assistant on the same machine.** The installer opens the node's MCP door at `http://127.0.0.1:7471/mcp` (or the next free port up to 7480), for this machine only and only with a token, and prints the one line that adds it to Claude Code. An assistant with a shell can use `arca call` instead. `sh install.sh --no-door` opens no port at all. | The installer opens no address another machine can reach. An assistant on another machine needs a tailnet or reverse-proxy listener, added to the config by hand: ["Tailnet listener" and "Proxy listener"](docs/OPERATING.md#tailnet-listener) in the guide. |
| **Turn on changes in one step.** [`arca init --enable-changes`](docs/QUICKSTART.md#next-a-node-that-changes-things), or `--enable-changes` on the install line. It asks before it writes (on the install line, the flag is your yes), writes only the keys the config lacks, names every token that would not wait for your approval, and keeps the config as it was. `arca init --watch-only` goes back, and removes nothing. | That step does not turn on virtual machines (the workspace and the GitHub and GitLab runner pools), tenants, or taking over a service that already runs on the machine. Each is set in the config by hand: ["Turning on changes"](docs/OPERATING.md#turning-on-changes) says what the step does not do. |
| **Approve on the machine itself.** The step names an approval page at `http://localhost:7471/approve` (the port is the door's), with no domain and no certificate. You enrol a passkey with a setup code from the machine's screen, or from `arca setup-code` in a terminal, and approve each proposal there. From another computer the page opens through one `ssh -L` line. | Approving from a phone, on a machine with no name. A phone cannot open that page: it needs the node reachable under a name with a certificate, on a tailnet or a domain, set by hand. That page is for a machine where every account is trusted, and it was driven by a test browser's virtual passkey, not yet by a real passkey manager: [read this first](docs/OPERATING.md#the-page-on-this-machine-alone-with-no-domain-and-no-certificate). |
| **Know where you are.** `arca status` says in words whether the node only looks or may change things, which doors are open, which page approves and whether it is open, whether a passkey is trusted, and ends with two lines: `Next:`, the one thing to do, and `More:`, where the rest is. `arca status --security` says the rest: what Arca may touch, and who can reach what. | It speaks for the unix socket, the door on this machine and the approval page. A tailnet or proxy address is named from the config file and not checked. |
| **Run services from the templates Arca ships.** After that step these run as containers, each fetching its own pinned image, and each starts once you approve it: Postgres (ask for a database: `postgres_create`, or the `postgres-autopg` service, which is the same thing), NATS, Omni and the Automagik server. An assistant proposes one, and nothing runs until you approve it with your passkey. | A service that needs a secret of yours does not start from the page alone: the Cloudflare tunnel is created stopped until its `TUNNEL_TOKEN` is set, Omni's Telegram and WhatsApp tools need theirs, and so do the runner pools and the Automagik workspace; setting a secret cannot be proposed today and needs a `doctor` token, which changes things without your approval. The step does not check that this machine's Incus can fetch a template's image. A new Automagik starts from Automagik's published image (`automagik-server`, or `automagik-workspace` in its own machine), pulled by its digest; Omni channels come through its welcome once Automagik's fixed release is pinned. The `automagik` template, for Omni channels until then and for publishing on the tailnet, needs an image built from a checkout first. The GitLab runner pool is new and has not run against a real GitLab. Arca cannot run any app or container image you name: a service comes from a template. |

## Get Arca

| | |
|---|---|
| **Linux** | [Download](https://github.com/automagik-si/arca/releases) · [five-minute quickstart](docs/QUICKSTART.md) |
| **Your agent** | Paste this to it: *"Install Automagik Arca on this Linux machine from github.com/automagik-si/arca, following docs/QUICKSTART.md. Show me each command first."* |

On a Linux machine with Incus ([what this does and does not do](docs/QUICKSTART.md#install-with-one-command)):

```sh
curl -fsSL https://get.automagik.si/arca | sh
```

This gives you a node that only looks: it reads your machines and changes nothing. One step lets it change things, with what your assistant proposes running only after you approve it with your passkey: [`arca init --enable-changes`](docs/QUICKSTART.md#next-a-node-that-changes-things), or `--enable-changes` on the install line (`curl -fsSL https://get.automagik.si/arca | sh -s -- --enable-changes`).

## How it works

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/readme/how-it-works-night.svg">
    <img src="assets/readme/how-it-works-day.svg" alt="You ask. Your agent proposes. Your passkey approves. Your machines run it." width="820">
  </picture>
</p>

**Say it.** Tell your agent what you need: a database, an app, another machine in the group.<br>
**Approve it.** Set up the way the quickstart shows, an assistant can only look and propose. You approve with your passkey.<br>
**Done.** Arca runs it on your machines and keeps an append-only journal of the changes it applies.

## What it runs

Your agents and everything around them: **Postgres** · **NATS** · **Omni** · **Automagik** · **tunnels** · **workspaces** · **build runners**.

Each comes from a template Arca ships. After `arca init --enable-changes`, ask your assistant for one by its name, and approve it on the page: a database is the `postgres-autopg` service.

## Yours

Open source, Apache-2.0. Self-hosted. Arca itself needs no outside service to work.

<p align="center">
  <a href="docs/QUICKSTART.md">Quickstart</a> ·
  <a href="docs/OPERATING.md">Guide</a> ·
  <a href="docs/ARCHITECTURE.md">Architecture</a> ·
  <a href="CHANGELOG.md">Changelog</a> ·
  <a href="https://github.com/automagik-si/arca/releases">Releases</a>
</p>

<p align="center">
  <a href="https://weunfold.ai/"><picture><source media="(prefers-color-scheme: dark)" srcset="assets/open-intelligence/open-intelligence-badge-ivory.svg"><img src="assets/open-intelligence/open-intelligence-badge-ink.svg" alt="Open Intelligence" width="170" height="32"></picture></a>
</p>
