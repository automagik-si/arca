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
# What it reads:
# - /etc/arca/settings.json: the service's settings, written by cloud-init at the first boot and fixed from then on.
#   Those a service_create gave (the template lists which it may) stand in place of the template's own.
#   publicUrl is the workspace's exact public origin (https://<host>, no path); empty, as the template ships it,
#   means "not set". accessTeamDomain, accessAudience and accessEmails are the owner's Cloudflare Access application for
#   that address (none of them a secret): all three are set, or none. serverImage is the server's image by digest.
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
# Every path, pause and external program below can be replaced through the environment, which is how the test in
# crates/arca/tests/automagik_workspace.rs drives the keeper against a fake docker.
set -u

: "${WS_SETTINGS:=/etc/arca/settings.json}"
: "${WS_SECRETS:=/etc/arca-secrets}"
: "${WS_DATA:=/var/lib/automagik-workspace/data}"
: "${WS_GATE_DATA:=/var/lib/automagik-workspace/gate}"
: "${WS_TAILNET_STATE:=/var/lib/automagik-workspace/tailnet}"
: "${WS_RUN:=/run/automagik-workspace}"
: "${WS_CONSOLE:=/dev/console}"
: "${WS_INSTALLED:=/usr/local/sbin/arca-automagik-workspace}"
: "${WS_UNIT:=/etc/systemd/system/arca-automagik-workspace.service}"
: "${WS_DOCKER_ETC:=/etc/docker}"
: "${WS_DATA_OWNER:=1000:1000}"      # the server image's own user, which the gate runs as too
: "${WS_TUNNEL_OWNER:=65532:65532}"  # the user the tunnel is run as; it alone reads the token's copy
: "${WS_TAILNET_OWNER:=0:0}"         # the tailnet image's own user; it alone reads the auth key's copy
: "${WS_CLAIM_MINUTES:=15}"          # how long the gate's claim code is good for, as the gate fixes it
: "${WS_TICK:=10}"            # seconds between two looks at the parts
: "${WS_STATUS_EVERY:=300}"   # seconds between two status lines when nothing changes
: "${WS_REFUSE_EVERY:=300}"   # seconds between two tellings of why the workspace does not start
: "${WS_BACKOFF_MAX:=300}"    # the longest pause before a part that keeps failing is started again, in seconds
: "${WS_APT_PAUSE:=20}"       # seconds between two tries of the first boot's package install

SERVER=automagik-server
GATE=automagik-gate
TUNNEL=automagik-tunnel
TAILNET=automagik-tailnet
RELAY=automagik-relay
RELAY_NET=automagik-relay

# What this script takes from the Automagik server image, each in this one place, as the Automagik repository has
# them at commit 5fe0be6 (server/compose.gate.yml, server/gate-entrypoint.mjs, server/gate-access.mjs,
# server/gate-claim.mjs). Those four files are the same at 7aaa512, the commit of release 3.136.4, the build the
# shipped template pins. A later build that changes one of them changes one line here.
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

# The relay's whole program, for the server image's node: it listens on the one address and port it is given and
# copies bytes to and from the same port on loopback. Bytes only: the visitor's Host header reaches the gate as the
# node sent it.
RELAY_PROGRAM='const net=require("net");const [ip,port]=process.argv.slice(1);net.createServer(c=>{const u=net.connect(Number(port),"127.0.0.1");c.on("error",()=>u.destroy());u.on("error",()=>c.destroy());c.pipe(u).pipe(c)}).listen(Number(port),ip)'

# Every program this script starts that bash does not have built in. The first boot refuses to go on while one of
# them is missing, and names it.
WS_PROGRAMS='docker jq systemctl apt-get hostname sha256sum install tr cut date sleep cat chmod chown mv rm grep
tail mkdir stat'

# The Debian packages the first boot installs. Recommended packages are not installed, so every package that is needed
# is named here. docker.io is the daemon alone: on Debian 13 the `docker` command is a package of its own, docker-cli,
# which docker.io only recommends. The ones the image already carries are named too, so the list says what the script
# stands on.
WS_PACKAGES='docker.io docker-cli jq ca-certificates coreutils'

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

install_workspace() {
  export DEBIAN_FRONTEND=noninteractive
  local try missing
  boot_say "first boot: installing $WS_PACKAGES"
  for try in 1 2 3 4 5 6; do
    # shellcheck disable=SC2086
    if apt-get update -q && apt-get install -y -q --no-install-recommends $WS_PACKAGES; then
      break
    fi
    boot_say "first boot: the packages could not be installed (try $try of 6); trying again in $WS_APT_PAUSE seconds"
    sleep "$WS_APT_PAUSE"
  done
  missing=$(missing_programs)
  if [ -n "$missing" ]; then
    boot_say "first boot failed: these commands are missing after the install: $missing. The packages asked for were: $WS_PACKAGES. The package manager's own words are in /var/log/cloud-init-output.log inside the machine. The machine has no workspace; service_remove it and create it again once it can reach its package mirror"
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

# The server's /data and the gate's /data: two directories on the machine's own disk, the server user's. Only the
# directories themselves are touched; what the server and the gate keep in them is never read or changed.
data_directories() {
  install -d -m 0750 -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" "$WS_DATA" "$WS_GATE_DATA" || return 1
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
  # A missing key is an empty one, and so is an empty list. A value of the wrong kind becomes "!", which no rule
  # below takes. Each entry of the list is one whole address: an entry that holds a comma, a space or a line break
  # is not one, so the comma-separated value the gate gets has exactly as many addresses as the list has entries.
  ACCESS_TEAM=$(jq -r '.accessTeamDomain | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  ACCESS_AUDIENCE=$(jq -r '.accessAudience | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  ACCESS_EMAILS=$(jq -r --arg address "\\A$address\\z" '.accessEmails | if . == null or . == [] then "" elif type == "array" and length <= 16 and all(.[]; type == "string" and test($address)) then join(",") else "!" end' "$WS_SETTINGS" 2>/dev/null) || ACCESS_EMAILS='!'
  ACCESS_COUNT=0
  if [ "$REACH" != cloudflare ] && [ "$REACH" != tailnet ]; then
    REACH=cloudflare
    refuse "settings.reach is how the workspace is reached: \"cloudflare\" (a Cloudflare Tunnel; also when it is left out) or \"tailnet\" (a node on your tailnet)"
    return
  fi
  if ! pinned "$SERVER_IMAGE"; then
    refuse "settings.serverImage is an image by digest: <registry>/<name>@sha256:<64 hex digits>"
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
  if ! [[ $ACCESS_EMAILS =~ ^$address(,$address){0,15}$ ]]; then
    refuse "settings.accessEmails is a list of 1 to 16 e-mail addresses, one in each entry"
    return
  fi
  ACCESS_COUNT=$(jq -r '.accessEmails | length' "$WS_SETTINGS")
}

pinned() {
  [[ $1 =~ ^[a-z0-9][a-z0-9._/:-]*@sha256:[0-9a-f]{64}$ ]]
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

# The last line of what docker said, on one line and bounded. No secret is on a docker command line here, so none is
# in its answer.
last_line() {
  printf '%s\n' "$1" | tail -n 1 | tr -d '\000-\037' | cut -c1-300
}

# The gate and the tunnel live in the server's network namespace. A server container that stopped or is gone took
# the namespace with it, so both are removed with it and start again beside the new one. The server is given no
# public name: see the head of this file.
keep_server() {
  local state out
  state=$(state_of "$SERVER")
  case $state in
    running | unknown) return 0 ;;
  esac
  docker rm -f "$GATE" "$TUNNEL" "$RELAY" "$TAILNET" >/dev/null 2>&1
  rm -f "$WS_RUN/tunnel-token.sha256" "$WS_RUN/tailnet-key.sha256"
  REGISTERED=''
  RELAY_IP=''
  if [ "$state" != absent ]; then
    say "the server stopped (exit code ${state#exited }); it is started again after a pause, and the gate and the $(reach_part) with it"
    docker rm -f "$SERVER" >/dev/null 2>&1
    failed server
    return 0
  fi
  due server || return 0
  say "starting the server (Docker downloads its image first when it is not on this machine yet)"
  if out=$(docker run -d --name "$SERVER" --restart no -v "$WS_DATA:/data" "$SERVER_IMAGE" 2>&1 >/dev/null); then
    say "the server is started from $SERVER_IMAGE"
  else
    docker rm -f "$SERVER" >/dev/null 2>&1
    failed server
    say "the server did not start: $(last_line "$out")"
  fi
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
  local args=(run -d --name "$GATE" --network "container:$SERVER" --restart no --read-only --cap-drop ALL
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
  if out=$(docker run -d --name "$RELAY" --network "container:$SERVER" --restart no --read-only --cap-drop ALL \
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
GATE_STATE=''
TUNNEL_STATE=''

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
  kill -USR1 "$$"
  if [ "$REACH" = tailnet ]; then
    say "workspace $WS_NAME: the server and the gate on loopback port $GATE_PORT in one network namespace, and the tailnet node in one of its own, reaching the gate through the relay alone; $(public_state)"
  else
    say "workspace $WS_NAME: the server, the gate on loopback port $GATE_PORT and the tunnel, in one network namespace; $(public_state)"
  fi
  while :; do
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
