#!/bin/bash
# The automagik-workspace service (templates/automagik-workspace.json): one virtual machine that runs one Automagik
# workspace, the gate in front of it and the one part that lets people reach it: a Cloudflare Tunnel connector
# (settings.reach "cloudflare") or a tailnet node (settings.reach "tailnet"). This one file is both halves of it.
#
# - Run with no argument it is the machine's first-boot script: cloud-init runs it once, as root. It installs Docker and
#   the few tools the keeper uses from the distribution's own packages, checks the settings, makes the two data
#   directories, copies itself to /usr/local/sbin/arca-automagik-workspace and starts the keeper as a systemd
#   service. What it says goes to the machine's console too (service_logs); the package manager's own output stays in
#   /var/log/cloud-init-output.log.
# - `arca-automagik-workspace run` is the keeper. It keeps three containers running, in the shape of Automagik's own
#   server/compose.gate.yml: the server has a network namespace of its own with no published port, and the other two
#   join that namespace (`--network container:automagik-server`, compose's `network_mode: service:dsh`). So the three
#   reach each other on that namespace's loopback, and nothing listens on the machine's own addresses.
#     automagik-server   the Automagik server image. It listens on its loopback (port 3081) and keeps its state in
#                        /data, which is the machine's /var/lib/automagik-workspace/data, owned by uid 1000.
#     automagik-gate     the same image by the same digest, started with the gate's entrypoint. It listens on the
#                        same loopback at port 3082 and is the only door to the server: it signs the owner
#                        in and holds the server's own sign-in credential, which never leaves the machine. Its state
#                        is in /var/lib/automagik-workspace/gate; it reads the server's data read-only. It is
#                        started only when a public address is set: the gate does not start without its origin.
#     automagik-tunnel   cloudflared, as a remotely-managed tunnel. It calls out to Cloudflare; nothing calls in. The
#                        route in Cloudflare names http://127.0.0.1:3082. Its metrics listener is put on loopback,
#                        where a container would otherwise open it to every address.
#   The server is never told its public name. The gate speaks to it as loopback, so it needs none. A route in
#   Cloudflare that names the server's port instead of the gate's then reaches a server that does not trust the
#   public name: its interface's requests and live connections are refused (HTTP 403), its page asks for the
#   server's own sign-in (HTTP 401), and only its static files load. Nothing of the workspace can be used that way.
#   That holds only while the route sends the visitor's own Host: a Host override to a loopback name on the route
#   would make the server accept the request, past the gate. No route of a workspace sets one.
#   With reach "tailnet" there is no tunnel, and two containers stand in its place:
#     automagik-tailnet  a Tailscale node, in userspace networking, IN A NETWORK NAMESPACE OF ITS OWN. It serves HTTPS
#                        on port 443 of the machine's own tailnet name (never Funnel) and hands every request to the
#                        relay. Its state is the machine's /var/lib/automagik-workspace/tailnet, which keeps the
#                        node's identity across restarts.
#     automagik-relay    the server image's node, in the server's namespace, copying bytes between one address on a
#                        private Docker network (automagik-relay, internal, joined by the server and the node and
#                        nothing else) and the gate's 127.0.0.1:3082. It is the only way from the node to the
#                        workspace, and it leads to the gate's port alone.
#   The node must never share the server's namespace. A Tailscale node in userspace networking passes every
#   connection that reaches its tailnet address, on ANY port it does not serve itself, to 127.0.0.1 on that port (in
#   Tailscale's source, wgengine/netstack: "case isTailscaleIP: dialIP = ipv4Loopback"). In the server's namespace
#   that loopback is the workspace's: the server's own port, the gate's in plain http, and every loopback listener a
#   program in the workspace opens would be open to the whole tailnet. In a namespace of its own the node's loopback
#   holds nothing. The keeper removes a node it finds in the server's namespace and never calls such a node serving.
#   Plain `docker run` gives this shape; Docker Compose is not installed.
#
#   With reach "tailnet" the gate signs its owner in itself, with passkeys. Its first owner claims it once, with a
#   code the gate writes into its own data (/data/claim-code in the gate's container). The keeper never reads that
#   code and never prints it; it looks only at whether the file is there. read_claim_code below is the one place
#   that reads it, and nothing in this script calls it.
#
# The member runner (settings.memberRunner, off unless a create turns it on). A member of the workspace is someone the
# owner lets in, and Automagik runs that member's commands under a host uid of the member's own, so a member cannot
# read the owner's files. DSH's container cannot make a user namespace, so the runner does it, OUTSIDE the container:
# Automagik's member-runner.mjs, run by Node 22 as a user of its own (automagik-runner) as a systemd service. It holds
# one /etc/subuid and one /etc/subgid range and maps each member, for good, to one uid of that range through
# newuidmap and newgidmap. It is rootless, needs no daemon, and is not in the docker group. DSH reaches it over one
# unix socket in a folder only DSH's own uid and group may enter; DSH itself holds no privilege. With the runner on:
#   - DSH's uid on the machine (the server image's own user, WS_DATA_OWNER) is a service uid and nothing else: a
#     cloud image gives that uid to its login user (Debian's "debian", in sudo with NOPASSWD). The first boot takes
#     its sudo rules, its supplementary groups, its password and its shell away. Nothing of Arca's logs in as that
#     user: Arca reaches the machine as root, through the Incus agent.
#   - The workspace DSH works in, /data/workspace in DSH's container, is a folder of the machine at that same path,
#     mounted over the server's /data, so a path names one place for DSH and for the runner. It is never all of
#     /data: that holds DSH's own state and its launch token.
#   - Every run of every member is in the runner's service, whose memory and task count are bounded (MemoryMax,
#     TasksMax). Delegate=yes lets the runner give each run a bound of its own.
#   - The runner's file is Automagik's release, fetched by URL and kept only when its sha256 is the one fixed here.
#     THE PAIR BELOW IS A PLACEHOLDER: Automagik has not released the runner yet. Its URL is empty and its sha256 is
#     that of the M1 spike's member-runner.mjs, for tests only. A create that turns the runner on before the release is
#     refused at the first boot, in words, unless the file is on the machine already.
#
# What it reads:
# - /etc/arca/settings.json: the service's settings, written by cloud-init at the first boot and fixed from then on.
#   Those a service_create gave (the template lists which it may) stand in place of the template's own.
#   publicUrl is the workspace's exact public origin (https://<host>, no path); empty, as the template ships it,
#   means "not set". accessTeamDomain, accessAudience and accessEmails are the owner's Cloudflare Access application for
#   that address (none of them a secret): all three are set, or none. accessEmails is text, one address or several
#   separated by commas, which the gate gets unchanged; a list with one address in each entry, as workspaces made
#   before took it, is taken too. serverImage is the server's image by digest. automagikPnpm is the absolute path
#   of pnpm 10 baked into that image, supplied by its owner; an empty or missing setting refuses the server start.
#   reach is "cloudflare" (also when it is left out) or "tailnet"; with "tailnet" publicUrl is the node's own
#   https://<machine>.<tailnet>.ts.net and the Access settings are refused.
# - /etc/arca-secrets/TUNNEL_TOKEN (reach "cloudflare") or /etc/arca-secrets/TAILSCALE_AUTHKEY (reach "tailnet"):
#   the one secret of the reach, written by Arca at each start (service_secret_set, then instance_start). The server
#   and the gate start without it; the tunnel or the node waits for it, and a new one replaces that part only.
#
# What it writes: one line on the machine's console (service_logs) whenever the state of a part changes, and the same
# status line at intervals. The server and the gate are healthy when the image's own check says so: HTTP 401 on the
# loopback port, the answer to a visitor who has not signed in.
#
# The secret is never printed or put on a command line. The keeper never holds it in a variable: it is copied, without
# line breaks, to a file on /run (memory) that only its part's user reads. cloudflared reads that file
# (`--token-file`) and the tailnet node is told the file's path (`TS_AUTHKEY=file:<path>`), so the secret is not in
# Docker's record of the container either. The keeper keeps only its digest, to see when it changes. The keeper never reads the server's or
# the gate's data, and it never copies their output to the console: the server's sign-in link is in both.
#
# The server's image is pulled only once its GitHub build provenance verifies: the attestation Automagik's release
# workflow makes for that digest, in the automagik-si/automagik repository, checked by cosign (Sigstore's own release,
# at the version and checksum fixed below, which the first boot downloads and checks). An image that does not verify
# is not pulled, and the server does not start; the keeper says why and tries again after a pause. Every container
# made from the server's image is started with `--pull never`, so Docker never pulls it by itself.
#
# After the install image is healthy, the keeper follows Automagik's stable server lane. A newer digest must match
# its fixed release record and pass the same provenance check before the pull. The server and gate restart on their
# existing data mounts. If either fails its health check, the keeper restarts them from the previous digest.
#
# Every path, pause and external program below can be replaced through the environment, which is how the test in
# crates/arca/tests/automagik_workspace.rs drives the keeper against a fake docker.
set -u

: "${WS_SETTINGS:=/etc/arca/settings.json}"
: "${WS_SECRETS:=/etc/arca-secrets}"
: "${WS_DATA:=/var/lib/automagik-workspace/data}"
: "${WS_GATE_DATA:=/var/lib/automagik-workspace/gate}"
: "${WS_TAILNET_STATE:=/var/lib/automagik-workspace/tailnet}"
: "${WS_RUN:=/run/automagik-workspace}"
: "${WS_FOLLOW_STATE:=${WS_DATA%/*}/follow.json}"  # root's record beside the data, across keeper and VM restarts
: "${WS_FOLLOW_EVERY:=3600}"   # seconds between lane checks, starting when the install image is healthy
: "${WS_FOLLOW_HEALTH:=300}"   # seconds for an update to become healthy before restoring the previous image
: "${WS_CONSOLE:=/dev/console}"
: "${WS_INSTALLED:=/usr/local/sbin/arca-automagik-workspace}"
: "${WS_UNIT:=/etc/systemd/system/arca-automagik-workspace.service}"
: "${WS_DOCKER_ETC:=/etc/docker}"
: "${WS_DATA_OWNER:=1000:1000}"      # the server image's own user, which the gate runs as too
# Automagik image contract: numeric uid 1000 (no /home entry); the image's entrypoint runs unchanged.
# All paths are inside the EXISTING WS_DATA bind at /data. Keep this layout together for image review.
NATIVE_HOME=/data
AUTOMAGIK_HOME=/data
AUTOMAGIK_BOTS_HOME=/data/bots
DSH_HOME=/data/dsh
AUTOMAGIK_WORKSPACE=/data/workspace
HERDR_APP_HOME=/data/herdr-app                         # PROPOSED: the image owner is adding this
HC_TERMINAL_SOCKET=/data/run/herdr-core/default.sock   # PROPOSED: supervised by the image entrypoint
: "${WS_TUNNEL_OWNER:=65532:65532}"  # the user the tunnel is run as; it alone reads the token's copy
: "${WS_TAILNET_OWNER:=0:0}"         # the tailnet image's own user; it alone reads the auth key's copy
: "${WS_CLAIM_MINUTES:=15}"          # how long the gate's claim code is good for, as the gate fixes it
: "${WS_TICK:=10}"            # seconds between two looks at the parts
: "${WS_STATUS_EVERY:=300}"   # seconds between two status lines when nothing changes
: "${WS_REFUSE_EVERY:=300}"   # seconds between two tellings of why the workspace does not start
: "${WS_BACKOFF_MAX:=300}"    # the longest pause before a part that keeps failing is started again, in seconds
: "${WS_APT_PAUSE:=20}"       # seconds between two tries of the first boot's package install
: "${WS_COSIGN:=/usr/local/lib/arca-automagik-workspace/cosign}"  # the provenance verifier, as the first boot installs it
: "${WS_PROC:=/proc}"          # where the kernel shows each process, for the namespace a container is in
: "${WS_COSIGN_SHA256:=}"     # the verifier's checksum; empty is the one fixed below for this machine's architecture
: "${WS_WORKSPACE:=/data/workspace}"                         # DSH's workspace, at the same path in its container
: "${WS_RUNNER_HOME:=/var/lib/automagik-member-runner}"      # the runner user's home: its state, members.json
: "${WS_RUNNER_PROGRAM:=/usr/local/lib/automagik-member-runner}"  # Node 22 and member-runner.mjs, root's
: "${WS_RUNNER_SOCKETS:=/run/automagik-member-runner}"       # the socket's folder, mounted into DSH's container
: "${WS_RUNNER_UNIT:=/etc/systemd/system/automagik-member-runner.service}"
: "${WS_RUNNER_TMPFILES:=/etc/tmpfiles.d/automagik-member-runner.conf}"
: "${WS_SUDOERS:=/etc/sudoers.d}"
: "${WS_RUNNER_SHA256:=}"     # the runner file's checksum; empty is the one fixed below
: "${WS_NODE_SHA256:=}"       # Node's checksum; empty is the one fixed below for this machine's architecture

SERVER=automagik-server
GATE=automagik-gate
TUNNEL=automagik-tunnel
TAILNET=automagik-tailnet
RELAY=automagik-relay
RELAY_NET=automagik-relay

# The Automagik server image's contract for the keeper (server/compose.gate.yml, server/gate-entrypoint.mjs,
# server/gate-access.mjs, server/gate-claim.mjs, and server/healthcheck.mjs at release 3.160.2).
SERVER_PORT=3081                                  # the server's own port, fixed in the image
GATE_PORT=3082                                    # the gate's port, the one upstream's compose file fixes
IMAGE_NODE=/nodejs/bin/node                       # the image's node
GATE_ENTRY=/opt/automagik/gate-entrypoint.mjs     # the gate's entrypoint, in the same image
HEALTH_ENTRY=/opt/automagik/healthcheck.mjs       # the image's own check: prints the HTTP code, succeeds on 401
GATE_ENABLED_ENV=AUTOMAGIK_GATE_ENABLED           # the gate: 1 turns it on
GATE_ORIGIN_ENV=AUTOMAGIK_GATE_ORIGIN             # the gate: the exact public origin
GATE_PORT_ENV=AUTOMAGIK_GATE_PORT                 # the gate: its loopback port
ACCESS_TEAM_ENV=AUTOMAGIK_GATE_ACCESS_TEAM_DOMAIN # the gate: the Cloudflare Access team's host name
ACCESS_AUDIENCE_ENV=AUTOMAGIK_GATE_ACCESS_AUD     # the gate: the Access application's audience tag
ACCESS_EMAILS_ENV=AUTOMAGIK_GATE_ACCESS_EMAILS    # the gate: the e-mail addresses that may sign in, comma-separated
GATE_CLAIM_ENV=AUTOMAGIK_GATE_CLAIM               # the gate without Access: "remote" takes the first claim on its main
                                                  # port, behind the code; "local" (its default) only on port 3083
CLAIM_CODE_FILE=/data/claim-code                  # the gate's claim code, in the gate's own data; gone once claimed

# The connector's image, by digest: the one templates/cloudflare-tunnel.json pins (cloudflared 2026.10.7). It is not a
# setting: a workspace does not choose its connector.
TUNNEL_IMAGE=docker.io/cloudflare/cloudflared@sha256:4955524aeaa379e154023d5a027c250f7b35041a2e7e950cf13f144bc9b9c5ba

# The tailnet node's image, by digest: Tailscale's own image from its official registry, release v1.104.4. Like the
# connector's it is not a setting. What this script takes from that image is in this one place: the names its start
# program reads, where it keeps its state, and its control socket, which `tailscale status` is pointed at.
TAILNET_IMAGE=docker.io/tailscale/tailscale@sha256:ed6480b33c6eb67c9cbd203ed42b9fb57e361986725b6856def71b7a3d9e276c
TAILNET_STATE_DIR=/var/lib/tailscale
TAILNET_SOCKET=/tmp/tailscaled.sock

# The provenance verifier: cosign v3.1.4, Sigstore's own release on GitHub, one binary for each architecture, by its
# sha256 (the release's cosign_checksums.txt). It trusts what it carries itself, Sigstore's public root, and nothing
# else: no key, no account and no token. The server's image verifies when it has a build-provenance attestation
# (SLSA v1) whose signing certificate GitHub Actions issued to a workflow of PROVENANCE_REPO.
COSIGN_RELEASE=https://github.com/sigstore/cosign/releases/download/v3.1.4
COSIGN_AMD64=424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa
COSIGN_ARM64=ade83b7aac62e131cacfe96d958948b2150cc9efec46a570a2649b8e27dedad2
PROVENANCE_REPO=automagik-si/automagik
PROVENANCE_ISSUER=https://token.actions.githubusercontent.com
LANE_IMAGE=ghcr.io/automagik-si/automagik-server
SERVER_RELEASES=https://github.com/automagik-si/automagik/releases/download
VERSION_PATTERN='\A(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\z'

# The member runner (see the head of this file). Its user, its uid and gid range (one /etc/subuid and one /etc/subgid
# line; far above the ranges useradd hands out from 100000), and the Node it runs on: Node 22.23.5, the Node.js
# project's own release, one tarball for each architecture, by its sha256 (the release's SHASUMS256.txt).
RUNNER_USER=automagik-runner
RUNNER_RANGE_START=1000000
RUNNER_RANGE_COUNT=65536
NODE_RELEASE=https://nodejs.org/dist/v22.23.5
NODE_VERSION=v22.23.5
NODE_AMD64=c54118104acc524633b5abe106bbf44a53126ec8b4e5fa105cb92b5d7de8a1a1
NODE_ARM64=8bd3dc69eee8f3f226315a80d7d1d4faccf217a4c8fae500168ac2e38f3bdba3
# PLACEHOLDER, one pair: Automagik's release of member-runner.mjs. There is no release yet, so the URL is empty, and the
# sha256 is the M1 spike's file (automagik-member-runner-spike, 2026-11-15), for tests only. Never ship this pair
# turned on: replace both with the release's URL and sha256 when Automagik publishes it.
RUNNER_URL=''
RUNNER_SHA256=0b6f3e4ac3800eb55f8365a5282c96c6a3e64adc4754f332c6517d5bc913d55c
# The packages only the member runner needs: newuidmap and newgidmap (setuid, so they are installed only with it).
RUNNER_PACKAGES='uidmap'

# The relay's whole program, for the server image's node: it listens on the one address and port it is given and
# copies bytes to and from the same port on loopback. Bytes only: the visitor's Host header reaches the gate as the
# node sent it.
RELAY_PROGRAM='const net=require("net");const [ip,port]=process.argv.slice(1);net.createServer(c=>{const u=net.connect(Number(port),"127.0.0.1");c.on("error",()=>u.destroy());u.on("error",()=>c.destroy());c.pipe(u).pipe(c)}).listen(Number(port),ip)'

# Every program this script starts that bash does not have built in. The first boot refuses to go on while one of
# them is missing, and names it.
WS_PROGRAMS='docker jq systemctl apt-get hostname sha256sum install tr cut date sleep cat chmod chown mv rm grep
tail mkdir stat curl uname readlink git setpriv find rmdir'

# The Debian packages the first boot installs. Recommended packages are not installed, so every package that is needed
# is named here. docker.io is the daemon alone: on Debian 13 the `docker` command is a package of its own, docker-cli,
# which docker.io only recommends. The ones the image already carries are named too, so the list says what the script
# stands on.
WS_PACKAGES='docker.io docker-cli jq ca-certificates coreutils curl git util-linux findutils'

say() { printf 'arca-automagik-workspace: %s\n' "$*"; }
now() { date +%s; }
# A pause a signal can end at once.
nap() {
  sleep "$1" &
  wait $! 2>/dev/null
}

# ---------------------------------------------------------------------------------------------------------------------
# First boot
# ---------------------------------------------------------------------------------------------------------------------

# cloud-init keeps what the first boot prints in /var/log/cloud-init-output.log, inside the machine. The machine's
# console is what service_logs returns, so the first boot says its own lines there as well: a first boot that fails
# must not leave a machine that is running and silent.
boot_say() {
  say "$*"
  { say "$*" >>"$WS_CONSOLE"; } 2>/dev/null
  return 0
}

# The programs of WS_PROGRAMS that are not on this machine, in one line.
missing_programs() {
  local program missing=''
  for program in $WS_PROGRAMS; do
    command -v "$program" >/dev/null 2>&1 || missing="$missing $program"
  done
  printf '%s' "${missing# }"
}

# Installs these packages from the distribution, without the ones they only recommend, in up to six tries. Whether
# they are there is for the caller to look at, by the commands it needs.
apt_install() {
  local try
  boot_say "first boot: installing $1"
  for try in 1 2 3 4 5 6; do
    # shellcheck disable=SC2086
    if apt-get update -q && apt-get install -y -q --no-install-recommends $1; then
      return 0
    fi
    boot_say "first boot: the packages could not be installed (try $try of 6); trying again in $WS_APT_PAUSE seconds"
    sleep "$WS_APT_PAUSE"
  done
  return 1
}

install_workspace() {
  export DEBIAN_FRONTEND=noninteractive
  local missing
  apt_install "$WS_PACKAGES"
  missing=$(missing_programs)
  if [ -n "$missing" ]; then
    boot_say "first boot failed: these commands are missing after the install: $missing. The packages asked for were: $WS_PACKAGES. The package manager's own words are in /var/log/cloud-init-output.log inside the machine. The machine has no workspace; service_remove it and create it again once it can reach its package mirror"
    exit 1
  fi
  if ! install_verifier; then
    boot_say "first boot failed: $REFUSAL. The machine has no workspace; service_remove it and create it again once it can reach github.com and release-assets.githubusercontent.com"
    exit 1
  fi
  mkdir -p "$WS_DOCKER_ETC"
  if [ ! -e "$WS_DOCKER_ETC/daemon.json" ]; then
    # The containers' own output is bounded, so it never fills the machine's disk.
    printf '%s\n' '{ "log-driver": "local", "log-opts": { "max-size": "20m", "max-file": "2" } }' \
      >"$WS_DOCKER_ETC/daemon.json"
  fi
  # Settings that cannot be used never change: they are part of the machine. That is said now, on the console, and
  # the keeper is installed all the same, so that every later look at service_logs finds the reason again.
  load_settings || boot_say "first boot: the settings cannot be used: $REFUSAL. The workspace will not start; service_remove it and create it again with settings that are right"
  if ! data_directories; then
    boot_say "first boot failed: the data directories $WS_DATA and $WS_GATE_DATA could not be made for the server's user ($WS_DATA_OWNER). The machine has no workspace"
    exit 1
  fi
  if [ "${MEMBER_RUNNER:-false}" = true ] && ! install_member_runner; then
    boot_say "first boot failed: the member runner could not be set up: $REFUSAL. The machine has no workspace; service_remove it and create it again, or create it with settings.memberRunner false"
    exit 1
  fi
  install -m 0755 "$0" "$WS_INSTALLED"
  cat >"$WS_UNIT" <<UNIT
[Unit]
Description=Arca Automagik workspace
After=docker.service network-online.target
Wants=network-online.target
Requires=docker.service

[Service]
ExecStart=$WS_INSTALLED run
Restart=always
RestartSec=10
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  if ! systemctl restart docker || ! systemctl enable --now arca-automagik-workspace.service; then
    boot_say "first boot failed: Docker or the keeper's service did not start. Inside the machine, 'systemctl status docker arca-automagik-workspace' says why. The machine has no workspace"
    exit 1
  fi
  boot_say "first boot done: the keeper is installed and started"
}

# The provenance verifier: Sigstore's release of cosign for this machine's architecture, downloaded and kept only
# when its sha256 is the one fixed above. A download that fails is tried again, as the package install is.
install_verifier() {
  local arch sum got try fetched part="$WS_COSIGN.part"
  case $(uname -m) in
    x86_64) arch=amd64 sum=$COSIGN_AMD64 ;;
    aarch64) arch=arm64 sum=$COSIGN_ARM64 ;;
    *)
      refuse "the provenance verifier (cosign) has no build fixed here for this machine's architecture, $(uname -m)"
      return
      ;;
  esac
  sum=${WS_COSIGN_SHA256:-$sum}
  boot_say "first boot: installing the provenance verifier, cosign-linux-$arch from $COSIGN_RELEASE"
  if ! mkdir -p "${WS_COSIGN%/*}"; then
    refuse "the provenance verifier's directory ${WS_COSIGN%/*} could not be made"
    return
  fi
  # GitHub answers the release's address with a redirect to release-assets.githubusercontent.com, which serves the
  # file: the machine needs https to both. Each try has a bound, so a transfer that stalls is a failed try.
  fetched=''
  for try in 1 2 3 4 5 6; do
    if curl -fsSL --proto '=https' --connect-timeout 30 --max-time 600 -o "$part" "$COSIGN_RELEASE/cosign-linux-$arch"; then
      fetched=1
      break
    fi
    boot_say "first boot: the provenance verifier could not be downloaded from github.com and release-assets.githubusercontent.com within 600 seconds (try $try of 6); trying again in $WS_APT_PAUSE seconds"
    sleep "$WS_APT_PAUSE"
  done
  if [ -z "$fetched" ]; then
    rm -f "$part"
    refuse "the provenance verifier could not be downloaded from $COSIGN_RELEASE/cosign-linux-$arch in 6 tries of at most 600 seconds each: the machine needs https to github.com and release-assets.githubusercontent.com"
    return
  fi
  got=$(sha256sum "$part" 2>/dev/null | cut -d' ' -f1)
  if [ "$got" != "$sum" ]; then
    rm -f "$part"
    refuse "the provenance verifier from $COSIGN_RELEASE/cosign-linux-$arch is not the one this script fixes (sha256 $got, where $sum was expected)"
    return
  fi
  if ! { chmod 0755 "$part" && mv -f "$part" "$WS_COSIGN"; }; then
    refuse "the provenance verifier could not be put at $WS_COSIGN"
  fi
}

# Downloads $1 to the file $3 and keeps it only when its sha256 is $2, in up to six tries of at most 600 seconds.
fetch_pinned() {
  local try got part="$3.part"
  for try in 1 2 3 4 5 6; do
    if curl -fsSL --proto '=https' --connect-timeout 30 --max-time 600 -o "$part" "$1"; then
      got=$(sha256sum "$part" 2>/dev/null | cut -d' ' -f1)
      if [ "$got" != "$2" ]; then
        rm -f "$part"
        refuse "$1 is not the file this script fixes (sha256 $got, where $2 was expected)"
        return 1
      fi
      mv -f "$part" "$3" && return 0
      rm -f "$part"
      refuse "$1 was downloaded and checked, but could not be put at $3"
      return 1
    fi
    boot_say "first boot: $1 could not be downloaded within 600 seconds (try $try of 6); trying again in $WS_APT_PAUSE seconds"
    sleep "$WS_APT_PAUSE"
  done
  rm -f "$part"
  refuse "$1 could not be downloaded in 6 tries of at most 600 seconds each"
}

# The member runner, as the head of this file says: its packages, Node, Automagik's file, DSH's uid made a service
# uid, the runner's user and range, the workspace folder, the socket's folder and the runner's service. Each step that
# fails says why in REFUSAL.
install_member_runner() {
  local dsh_uid=${WS_DATA_OWNER%%:*} dsh_gid=${WS_DATA_OWNER##*:} arch sum dsh_name dsh_home file line who start count
  local runner=$WS_RUNNER_PROGRAM/member-runner.mjs node=$WS_RUNNER_PROGRAM/node/bin/node
  local want=${WS_RUNNER_SHA256:-$RUNNER_SHA256} got
  boot_say "first boot: setting up the member runner"
  apt_install "$RUNNER_PACKAGES" || true
  for file in newuidmap newgidmap unshare setpriv getent useradd usermod systemd-tmpfiles tar sed; do
    command -v "$file" >/dev/null 2>&1 || {
      refuse "the command $file is missing after installing $RUNNER_PACKAGES"
      return 1
    }
  done
  mkdir -p "$WS_RUNNER_PROGRAM" || {
    refuse "the folder $WS_RUNNER_PROGRAM could not be made"
    return 1
  }

  # Automagik's file before Node and the users: a release that is not there yet is said before any user is changed.
  # uidmap and the runner's folder are in place by then; a first boot that ends here leaves no keeper and no runner.
  got=$(sha256sum "$runner" 2>/dev/null | cut -d' ' -f1)
  if [ "$got" != "$want" ]; then
    if [ -z "$RUNNER_URL" ]; then
      refuse "Automagik has not released the member runner yet: this template's runner URL is a placeholder"
      return 1
    fi
    fetch_pinned "$RUNNER_URL" "$want" "$runner" || return 1
  fi
  chmod 0644 "$runner"

  case $(uname -m) in
    x86_64) arch=x64 sum=$NODE_AMD64 ;;
    aarch64) arch=arm64 sum=$NODE_ARM64 ;;
    *)
      refuse "Node has no build fixed here for this machine's architecture, $(uname -m)"
      return 1
      ;;
  esac
  sum=${WS_NODE_SHA256:-$sum}
  if [ "$("$node" --version 2>/dev/null)" != "$NODE_VERSION" ]; then
    fetch_pinned "$NODE_RELEASE/node-$NODE_VERSION-linux-$arch.tar.gz" "$sum" "$WS_RUNNER_PROGRAM/node.tar.gz" || return 1
    rm -rf "$WS_RUNNER_PROGRAM/node" && mkdir -p "$WS_RUNNER_PROGRAM/node" &&
      tar -xzf "$WS_RUNNER_PROGRAM/node.tar.gz" -C "$WS_RUNNER_PROGRAM/node" --strip-components=1 --no-same-owner
    rm -f "$WS_RUNNER_PROGRAM/node.tar.gz"
    if [ "$("$node" --version 2>/dev/null)" != "$NODE_VERSION" ]; then
      refuse "Node $NODE_VERSION did not unpack to $WS_RUNNER_PROGRAM/node"
      return 1
    fi
  fi

  # DSH's uid is a service uid only: whoever the image gave it to loses sudo, its groups, its password and its shell.
  dsh_name=$(getent passwd "$dsh_uid" | cut -d: -f1)
  if [ -n "$dsh_name" ]; then
    if ! [[ $dsh_name =~ ^[a-z_][a-z0-9_-]*$ ]]; then
      refuse "the user with DSH's uid $dsh_uid has a name this script does not take apart safely: $dsh_name"
      return 1
    fi
    dsh_home=$(getent passwd "$dsh_uid" | cut -d: -f6)
    for file in "$WS_SUDOERS"/*; do
      [ -f "$file" ] && grep -q "^[[:space:]]*${dsh_name}[[:space:]]" "$file" && sed -i "/^[[:space:]]*${dsh_name}[[:space:]]/d" "$file"
    done
    if ! usermod --lock --shell /usr/sbin/nologin -G '' "$dsh_name"; then
      refuse "the user $dsh_name, who has DSH's uid $dsh_uid, could not be made a service user"
      return 1
    fi
    [ -z "$dsh_home" ] || rm -f "$dsh_home/.ssh/authorized_keys"
    boot_say "first boot: uid $dsh_uid ($dsh_name) is DSH's service uid now: no sudo, no groups, no password, no shell"
  fi

  if ! getent passwd "$RUNNER_USER" >/dev/null; then
    useradd --system --user-group --home-dir "$WS_RUNNER_HOME" --create-home --shell /usr/sbin/nologin "$RUNNER_USER" || {
      refuse "the runner's user $RUNNER_USER could not be made"
      return 1
    }
  fi
  chmod 0700 "$WS_RUNNER_HOME"
  # One line in each of /etc/subuid and /etc/subgid, and no other line that overlaps it.
  for file in /etc/subuid /etc/subgid; do
    while IFS=: read -r who start count; do
      [[ $start =~ ^[0-9]+$ && $count =~ ^[0-9]+$ ]] || continue
      [ "$who" = "$RUNNER_USER" ] && [ "$start" = "$RUNNER_RANGE_START" ] && [ "$count" = "$RUNNER_RANGE_COUNT" ] && continue
      if [ "$start" -lt $((RUNNER_RANGE_START + RUNNER_RANGE_COUNT)) ] && [ $((start + count)) -gt "$RUNNER_RANGE_START" ]; then
        refuse "$file gives $who ids $start to $((start + count - 1)), inside the runner's range $RUNNER_RANGE_START to $((RUNNER_RANGE_START + RUNNER_RANGE_COUNT - 1))"
        return 1
      fi
    done <"$file"
  done 2>/dev/null
  line="$RUNNER_USER:$RUNNER_RANGE_START:$RUNNER_RANGE_COUNT"
  if ! grep -qx "$line" /etc/subuid 2>/dev/null || ! grep -qx "$line" /etc/subgid 2>/dev/null; then
    usermod --add-subuids "$RUNNER_RANGE_START-$((RUNNER_RANGE_START + RUNNER_RANGE_COUNT - 1))" \
      --add-subgids "$RUNNER_RANGE_START-$((RUNNER_RANGE_START + RUNNER_RANGE_COUNT - 1))" "$RUNNER_USER" || {
      refuse "the runner's range could not be written to /etc/subuid and /etc/subgid"
      return 1
    }
  fi

  # The workspace: DSH's, and others may pass through it to a member's own folder but not list it. The socket's
  # folder: the runner's, in DSH's group, setgid so the socket is DSH's group's too; nobody else enters it.
  if ! { install -d -m 0711 -o "$dsh_uid" -g "$dsh_gid" "$WS_WORKSPACE" &&
    printf 'd %s 2750 %s %s -\n' "$WS_RUNNER_SOCKETS" "$RUNNER_USER" "$dsh_gid" >"$WS_RUNNER_TMPFILES" &&
    systemd-tmpfiles --create "$WS_RUNNER_TMPFILES"; }; then
    refuse "the folders $WS_WORKSPACE and $WS_RUNNER_SOCKETS could not be made"
    return 1
  fi

  cat >"$WS_RUNNER_UNIT" <<UNIT
[Unit]
Description=Automagik member runner: members' commands under uids of their own
After=local-fs.target systemd-tmpfiles-setup.service

[Service]
User=$RUNNER_USER
Group=$RUNNER_USER
ExecStart=$node $runner --socket $WS_RUNNER_SOCKETS/runner.sock --socket-mode 660 --state $WS_RUNNER_HOME/members.json --workspace-root $WS_WORKSPACE --refuse-uid $dsh_uid
Restart=always
RestartSec=5
# Every run of every member is in this service: these bound them all together. Delegate lets the runner bound each.
MemoryMax=50%
TasksMax=1024
Delegate=yes
# NoNewPrivileges must stay off: newuidmap and newgidmap are setuid, and the runner needs them for every run.
StandardOutput=journal
StandardError=journal+console

[Install]
WantedBy=multi-user.target
UNIT
  if ! { systemctl daemon-reload && systemctl enable --now automagik-member-runner.service; }; then
    refuse "its service did not start; inside the machine, 'systemctl status automagik-member-runner' says why"
    return 1
  fi
  boot_say "first boot: the member runner is started as $RUNNER_USER, ids $RUNNER_RANGE_START to $((RUNNER_RANGE_START + RUNNER_RANGE_COUNT - 1)), socket $WS_RUNNER_SOCKETS/runner.sock"
}

# The server's /data and the gate's /data: two directories on the machine's own disk, the server user's. Only the
# directories and the server's explicit homes are prepared; existing file contents are never read or changed.
data_directories() {
  install -d -m 0750 -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" "$WS_GATE_DATA" || return 1
  # The image seeds directories, but this bind hides them. Prepare them for uid 1000 before the entrypoint runs.
  # No extra volume, no recursive ownership changes and NO .herdr/bots.json or roster: the image seeds those.
  local workspace_path
  for workspace_path in "$NATIVE_HOME" "$AUTOMAGIK_HOME" "$AUTOMAGIK_BOTS_HOME" "$DSH_HOME" "$HERDR_APP_HOME" \
    "$AUTOMAGIK_WORKSPACE" "$AUTOMAGIK_HOME/acp" "$AUTOMAGIK_HOME/acp/clis" /data/secrets /data/run \
    "${HC_TERMINAL_SOCKET%/*}"; do
    install -d -m 0700 -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" "$WS_DATA${workspace_path#/data}" || return 1
  done
  # The tailnet node's state: its identity on the tailnet. Made only for the reach that has a node.
  [ "${REACH:-cloudflare}" != tailnet ] ||
    install -d -m 0700 -o "${WS_TAILNET_OWNER%%:*}" -g "${WS_TAILNET_OWNER##*:}" "$WS_TAILNET_STATE"
}

# ---------------------------------------------------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------------------------------------------------

REFUSAL=''
refuse() {
  REFUSAL=$*
  return 1
}

# Reads the settings into REACH, PUBLIC_URL, SERVER_IMAGE and the ACCESS_ values. When they cannot be used it fails,
# with the reason, in words, in REFUSAL.
load_settings() {
  local url port address='[A-Za-z0-9._%+-]+@[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?'
  REFUSAL=''
  if ! jq -e 'type == "object"' "$WS_SETTINGS" >/dev/null 2>&1; then
    refuse "the settings are missing or are not one JSON object ($WS_SETTINGS)"
    return
  fi
  # An empty publicUrl, as the template ships it, means "not set". One that is not a string becomes "!", which
  # the rules below refuse.
  url=$(jq -r '.publicUrl | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  REACH=$(jq -r '.reach | if . == null then "cloudflare" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  SERVER_IMAGE=$(jq -r '.serverImage | if type == "string" then . else "" end' "$WS_SETTINGS")
  AUTOMAGIK_PNPM=$(jq -r '.automagikPnpm | if type == "string" then . else "" end' "$WS_SETTINGS")
  MEMBER_RUNNER=$(jq -r '.memberRunner | if . == null then "false" elif type == "boolean" then tostring else "!" end' "$WS_SETTINGS")
  # A missing key is an empty one, and so is an empty list or text of spaces alone. A value of the wrong kind
  # becomes "!", which no rule below takes. accessEmails is text the gate gets unchanged: one address or several,
  # separated by commas, spaces around each allowed (the gate trims each one and compares them without regard to
  # case). The list form, one whole address in each entry, is joined with commas: an entry that holds a comma, a
  # space or a line break is not one address, so the value the gate gets has as many addresses as the list entries.
  ACCESS_TEAM=$(jq -r '.accessTeamDomain | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  ACCESS_AUDIENCE=$(jq -r '.accessAudience | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  ACCESS_EMAILS=$(jq -r --arg address "\\A$address\\z" '.accessEmails | if . == null or . == [] then "" elif type == "string" then (if test("\\A *\\z") then "" else . end) elif type == "array" and length <= 16 and all(.[]; type == "string" and test($address)) then join(",") else "!" end' "$WS_SETTINGS" 2>/dev/null) || ACCESS_EMAILS='!'
  ACCESS_COUNT=0
  if [ "$REACH" != cloudflare ] && [ "$REACH" != tailnet ]; then
    REACH=cloudflare
    refuse "settings.reach is how the workspace is reached: \"cloudflare\" (a Cloudflare Tunnel; also when it is left out) or \"tailnet\" (a node on your tailnet)"
    return
  fi
  if [ "$MEMBER_RUNNER" != true ] && [ "$MEMBER_RUNNER" != false ]; then
    MEMBER_RUNNER=false
    refuse "settings.memberRunner is true (members' commands run under uids of their own, through the member runner) or false (no member runner; also when it is left out)"
    return
  fi
  if ! pinned "$SERVER_IMAGE"; then
    refuse "settings.serverImage is an image by digest: <registry>/<name>@sha256:<64 hex digits>"
    return
  fi
  # There is deliberately no fallback to PATH or AUTOMAGIK_APP_BROWSER: the store needs the image's pinned pnpm 10.
  if ! [[ $AUTOMAGIK_PNPM =~ ^/([A-Za-z0-9._+-]+/)*[A-Za-z0-9._+-]+$ ]] ||
    [[ $AUTOMAGIK_PNPM =~ /\.\.?(/|$) ]]; then
    refuse "settings.automagikPnpm must name the absolute path of pnpm 10 baked into the image; AUTOMAGIK_PNPM is required to start the CLI store"
    return
  fi
  REPO_URL=$(jq -r '.repoUrl | if type == "string" then . else "" end' "$WS_SETTINGS")
  REPO_REF=$(jq -r '.repoRef | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  if ! [[ $REPO_URL =~ ^https://github\.com/[A-Za-z0-9-]+/[A-Za-z0-9._-]+$ ]] ||
    ! REPO_IDENTITY=$(github_identity "$REPO_URL"); then
    refuse "settings.repoUrl is required: https://github.com/<owner>/<repo>[.git], with no credentials or other host"
    return
  fi
  if [ -n "$REPO_REF" ] && { ! [[ $REPO_REF =~ ^[A-Za-z0-9._/-]+$ ]] ||
    ! git check-ref-format --branch "$REPO_REF" >/dev/null 2>&1; }; then
    refuse "settings.repoRef must be a plain Git ref name; empty selects the remote default branch"
    return
  fi
  # The gate takes its origin exactly as a browser writes it, and stops when it is written another way: no path, no
  # closing slash, and a port only when it is not https's own 443.
  if [ -z "$url" ]; then
    PUBLIC_URL=''
  elif [[ $url =~ ^https://[0-9.]+(:[0-9]*)?$ ]]; then
    refuse "settings.publicUrl is a name, not an IP address: the gate takes none as its origin, and no passkey belongs to one"
    return
  elif [[ $url =~ ^https://[a-z0-9]([a-z0-9.-]*[a-z0-9])?(:([1-9][0-9]{0,4}))?$ ]] &&
    port=${BASH_REMATCH[3]} && { [ -z "$port" ] || { [ "$port" -le 65535 ] && [ "$port" -ne 443 ]; }; }; then
    PUBLIC_URL=$url
  else
    refuse "settings.publicUrl is the workspace's exact public origin: https://<host name>, in lower case, with no path or closing slash, and a port only when it is not 443 (1 to 65535, written without a leading 0)"
    return
  fi
  if [ "$REACH" = tailnet ]; then
    # The node serves its own tailnet name on port 443 and no other, and the gate's passkeys belong to that name.
    if [ -n "$ACCESS_TEAM$ACCESS_AUDIENCE$ACCESS_EMAILS" ]; then
      refuse "the Cloudflare Access settings do not go with settings.reach \"tailnet\": on a tailnet the gate signs its owner in itself, with passkeys. Leave all three empty"
      return
    fi
    if ! [[ $PUBLIC_URL =~ ^https://([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)\.[a-z0-9]([a-z0-9-]*[a-z0-9])?\.ts\.net$ ]]; then
      refuse "with settings.reach \"tailnet\", settings.publicUrl is the machine's own tailnet address: https://<machine>.<tailnet>.ts.net, in lower case, with no port and no path"
      return
    fi
    TAILNET_NAME=${BASH_REMATCH[1]}
    return 0
  fi
  [ -n "$ACCESS_TEAM$ACCESS_AUDIENCE$ACCESS_EMAILS" ] || return 0
  if [ -z "$ACCESS_TEAM" ] || [ -z "$ACCESS_AUDIENCE" ] || [ -z "$ACCESS_EMAILS" ]; then
    refuse "settings.accessTeamDomain, settings.accessAudience and settings.accessEmails go together: set all three, or none (the gate does not start with one or two)"
    return
  fi
  if [ -z "$PUBLIC_URL" ]; then
    refuse "the Cloudflare Access settings need settings.publicUrl: the address the Access application protects"
    return
  fi
  if ! [[ $ACCESS_TEAM =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?\.cloudflareaccess\.com$ ]]; then
    refuse "settings.accessTeamDomain is the Cloudflare Access team's host name alone, in lower case and without https://: <team>.cloudflareaccess.com"
    return
  fi
  if ! [[ $ACCESS_AUDIENCE =~ ^[A-Za-z0-9]{32,128}$ ]]; then
    refuse "settings.accessAudience is the Access application's audience tag: 32 to 128 letters and digits"
    return
  fi
  if ! [[ $ACCESS_EMAILS =~ ^\ *$address\ *(,\ *$address\ *){0,15}$ ]]; then
    refuse "settings.accessEmails is 1 to 16 e-mail addresses: one, or several separated by commas (owner@example.com, second@example.com), or a list with one address in each entry"
    return
  fi
  # One address for each comma, and one more.
  local commas=${ACCESS_EMAILS//[^,]/}
  ACCESS_COUNT=$((${#commas} + 1))
}

pinned() {
  [[ $1 =~ ^[a-z0-9][a-z0-9._/:-]*@sha256:[0-9a-f]{64}$ ]]
}

# Git always runs as the image user, with the same HOME as its agents. No secret is in argv or the environment.
checkout_git() {
  HOME=$WS_DATA setpriv --reuid "${WS_DATA_OWNER%%:*}" --regid "${WS_DATA_OWNER##*:}" --clear-groups -- git "$@"
}

# Accept only GitHub HTTPS identities. Userinfo in an existing origin is discarded before any log is made.
github_identity() {
  local value=${1,,} path
  [[ $value =~ ^https://([^/@]+@)?github\.com/([a-z0-9-]+)/([a-z0-9._-]+)$ ]] || return 1
  path=${BASH_REMATCH[2]}/${BASH_REMATCH[3]}
  path=${path%.git}
  [ "${path#*/}" != . ] && [ "${path#*/}" != .. ] && [ -n "${path#*/}" ] || return 1
  printf 'github.com/%s' "$path"
}

checkout_wait() {
  CHECKOUT_READY=false
  if [ "$CHECKOUT_REASON" != "$*" ]; then
    say "checkout not ready: $*"
    CHECKOUT_REASON=$*
  fi
  return 1
}

# A copied value is used only as file contents. Digest changes bypass the clone's existing backoff.
checkout_token() {
  local digest
  [ -s "$WS_SECRETS/GITHUB_TOKEN" ] || return 0
  digest=$(sha256sum <"$WS_SECRETS/GITHUB_TOKEN" | cut -d' ' -f1)
  [ "$digest" != "$CHECKOUT_TOKEN_DIGEST" ] || return 0
  install -d -m 0700 -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" "$WS_RUN-github" || return 1
  install -m 0600 -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" \
    "$WS_SECRETS/GITHUB_TOKEN" "$WS_RUN-github/token.new" || return 1
  mv "$WS_RUN-github/token.new" "$WS_RUN-github/token" || return 1
  install -m 0600 -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" \
    "$WS_RUN-github/token" "$WS_DATA/secrets/github-token.new" || return 1
  mv "$WS_DATA/secrets/github-token.new" "$WS_DATA/secrets/github-token" || return 1
  CHECKOUT_TOKEN_DIGEST=$digest
  proven checkout
}

# The API's diagnostics and body never reach logs. A failure means offline, not an identity match.
checkout_repo_id() {
  local token answer
  CHECKOUT_REMOTE_ID=null
  [ -s "$WS_RUN-github/token" ] || return 1
  token=$(cat "$WS_RUN-github/token")
  [[ $token =~ ^[A-Za-z0-9_.-]+$ ]] || return 1
  answer=$(curl --silent --fail --connect-timeout 5 --max-time 15 --config - \
    "https://api.github.com/repos/${REPO_IDENTITY#github.com/}" 2>/dev/null <<EOF
header = "Authorization: Bearer $token"
header = "Accept: application/vnd.github+json"
header = "X-GitHub-Api-Version: 2022-11-28"
EOF
  ) || return 1
  CHECKOUT_REMOTE_ID=$(jq -er '.id | select(type == "number" and . > 0 and floor == .)' <<<"$answer" 2>/dev/null) || { CHECKOUT_REMOTE_ID=null; return 1; }
}

checkout_marker_valid() {
  jq -e --arg repo "$REPO_IDENTITY" '
    type == "object" and .repo == $repo and
    (.repoId == null or (.repoId | type == "number" and . > 0 and floor == .)) and
    (.clonedAt | type == "number" and . > 0) and
    (.rosterTrackedAtClone | type == "boolean") and
    (.rosterBeforeSeed == null or (.rosterBeforeSeed | type == "boolean")) and
    (.seedStartedAt == null or (.seedStartedAt | type == "number" and . > 0)) and
    (if .seedStartedAt != null then .rosterBeforeSeed == false else true end)
  ' "$1" >/dev/null 2>&1
}

checkout_tracked_roster() {
  checkout_git -C "$1" ls-files --error-unmatch .herdr/bots.json >/dev/null 2>&1 ||
    checkout_git -C "$1" ls-files --error-unmatch .automagik/bots/bots.json >/dev/null 2>&1
}
checkout_current_roster_tracked() {
  checkout_git -C "$CHECKOUT_TARGET" cat-file -e HEAD:.herdr/bots.json >/dev/null 2>&1 ||
    checkout_git -C "$CHECKOUT_TARGET" cat-file -e HEAD:.automagik/bots/bots.json >/dev/null 2>&1
}
checkout_has_roster() {
  local path
  for path in .herdr/bots.json .automagik/bots/bots.json; do
    [ ! -e "$CHECKOUT_TARGET/$path" ] && [ ! -L "$CHECKOUT_TARGET/$path" ] || return 0
  done
  return 1
}
checkout_roster_valid() {
  # Automagik's file contracts: .herdr is v1; .automagik/bots is the repository's v2 registry.
  # The separate /data/bots/bots.json is the home's registry, never checkout provenance.
  if [ -e "$CHECKOUT_TARGET/.herdr/bots.json" ] || [ -L "$CHECKOUT_TARGET/.herdr/bots.json" ]; then
    jq -e 'type == "object" and .version == 1 and (.bots | type == "array")' \
      "$CHECKOUT_TARGET/.herdr/bots.json" >/dev/null 2>&1 || return 1
  fi
  if [ -e "$CHECKOUT_TARGET/.automagik/bots/bots.json" ] || [ -L "$CHECKOUT_TARGET/.automagik/bots/bots.json" ]; then
    jq -e 'type == "object" and .version == 2 and (.bots | type == "array") and
      (has("projects") or has("shown") | not) and
      ([.. | objects | has("sessionId") or has("sessions") or has("segments")] | any | not)' \
      "$CHECKOUT_TARGET/.automagik/bots/bots.json" >/dev/null 2>&1 || return 1
  fi
}

# Marker writes are root 0600 in a root-only directory outside /data and outside the checkout.
checkout_marker_new() {
  local tracked=$1
  (umask 077; jq -n --arg repo "$REPO_IDENTITY" --argjson id "$CHECKOUT_REMOTE_ID" \
    --argjson at "$(now)" --argjson tracked "$tracked" \
    '{repo:$repo,repoId:$id,clonedAt:$at,rosterTrackedAtClone:$tracked,rosterBeforeSeed:null,seedStartedAt:null}' >"$CHECKOUT_MARKER.new")
}

checkout_authorship() {
  local helper name email
  helper=$(checkout_git config --file "$WS_DATA/.gitconfig" --get credential.https://github.com.helper 2>/dev/null) || helper=''
  # Presence, even an empty helper, belongs to the owner.
  if ! checkout_git config --file "$WS_DATA/.gitconfig" --get credential.https://github.com.helper >/dev/null 2>&1; then
    # Expanded by Git's helper shell, never by this keeper.
    # shellcheck disable=SC2016
    helper='!f() { if [ "$1" = get ]; then printf "username=x-access-token\\npassword="; cat /data/secrets/github-token; printf "\\n"; fi; }; f'
    checkout_git config --file "$WS_DATA/.gitconfig" credential.https://github.com.helper "$helper" || return 1
  fi
  name=$(checkout_git -C "$CHECKOUT_TARGET" config --get user.name 2>/dev/null) || name=''
  email=$(checkout_git -C "$CHECKOUT_TARGET" config --get user.email 2>/dev/null) || email=''
  if [ -z "$name$email" ]; then
    # GitHub's verified bot identity is delivered with the credential; never infer its user ID from an App ID.
    [ -s "$WS_SECRETS/GITHUB_BOT_IDENTITY" ] || return 0
    local identity bot_id bot_login
    # Read the verified ID and login as one bundle; separate file updates must not mix two identities.
    identity=$(cat "$WS_SECRETS/GITHUB_BOT_IDENTITY") || return 1
    bot_id=$(jq -er '.id | select(type == "string" and test("^[1-9][0-9]*$"))' <<<"$identity" 2>/dev/null) || return 1
    bot_login=$(jq -er '.login | select(type == "string" and test("^[A-Za-z0-9][A-Za-z0-9_.-]*\\[bot\\]$"))' <<<"$identity" 2>/dev/null) || return 1
    checkout_git config --file "$WS_DATA/.gitconfig" user.name "$bot_login" &&
      checkout_git config --file "$WS_DATA/.gitconfig" user.email "$bot_id+$bot_login@users.noreply.github.com" || return 1
  fi
}

CHECKOUT_READY=false CHECKOUT_REASON='' CHECKOUT_TOKEN_DIGEST='' CHECKOUT_ID_CHECKED=false
keep_checkout() {
  local origin tracked=false temp marker_id clone_mode=0700
  CHECKOUT_READY=false
  CHECKOUT_TARGET=$WS_DATA/workspace
  if [ "$MEMBER_RUNNER" = true ]; then
    CHECKOUT_TARGET=$WS_WORKSPACE
    clone_mode=0711
  fi
  CHECKOUT_MARKER=${WS_DATA%/*}/checkout.json
  temp=$CHECKOUT_TARGET.arca-clone
  checkout_token || { checkout_wait 'the credential copy failed; it will be tried again'; return; }
  if [ ! -e "$CHECKOUT_TARGET/.git" ]; then
    # Only the reserved, disposable clone is removed. Other target contents are never touched.
    if [ -d "$CHECKOUT_TARGET" ]; then
      local entries
      entries=$(find "$CHECKOUT_TARGET" -mindepth 1 -maxdepth 1 -print -quit)
      [ -z "$entries" ] || { checkout_wait 'the target is not empty and has no .git; keep its data and choose an empty checkout'; return; }
    elif [ -e "$CHECKOUT_TARGET" ] || [ -L "$CHECKOUT_TARGET" ]; then
      checkout_wait 'the target is not a directory; keep its data and choose an empty checkout'; return
    fi
    due checkout || return 0
    rm -rf -- "$temp"
    rm -f -- "$CHECKOUT_MARKER.new"
    [ -s "$WS_RUN-github/token" ] || { failed checkout; checkout_wait 'checkout waits for GITHUB_TOKEN; Arca must mint a repository token'; return; }
    install -d -m "$clone_mode" -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" "$temp" || return 1
    local helper
    # Path is configuration, never the credential. A single-quoted shell word protects it in Git's helper shell.
    helper="!f() { if [ \"\$1\" = get ]; then printf 'username=x-access-token\\npassword='; cat '${WS_RUN//\'/\'\\\'\'}-github/token'; printf '\\n'; fi; }; f"
    local ref=()
    [ -z "$REPO_REF" ] || ref=(--branch "$REPO_REF")
    if ! checkout_git -c credential.helper= -c "credential.helper=$helper" clone "${ref[@]}" -- "$REPO_URL" "$temp" >/dev/null 2>&1; then
      rm -rf -- "$temp"; rm -f -- "$CHECKOUT_MARKER.new"
      failed checkout
      checkout_wait 'clone failed (credential, permission or network); Arca must supply a token with repository access; retrying after a pause'
      return
    fi
    checkout_tracked_roster "$temp" && tracked=true
    checkout_repo_id || CHECKOUT_REMOTE_ID=null
    if ! checkout_marker_new "$tracked"; then rm -rf -- "$temp"; return 1; fi
    # rmdir is the final empty-target check: no file that arrived while cloning can be replaced.
    if { [ ! -d "$CHECKOUT_TARGET" ] || rmdir -- "$CHECKOUT_TARGET"; } && mv -- "$temp" "$CHECKOUT_TARGET"; then
      mv -- "$CHECKOUT_MARKER.new" "$CHECKOUT_MARKER" || return 1
    else
      rm -rf -- "$temp"; rm -f -- "$CHECKOUT_MARKER.new"
      checkout_wait 'the target changed during clone; its data is kept; retry with an empty target'; return
    fi
  fi
  origin=$(checkout_git -C "$CHECKOUT_TARGET" config --get remote.origin.url 2>/dev/null) || origin=''
  origin=$(github_identity "$origin") || { checkout_wait 'origin is not a GitHub URL; set the intended origin before starting'; return; }
  [ "$origin" = "$REPO_IDENTITY" ] || { checkout_wait "origin is $origin, repoUrl is $REPO_IDENTITY; choose the intended repository before starting"; return; }
  if [ ! -e "$CHECKOUT_MARKER" ] && checkout_marker_valid "$CHECKOUT_MARKER.new"; then
    mv -- "$CHECKOUT_MARKER.new" "$CHECKOUT_MARKER" || return 1
  fi
  if [ -e "$CHECKOUT_MARKER" ] && ! checkout_marker_valid "$CHECKOUT_MARKER"; then
    # Never overwrite an earlier corrupt marker: it may be needed for owner recovery.
    [ ! -e "$CHECKOUT_MARKER.corrupt" ] || { checkout_wait 'checkout marker is corrupt; the earlier corrupt marker is kept for the owner'; return; }
    mv -- "$CHECKOUT_MARKER" "$CHECKOUT_MARKER.corrupt" || return 1
  fi
  if [ ! -e "$CHECKOUT_MARKER" ]; then
    if checkout_has_roster; then
      if checkout_current_roster_tracked; then
        checkout_wait 'foreign roster tracked in the checkout; the roster is kept and the seed is blocked'
      else
        checkout_wait 'roster provenance unknown: marker missing; the roster is kept'
      fi
      # Receipt verification and the owner's recovery screen are UNAVAILABLE. Never infer provenance.
      return
    fi
    checkout_repo_id || CHECKOUT_REMOTE_ID=null
    checkout_marker_new false && mv -- "$CHECKOUT_MARKER.new" "$CHECKOUT_MARKER" || return 1
  fi
  if [ "$CHECKOUT_ID_CHECKED" != true ]; then
    if checkout_repo_id; then
      marker_id=$(jq -r '.repoId' "$CHECKOUT_MARKER")
      if [ "$marker_id" != null ] && [ "$marker_id" != "$CHECKOUT_REMOTE_ID" ]; then
        checkout_wait 'repository id changed; keep this checkout and ask the owner to select the intended repository'; return
      fi
      if [ "$marker_id" = null ]; then
        (umask 077; jq --argjson id "$CHECKOUT_REMOTE_ID" '.repoId=$id' "$CHECKOUT_MARKER" >"$CHECKOUT_MARKER.new") &&
          mv -- "$CHECKOUT_MARKER.new" "$CHECKOUT_MARKER" || return 1
      fi
    else
      say 'checkout repository id check skipped: GitHub is unavailable; the origin matches; checked again at the next boot'
    fi
    CHECKOUT_ID_CHECKED=true
  fi
  if [ "$(jq -r '.rosterTrackedAtClone' "$CHECKOUT_MARKER")" = true ] ||
    [ "$(jq -r '.rosterBeforeSeed' "$CHECKOUT_MARKER")" = true ]; then
    checkout_wait 'foreign roster predates this seed; the roster is kept and the seed is blocked'; return
  fi
  if checkout_has_roster; then
    if [ "$(jq -r '.seedStartedAt' "$CHECKOUT_MARKER")" = null ]; then
      (umask 077; jq '.rosterBeforeSeed=true' "$CHECKOUT_MARKER" >"$CHECKOUT_MARKER.new") &&
        mv -- "$CHECKOUT_MARKER.new" "$CHECKOUT_MARKER" || return 1
      checkout_wait 'foreign roster is present before seed; the roster is kept and the seed is blocked'; return
    fi
    if ! checkout_roster_valid; then
      checkout_wait 'foreign roster is not the expected roster format; the roster is kept and the seed is blocked'; return
    fi
  fi
  checkout_authorship || { checkout_wait 'Git authentication or authorship could not be configured; retry after fixing the configuration'; return; }
  CHECKOUT_READY=true CHECKOUT_REASON=''
  proven checkout
}

# Persist provenance BEFORE docker run: the entrypoint may seed before docker returns.
checkout_before_seed() {
  local before=false
  [ "$CHECKOUT_READY" = true ] || return 1
  if [ "$(jq -r '.seedStartedAt' "$CHECKOUT_MARKER")" = null ]; then
    checkout_has_roster && before=true
    (umask 077; jq --argjson before "$before" --argjson at "$(now)" \
      '.rosterBeforeSeed=$before | .seedStartedAt=(if $before then null else $at end)' \
      "$CHECKOUT_MARKER" >"$CHECKOUT_MARKER.new") && mv -- "$CHECKOUT_MARKER.new" "$CHECKOUT_MARKER" || return 1
    [ "$before" = false ] || { checkout_wait 'foreign roster present before the first server start; the roster is kept'; return; }
  fi
}

# ---------------------------------------------------------------------------------------------------------------------
# The three parts
# ---------------------------------------------------------------------------------------------------------------------

declare -A NEXT_TRY=() PAUSE=()

# Whether a part that failed may be started again now.
due() {
  [ "$(now)" -ge "${NEXT_TRY[$1]:-0}" ]
}

# A part failed (it did not start, or it stopped): its next start waits, twice as long each time, up to
# WS_BACKOFF_MAX.
failed() {
  local pause=${PAUSE[$1]:-5}
  [ "$pause" -le "$WS_BACKOFF_MAX" ] || pause=$WS_BACKOFF_MAX
  NEXT_TRY[$1]=$(($(now) + pause))
  PAUSE[$1]=$((pause * 2))
}

# A part was seen working (the server or the gate healthy, the tunnel registered), or is given a new start: no pause
# is owed.
proven() {
  NEXT_TRY[$1]=0
  PAUSE[$1]=5
}

# What Docker says of a container: "running", "exited <code>", "absent", or "unknown" when Docker could not be asked
# (a daemon that is restarting, a call that failed). Nothing is ever removed or started on "unknown": a container
# is only acted on when Docker has said that it stopped or that it is not there.
state_of() {
  local out
  # Docker's words on its standard error are read only when it failed: a warning beside a good answer is not one.
  if out=$(docker inspect --type container -f '{{.State.Running}} {{.State.ExitCode}}' "$1" 2>"$WS_RUN/docker.err"); then
    case $out in
      'true '*) echo running ;;
      'false '[0-9]*) echo "exited ${out#false }" ;;
      *) echo unknown ;;
    esac
  else
    out=$(cat "$WS_RUN/docker.err" 2>/dev/null)
    case ${out,,} in
      *'no such object'* | *'no such container'*) echo absent ;;
      *) echo unknown ;;
    esac
  fi
}

running() {
  [ "$(state_of "$1")" = running ]
}

image_of() {
  docker inspect --type container -f '{{.Config.Image}}' "$1" 2>/dev/null
}

# The last line of what docker said, on one line and bounded. No secret is on a docker command line here, so none is
# in its answer.
last_line() {
  printf '%s\n' "$1" | tail -n 1 | tr -d '\000-\037' | cut -c1-300
}

# The parts that follow the server: the gate, the tunnel and the relay live in its network namespace, and the tailnet
# node reaches the gate through the relay. They are removed, and start again beside the server.
leave_server_parts() {
  docker rm -f "$GATE" "$TUNNEL" "$RELAY" "$TAILNET" >/dev/null 2>&1
  rm -f "$WS_RUN/tunnel-token.sha256" "$WS_RUN/tailnet-key.sha256"
  REGISTERED=''
  RELAY_IP=''
}

# The network namespace a running container is in, as the kernel names it (net:[<number>]), from its first process.
# Nothing when Docker or /proc cannot say: then nothing is done about it.
netns_of() {
  local pid
  pid=$(docker inspect --type container -f '{{.State.Pid}}' "$1" 2>/dev/null) || return 0
  [[ $pid =~ ^[1-9][0-9]*$ ]] || return 0
  readlink "$WS_PROC/$pid/ns/net" 2>/dev/null
  return 0
}

# A part that joins the server's namespace keeps the namespace the server had when the part started. A server that
# Docker started again in place (`docker restart`, or a start after a stop the keeper did not see) has a new one,
# and the part runs on in the old one: healthy on its own loopback, and reaching nothing (the gate answers HTTP 502,
# "DSH unavailable"). A part found so is removed with the others, and they start again beside the server.
leave_stale_namespace() {
  local server part ns
  server=$(netns_of "$SERVER")
  [ -n "$server" ] || return 0
  for part in "$GATE" "$TUNNEL" "$RELAY"; do
    running "$part" || continue
    ns=$(netns_of "$part")
    if [ -n "$ns" ] && [ "$ns" != "$server" ]; then
      say "$part is not in the server's network namespace: the server was started again beside it, so it reaches nothing. The gate and the $(reach_part) are started again beside the server"
      leave_server_parts
      return 0
    fi
  done
}

# The gate and the tunnel live in the server's network namespace. A server container that stopped or is gone took
# the namespace with it, so both are removed with it and start again beside the new one. The server is given no
# public name: see the head of this file.
keep_server() {
  local state out image
  state=$(state_of "$SERVER")
  case $state in
    running)
      leave_stale_namespace
      if [ "$CHECKOUT_READY" = true ] && [ -n "$FOLLOW_STATE" ] &&
        image=$(image_of "$SERVER") && [ -n "$image" ] && [ "$image" != "$SERVER_IMAGE" ]; then
        leave_server_parts
        if docker rm -f "$SERVER" >/dev/null 2>&1; then
          proven server
          proven gate
        fi
      fi
      return 0
      ;;
    unknown) return 0 ;;
  esac
  leave_server_parts
  if [ "$state" != absent ]; then
    say "the server stopped (exit code ${state#exited }); it is started again after a pause, and the gate and the $(reach_part) with it"
    docker rm -f "$SERVER" >/dev/null 2>&1
    failed server
    return 0
  fi
  [ "$CHECKOUT_READY" = true ] || return 0
  due server || return 0
  server_image_here || return 0
  say "starting the server"
  local mounts=(-v "$WS_DATA:/data")
  # With the member runner: the workspace, at the same path as on the machine, over /data's own workspace folder, and
  # the runner's socket folder. --mount, not -v: a folder that is missing stops the start instead of being made, as
  # root's, by Docker.
  [ "${MEMBER_RUNNER:-false}" != true ] ||
    mounts+=(--mount "type=bind,source=$WS_WORKSPACE,target=$WS_WORKSPACE"
      --mount "type=bind,source=$WS_RUNNER_SOCKETS,target=$WS_RUNNER_SOCKETS")
  checkout_before_seed || return 0
  if out=$(docker run -d --name "$SERVER" --pull never --restart no "${mounts[@]}" \
    --user "$WS_DATA_OWNER" -e "HOME=$NATIVE_HOME" \
    -e "AUTOMAGIK_HOME=$AUTOMAGIK_HOME" -e "AUTOMAGIK_BOTS_HOME=$AUTOMAGIK_BOTS_HOME" \
    -e "DSH_HOME=$DSH_HOME" -e "HERDR_APP_HOME=$HERDR_APP_HOME" \
    -e "AUTOMAGIK_WORKSPACE=$AUTOMAGIK_WORKSPACE" -e "HC_TERMINAL_SOCKET=$HC_TERMINAL_SOCKET" \
    -e "AUTOMAGIK_PNPM=$AUTOMAGIK_PNPM" "$SERVER_IMAGE" 2>&1 >/dev/null); then
    say "the server is started from $SERVER_IMAGE"
  else
    docker rm -f "$SERVER" >/dev/null 2>&1
    failed server
    say "the server did not start: $(last_line "$out")"
  fi
}

# The server's image on this machine, pulled only once its build provenance verifies. An image that is here already
# was pulled that way: every container made from it is started with --pull never. When it does not verify or cannot be
# pulled, that is said, the server waits a pause, and the status line says why the server is not started.
server_image_here() {
  local out why=''
  docker image inspect "$SERVER_IMAGE" >/dev/null 2>&1 && return 0
  say "verifying the server image's build provenance before pulling it: $SERVER_IMAGE, built on GitHub by a workflow of $PROVENANCE_REPO"
  if [ ! -x "$WS_COSIGN" ]; then
    why="the verifier $WS_COSIGN is not on this machine: the first boot did not finish"
  elif ! out=$(verify_provenance "$SERVER_IMAGE" 2>&1 >/dev/null); then
    out=$(last_line "$out")
    why="cosign says: ${out:-nothing}"
  fi
  if [ -n "$why" ]; then
    failed server
    UNVERIFIED="the server is not started: its image's build provenance did not verify, so the image is not pulled ($why)"
    say "$UNVERIFIED. It is tried again after a pause; when it keeps failing, the image is not one Automagik's release workflow built"
    return 1
  fi
  UNVERIFIED=''
  say "the server image's build provenance verifies: built on GitHub by a workflow of $PROVENANCE_REPO; pulling it"
  if ! out=$(docker pull -q "$SERVER_IMAGE" 2>&1 >/dev/null); then
    failed server
    say "the server's image could not be pulled: $(last_line "$out")"
    return 1
  fi
}

# cosign's check of the server's image, with a home of its own on the machine's memory for Sigstore's root, which it
# refreshes from Sigstore's own address. It needs no account: the image and its attestation are public.
verify_provenance() {
  mkdir -p "$WS_RUN/cosign" &&
    HOME="$WS_RUN/cosign" "$WS_COSIGN" verify-attestation --type slsaprovenance1 \
      --certificate-oidc-issuer "$PROVENANCE_ISSUER" \
      --certificate-identity-regexp "^https://github\\.com/$PROVENANCE_REPO/" \
      --certificate-github-workflow-repository "$PROVENANCE_REPO" "$1"
}

# The gate: the same image and digest as the server, the gate's entrypoint, the server's namespace, a file system it
# cannot write but for its own /data, and no capability. It reads the server's data read-only, for the server's
# sign-in credential.
keep_gate() {
  local state out
  [ -n "$PUBLIC_URL" ] || return 0
  running "$SERVER" || return 0
  state=$(state_of "$GATE")
  case $state in
    running | unknown) return 0 ;;
    exited*)
      say "the gate stopped (exit code ${state#exited }); it is started again after a pause. The gate says nothing of its settings in its own output; when it keeps stopping, the usual causes are a server image that has no gate, or settings its gate does not take"
      docker rm -f "$GATE" >/dev/null 2>&1
      failed gate
      return 0
      ;;
  esac
  due gate || return 0
  local args=(run -d --name "$GATE" --pull never --network "container:$SERVER" --restart no --read-only --cap-drop ALL
    --security-opt no-new-privileges:true --entrypoint "$IMAGE_NODE"
    -e "$GATE_ENABLED_ENV=1" -e "$GATE_ORIGIN_ENV=$PUBLIC_URL" -e "$GATE_PORT_ENV=$GATE_PORT")
  [ "$ACCESS_COUNT" -eq 0 ] || args+=(-e "$ACCESS_TEAM_ENV=$ACCESS_TEAM" -e "$ACCESS_AUDIENCE_ENV=$ACCESS_AUDIENCE"
    -e "$ACCESS_EMAILS_ENV=$ACCESS_EMAILS")
  # On a tailnet the first owner claims the gate on its main port, behind the gate's code: the node forwards that
  # port and no other, so the gate's claim-only port (3083) is never opened, forwarded or published.
  [ "$REACH" != tailnet ] || args+=(-e "$GATE_CLAIM_ENV=remote")
  args+=(-v "$WS_GATE_DATA:/data" -v "$WS_DATA:/dsh:ro")
  say "starting the gate"
  if out=$(docker "${args[@]}" "$SERVER_IMAGE" "$GATE_ENTRY" 2>&1 >/dev/null); then
    say "the gate is started from $SERVER_IMAGE"
  else
    docker rm -f "$GATE" >/dev/null 2>&1
    failed gate
    say "the gate did not start: $(last_line "$out")"
  fi
}

# A secret's digest, from its file without its line breaks. The secret itself never leaves the pipe.
secret_digest() {
  tr -d '[:space:]' <"$WS_SECRETS/$1" 2>/dev/null | sha256sum | cut -d' ' -f1
}

token_digest() {
  secret_digest TUNNEL_TOKEN
}

# The part that lets people reach the workspace, as the status line names it.
reach_part() {
  if [ "${REACH:-cloudflare}" = tailnet ]; then echo "tailnet node"; else echo tunnel; fi
}

# The digest of nothing: what secret_digest gives for a file that holds no secret.
EMPTY_DIGEST=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
TUNNEL_TRIED=''
REGISTERED=''

# Whether a connector may run. A gate without the Access settings signs nobody in through the tunnel; it waits for
# a first claim on a port of its own (3083) on the loopback the connector shares. No route may ever name that port,
# so beside such a gate the connector is not started at all: there is nothing it could usefully reach.
tunnel_has_a_door() {
  [ -z "$PUBLIC_URL" ] || [ "$ACCESS_COUNT" -gt 0 ]
}

keep_tunnel() {
  local dir=$WS_RUN/tunnel digest state out
  [ "$REACH" = cloudflare ] || return 0
  tunnel_has_a_door || return 0
  running "$SERVER" || return 0
  [ -e "$WS_SECRETS/TUNNEL_TOKEN" ] || return 0
  digest=$(token_digest)
  [ "$digest" != "$EMPTY_DIGEST" ] || return 0
  state=$(state_of "$TUNNEL")
  case $state in
    unknown) return 0 ;;
    running)
      [ "$digest" != "$(cat "$WS_RUN/tunnel-token.sha256" 2>/dev/null)" ] || return 0
      say "TUNNEL_TOKEN changed: the tunnel is started again with the new one"
      docker rm -f "$TUNNEL" >/dev/null 2>&1
      ;;
    exited*)
      say "the tunnel stopped (exit code ${state#exited }); it is started again after a pause. When it keeps stopping, the usual cause is a token Cloudflare does not accept: set TUNNEL_TOKEN again (service_secret_set, then instance_start)"
      docker rm -f "$TUNNEL" >/dev/null 2>&1
      failed tunnel
      ;;
  esac
  # A token that was not tried yet does not wait for the pause its predecessor earned.
  [ "$digest" = "$TUNNEL_TRIED" ] || proven tunnel
  due tunnel || return 0
  TUNNEL_TRIED=$digest
  REGISTERED=''
  rm -f "$WS_RUN/tunnel-token.sha256"
  # The token's copy: memory only, and the tunnel's user alone enters the directory and reads the file.
  if ! install -d -m 0700 -o "${WS_TUNNEL_OWNER%%:*}" -g "${WS_TUNNEL_OWNER##*:}" "$dir" ||
    ! (umask 077 && tr -d '[:space:]' <"$WS_SECRETS/TUNNEL_TOKEN" >"$dir/token.new") ||
    ! chown "$WS_TUNNEL_OWNER" "$dir/token.new" || ! chmod 0400 "$dir/token.new" ||
    ! mv -f "$dir/token.new" "$dir/token"; then
    rm -f "$dir/token.new"
    failed tunnel
    say "the tunnel did not start: the token's copy could not be written under $WS_RUN"
    return 0
  fi
  say "starting the tunnel (Docker downloads its image first when it is not on this machine yet)"
  if out=$(docker run -d --name "$TUNNEL" --network "container:$SERVER" --restart no --read-only --cap-drop ALL \
    --security-opt no-new-privileges:true --user "$WS_TUNNEL_OWNER" -v "$dir:/run/arca-tunnel:ro" "$TUNNEL_IMAGE" \
    tunnel --no-autoupdate --metrics 127.0.0.1:20241 run --token-file /run/arca-tunnel/token 2>&1 >/dev/null); then
    printf '%s\n' "$digest" >"$WS_RUN/tunnel-token.sha256"
    say "the tunnel is started from $TUNNEL_IMAGE"
  else
    docker rm -f "$TUNNEL" >/dev/null 2>&1
    failed tunnel
    say "the tunnel did not start: $(last_line "$out")"
  fi
}

TAILNET_TRIED=''
RELAY_IP=''

# The server's address on the relay's private network; nothing when it has none.
relay_address() {
  docker inspect --type container -f "{{with index .NetworkSettings.Networks \"$RELAY_NET\"}}{{.IPAddress}}{{end}}" \
    "$SERVER" 2>/dev/null | tr -cd '0-9.'
}

# The network mode Docker recorded for a container when it was made: "container:<name>" for one that joined
# another's namespace.
network_mode() {
  docker inspect --type container -f '{{.HostConfig.NetworkMode}}' "$1" 2>/dev/null
}

# With reach "tailnet": the private network, the server's place on it, and the relay. The network is internal (no
# way out of it and none in) and holds two addresses, the server's and the node's. The relay runs in the server's
# namespace, where the gate's loopback is, and listens on the server's address on that network and nowhere else.
keep_relay() {
  local state out address internal
  [ "$REACH" = tailnet ] || return 0
  running "$SERVER" || return 0
  state=$(state_of "$RELAY")
  case $state in
    unknown) return 0 ;;
    running)
      [ -n "$RELAY_IP" ] || RELAY_IP=$(relay_address)
      return 0
      ;;
    exited*)
      say "the relay to the gate stopped (exit code ${state#exited }); it is started again after a pause"
      docker rm -f "$RELAY" >/dev/null 2>&1
      failed relay
      return 0
      ;;
  esac
  due relay || return 0
  RELAY_IP=''
  # The network is the keeper's own making, and internal. One of that name that is not internal is not trusted to
  # be private: it is removed and made again.
  internal=$(docker network inspect -f '{{.Internal}}' "$RELAY_NET" 2>/dev/null)
  if [ -n "$internal" ] && [ "$internal" != true ]; then
    say "a Docker network named $RELAY_NET is there and is not internal: it is removed and made again"
    if ! out=$(docker network rm "$RELAY_NET" 2>&1 >/dev/null); then
      failed relay
      say "the relay to the gate did not start: that network could not be removed: $(last_line "$out")"
      return 0
    fi
    internal=''
  fi
  if [ -z "$internal" ] && ! out=$(docker network create --internal "$RELAY_NET" 2>&1 >/dev/null); then
    failed relay
    say "the relay to the gate did not start: its private network could not be made: $(last_line "$out")"
    return 0
  fi
  address=$(relay_address)
  if [ -z "$address" ]; then
    out=$(docker network connect "$RELAY_NET" "$SERVER" 2>&1 >/dev/null)
    address=$(relay_address)
  fi
  if ! [[ $address =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]]; then
    failed relay
    say "the relay to the gate did not start: the server has no address on its private network: $(last_line "${out-}")"
    return 0
  fi
  say "starting the relay to the gate"
  if out=$(docker run -d --name "$RELAY" --pull never --network "container:$SERVER" --restart no --read-only --cap-drop ALL \
    --security-opt no-new-privileges:true --entrypoint "$IMAGE_NODE" "$SERVER_IMAGE" \
    -e "$RELAY_PROGRAM" "$address" "$GATE_PORT" 2>&1 >/dev/null); then
    RELAY_IP=$address
    proven relay
    say "the relay to the gate is started: $address:$GATE_PORT on the private network, to the gate's loopback port"
  else
    docker rm -f "$RELAY" >/dev/null 2>&1
    failed relay
    say "the relay to the gate did not start: $(last_line "$out")"
  fi
}

# The tailnet node, when settings.reach is "tailnet": in a network namespace of its own (see the head of this file
# for why it must be), on Docker's default network for its way out to the tailnet and on the relay's private network
# for its one way in to the workspace. Its serve file, written here, is all it serves: HTTPS on port 443 of its own
# tailnet name, every path handed to the relay, which leads to the gate's port alone. There is no other port in it
# and no Funnel. Tailscale's proxy keeps the visitor's Host header for such a target, which the gate needs, and
# carries WebSockets.
keep_tailnet() {
  local dir=$WS_RUN/tailnet digest state out mode
  [ "$REACH" = tailnet ] || return 0
  running "$SERVER" || return 0
  state=$(state_of "$TAILNET")
  # A node that is not in a namespace of its own (one that joined the server's, or the machine's) is never left
  # running, whatever else is true. Docker's own record of how it was made is what is asked.
  mode=$(network_mode "$TAILNET")
  if [ "$state" != absent ] && [ "$state" != unknown ] && [ -n "$mode" ] && [ "$mode" != bridge ]; then
    say "the tailnet node is not in a network namespace of its own ($mode), where every loopback port it shares would be open to the tailnet: it is removed and started again in a namespace of its own"
    docker rm -f "$TAILNET" >/dev/null 2>&1
    rm -f "$WS_RUN/tailnet-key.sha256"
    state=absent
  fi
  [ -e "$WS_SECRETS/TAILSCALE_AUTHKEY" ] || return 0
  digest=$(secret_digest TAILSCALE_AUTHKEY)
  [ "$digest" != "$EMPTY_DIGEST" ] || return 0
  case $state in
    unknown) return 0 ;;
    running)
      [ "$digest" != "$(cat "$WS_RUN/tailnet-key.sha256" 2>/dev/null)" ] || return 0
      say "TAILSCALE_AUTHKEY changed: the tailnet node is started again with the new one"
      docker rm -f "$TAILNET" >/dev/null 2>&1
      ;;
    exited*)
      say "the tailnet node stopped (exit code ${state#exited }); it is started again after a pause. When it keeps stopping, the usual cause is a key the tailnet does not take (used already, expired or revoked): set TAILSCALE_AUTHKEY again (service_secret_set, then instance_start)"
      docker rm -f "$TAILNET" >/dev/null 2>&1
      failed tailnet
      ;;
  esac
  # The node is started only once its one way to the gate is there.
  [ -n "$RELAY_IP" ] && running "$RELAY" || return 0
  # A key that was not tried yet does not wait for the pause its predecessor earned.
  [ "$digest" = "$TAILNET_TRIED" ] || proven tailnet
  due tailnet || return 0
  TAILNET_TRIED=$digest
  rm -f "$WS_RUN/tailnet-key.sha256"
  # The key's copy and the serve file: memory only, and the node's user alone enters the directory.
  if ! install -d -m 0700 -o "${WS_TAILNET_OWNER%%:*}" -g "${WS_TAILNET_OWNER##*:}" "$dir" ||
    ! (umask 077 && tr -d '[:space:]' <"$WS_SECRETS/TAILSCALE_AUTHKEY" >"$dir/authkey.new") ||
    ! chown "$WS_TAILNET_OWNER" "$dir/authkey.new" || ! chmod 0400 "$dir/authkey.new" ||
    ! mv -f "$dir/authkey.new" "$dir/authkey" ||
    ! (umask 077 && printf '%s\n' "{\"TCP\":{\"443\":{\"HTTPS\":true}},\"Web\":{\"\${TS_CERT_DOMAIN}:443\":{\"Handlers\":{\"/\":{\"Proxy\":\"http://$RELAY_IP:$GATE_PORT\"}}}}}" >"$dir/serve.json") ||
    ! chown "$WS_TAILNET_OWNER" "$dir/serve.json"; then
    rm -f "$dir/authkey.new"
    failed tailnet
    say "the tailnet node did not start: the key's copy could not be written under $WS_RUN"
    return 0
  fi
  say "starting the tailnet node (Docker downloads its image first when it is not on this machine yet)"
  # --network bridge: a namespace of its own, on Docker's default network. The key is named by its file; with
  # TS_AUTH_ONCE a node that is already logged in does not use it again.
  if out=$(docker run -d --name "$TAILNET" --network bridge --restart no --cap-drop ALL \
    --security-opt no-new-privileges:true \
    -e TS_AUTHKEY=file:/run/arca-tailnet/authkey -e TS_AUTH_ONCE=true -e TS_USERSPACE=true \
    -e "TS_STATE_DIR=$TAILNET_STATE_DIR" -e "TS_SOCKET=$TAILNET_SOCKET" -e "TS_HOSTNAME=$TAILNET_NAME" \
    -e TS_SERVE_CONFIG=/run/arca-tailnet/serve.json -e TS_ACCEPT_DNS=false \
    -v "$WS_TAILNET_STATE:$TAILNET_STATE_DIR" -v "$dir:/run/arca-tailnet:ro" "$TAILNET_IMAGE" 2>&1 >/dev/null) &&
    out=$(docker network connect "$RELAY_NET" "$TAILNET" 2>&1 >/dev/null); then
    printf '%s\n' "$digest" >"$WS_RUN/tailnet-key.sha256"
    say "the tailnet node is started from $TAILNET_IMAGE"
  else
    docker rm -f "$TAILNET" >/dev/null 2>&1
    failed tailnet
    say "the tailnet node did not start: $(last_line "$out")"
  fi
}

# The gate's claim code, read from the gate's own container: exactly this one file and nothing else. NOTHING IN THIS
# SCRIPT CALLS IT, and no command of this script reaches it. It is the one place for whoever is to show the code to
# the workspace's owner, once, after the owner has proved who they are; the code must never reach a console, a log
# or an assistant. A code is good for fifteen minutes; inside the gate's container,
# `/nodejs/bin/node /opt/automagik/gate-control.mjs renew-claim` makes a new one.
# shellcheck disable=SC2329
read_claim_code() {
  docker exec "$GATE" "$IMAGE_NODE" -e \
    "process.stdout.write(require('fs').readFileSync('$CLAIM_CODE_FILE','utf8'))"
}

# ---------------------------------------------------------------------------------------------------------------------
# The state, in words
# ---------------------------------------------------------------------------------------------------------------------

# The image's own check, run inside a container: it asks that container's loopback port and prints the HTTP code
# (000 for no answer). In the gate's container it asks the gate's port, in the server's the server's.
health_code() {
  local code
  code=$(docker exec "$1" "$IMAGE_NODE" "$HEALTH_ENTRY" 2>/dev/null | tail -n 1 | tr -cd '0-9')
  printf '%s' "${code:-000}"
}

# What one part's check means, in words: $1 the part's name in the status line, $2 its container, $3 its port.
# Each look sets one variable rather than printing, so what it learns (a part seen working) stays with the keeper.
health_words() {
  local code
  case $(state_of "$2") in
    running) ;;
    unknown)
      WORDS="Docker did not answer about the $1"
      return
      ;;
    *)
      WORDS="the $1 is not running"
      return
      ;;
  esac
  code=$(health_code "$2")
  case $code in
    401)
      proven "$1"
      WORDS="the $1 is healthy (HTTP 401 on its loopback port $3)"
      ;;
    000) WORDS="the $1 is running and does not answer on its loopback port $3 yet" ;;
    200) WORDS="the $1 is NOT healthy: its loopback port $3 answers HTTP 200 without a sign-in, so its sign-in fence is not up" ;;
    *) WORDS="the $1 is NOT healthy: its loopback port $3 answers HTTP $code, and a healthy $1 answers 401" ;;
  esac
}

look_at_server() {
  health_words server "$SERVER" "$SERVER_PORT"
  SERVER_STATE=$WORDS
  [ -z "$UNVERIFIED" ] || [ "$WORDS" != "the server is not running" ] || SERVER_STATE=$UNVERIFIED
}

look_at_gate() {
  if [ -z "$PUBLIC_URL" ]; then
    GATE_STATE="no gate is started, because no public address is set (settings.publicUrl is empty)"
    return
  fi
  health_words gate "$GATE" "$GATE_PORT"
  GATE_STATE=$WORDS
}

# What the tailnet node says of itself, without its key: whether it is logged in, and under which name. A node that
# is not logged in after a start says so; its usual cause is a key the tailnet does not take.
look_at_tailnet() {
  local status backend name
  if [ ! -e "$WS_SECRETS/TAILSCALE_AUTHKEY" ]; then
    TUNNEL_STATE="the tailnet node waits for TAILSCALE_AUTHKEY (service_secret_set, then instance_start)"
    return
  elif [ "$(secret_digest TAILSCALE_AUTHKEY)" = "$EMPTY_DIGEST" ]; then
    TUNNEL_STATE="the tailnet node waits: TAILSCALE_AUTHKEY is empty (service_secret_set, then instance_start)"
    return
  fi
  case $(state_of "$TAILNET") in
    running) ;;
    unknown)
      TUNNEL_STATE="Docker did not answer about the tailnet node"
      return
      ;;
    *)
      TUNNEL_STATE="the tailnet node is not running"
      return
      ;;
  esac
  case $(network_mode "$TAILNET") in
    bridge) ;;
    '')
      TUNNEL_STATE="Docker did not answer about the tailnet node"
      return
      ;;
    *)
      TUNNEL_STATE="the tailnet node is NOT in a network namespace of its own, so it is not serving: it is removed and started again"
      return
      ;;
  esac
  status=$(docker exec "$TAILNET" tailscale --socket "$TAILNET_SOCKET" status --json 2>/dev/null)
  backend=$(printf '%s' "$status" | jq -r '.BackendState // empty' 2>/dev/null | tr -cd 'A-Za-z')
  name=$(printf '%s' "$status" | jq -r '.Self.DNSName // empty' 2>/dev/null | tr -cd 'a-z0-9.-')
  name=${name%.}
  case $backend in
    Running)
      if [ "https://$name" = "$PUBLIC_URL" ] && ! running "$RELAY"; then
        TUNNEL_STATE="the tailnet node is logged in as $name, and the relay to the gate is not running"
      elif [ "https://$name" = "$PUBLIC_URL" ]; then
        proven tailnet
        TUNNEL_STATE="the tailnet node is logged in and serves https://$name"
      else
        TUNNEL_STATE="the tailnet node is logged in as ${name:-a name it does not tell}, which is NOT the name in settings.publicUrl: the gate refuses every request under another name. Free the name on the tailnet, or create the workspace again with the name the node has"
      fi
      ;;
    NeedsLogin) TUNNEL_STATE="the tailnet node is NOT logged in: the tailnet did not take its key (used already, expired or revoked). Set TAILSCALE_AUTHKEY again (service_secret_set, then instance_start)" ;;
    NeedsMachineAuth) TUNNEL_STATE="the tailnet node is NOT logged in yet: it waits for approval in the tailnet's admin console. A pre-approved key needs none" ;;
    *) TUNNEL_STATE="the tailnet node is running and is not logged in yet" ;;
  esac
}

# Whether the gate has its owner, from the one thing the gate shows of it: its claim code's file is there until the
# first claim and gone after it. Only the file's name and age are looked at, from the machine's side of the gate's
# data; the code in it is never read.
claim_state() {
  local file=$WS_GATE_DATA/${CLAIM_CODE_FILE##*/} made
  if ! made=$(stat -c %Y "$file" 2>/dev/null); then
    echo "claimed"
  elif [ $(($(now) - made)) -ge $((WS_CLAIM_MINUTES * 60)) ]; then
    echo "waiting for its first claim, with a claim code that is past its $WS_CLAIM_MINUTES minutes: a new one has to be made"
  else
    echo "waiting for its first claim"
  fi
}

look_at_tunnel() {
  if [ "$REACH" = tailnet ]; then
    look_at_tailnet
    return
  fi
  if ! tunnel_has_a_door; then
    TUNNEL_STATE="the tunnel is not started: without the Cloudflare Access settings nobody could sign in through it"
    return
  fi
  if [ ! -e "$WS_SECRETS/TUNNEL_TOKEN" ]; then
    TUNNEL_STATE="the tunnel waits for TUNNEL_TOKEN (service_secret_set, then instance_start)"
  elif [ "$(token_digest)" = "$EMPTY_DIGEST" ]; then
    TUNNEL_STATE="the tunnel waits: TUNNEL_TOKEN is empty (service_secret_set, then instance_start)"
  elif [ "$(state_of "$TUNNEL")" = unknown ]; then
    TUNNEL_STATE="Docker did not answer about the tunnel"
  elif ! running "$TUNNEL"; then
    TUNNEL_STATE="the tunnel is not running"
  else
    # cloudflared says this line once for each connection it opens to Cloudflare.
    if [ -z "$REGISTERED" ] && docker logs "$TUNNEL" 2>&1 | grep -q 'Registered tunnel connection'; then
      REGISTERED=1
      proven tunnel
    fi
    if [ -n "$REGISTERED" ]; then
      TUNNEL_STATE="the tunnel is running and has registered with Cloudflare"
    else
      TUNNEL_STATE="the tunnel is running and has not registered with Cloudflare yet"
    fi
  fi
}

public_state() {
  if [ "$REACH" = tailnet ]; then
    if [ "$GATE_STATE" = "${GATE_STATE#the gate is healthy}" ]; then
      echo "reach tailnet: $PUBLIC_URL, sign-in by passkey"
    else
      echo "reach tailnet: $PUBLIC_URL, sign-in by passkey, $(claim_state)"
    fi
  elif [ -z "$PUBLIC_URL" ]; then
    echo "reach cloudflare: no public address"
  elif [ "$ACCESS_COUNT" -gt 0 ]; then
    echo "reach cloudflare: public address $PUBLIC_URL, sign-in by Cloudflare Access (team $ACCESS_TEAM, $ACCESS_COUNT e-mail address(es))"
  else
    echo "reach cloudflare: public address $PUBLIC_URL, with no Cloudflare Access settings, so nobody can sign in through the tunnel"
  fi
}

# ---------------------------------------------------------------------------------------------------------------------
# The keeper
# ---------------------------------------------------------------------------------------------------------------------

STOP=''
KEEPING=''
KEEPER=''
WORDS=''
SERVER_STATE=''
UNVERIFIED=''
GATE_STATE=''
TUNNEL_STATE=''

# Follow state lives beside the data mounts and is written before containers are replaced. After a restart, a move
# or rollback still being checked gets a fresh health window. Retired images are removed after the check settles.
FOLLOW_STATE='' FOLLOW_CHECKING='' FOLLOW_DEADLINE=0 FOLLOW_NEXT=0 SERVER_VERSION=''

follow_use() {
  local fields
  fields=$(jq -r '[.current.ref, .current.version, (.checking // "")] | join("|")' <<<"$1") || return 1
  IFS='|' read -r SERVER_IMAGE SERVER_VERSION FOLLOW_CHECKING <<<"$fields"
  FOLLOW_STATE=$1
  FOLLOW_DEADLINE=$(($(now) + WS_FOLLOW_HEALTH))
}

follow_read() {
  local state version
  if [ ! -e "$WS_FOLLOW_STATE" ]; then
    [ "${SERVER_IMAGE%@sha256:*}" = "$LANE_IMAGE" ] || return 0
    # Settings from before lane following have no install version to compare with.
    if ! version=$(jq -er --arg version "$VERSION_PATTERN" '.serverVersion | select(type == "string" and test($version))' "$WS_SETTINGS"); then
      return 0
    fi
    state=$(jq -cn --arg ref "$SERVER_IMAGE" --arg version "$version" \
      '{schema: 1, current: {ref: $ref, version: $version}}') || return 1
  else
    state=$(jq -ces --arg image "$LANE_IMAGE" --arg version "$VERSION_PATTERN" '
      def digest: type == "string" and startswith($image + "@") and
        (ltrimstr($image + "@") | test("\\Asha256:[0-9a-f]{64}\\z"));
      def release: type == "object" and (.ref | digest) and (.version | type == "string" and test($version));
      select(length == 1) | .[0] |
      select(.schema == 1 and (.current | release) and (.previous == null or (.previous | release)) and
        (.retired == null or (.retired | release)) and (.refused == null or (.refused | digest)) and
        (.checking == null or (.checking == "move" and .previous != null) or .checking == "back"))
      ' "$WS_FOLLOW_STATE" 2>/dev/null) || return 1
  fi
  follow_use "$state"
}

follow_write() {
  if (umask 077 && printf '%s\n' "$1" >"$WS_FOLLOW_STATE.new" && mv -f "$WS_FOLLOW_STATE.new" "$WS_FOLLOW_STATE"); then
    follow_use "$1"
  else
    rm -f "$WS_FOLLOW_STATE.new"
    say "follow: could not save $WS_FOLLOW_STATE; keeping $SERVER_VERSION"
    return 1
  fi
}

# Print the digest ref and version of a single, bounded release record. Callers report any failure on one line.
follow_record() {
  local out
  if ! out=$(curl -fsSL --proto '=https' --proto-redir '=https' --connect-timeout 30 --max-time 60 \
    --max-filesize 65536 "$1" 2>&1); then
    last_line "$out" >&2
    return 1
  fi
  jq -ers --arg image "$LANE_IMAGE" --arg version "$VERSION_PATTERN" '
    select(length == 1) | .[0] |
    select(.schema == 1 and .image == $image and
      (.digest | type == "string" and test("\\Asha256:[0-9a-f]{64}\\z")) and
      .ref == (.image + "@" + .digest) and (.version | type == "string" and test($version)))
    | .ref + " " + .version' <<<"$out" 2>/dev/null || {
    printf '%s\n' 'invalid server-image.json record' >&2
    return 1
  }
}

follow_above() {
  local a b part
  IFS=. read -r -a a <<<"$1"
  IFS=. read -r -a b <<<"$2"
  for part in 0 1 2; do
    if [ "${a[part]}" -ne "${b[part]}" ]; then
      [ "${a[part]}" -gt "${b[part]}" ]
      return
    fi
  done
  return 1
}

follow_step() {
  local lane fixed ref version out state before=$SERVER_VERSION
  if ! lane=$(follow_record "$SERVER_RELEASES/server-lane/server-image.json" 2>&1); then
    say "follow: the server lane could not be read: $lane"
    return
  fi
  read -r ref version <<<"$lane"
  if [ "$ref" = "$SERVER_IMAGE" ]; then
    say "follow: the server lane names $version, which already runs"
    return
  fi
  if ! follow_above "$version" "$SERVER_VERSION"; then
    say "follow: the server lane names $version; keeping $SERVER_VERSION"
    return
  fi
  if [ "$(jq -r '.refused // ""' <<<"$FOLLOW_STATE")" = "$ref" ]; then
    say "follow: $version ($ref) was already rolled back"
    return
  fi
  if ! fixed=$(follow_record "$SERVER_RELEASES/v$version/server-image.json" 2>&1); then
    say "follow: the fixed release record for $version could not be read: $fixed"
    return
  fi
  if [ "$fixed" != "$lane" ]; then
    say "follow: the fixed release record disagrees with the server lane for $version; keeping $SERVER_VERSION"
    return
  fi
  if ! out=$(verify_provenance "$ref" 2>&1 >/dev/null); then
    say "follow: build provenance for $version failed; leaving the image unpulled: $(last_line "$out")"
    return
  fi
  if ! out=$(docker pull -q "$ref" 2>&1 >/dev/null); then
    say "follow: the verified image for $version could not be pulled: $(last_line "$out")"
    return
  fi
  state=$(jq -c --arg ref "$ref" --arg version "$version" '
    .retired = .previous | .previous = .current | .current = {ref: $ref, version: $version} | .checking = "move"
    ' <<<"$FOLLOW_STATE") || return 1
  follow_write "$state" &&
    say "follow: verified and pulled $version; moving from $before on the same data, with $WS_FOLLOW_HEALTH seconds to become healthy"
}

follow_healthy() {
  [[ $SERVER_STATE = 'the server is healthy '* ]] && [ "$(image_of "$SERVER")" = "$SERVER_IMAGE" ] || return 1
  [ -z "$PUBLIC_URL" ] ||
    { [[ $GATE_STATE = 'the gate is healthy '* ]] && [ "$(image_of "$GATE")" = "$SERVER_IMAGE" ]; }
}

follow_settle() {
  local state retired out cleanup='' version=$SERVER_VERSION phase=$FOLLOW_CHECKING
  if [ "$phase:$1" = move:unhealthy ]; then
    state=$(jq -c '
      .current as $failed | .current = .previous | .previous = .retired |
      .retired = $failed | .refused = $failed.ref | .checking = "back"' <<<"$FOLLOW_STATE") || return 1
    follow_write "$state" &&
      say "follow: $version was not healthy ($SERVER_STATE; $GATE_STATE); rolling back to $SERVER_VERSION on the same data"
    return
  fi
  retired=$(jq -r 'if .retired.ref != .current.ref and .retired.ref != .previous.ref then .retired.ref // "" else "" end' \
    <<<"$FOLLOW_STATE")
  if [ -n "$retired" ] && ! out=$(docker image rm "$retired" 2>&1 >/dev/null); then
    cleanup="; could not remove $retired: $(last_line "$out")"
  fi
  state=$(jq -c 'del(.retired, .checking)' <<<"$FOLLOW_STATE") || return 1
  follow_write "$state" || return 1
  case $phase:$1 in
    move:healthy) say "follow: $version is healthy on the same data; the previous image stays available for rollback$cleanup" ;;
    back:healthy) say "follow: back on $version, healthy$cleanup" ;;
    back:unhealthy) say "follow: back on $version, still unhealthy ($SERVER_STATE; $GATE_STATE)$cleanup" ;;
  esac
}

keep_follow() {
  [ -n "$FOLLOW_STATE" ] || return 0
  if [ -n "$FOLLOW_CHECKING" ]; then
    if follow_healthy; then
      follow_settle healthy
    elif [ "$(now)" -ge "$FOLLOW_DEADLINE" ]; then
      follow_settle unhealthy
    fi
  elif [ "$CHECKOUT_READY" = true ] && [ "$(now)" -ge "$FOLLOW_NEXT" ] && follow_healthy; then
    follow_step
    FOLLOW_NEXT=$(($(now) + WS_FOLLOW_EVERY))
  fi
}

# The keeping itself. It runs as a child of the keeper and takes no signal: the keeper ends it. It tells the keeper,
# once, that nothing keeps the workspace from starting any more.
keep_workspace() {
  local missing state last='' said=0
  trap - TERM INT USR1
  # Whatever keeps the workspace from starting is said at intervals by a keeper that stays up: one that exits is
  # started again by systemd every few seconds, with a line on the console each time.
  until missing=$(missing_programs) && [ -z "$missing" ]; do
    say "workspace $WS_NAME does not start: these commands are missing on this machine: $missing. The first boot did not finish; /var/log/cloud-init-output.log inside the machine says why"
    nap "$WS_REFUSE_EVERY"
  done
  until load_settings; do
    say "workspace $WS_NAME does not start: its settings cannot be used: $REFUSAL. They are fixed when the machine is created: service_remove it and create it again with settings that are right"
    nap "$WS_REFUSE_EVERY"
  done
  until mkdir -p "$WS_RUN" && chmod 0700 "$WS_RUN" && data_directories; do
    say "workspace $WS_NAME does not start: $WS_RUN or one of its data directories under ${WS_DATA%/*} could not be made"
    nap "$WS_REFUSE_EVERY"
  done
  until follow_read; do
    say "workspace $WS_NAME does not start: the follow state $WS_FOLLOW_STATE cannot be read"
    nap "$WS_REFUSE_EVERY"
  done
  kill -USR1 "$$"
  if [ "$REACH" = tailnet ]; then
    say "workspace $WS_NAME: the server and the gate on loopback port $GATE_PORT in one network namespace, and the tailnet node in one of its own, reaching the gate through the relay alone; $(public_state)"
  else
    say "workspace $WS_NAME: the server, the gate on loopback port $GATE_PORT and the tunnel, in one network namespace; $(public_state)"
  fi
  while :; do
    if ! keep_checkout && [ -z "$CHECKOUT_REASON" ]; then
      checkout_wait 'checkout files could not be prepared; fix disk space and directory permissions before retrying'
    fi
    keep_server
    keep_gate
    keep_relay
    keep_tunnel
    keep_tailnet
    look_at_server
    look_at_gate
    look_at_tunnel
    state="$SERVER_STATE; $GATE_STATE; $TUNNEL_STATE; $(public_state)"
    if [ "$state" != "$last" ] || [ $(($(now) - said)) -ge "$WS_STATUS_EVERY" ]; then
      say "status: $state"
      last=$state
      said=$(now)
    fi
    keep_follow
    nap "$WS_TICK"
  done
}

# The keeper: the process systemd stops. From the line that sets its traps on it runs NO command substitution, and
# that is the whole reason the keeping is a child of it. bash 5.2 runs a trap that is waiting while it looks for the
# end of a $( ) in a word, and reads the trap's own text as if it stood inside that $( ): the text does not parse,
# nothing is run and the signal is gone. A keeper that kept the workspace itself, with a trap that set a flag, lost
# one stop in several that way and ran on until systemd killed it. So this process only waits. A stop ends the child
# at once, wherever it is: a look that the stop interrupted says nothing, and a container being started is Docker's
# to finish. The child has no trap, so it has no text to lose.
run_workspace() {
  local ended again
  : "${WS_NAME:=$(hostname)}"
  trap 'STOP=1; [ -z "$KEEPER" ] || kill "$KEEPER" 2>/dev/null' TERM INT
  trap 'KEEPING=1' USR1
  keep_workspace &
  KEEPER=$!
  while [ -z "$STOP" ]; do
    wait "$KEEPER"
    ended=$?
    # wait also returns when a trap ran. Only a child that is gone ends the keeper, with the child's own code, as a
    # keeper that kept the workspace itself would have ended. A wait that a trap cut short just as the child ended
    # is asked again for that code.
    if [ -z "$STOP" ] && ! kill -0 "$KEEPER" 2>/dev/null; then
      wait "$KEEPER"
      again=$?
      [ "$again" = 127 ] || ended=$again
      # The child has no process number any more: nothing is to be sent to that number from here on.
      KEEPER=''
      # A child that a SIGTERM ended (128 + 15) was stopped with the keeper. systemd signals every process of the
      # service, and it may reach the child before the keeper: that is a stop that was asked for, and the keeper ends
      # as it does for its own signal. A SIGTERM that reaches the child alone, with no stop asked, is told the same
      # way, as a stop, and systemd then starts the keeper again.
      [ "$ended" = 143 ] || exit "$ended"
      STOP=1
    fi
  done
  # A stop that came before the child had a name, or before the child had put down the traps it was born with, has
  # not ended it yet.
  while [ -n "$KEEPER" ] && kill "$KEEPER" 2>/dev/null; do
    sleep 0.05
  done
  # A keeper that was still saying why the workspace does not start stops without a word, as it always has.
  [ -z "$KEEPING" ] || say "the keeper stops; the three containers run on until the machine stops them"
  exit 0
}

case ${1-} in
  '') install_workspace ;;
  run) run_workspace ;;
  *)
    say "usage: $0 [run]"
    exit 2
    ;;
esac
