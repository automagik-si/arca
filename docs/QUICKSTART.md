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
  `sh install.sh --version VERSION` names another release; without it, an older release is never put over a newer program.
  The program is checked against that release's `SHA256SUMS`. That catches a download that was damaged or cut short. It
  does not catch a release that was tampered with, because the checksums come from the same release.
- **It writes the quickstart's config and mints one `viewer` token**, only where there is no config. The config opens
  the HTTP MCP door on this machine only: `127.0.0.1`, port 7471, or the next free port up to 7480, and the report
  says which. A config it did not write is not changed. Its own config from before the door gains the door, and
  nothing else, on the run that replaces a program from before the door with one that has it.
- **`sh install.sh --no-door` opens no TCP port at all.** It writes `"loopback": null` into the config, which is the
  config's own word for no door, and no later run or upgrade opens one in a config that says so, or in one whose door
  you took out by hand. `sh install.sh --door` opens it again. A config the script did not write is not changed by
  either flag.
- **It runs the node as a service**, `arca.service`, a systemd user unit, and only for a config it wrote. For a config
  it did not write, and where `systemctl --user` does not answer, it starts nothing, prints the one command that starts
  the node, and ends with exit code 3: the program is installed and no node was started.
- **It ends with the node's own answer** to `arca_status`: the mode (`watch-only`), where the token's file is (never the
  token), and the two ways to connect an assistant from step 4 below, with the one line that adds the node to Claude
  Code and one line that says where the cautions are (`arca status --security`). Exit code 0 means the node runs, answered, and reaches Incus.
- **A second run** replaces the program in place and restarts the service. Where the node already has service
  templates (it ran `arca init`), the run also brings them up to the release: a template that is an earlier
  release's own file is replaced and named, and one you edited is kept and named, with the command that replaces it. `sh install.sh --uninstall` removes the
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
  "listen": { "unix": "$XDG_RUNTIME_DIR/arca.sock", "loopback": "127.0.0.1:7471" },
  "tokens": { "hashDir": "$HOME/.config/arca/tokens" },
  "kernel": { "droiddProjects": [] }
}
EOF
```

`kernel.droiddProjects` names the Incus projects that belong to droidd, another manager Arca stays out of; leave it empty
when the host has none. `listen.unix` is a unix socket path, so it must stay under about 100 characters; a login
session sets `XDG_RUNTIME_DIR`. `listen.loopback` is the HTTP MCP door for this machine only; take another port when
7471 is in use, and write `"loopback": null` to have no TCP port at all (the installer reads `null` as your no and
keeps it; a config that only leaves the key out reads as one from before the door).

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
over the default. Every command finds its config this way; one that writes to a node or acts on it (`serve`,
`token mint`, `adopt` and the like) first says on stderr which config it is using when `--config` did not name it.
A command that hands out, removes or rewrites something (`token mint`, `token revoke`, `init`, a call with `--peer` and
a few more) is
never steered by `ARCA_CONFIG` alone: when the variable names another file than the default, it refuses and says the
`--config` to run it with. The answer is the node in words: its
version, `Mode: watch-only`, whether Incus answers, where the MCP door is, and the next step. `arca status --json` prints
the `arca_status` tool's own answer, the same as `arca call arca_status`: one JSON object with `"mode":"watch-only"`,
`"incusReachable":true` and the release's `"version"`. `arca call watch_report` lists every project and instance with
its owner, and `arca call` alone lists every verb.

A token minted while the node runs is refused until the node reads it: `serve` reads its tokens when it starts and on
SIGHUP, so run `arca token reload --config ~/.config/arca/node.json` after a later `arca token mint`, or restart the
node. `token reload` finds the node that runs on that config, signals nothing else, and says whether it read its tokens.

**4. Connect an assistant over MCP.** The node is an MCP server over streamable HTTP: `POST /mcp`, JSON answers, and a
bearer token on every request. It speaks protocol version `2026-07-28` (no handshake: each request names its version)
and still `2025-11-25` (`initialize` first), so a client of either connects with no setting; `server/discover` names
both. ["The MCP server"](OPERATING.md#the-mcp-server) in the operating guide has the details. With the `viewer` token
from step 3, this read-only node lists 18 tools, among them `arca_status`, `node_metrics`, `watch_report`, `alerts_list`
and `service_list`. Three ways to reach it work today:

- **An assistant that takes an MCP server URL** (any client with an HTTP transport and a custom header) uses the
  door the config above opens, `http://127.0.0.1:7471/mcp`, from this machine. For Claude Code it is one line; the
  shell reads the token from its file:

  ```sh
  claude mcp add --transport http arca http://127.0.0.1:7471/mcp --header "Authorization: Bearer $(cat ~/.config/arca/assistant-1.secret)"
  ```

  Any other client takes this entry, with the content of `assistant-1.secret` in place of `<token>`:

  ```json
  {
    "mcpServers": {
      "arca": {
        "type": "http",
        "url": "http://127.0.0.1:7471/mcp",
        "headers": { "Authorization": "Bearer <token>" }
      }
    }
  }
  ```

  The client then keeps the token in its own config, in clear text: whoever can read that file can ask the node
  whatever the token may ask. Running the `claude mcp add` line also puts the token in that command's arguments for
  a moment, where other users of the machine can read it; pasting the entry into the client's config avoids that.

  Use the address `127.0.0.1` in the URL, not the name `localhost`: the name can also mean `::1`, where the node does
  not listen and another program could. The door answers this machine only. It refuses a request that carries an
  `Origin` header, so a web page cannot call the node, and a request whose `Host` is not `127.0.0.1:7471` or
  `localhost:7471`, so a name that was pointed at this machine cannot either. Every program of this machine can
  connect to the port; none gets an answer without the token.

  On a machine shared with other accounts, another account can take the port while the node is not running, and
  would then receive what a client sends there, the token included. `arca call` below is not exposed to that. To
  have no door, write `"loopback": null`, or run `sh install.sh --no-door`. A port that is taken does not stop the
  node: it runs without the door, keeps trying the port, and `arca_status` says `"loopback": { "open": false, ... }`
  meanwhile.
- **An assistant with a shell on the node** runs `arca call <verb>` as in step 3. `arca call` is an MCP client over the
  unix socket, and the socket answers only the user who runs `serve`.
- **An assistant on another machine** needs an address that machine can reach, and the node opens one only when the
  config names it: `listen.tailnet`, one address on the host's tailnet, or `listen.proxy`, one address for a reverse
  proxy on the same host that adds TLS and a host name. Both are off in the config above;
  ["The config file"](OPERATING.md#the-config-file) and
  ["Publishing through a reverse proxy"](OPERATING.md#publishing-through-a-reverse-proxy-each-step-approved-by-the-owner-at-that-moment)
  in the operating guide say what each address may be and how to turn it on.

Streamable HTTP is the node's one MCP transport. A client connects to the node's socket or to an address the node
serves; the node is not started as a client's local command.

**Not yet:** Linux x86_64 is the only published binary.

## Next: a node that changes things

The node this page leaves only looks: `arca_status` answers `"mode":"watch-only"`, and a watch-only node writes
nothing. One step lets it change things, with what your assistant proposes still waiting for you:

```sh
arca init --enable-changes --config ~/.config/arca/node.json
```

It asks before it writes: whether Arca may make changes on this machine, and whether the service network it found
free may be used. What an assistant proposes then runs only after you approve it with your passkey. A `doctor` or
`operator` token, if you have minted one, does not wait for that: the step names every such token before it asks,
and the installer mints none. Then it names the Incus project Arca's services live in, that
network, and the approval page on this machine's own door, `http://localhost:7471/approve`, installs the shipped
templates and restarts the installer's service. `sh install.sh --enable-changes` does the same at install time,
with the flag as your yes.

It ends in five lines: what changed, which tokens (if any) do not wait for you, one line that starts `Next:` with
the one thing to do, where a database comes from, and one line that starts `More:`. Do the `Next:` line. Everything
else it could say is one command away, `arca status --security --config ~/.config/arca/node.json`, and `arca status`
always ends with the next step.

What comes after is yours:

1. **Enrol your passkey.** Open `http://localhost:7471/approve` in a browser on the machine and press **Get a setup
   code**. On a machine with no screen, `arca setup-code --config ~/.config/arca/node.json` prints the code in a
   terminal. Until a passkey is trusted, nothing can be approved.
2. **Ask your assistant for something**, such as a NATS service. It proposes; nothing runs.
3. **Approve it on the page** with your passkey. Arca runs it and writes it in its journal.

**A database:** ask your assistant for the `postgres-autopg` service. It is one of the shipped templates, it fetches
its own pinned image, it makes its own password, and it waits for your approval like any change: once you approve,
it starts. A node whose templates were installed by 0.12.0 or older has a template that does not make the password.
Upgrading with the installer replaces it, and every other shipped template nobody edited, with this release's, and
says so; `arca templates install --config ~/.config/arca/node.json` does the same by hand. (`postgres_create`,
proposed with a name, makes that same service.)

That page is for a machine where every account is trusted, and it cannot be opened from a phone; read
["The page on this machine alone"](OPERATING.md#the-page-on-this-machine-alone-with-no-domain-and-no-certificate)
before you rely on it. ["Turning on changes"](OPERATING.md#turning-on-changes) says everything the step writes,
what it refuses, and what it does not do.

To go back to a node that only looks: `arca init --watch-only --config ~/.config/arca/node.json`. Nothing is removed.

To run the node as a service by hand, read ["Running as a user service"](OPERATING.md#running-as-a-user-service).
