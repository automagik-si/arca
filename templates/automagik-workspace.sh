#!/bin/bash
# The automagik-workspace service (templates/automagik-workspace.json): one virtual machine that runs one Automagik
# workspace, the gate in front of it and the Cloudflare Tunnel connector that publishes it. This one file is both
# halves of it.
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
#   The server is never told its public name. The gate speaks to it as loopback, so it needs none, and without one it
#   refuses (HTTP 403) every request that comes under a public name: a route in Cloudflare that names the server's
#   port instead of the gate's shows that refusal at once, and never the workspace without its gate.
#   Plain `docker run` gives this shape; Docker Compose is not installed.
#
# What it reads:
# - /etc/arca/settings.json: the service's settings, written by cloud-init at the first boot and fixed from then on.
#   publicUrl is the workspace's exact public origin (https://<host>, no path); the shipped example address means
#   "not set". accessTeamDomain, accessAudience and accessEmails are the owner's Cloudflare Access application for
#   that address (none of them a secret): all three are set, or none. serverImage is the server's image by digest.
# - /etc/arca-secrets/TUNNEL_TOKEN: the tunnel's token, written by Arca at each start (service_secret_set, then
#   instance_start). The server and the gate start without it; the tunnel waits for it, and a new token replaces the
#   tunnel only.
#
# What it writes: one line on the machine's console (service_logs) whenever the state of a part changes, and the same
# status line at intervals. The server and the gate are healthy when the image's own check says so: HTTP 401 on the
# loopback port, the answer to a visitor who has not signed in.
#
# The token is never printed or put on a command line. The keeper never holds it in a variable: it is copied, without
# line breaks, to a file on /run (memory) that only the tunnel's user reads, and cloudflared reads that file
# (`--token-file`), so it is not in Docker's record of the container either. The keeper never reads the server's or
# the gate's data, and it never copies their output to the console: the server's sign-in link is in both.
#
# Every path, pause and external program below can be replaced through the environment, which is how the test in
# crates/arca/tests/automagik_workspace.rs drives the keeper against a fake docker.
set -u

: "${WS_SETTINGS:=/etc/arca/settings.json}"
: "${WS_SECRETS:=/etc/arca-secrets}"
: "${WS_DATA:=/var/lib/automagik-workspace/data}"
: "${WS_GATE_DATA:=/var/lib/automagik-workspace/gate}"
: "${WS_RUN:=/run/automagik-workspace}"
: "${WS_CONSOLE:=/dev/console}"
: "${WS_INSTALLED:=/usr/local/sbin/arca-automagik-workspace}"
: "${WS_UNIT:=/etc/systemd/system/arca-automagik-workspace.service}"
: "${WS_DOCKER_ETC:=/etc/docker}"
: "${WS_DATA_OWNER:=1000:1000}"      # the server image's own user, which the gate runs as too
: "${WS_TUNNEL_OWNER:=65532:65532}"  # the user the tunnel is run as; it alone reads the token's copy
: "${WS_TICK:=10}"            # seconds between two looks at the parts
: "${WS_STATUS_EVERY:=300}"   # seconds between two status lines when nothing changes
: "${WS_REFUSE_EVERY:=300}"   # seconds between two tellings of why the workspace does not start
: "${WS_BACKOFF_MAX:=300}"    # the longest pause before a part that keeps failing is started again, in seconds
: "${WS_APT_PAUSE:=20}"       # seconds between two tries of the first boot's package install

# The address the shipped template carries for settings.publicUrl. It is nobody's, and means "not set".
EXAMPLE_URL='https://workspace.example.com'
SERVER=automagik-server
GATE=automagik-gate
TUNNEL=automagik-tunnel

# What this script takes from the Automagik server image, each in this one place, as the Automagik repository has
# them at commit 8fb2e6f (server/compose.gate.yml, server/gate-entrypoint.mjs, server/gate-access.mjs): the build the
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

# The connector's image, by digest: the one templates/cloudflare-tunnel.json pins (cloudflared 2026.10.7). It is not a
# setting: a workspace does not choose its connector.
TUNNEL_IMAGE=docker.io/cloudflare/cloudflared@sha256:4955524aeaa379e154023d5a027c250f7b35041a2e7e950cf13f144bc9b9c5ba

# Every program this script starts that bash does not have built in. The first boot refuses to go on while one of
# them is missing, and names it.
WS_PROGRAMS='docker jq systemctl apt-get hostname sha256sum install tr cut date sleep cat chmod chown mv rm grep
tail mkdir'

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
  load_settings || boot_say "first boot: the settings cannot be used: $REFUSAL. The workspace will not start; service_remove it and create it again from a template whose settings are right"
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
  install -d -m 0750 -o "${WS_DATA_OWNER%%:*}" -g "${WS_DATA_OWNER##*:}" "$WS_DATA" "$WS_GATE_DATA"
}

# ---------------------------------------------------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------------------------------------------------

REFUSAL=''
refuse() {
  REFUSAL=$*
  return 1
}

# Reads the settings into PUBLIC_URL, SERVER_IMAGE and the ACCESS_ values. When they cannot be used it fails, with
# the reason, in words, in REFUSAL.
load_settings() {
  local url port address='[A-Za-z0-9._%+-]+@[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?'
  REFUSAL=''
  if ! jq -e 'type == "object"' "$WS_SETTINGS" >/dev/null 2>&1; then
    refuse "the settings are missing or are not one JSON object ($WS_SETTINGS)"
    return
  fi
  url=$(jq -r '.publicUrl | if type == "string" then . else "" end' "$WS_SETTINGS")
  SERVER_IMAGE=$(jq -r '.serverImage | if type == "string" then . else "" end' "$WS_SETTINGS")
  # A missing key is an empty one, and so is an empty list. A value of the wrong kind becomes "!", which no rule
  # below takes. Each entry of the list is one whole address: an entry that holds a comma, a space or a line break
  # is not one, so the comma-separated value the gate gets has exactly as many addresses as the list has entries.
  ACCESS_TEAM=$(jq -r '.accessTeamDomain | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  ACCESS_AUDIENCE=$(jq -r '.accessAudience | if . == null then "" elif type == "string" then . else "!" end' "$WS_SETTINGS")
  ACCESS_EMAILS=$(jq -r --arg address "\\A$address\\z" '.accessEmails | if . == null or . == [] then "" elif type == "array" and length <= 16 and all(.[]; type == "string" and test($address)) then join(",") else "!" end' "$WS_SETTINGS" 2>/dev/null) || ACCESS_EMAILS='!'
  ACCESS_COUNT=0
  if ! pinned "$SERVER_IMAGE"; then
    refuse "settings.serverImage is an image by digest: <registry>/<name>@sha256:<64 hex digits>"
    return
  fi
  # The gate takes its origin exactly as a browser writes it, and stops when it is written another way: no path, no
  # closing slash, and a port only when it is not https's own 443.
  if [ "$url" = "$EXAMPLE_URL" ]; then
    PUBLIC_URL=''
  elif [[ $url =~ ^https://[a-z0-9]([a-z0-9.-]*[a-z0-9])?(:([1-9][0-9]{0,4}))?$ ]] &&
    port=${BASH_REMATCH[3]} && { [ -z "$port" ] || { [ "$port" -le 65535 ] && [ "$port" -ne 443 ]; }; }; then
    PUBLIC_URL=$url
  else
    refuse "settings.publicUrl is the workspace's exact public origin: https://<host name>, in lower case, with no path or closing slash, and a port only when it is not 443 (1 to 65535, written without a leading 0)"
    return
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
  if out=$(docker inspect -f '{{.State.Running}} {{.State.ExitCode}}' "$1" 2>&1); then
    case $out in
      'true '*) echo running ;;
      'false '[0-9]*) echo "exited ${out#false }" ;;
      *) echo unknown ;;
    esac
  else
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
  docker rm -f "$GATE" "$TUNNEL" >/dev/null 2>&1
  rm -f "$WS_RUN/tunnel-token.sha256"
  REGISTERED=''
  if [ "$state" != absent ]; then
    say "the server stopped (exit code ${state#exited }); it is started again after a pause, and the gate and the tunnel with it"
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

# The token's digest, from the secret file without its line breaks. The token itself never leaves the pipe.
token_digest() {
  tr -d '[:space:]' <"$WS_SECRETS/TUNNEL_TOKEN" 2>/dev/null | sha256sum | cut -d' ' -f1
}

# The digest of nothing: what token_digest gives for a file that holds no token.
EMPTY_DIGEST=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
TUNNEL_TRIED=''
REGISTERED=''

keep_tunnel() {
  local dir=$WS_RUN/tunnel digest state out
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
    GATE_STATE="no gate is started, because no public address is set (settings.publicUrl is the template's example)"
    return
  fi
  health_words gate "$GATE" "$GATE_PORT"
  GATE_STATE=$WORDS
}

look_at_tunnel() {
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
  if [ -z "$PUBLIC_URL" ]; then
    echo "no public address"
  elif [ "$ACCESS_COUNT" -gt 0 ]; then
    echo "public address $PUBLIC_URL, sign-in by Cloudflare Access (team $ACCESS_TEAM, $ACCESS_COUNT e-mail address(es))"
  else
    echo "public address $PUBLIC_URL, with no Cloudflare Access settings, so the gate has no sign-in to offer"
  fi
}

# ---------------------------------------------------------------------------------------------------------------------
# The keeper
# ---------------------------------------------------------------------------------------------------------------------

STOP=''
WORDS=''
SERVER_STATE=''
GATE_STATE=''
TUNNEL_STATE=''

run_workspace() {
  local missing state last='' said=0
  : "${WS_NAME:=$(hostname)}"
  trap 'STOP=1' TERM INT
  # Whatever keeps the workspace from starting is said at intervals by a keeper that stays up: one that exits is
  # started again by systemd every few seconds, with a line on the console each time.
  until missing=$(missing_programs) && [ -z "$missing" ]; do
    say "workspace $WS_NAME does not start: these commands are missing on this machine: $missing. The first boot did not finish; /var/log/cloud-init-output.log inside the machine says why"
    nap "$WS_REFUSE_EVERY"
    [ -z "$STOP" ] || exit 0
  done
  until load_settings; do
    say "workspace $WS_NAME does not start: its settings cannot be used: $REFUSAL. They are fixed when the machine is created: service_remove it and create it again from a template whose settings are right"
    nap "$WS_REFUSE_EVERY"
    [ -z "$STOP" ] || exit 0
  done
  until mkdir -p "$WS_RUN" && chmod 0700 "$WS_RUN" && data_directories; do
    say "workspace $WS_NAME does not start: $WS_RUN, $WS_DATA or $WS_GATE_DATA could not be made"
    nap "$WS_REFUSE_EVERY"
    [ -z "$STOP" ] || exit 0
  done
  say "workspace $WS_NAME: the server, the gate on loopback port $GATE_PORT and the tunnel, in one network namespace; $(public_state)"
  while [ -z "$STOP" ]; do
    keep_server
    keep_gate
    keep_tunnel
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
  say "the keeper stops; the three containers run on until the machine stops them"
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
