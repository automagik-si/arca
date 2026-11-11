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

## Get Arca

| | |
|---|---|
| **Linux** | [Download](https://github.com/automagik-si/arca/releases) · [five-minute quickstart](docs/QUICKSTART.md) |
| **Your agent** | Paste this to it: *"Install Automagik Arca on this Linux machine from github.com/automagik-si/arca, following docs/QUICKSTART.md. Show me each command first."* |

On a Linux machine with Incus ([what this does and does not do](docs/QUICKSTART.md#install-with-one-command)):

```sh
curl -fsSL https://raw.githubusercontent.com/automagik-si/arca/dev/scripts/install.sh | sh
```

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
