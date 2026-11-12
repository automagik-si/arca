# Quickstart: a read-only node in five minutes

You need an x86_64 Linux host with glibc 2.34 or newer (Enterprise Linux 9, Ubuntu 22.04, Debian 12, or newer), a running
Incus, and a user who can read the Incus socket (on most installs, a member of the `incus-admin` group). This quickstart
names no kernel project, so the node only reads: it sends Incus nothing but GET.

## Install with one command

[`scripts/install.sh`](../scripts/install.sh) does the quickstart below for you, for the user who runs it. It needs what
the quickstart needs (x86_64 Linux with glibc 2.34 or newer, a running Incus that this user can reach) and `curl` and
`sha256sum`.

**Download it, read it, run it.**

```sh
curl -fsSLO https://github.com/automagik-si/arca/releases/latest/download/install.sh
less install.sh
sh install.sh
```

**Or in one line.** The script runs as soon as it has arrived. This address is the newest release's own
`install.sh`, the file that release's `SHA256SUMS` lists; in one line nothing checks it before it runs. Use this only
when you trust the address:

```sh
curl -fsSL https://github.com/automagik-si/arca/releases/latest/download/install.sh | sh
```

To hand it to a coding agent, paste this: *Download
`https://github.com/automagik-si/arca/releases/latest/download/install.sh`, read it, run it with `sh`, and tell me
what it printed.*

- **It checks first.** Linux, x86_64, glibc, `curl`, `sha256sum`, a runtime directory that is yours alone, and an Incus
  socket this user may open. When one is missing it says which and how to fix it, and stops before it has changed
  anything. It never runs `sudo`, it refuses to run as root, and it writes only in your home directory.
- **It installs one named release**, the one its own `ARCA_VERSION` line names, at `~/.local/bin/arca`. Each release
  sets that line to its own version.
  `sh install.sh --version <version>` names another; without it, an older release is never put over a newer program.
  The program is checked against that release's `SHA256SUMS`. That catches a download that was damaged or cut short. It
  does not catch a release that was tampered with, because the checksums come from the same release.
- **It writes the quickstart's config and mints one `viewer` token**, only where there is no config. A config that
  exists is not changed.
- **It runs the node as a service**, `arca.service`, a systemd user unit, and only for a config it wrote. For a config
  it did not write, and where `systemctl --user` does not answer, it starts nothing, prints the one command that starts
  the node, and ends with exit code 3: the program is installed and no node was started.
- **It ends with the node's own answer** to `arca_status`: the mode (`watch-only`), where the token's file is (never the
  token), and the two ways to connect an assistant from step 4 below. Exit code 0 means the node runs, answered, and
  reaches Incus.
- **A second run** replaces the program in place and restarts the service. `sh install.sh --uninstall` removes the
  service and the program, while the program is still the file it installed, and keeps the config, the tokens and the
  state.

The addresses above give the `install.sh` of the newest release, which installs that release. To check the script
itself, download `SHA256SUMS` from the same address beside it and run `sha256sum --check --ignore-missing SHA256SUMS`;
it must print `install.sh: OK`. Like the program's checksum, that catches a damaged download and not a release that
was tampered with.

**Not yet:** the script is tested
against stand-ins for the download, the program and systemd, and it was run for real on one machine that already had
Incus and Arca; it has not been run on a machine that never had either. There is no installer for a Mac.

## By hand

**1. Download the binary and check it.**

```sh
v=$(curl -fsSL -o /dev/null -w '%{url_effective}' https://github.com/automagik-si/arca/releases/latest | sed -n 's#.*/releases/tag/v##p')
echo "the newest release is ${v:?none is marked Latest; pick a version on the releases page and set v to it}"
base=https://github.com/automagik-si/arca/releases/download/v$v
curl -fL -O "$base/arca-$v-linux-x86_64" -O "$base/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
install -D -m 0755 "arca-$v-linux-x86_64" ~/.local/bin/arca
arca --version
```

The first two lines ask GitHub which release is the newest; a pre-release is never the answer. To install another
release, set `v` yourself to a version from the releases page, for example `v=0.8.0`. `sha256sum` must answer
`arca-<version>-linux-x86_64: OK`: stop unless it prints that. `--ignore-missing` is there because `SHA256SUMS` also
lists the release's `install.sh`, which this step does not download. `arca --version` prints
`Automagik Arca <version> (<commit>, x86_64-unknown-linux-gnu)`; if the shell does not find `arca`, put
`~/.local/bin` on your `PATH`.

**2. Write the smallest config.** These five keys are the required ones. The two directories must be mode 0700.

```sh
mkdir -p ~/.config/arca/tokens ~/.local/state/arca-node
chmod 0700 ~/.config/arca/tokens ~/.local/state/arca-node
cat > ~/.config/arca/node.json <<EOF
{
  "stateDir": "$HOME/.local/state/arca-node",
  "incus": { "socket": "/var/lib/incus/unix.socket" },
  "listen": { "unix": "$XDG_RUNTIME_DIR/arca.sock" },
  "tokens": { "hashDir": "$HOME/.config/arca/tokens" },
  "kernel": { "droiddProjects": [] }
}
EOF
```

`kernel.droiddProjects` names the Incus projects that belong to droidd, another manager Arca stays out of; leave it empty
when the host has none. `listen.unix` is a unix socket path, so it must stay under about 100 characters; a login
session sets `XDG_RUNTIME_DIR`.

**3. Mint a token and start the node.** `serve` reads its tokens when it starts, so mint first.

```sh
arca token mint --scope viewer --id assistant-1 --config ~/.config/arca/node.json --out ~/.config/arca/assistant-1.secret
arca serve --config ~/.config/arca/node.json
```

`serve` runs until Ctrl-C. From a second terminal, ask it for its status:

```sh
arca status
```

It takes no flag: `--config` and `--token-file` default to the two files above (`~/.config/arca/node.json` and
`~/.config/arca/assistant-1.secret`). A flag wins over the `ARCA_CONFIG` or `ARCA_TOKEN_FILE` variable, and the variable
over the default. Only a command that reads a node takes the defaults; one that writes to a node, starts one or calls a
peer (`serve`, `token mint`, `adopt` and the like) takes `--config` as a flag. The answer is the node in words: its
version, `Mode: watch-only`, whether Incus answers, where the MCP door is, and the next step. `arca status --json` prints
the `arca_status` tool's own answer, the same as `arca call arca_status`: one JSON object with `"mode":"watch-only"`,
`"incusReachable":true` and the release's `"version"`. `arca call watch_report` lists every project and instance with
its owner, and `arca call` alone lists every verb.

A token minted while the node runs is refused until the node reads it: `serve` reads its tokens when it starts and on
SIGHUP, so run `kill -HUP $(cat <stateDir>/serve.pid)` after a later `arca token mint`, or restart the node.

**4. Connect an assistant over MCP.** The node is an MCP server over streamable HTTP: `POST /mcp`, JSON answers, protocol
version `2025-11-25`, and a bearer token on every request. With the `viewer` token from step 3, this read-only node lists
18 tools, among them `arca_status`, `node_metrics`, `watch_report`, `alerts_list` and `service_list`. Two ways to reach it
work today:

- **An assistant with a shell on the node** runs `arca call <verb>` as in step 3. `arca call` is an MCP client over the
  unix socket, and the socket answers only the user who runs `serve`.
- **An assistant that takes an MCP server URL** (any client with an HTTP transport and a custom header) needs a TCP
  address, and the node opens one only when the config names it: `listen.tailnet`, one address on the host's tailnet, or
  `listen.proxy`, one address for a reverse proxy on the same host that adds TLS and a host name. The client's entry is
  then the URL and the token:

  ```json
  {
    "mcpServers": {
      "arca": {
        "type": "http",
        "url": "http://<tailnet address>:<port>/mcp",
        "headers": { "Authorization": "Bearer <content of assistant-1.secret>" }
      }
    }
  }
  ```

  Behind a reverse proxy the URL is `https://<host name>/mcp`. Both listeners are off in the config above;
  ["The config file"](OPERATING.md#the-config-file) and
  ["Publishing through a reverse proxy"](OPERATING.md#publishing-through-a-reverse-proxy-each-step-approved-by-the-owner-at-that-moment)
  in the operating guide say what each address may be and how to turn it on. A request that carries an `Origin` header
  is refused, so a web page cannot call the node.

**Not yet:** there is no `arca init`, so the config and its directories are written by hand as above; there is no stdio
MCP door, so a client that can only start a local command cannot connect; and Linux x86_64 is the only published binary.

To let the node change things (kernel projects, the owner's passkey and the approval page), and to run it as a service,
read [`docs/OPERATING.md`](OPERATING.md).
