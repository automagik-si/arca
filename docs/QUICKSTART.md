# Quickstart: a read-only node in five minutes

You need an x86_64 Linux host with glibc 2.34 or newer (Enterprise Linux 9, Ubuntu 22.04, Debian 12, or newer), a running
Incus, and a user who can read the Incus socket (on most installs, a member of the `incus-admin` group). This quickstart
names no kernel project, so the node only reads: it sends Incus nothing but GET.

**What you have at the end:** a node that looks, not one that changes things. An assistant connected to it can read your
projects, instances, host readings and alerts. It cannot create a database or a service, and there is no approval page.
Letting the node change things is a further step, done by hand today:
["Next: a node that changes things"](#next-a-node-that-changes-things), at the end of this page.

## Install with one command

[`scripts/install.sh`](../scripts/install.sh) does the quickstart below for you, for the user who runs it. It needs what
the quickstart needs (x86_64 Linux with glibc 2.34 or newer, a running Incus that this user can reach) and `curl` and
`sha256sum`.

**Download it, read it, run it.**

```sh
curl -fsSL -o install.sh https://get.automagik.si/arca
less install.sh
sh install.sh
```

**Or in one line.** The script runs as soon as it has arrived. The address gives the newest release's own
`install.sh` ([what the address is](#what-the-short-address-is)); in one line nothing checks it before it runs. Use
this only when you trust the address:

```sh
curl -fsSL https://get.automagik.si/arca | sh
```

To hand it to a coding agent, paste this: *Download `https://get.automagik.si/arca` with
`curl -fsSL -o install.sh`, read it, run it with `sh install.sh`, and tell me
what it printed. Then tell me plainly that this node only looks: it can read my machines and it cannot change
anything yet, and there is no approval page. Tell me the next step is "Next: a node that changes things" in
docs/QUICKSTART.md, which is done by hand today, and do not start it unless I ask.*

**If you are an agent following this page:** end your report with those same words. The install is done when the node
answers `arca_status` with `"mode":"watch-only"`. That is a node that reads. Do not tell the person that Arca is ready
to run their services.

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

### What the short address is

`https://get.automagik.si/arca` holds no script of its own. It answers with a redirect to the newest release's
`install.sh` on GitHub, so the script, and the `SHA256SUMS` that lists it, still come from the release, and that
script installs that release. `curl` follows the redirect because of `-L`. With the short address you trust two
names, `get.automagik.si` and GitHub. To skip the redirect, or to check the script itself, use the long address and
the `SHA256SUMS` beside it:

```sh
base=https://github.com/automagik-si/arca/releases/latest/download
curl -fsSL -O "$base/install.sh" -O "$base/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
```

It must print `install.sh: OK`. Like the program's checksum, that catches a damaged download and not a release that
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
arca call arca_status --config ~/.config/arca/node.json --token-file ~/.config/arca/assistant-1.secret
```

The answer is one JSON object with `"mode":"watch-only"`, `"incusReachable":true` and the release's `"version"`.
`arca call watch_report ...` lists every project and instance with its owner.

**4. Connect an assistant over MCP.** The node is an MCP server over streamable HTTP: `POST /mcp`, JSON answers, and a
bearer token on every request. It speaks protocol version `2026-07-28` (no handshake: each request names its version)
and still `2025-11-25` (`initialize` first), so a client of either connects with no setting; `server/discover` names
both. ["The MCP server"](OPERATING.md#the-mcp-server) in the operating guide has the details. With the `viewer` token
from step 3, this read-only node lists 18 tools, among them `arca_status`, `node_metrics`, `watch_report`, `alerts_list`
and `service_list`. Two ways to reach it work today:

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

Streamable HTTP is the node's one MCP transport. A client connects to the node's socket or to an address the node
serves; the node is not started as a client's local command.

**Not yet:** there is no `arca init`, so the config and its directories are written by hand as above; no TCP address is
open until the config names one, so an assistant that takes a URL cannot connect to the node as this page leaves it;
and Linux x86_64 is the only published binary.

## Next: a node that changes things

The node this page leaves only looks: `arca_status` answers `"mode":"watch-only"`, and a watch-only node writes nothing.
It serves no approval page. Neither the installer nor the steps above turn these on. Today that is done by hand in the
node's config, and a one-step way is being built.

This is a list of what to set and where the operating guide says how. The guide is a reference, not a walk-through, and
the work needs a person comfortable with a terminal.

1. **Name the projects Arca may write in.** `kernel.projects` in `~/.config/arca/node.json`. With projects named, the
   node's mode is `kernel`; with none it stays `watch-only`. ["The config file"](OPERATING.md#the-config-file) says
   what this key and the keys beside it (`kernel.droiddProjects`, `kernel.serviceCidr`) may hold, and which configs
   `serve` refuses.
2. **Install the shipped templates.** `arca templates install --config ~/.config/arca/node.json` writes them into the
   node's state directory. A service is created from one of them.
3. **Set up the approval page and your passkey.** `approvalOrigin` in the config, an HTTPS address in front of the
   node, and a first passkey enrolled with a one-time code. ["The approval page"](OPERATING.md#the-approval-page) says
   what the page serves; the numbered steps begin at "To use it:" further down that section. Until a passkey is
   trusted, every approval fails closed.
4. **Restart the node** so it reads the config again: `systemctl --user restart arca.service` where the installer
   started it, or stop and start `arca serve`.

To run the node as a service by hand, read ["Running as a user service"](OPERATING.md#running-as-a-user-service).
